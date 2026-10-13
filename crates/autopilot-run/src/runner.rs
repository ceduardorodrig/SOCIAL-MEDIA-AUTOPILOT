//! The orchestrator: chain the stage crates for exactly one persona pack.
//!
//! The data flow is a single pass: harvest → triage → creation → editorial
//! review → guardrail → publish. Each stage records a [`StageOutcome`] that is
//! both printed by the caller and appended to the run's [`Journal`]. The pure
//! decisions (channel resolution, topic selection, editor fold, guardrail
//! partition) are separated from the effectful calls so they can be unit-tested
//! without any network.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use autopilot_core::{PersonaPack, Platform, load_persona, now_unix};
use autopilot_guardrail::{Verdict, evaluate};
use autopilot_harvest::{SearxngClient, Topic};
use autopilot_pipeline::{
    EditorOutcome, TopicScore, Variant, build_creation_prompt, build_editor_prompt,
    build_triage_prompt, kept_variants, parse_editor_response, parse_shortlist, parse_variants,
    review_or_keep,
};
use autopilot_providers::{JobKind, complete_with_chain, load_config};
use autopilot_publish::{
    BlueskyCredentials, CARD_HEIGHT, CARD_MIME_TYPE, CARD_TEMPLATE_VERSION, CARD_WIDTH, CardSpec,
    DEFAULT_BLUESKY_BASE_URL, DEFAULT_RSVG_BINARY, PostImage, Session, alt_text,
    build_post_record_with_images, create_post, create_session, now_rfc3339_millis, post_url,
    render_png, upload_blob,
};
use chrono::{Local, Timelike};

use crate::journal::Journal;

/// How long to wait for one LLM completion.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(300);

/// How far from a declared publishing window a run may still publish: the
/// scheduler fires a few minutes into the window's hour.
const WINDOW_TOLERANCE_MINUTES: i64 = 45;

/// How long to wait for one publishing API call.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(30);

/// Where generated cards are kept (gitignored, alongside the run journal).
const DEFAULT_CARD_DIR: &str = "var/cards";

/// Whether `autopilot run` attaches a generated card to a Bluesky post.
///
/// Text-only posting is the default; a card is opt-in per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CardAttachment {
    /// Never attach a card — the text-only path, unchanged.
    #[default]
    Off,
    /// Attach a card only when the variant carries an `image_prompt`.
    WhenPrompted,
    /// Attach a card to every post.
    Always,
}

impl CardAttachment {
    /// Stable, lower-case label for the console and the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            CardAttachment::Off => "off",
            CardAttachment::WhenPrompted => "prompted",
            CardAttachment::Always => "always",
        }
    }

    /// Whether a card should be generated for a variant with this image prompt.
    pub fn should_attach(self, image_prompt: &str) -> bool {
        match self {
            CardAttachment::Off => false,
            CardAttachment::WhenPrompted => !image_prompt.trim().is_empty(),
            CardAttachment::Always => true,
        }
    }
}

/// Options controlling one `autopilot run`.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Persona pack to execute.
    pub persona_path: PathBuf,
    /// Harvest query (defaults to the persona niche, humanised).
    pub query: Option<String>,
    /// Maximum number of harvested topics.
    pub limit: usize,
    /// SearXNG endpoint used by the harvest stage.
    pub endpoint: String,
    /// Restrict the run to a single platform channel (default: every enabled one).
    pub platform: Option<Platform>,
    /// Provider registry path (read only in a live run).
    pub config_path: PathBuf,
    /// Journal file (defaults to `var/runs/<run-id>.jsonl`).
    pub journal_path: Option<PathBuf>,
    /// When set, no LLM call, no GPU work, no network and no publish happen.
    pub dry_run: bool,
    /// Whether a generated post card is attached to the published post.
    pub card: CardAttachment,
}

/// How a stage ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    /// The stage did its work.
    Ok,
    /// The stage was intentionally not run (dry run, draft persona, no input).
    Skipped,
    /// The guardrail refused a draft.
    Rejected,
    /// A post was published.
    Published,
    /// The stage was described but not executed (dry run).
    DryRun,
    /// The stage failed (provider, parse or adapter error).
    Error,
}

impl StageStatus {
    /// Stable, lower-case label used by the console and the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            StageStatus::Ok => "ok",
            StageStatus::Skipped => "skipped",
            StageStatus::Rejected => "rejected",
            StageStatus::Published => "published",
            StageStatus::DryRun => "dry-run",
            StageStatus::Error => "error",
        }
    }
}

/// One stage decision.
#[derive(Debug, Clone)]
pub struct StageOutcome {
    pub stage: String,
    pub status: StageStatus,
    pub detail: String,
    pub platform: Option<String>,
    pub variant: Option<String>,
    /// What the editorial review changed, when the stage is `editor` (additive:
    /// absent for every other stage, so the journal shape is unchanged).
    pub changes: Option<Vec<String>>,
}

impl StageOutcome {
    fn new(stage: &str, status: StageStatus, detail: impl Into<String>) -> Self {
        Self {
            stage: stage.to_string(),
            status,
            detail: detail.into(),
            platform: None,
            variant: None,
            changes: None,
        }
    }

    fn with_platform(mut self, platform: Platform) -> Self {
        self.platform = Some(platform.as_str().to_string());
        self
    }

    fn with_variant(mut self, variant: &str) -> Self {
        self.variant = Some(variant.to_string());
        self
    }

    fn with_changes(mut self, changes: Vec<String>) -> Self {
        self.changes = Some(changes);
        self
    }
}

/// A post that was actually published.
#[derive(Debug, Clone)]
pub struct PublishedPost {
    pub platform: String,
    pub variant: String,
    pub uri: String,
    pub url: String,
}

/// The full result of a run: every decision plus what was published.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub run_id: String,
    pub persona: String,
    pub dry_run: bool,
    pub journal: PathBuf,
    pub stages: Vec<StageOutcome>,
    pub published: Vec<PublishedPost>,
    /// True when a stage failed (never set by a guardrail rejection alone).
    pub failed: bool,
}

/// Human-readable harvest query derived from the persona niche.
pub fn humanize_niche(niche: &str) -> String {
    niche
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Enabled channels for the run, optionally narrowed to one platform.
pub fn resolve_channels(persona: &PersonaPack, only: Option<Platform>) -> Vec<Platform> {
    let mut platforms: Vec<Platform> = persona
        .channels
        .iter()
        .filter(|channel| channel.enabled)
        .map(|channel| channel.platform)
        .filter(|platform| only.is_none_or(|wanted| wanted == *platform))
        .collect();
    platforms.dedup();
    platforms
}

/// Pick the highest-scoring topic that clears the persona's rubric floor.
///
/// The shortlist is already sorted by score descending, so the first match wins.
pub fn select_topic(shortlist: &[TopicScore], min_score: f32) -> Option<&TopicScore> {
    shortlist.iter().find(|topic| topic.score >= min_score)
}

/// Parse a `"HH:MM"` window into minutes since midnight.
fn parse_window(window: &str) -> Option<i64> {
    let (hours, minutes) = window.trim().split_once(':')?;
    let hours: i64 = hours.trim().parse().ok()?;
    let minutes: i64 = minutes.trim().parse().ok()?;
    if !(0..24).contains(&hours) || !(0..60).contains(&minutes) {
        return None;
    }
    Some(hours * 60 + minutes)
}

/// Minutes since local midnight (`0..=1439`).
fn local_minutes_now() -> i64 {
    let now = Local::now();
    i64::from(now.hour()) * 60 + i64::from(now.minute())
}

/// The declared window a run at `now` belongs to, if any.
///
/// The scheduler fires more often than the persona posts (it is aligned to the
/// `hl-infer` batch windows), so this gate is what turns "four fires a day" into
/// the persona's own cadence. A window is a point in time and the scheduler fires
/// a few minutes into its hour, hence the tolerance. A missed window is never
/// compensated with a burst — the run simply skips.
pub fn matching_window(windows: &[String], now: i64, tolerance: i64) -> Option<String> {
    windows
        .iter()
        .find(|window| {
            parse_window(window).is_some_and(|start| {
                let delta = (now - start).abs();
                delta <= tolerance || delta >= 24 * 60 - tolerance
            })
        })
        .cloned()
}

/// Split drafts into guardrail-approved and rejected (with the verdict).
pub fn partition_variants<'a>(
    persona: &PersonaPack,
    platform: Platform,
    variants: &'a [Variant],
) -> (Vec<&'a Variant>, Vec<(&'a Variant, Verdict)>) {
    let mut approved = Vec::new();
    let mut rejected = Vec::new();
    for variant in variants {
        let verdict = evaluate(persona, platform, &variant.text);
        if verdict.approved {
            approved.push(variant);
        } else {
            rejected.push((variant, verdict));
        }
    }
    (approved, rejected)
}

/// How many approved variants one platform publishes per slot.
///
/// **One.** Two near-identical variants posted in the same minute read as spam
/// and split the audience; the A/B comparison is therefore **sequential**, not
/// simultaneous (see [`select_variants`]).
const VARIANTS_PER_PLATFORM_PER_SLOT: usize = 1;

/// Choose which approved variants a slot publishes.
///
/// A slot releases at most `per_platform` variants per platform, starting at
/// `rotation` and wrapping, so consecutive slots **alternate** between the
/// approved variants: over time both styles are exercised and compared through
/// their metrics, while no single moment shows two near-identical posts.
pub fn select_variants<'a>(
    approved: &'a [(Platform, &'a Variant)],
    per_platform: usize,
    rotation: usize,
) -> Vec<(Platform, &'a Variant)> {
    let take = per_platform.max(1);
    let mut platforms: Vec<Platform> = Vec::new();
    for (platform, _) in approved {
        if !platforms.contains(platform) {
            platforms.push(*platform);
        }
    }

    let mut selected = Vec::new();
    for platform in platforms {
        let group: Vec<&Variant> = approved
            .iter()
            .filter(|(candidate, _)| *candidate == platform)
            .map(|(_, variant)| *variant)
            .collect();
        if group.is_empty() {
            continue;
        }
        let start = rotation % group.len();
        for offset in 0..take.min(group.len()) {
            selected.push((platform, group[(start + offset) % group.len()]));
        }
    }
    selected
}

/// Deterministic per-slot rotation: the last digit of the run id (a Unix
/// timestamp) makes consecutive slots alternate which variant they publish.
fn rotation_offset(run_id: &str) -> usize {
    run_id
        .chars()
        .rev()
        .find_map(|c| c.to_digit(10))
        .map_or(0, |digit| digit as usize)
}

/// Execute the pipeline for one persona pack.
pub async fn run(options: RunOptions) -> Result<RunReport> {
    let persona = load_persona(&options.persona_path).await?;
    validate_or_bail(&persona)?;

    let run_id = format!("{}-{}", persona.persona, now_unix());
    let journal_path = options
        .journal_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("var/runs/{run_id}.jsonl")));
    let journal = Journal::new(
        run_id.clone(),
        persona.persona.clone(),
        journal_path.clone(),
    );

    let mut report = RunReport {
        run_id,
        persona: persona.persona.clone(),
        dry_run: options.dry_run,
        journal: journal_path,
        stages: Vec::new(),
        published: Vec::new(),
        failed: false,
    };

    // A draft pack is validated but never executed for real (spec invariant).
    if !persona.active && !options.dry_run {
        record(
            &mut report,
            &journal,
            StageOutcome::new(
                "run",
                StageStatus::Skipped,
                "persona is a draft (active: false) — set active: true to execute the pipeline",
            ),
        )
        .await?;
        return Ok(report);
    }

    let channels = resolve_channels(&persona, options.platform);
    if channels.is_empty() {
        bail!(
            "persona `{}` has no enabled channel to run{}",
            persona.persona,
            platform_suffix(options.platform)
        );
    }

    // Cadence gate: a live run publishes only inside one of the persona's own
    // windows. The scheduler is aligned to the hl-infer batch windows and fires
    // more often than the persona posts, so this is what keeps the account at
    // its declared cadence instead of one post per fire.
    if !options.dry_run {
        let windows: Vec<String> = persona
            .channels
            .iter()
            .filter(|channel| channel.enabled)
            .filter(|channel| match options.platform {
                Some(platform) => platform == channel.platform,
                None => true,
            })
            .flat_map(|channel| channel.windows.clone())
            .collect();

        if !windows.is_empty()
            && matching_window(&windows, local_minutes_now(), WINDOW_TOLERANCE_MINUTES).is_none()
        {
            record(
                &mut report,
                &journal,
                StageOutcome::new(
                    "run",
                    StageStatus::Skipped,
                    format!(
                        "outside the publishing windows {windows:?} — waiting for the next one"
                    ),
                ),
            )
            .await?;
            return Ok(report);
        }
    }

    let query = options
        .query
        .clone()
        .unwrap_or_else(|| humanize_niche(&persona.niche));

    let topics = stage_harvest(&mut report, &journal, &options, &query).await?;

    // A dry run never calls an LLM, the GPU, the network or a publisher: it only
    // describes the decisions the live run would take.
    if options.dry_run {
        stage_dry_run_plan(&mut report, &journal, &persona, &channels, options.card).await?;
        return Ok(report);
    }

    let registry = load_config(&options.config_path).await?;
    let Some(topic_title) =
        stage_triage(&mut report, &journal, &persona, &topics, &registry).await?
    else {
        return Ok(report);
    };

    let drafts = stage_create(
        &mut report,
        &journal,
        &persona,
        &channels,
        &registry,
        &topic_title,
    )
    .await?;

    // Stage [3.2] — the editorial review runs before the guardrail so the
    // deterministic gate always validates the polished text, never the draft.
    let drafts = stage_editor(
        &mut report,
        &journal,
        &persona,
        &drafts,
        &registry,
        &topic_title,
    )
    .await?;

    let approved = stage_guardrail(&mut report, &journal, &persona, &drafts).await?;
    if approved.is_empty() {
        record(
            &mut report,
            &journal,
            StageOutcome::new(
                "publish",
                StageStatus::Skipped,
                "no draft cleared the guardrail — nothing was published",
            ),
        )
        .await?;
        return Ok(report);
    }

    stage_publish(&mut report, &journal, &options, &topic_title, &approved).await?;
    Ok(report)
}

/// Stage [1] — harvest. Returns the topics (empty in a dry run or on failure).
async fn stage_harvest(
    report: &mut RunReport,
    journal: &Journal,
    options: &RunOptions,
    query: &str,
) -> Result<Vec<Topic>> {
    if options.dry_run {
        record(
            report,
            journal,
            StageOutcome::new(
                "harvest",
                StageStatus::DryRun,
                format!(
                    "would search \"{query}\" via {} (limit {})",
                    options.endpoint, options.limit
                ),
            ),
        )
        .await?;
        return Ok(Vec::new());
    }

    match SearxngClient::new(options.endpoint.clone())
        .search(query, options.limit)
        .await
    {
        Ok(topics) => {
            record(
                report,
                journal,
                StageOutcome::new(
                    "harvest",
                    StageStatus::Ok,
                    format!(
                        "{} topic(s) for \"{query}\" via {}",
                        topics.len(),
                        options.endpoint
                    ),
                ),
            )
            .await?;
            Ok(topics)
        }
        Err(err) => {
            report.failed = true;
            record(
                report,
                journal,
                StageOutcome::new(
                    "harvest",
                    StageStatus::Error,
                    format!("harvest failed: {err:#}"),
                ),
            )
            .await?;
            Ok(Vec::new())
        }
    }
}

/// Describe the remaining stages without running them (dry run only).
async fn stage_dry_run_plan(
    report: &mut RunReport,
    journal: &Journal,
    persona: &PersonaPack,
    channels: &[Platform],
    card: CardAttachment,
) -> Result<()> {
    record(
        report,
        journal,
        StageOutcome::new(
            "triage",
            StageStatus::DryRun,
            format!(
                "would score the candidates against \"{}\" and pick the first above the rubric floor ({})",
                persona.niche, persona.rubric.min_score
            ),
        ),
    )
    .await?;

    for platform in channels {
        record(
            report,
            journal,
            StageOutcome::new(
                "create",
                StageStatus::DryRun,
                format!("would draft 2 A/B variants for {}", platform.as_str()),
            )
            .with_platform(*platform),
        )
        .await?;
    }

    record(
        report,
        journal,
        StageOutcome::new(
            "editor",
            StageStatus::DryRun,
            "would rewrite each draft as a sceptical editor (AI tells, facts preserved) before the guardrail",
        ),
    )
    .await?;

    record(
        report,
        journal,
        StageOutcome::new(
            "guardrail",
            StageStatus::DryRun,
            "would evaluate every draft deterministically (blocklist, banned phrases, hashtags, platform limit, emoji) before publishing",
        ),
    )
    .await?;

    for platform in channels {
        let card_note = if *platform == Platform::Bluesky && card != CardAttachment::Off {
            format!(" with a generated card (mode: {})", card.as_str())
        } else {
            String::new()
        };
        record(
            report,
            journal,
            StageOutcome::new(
                "publish",
                StageStatus::DryRun,
                format!(
                    "would publish the approved variants to {}{card_note}",
                    platform.as_str()
                ),
            )
            .with_platform(*platform),
        )
        .await?;
    }
    Ok(())
}

/// Stage [2] — triage. Returns the chosen topic title, or `None` to stop.
async fn stage_triage(
    report: &mut RunReport,
    journal: &Journal,
    persona: &PersonaPack,
    topics: &[Topic],
    registry: &autopilot_providers::ProvidersConfig,
) -> Result<Option<String>> {
    if topics.is_empty() {
        record(
            report,
            journal,
            StageOutcome::new(
                "triage",
                StageStatus::Skipped,
                "no candidate topics harvested — the next stage does not run",
            ),
        )
        .await?;
        return Ok(None);
    }

    let prompt = build_triage_prompt(persona, topics);
    let completion =
        match complete_with_chain(registry, &prompt, JobKind::Triage, COMPLETION_TIMEOUT).await {
            Ok(completion) => completion,
            Err(err) => {
                report.failed = true;
                record(
                    report,
                    journal,
                    StageOutcome::new(
                        "triage",
                        StageStatus::Error,
                        format!("provider chain failed: {err:#}"),
                    ),
                )
                .await?;
                return Ok(None);
            }
        };

    let shortlist = match parse_shortlist(&completion.text) {
        Ok(shortlist) => shortlist,
        Err(err) => {
            report.failed = true;
            record(
                report,
                journal,
                StageOutcome::new(
                    "triage",
                    StageStatus::Error,
                    format!("could not parse the shortlist: {err:#}"),
                ),
            )
            .await?;
            return Ok(None);
        }
    };

    let Some(topic) = select_topic(&shortlist, persona.rubric.min_score) else {
        let best = shortlist.first().map_or(0.0, |entry| entry.score);
        record(
            report,
            journal,
            StageOutcome::new(
                "triage",
                StageStatus::Skipped,
                format!(
                    "no candidate scored >= {} (best {:.2}); nothing to publish",
                    persona.rubric.min_score, best
                ),
            ),
        )
        .await?;
        return Ok(None);
    };

    let title = topic.title.clone();
    record(
        report,
        journal,
        StageOutcome::new(
            "triage",
            StageStatus::Ok,
            format!(
                "selected \"{title}\" (score {:.2}) via {} ({})",
                topic.score, completion.provider_id, completion.model
            ),
        ),
    )
    .await?;
    Ok(Some(title))
}

/// Stage [3] — creation. Returns the drafts grouped by platform.
async fn stage_create(
    report: &mut RunReport,
    journal: &Journal,
    persona: &PersonaPack,
    channels: &[Platform],
    registry: &autopilot_providers::ProvidersConfig,
    topic: &str,
) -> Result<Vec<(Platform, Vec<Variant>)>> {
    let mut drafts = Vec::new();
    for platform in channels {
        let prompt = build_creation_prompt(persona, *platform, topic);
        match complete_with_chain(registry, &prompt, JobKind::Create, COMPLETION_TIMEOUT).await {
            Ok(completion) => match parse_variants(&completion.text) {
                Ok(variants) => {
                    record(
                        report,
                        journal,
                        StageOutcome::new(
                            "create",
                            StageStatus::Ok,
                            format!(
                                "{} variant(s) via {} ({})",
                                variants.len(),
                                completion.provider_id,
                                completion.model
                            ),
                        )
                        .with_platform(*platform),
                    )
                    .await?;
                    drafts.push((*platform, variants));
                }
                Err(err) => {
                    report.failed = true;
                    record(
                        report,
                        journal,
                        StageOutcome::new(
                            "create",
                            StageStatus::Error,
                            format!("could not parse the variants: {err:#}"),
                        )
                        .with_platform(*platform),
                    )
                    .await?;
                }
            },
            Err(err) => {
                report.failed = true;
                record(
                    report,
                    journal,
                    StageOutcome::new(
                        "create",
                        StageStatus::Error,
                        format!("provider chain failed: {err:#}"),
                    )
                    .with_platform(*platform),
                )
                .await?;
            }
        }
    }
    Ok(drafts)
}

/// Stage [3.2] — editorial review. Returns the drafts the guardrail must see.
///
/// The review is a **best-effort polish layer**, so it degrades gracefully: when
/// the editor is unreachable or its answer cannot be parsed, the original drafts
/// are kept and the journal records `editor: skipped (reason)`. A transient
/// error therefore never stalls the pipeline, never costs a post, and is never
/// hidden — the run still goes through the guardrail, on the original text.
async fn stage_editor(
    report: &mut RunReport,
    journal: &Journal,
    persona: &PersonaPack,
    drafts: &[(Platform, Vec<Variant>)],
    registry: &autopilot_providers::ProvidersConfig,
    topic: &str,
) -> Result<Vec<(Platform, Vec<Variant>)>> {
    let mut polished = Vec::new();
    for (platform, variants) in drafts {
        let prompt = build_editor_prompt(persona, *platform, topic, variants);
        let (edits, via) = match complete_with_chain(
            registry,
            &prompt,
            JobKind::Other,
            COMPLETION_TIMEOUT,
        )
        .await
        {
            Ok(completion) => match parse_editor_response(&completion.text) {
                Ok(edits) => (
                    Some(edits),
                    format!(" via {} ({})", completion.provider_id, completion.model),
                ),
                Err(err) => {
                    record(
                        report,
                        journal,
                        StageOutcome::new(
                            "editor",
                            StageStatus::Skipped,
                            format!("skipped (unparseable response: {err:#})"),
                        )
                        .with_platform(*platform),
                    )
                    .await?;
                    (None, String::new())
                }
            },
            Err(err) => {
                record(
                    report,
                    journal,
                    StageOutcome::new(
                        "editor",
                        StageStatus::Skipped,
                        format!("skipped (provider chain failed: {err:#})"),
                    )
                    .with_platform(*platform),
                )
                .await?;
                (None, String::new())
            }
        };

        let outcomes = review_or_keep(variants, edits.as_deref());
        for outcome in &outcomes {
            let outcome = match outcome {
                EditorOutcome::Polished { changes, .. } => StageOutcome::new(
                    "editor",
                    StageStatus::Ok,
                    format!(
                        "ok — polished variant {}{via}; changes: {}",
                        outcome.label(),
                        render_changes(changes)
                    ),
                )
                .with_variant(outcome.label())
                .with_changes(changes.clone()),
                EditorOutcome::Rejected { reason, .. } => StageOutcome::new(
                    "editor",
                    StageStatus::Rejected,
                    format!("rejected variant {}: {reason}", outcome.label()),
                )
                .with_variant(outcome.label()),
                EditorOutcome::Unchanged { .. } => StageOutcome::new(
                    "editor",
                    StageStatus::Skipped,
                    format!(
                        "unchanged variant {}{via} (no usable rewrite returned)",
                        outcome.label()
                    ),
                )
                .with_variant(outcome.label()),
            };
            record(report, journal, outcome.with_platform(*platform)).await?;
        }

        let kept = kept_variants(outcomes);
        if !kept.is_empty() {
            polished.push((*platform, kept));
        }
    }
    Ok(polished)
}

/// Stage [3.5] — guardrail. Returns the approved drafts (a hard gate).
async fn stage_guardrail<'a>(
    report: &mut RunReport,
    journal: &Journal,
    persona: &PersonaPack,
    drafts: &'a [(Platform, Vec<Variant>)],
) -> Result<Vec<(Platform, &'a Variant)>> {
    let mut approved = Vec::new();
    for (platform, variants) in drafts {
        let (ok, rejected) = partition_variants(persona, *platform, variants);
        for variant in &ok {
            record(
                report,
                journal,
                StageOutcome::new(
                    "guardrail",
                    StageStatus::Ok,
                    format!("variant {} approved", variant.label),
                )
                .with_platform(*platform)
                .with_variant(&variant.label),
            )
            .await?;
        }
        for (variant, verdict) in &rejected {
            record(
                report,
                journal,
                StageOutcome::new(
                    "guardrail",
                    StageStatus::Rejected,
                    format!(
                        "variant {} rejected — {}",
                        variant.label,
                        render_issues(verdict)
                    ),
                )
                .with_platform(*platform)
                .with_variant(&variant.label),
            )
            .await?;
        }
        approved.extend(ok.into_iter().map(|variant| (*platform, variant)));
    }
    Ok(approved)
}

/// Stage [4] — publish. Only the platforms with an adapter are served.
async fn stage_publish(
    report: &mut RunReport,
    journal: &Journal,
    options: &RunOptions,
    topic: &str,
    approved: &[(Platform, &Variant)],
) -> Result<()> {
    let mut session: Option<Session> = None;
    let mut handle = String::new();
    let run_id = report.run_id.clone();

    // One slot, one variant per platform: publishing two near-identical variants
    // at the same moment reads as spam, so the A/B comparison is sequential and
    // the rotation alternates which variant each slot releases.
    let selection = select_variants(
        approved,
        VARIANTS_PER_PLATFORM_PER_SLOT,
        rotation_offset(&run_id),
    );

    for (platform, variant) in approved {
        if !selection
            .iter()
            .any(|(chosen, selected)| chosen == platform && selected.label == variant.label)
        {
            record(
                report,
                journal,
                StageOutcome::new(
                    "publish",
                    StageStatus::Skipped,
                    "held back — a slot publishes one variant per platform (sequential A/B)",
                )
                .with_platform(*platform)
                .with_variant(&variant.label),
            )
            .await?;
            continue;
        }

        if *platform != Platform::Bluesky {
            record(
                report,
                journal,
                StageOutcome::new(
                    "publish",
                    StageStatus::Skipped,
                    format!("no publish adapter for `{}` yet", platform.as_str()),
                )
                .with_platform(*platform)
                .with_variant(&variant.label),
            )
            .await?;
            continue;
        }

        if session.is_none() {
            match open_bluesky_session().await {
                Ok(created) => {
                    handle = created.handle.clone();
                    session = Some(created);
                }
                Err(err) => {
                    report.failed = true;
                    record(
                        report,
                        journal,
                        StageOutcome::new(
                            "publish",
                            StageStatus::Error,
                            format!("bluesky authentication failed: {err:#}"),
                        )
                        .with_platform(*platform)
                        .with_variant(&variant.label),
                    )
                    .await?;
                    continue;
                }
            }
        }

        let Some(current) = session.as_ref() else {
            continue;
        };

        // The guardrail already approved the text; the card is a rendering of it.
        let images =
            match stage_card_image(report, journal, options, &run_id, topic, variant, current)
                .await?
            {
                Some(image) => vec![image],
                None => Vec::new(),
            };

        let post = build_post_record_with_images(&variant.text, &now_rfc3339_millis(), &images)?;
        match create_post(DEFAULT_BLUESKY_BASE_URL, current, &post, PUBLISH_TIMEOUT).await {
            Ok(uri) => {
                let url = post_url(&handle, &uri);
                record(
                    report,
                    journal,
                    StageOutcome::new(
                        "publish",
                        StageStatus::Published,
                        format!("published to bluesky: {url}"),
                    )
                    .with_platform(*platform)
                    .with_variant(&variant.label),
                )
                .await?;
                report.published.push(PublishedPost {
                    platform: platform.as_str().to_string(),
                    variant: variant.label.clone(),
                    uri,
                    url,
                });
            }
            Err(err) => {
                report.failed = true;
                record(
                    report,
                    journal,
                    StageOutcome::new(
                        "publish",
                        StageStatus::Error,
                        format!("bluesky rejected the post: {err:#}"),
                    )
                    .with_platform(*platform)
                    .with_variant(&variant.label),
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Render and upload a post card for a variant, or `None` when not requested.
///
/// A card failure never costs the post: the run falls back to a text-only post,
/// journals the failure and marks the run as failed so it is not mistaken for a
/// clean run.
async fn stage_card_image(
    report: &mut RunReport,
    journal: &Journal,
    options: &RunOptions,
    run_id: &str,
    topic: &str,
    variant: &Variant,
    session: &Session,
) -> Result<Option<PostImage>> {
    if !options.card.should_attach(&variant.image_prompt) {
        return Ok(None);
    }

    let spec = CardSpec::from_post(topic, &variant.text, &session.handle);
    let png = PathBuf::from(DEFAULT_CARD_DIR).join(format!("{run_id}-{}.png", variant.label));
    let result = async {
        let svg = render_png(&spec, &png, DEFAULT_RSVG_BINARY).await?;
        let bytes = tokio::fs::read(&png).await?;
        let blob = upload_blob(
            DEFAULT_BLUESKY_BASE_URL,
            session,
            &bytes,
            CARD_MIME_TYPE,
            PUBLISH_TIMEOUT,
        )
        .await?;
        Ok::<_, anyhow::Error>((svg, blob))
    }
    .await;

    match result {
        Ok((svg, blob)) => {
            record(
                report,
                journal,
                StageOutcome::new(
                    "card",
                    StageStatus::Ok,
                    format!(
                        "attached {CARD_TEMPLATE_VERSION} card {}x{} ({} bytes) from {} and {}",
                        CARD_WIDTH,
                        CARD_HEIGHT,
                        blob.size,
                        png.display(),
                        svg.display()
                    ),
                )
                .with_platform(Platform::Bluesky)
                .with_variant(&variant.label),
            )
            .await?;
            Ok(Some(PostImage {
                alt: alt_text(&spec),
                blob,
                width: CARD_WIDTH,
                height: CARD_HEIGHT,
            }))
        }
        Err(err) => {
            report.failed = true;
            record(
                report,
                journal,
                StageOutcome::new(
                    "card",
                    StageStatus::Error,
                    format!("card generation failed — publishing text only: {err:#}"),
                )
                .with_platform(Platform::Bluesky)
                .with_variant(&variant.label),
            )
            .await?;
            Ok(None)
        }
    }
}

/// Authenticate against Bluesky using the credentials in the environment.
async fn open_bluesky_session() -> Result<Session> {
    let credentials = BlueskyCredentials::from_env()?;
    create_session(DEFAULT_BLUESKY_BASE_URL, &credentials, PUBLISH_TIMEOUT).await
}

/// Append one decision to both the journal and the in-memory report.
async fn record(report: &mut RunReport, journal: &Journal, outcome: StageOutcome) -> Result<()> {
    journal.record(&outcome).await?;
    report.stages.push(outcome);
    Ok(())
}

/// Render the guardrail issues of a rejected draft as one line.
fn render_issues(verdict: &Verdict) -> String {
    verdict
        .issues
        .iter()
        .map(|issue| format!("{}: {}", issue.rule, issue.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Render what the editor changed as one line (or `none`).
fn render_changes(changes: &[String]) -> String {
    if changes.is_empty() {
        return "none".to_string();
    }
    changes.join("; ")
}

/// Reject an invalid persona before any stage runs.
fn validate_or_bail(persona: &PersonaPack) -> Result<()> {
    let issues = persona.validate();
    if issues.is_empty() {
        return Ok(());
    }
    let rendered: Vec<String> = issues
        .iter()
        .map(|issue| format!("{}: {}", issue.field, issue.problem))
        .collect();
    bail!(
        "persona `{}` is invalid: {}",
        persona.persona,
        rendered.join("; ")
    )
}

/// Human-readable suffix for a platform filter in error messages.
fn platform_suffix(platform: Option<Platform>) -> String {
    platform.map_or_else(String::new, |value| {
        format!(" for platform `{}`", value.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopilot_core::{Channel, Rubric, Voice};
    use autopilot_pipeline::EditedVariant;

    fn persona() -> PersonaPack {
        PersonaPack {
            persona: "demo".into(),
            display_name: "Demo".into(),
            niche: "science-curiosities".into(),
            language: "pt-BR".into(),
            active: false,
            voice: Voice {
                tone: "curious".into(),
                audience: "adults".into(),
                guidelines: vec![],
                banned_phrases: vec!["não acredite".into()],
                max_hashtags: 3,
                emojis_allowed: true,
            },
            few_shot: vec![],
            blocklist: vec!["terraplanismo".into()],
            rubric: Rubric {
                min_score: 0.75,
                criteria: vec![],
            },
            channels: vec![
                Channel {
                    platform: Platform::Bluesky,
                    handle: String::new(),
                    enabled: true,
                    posts_per_day: 1,
                    windows: vec![],
                },
                Channel {
                    platform: Platform::X,
                    handle: String::new(),
                    enabled: true,
                    posts_per_day: 1,
                    windows: vec![],
                },
                Channel {
                    platform: Platform::Instagram,
                    handle: String::new(),
                    enabled: false,
                    posts_per_day: 1,
                    windows: vec![],
                },
            ],
        }
    }

    fn variant(label: &str, text: &str) -> Variant {
        Variant {
            label: label.into(),
            text: text.into(),
            hashtags: vec![],
            image_prompt: String::new(),
        }
    }

    #[test]
    fn a_slot_publishes_one_variant_per_platform_and_alternates() {
        let a = variant("A", "texto a");
        let b = variant("B", "texto b");
        let approved = vec![(Platform::Bluesky, &a), (Platform::Bluesky, &b)];

        // Rotation 0 → A, rotation 1 → B: consecutive slots alternate, so no
        // moment ever shows two near-identical posts.
        let first = select_variants(&approved, 1, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1.label, "A");

        let second = select_variants(&approved, 1, 1);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].1.label, "B");
    }

    #[test]
    fn every_platform_still_gets_its_variant_in_the_same_slot() {
        let a = variant("A", "texto a");
        let b = variant("B", "texto b");
        let approved = vec![(Platform::Bluesky, &a), (Platform::X, &b)];
        let selected = select_variants(&approved, 1, 0);
        assert_eq!(selected.len(), 2);
        assert!(selected.iter().any(|(p, _)| *p == Platform::Bluesky));
        assert!(selected.iter().any(|(p, _)| *p == Platform::X));
    }

    #[test]
    fn the_rotation_comes_from_the_run_id_digits() {
        assert_eq!(rotation_offset("tech-curiosities-1791849666"), 6);
        assert_eq!(rotation_offset("persona-1791849661"), 1);
        assert_eq!(rotation_offset("no-digits"), 0);
    }

    #[test]
    fn a_window_matches_only_near_its_own_time() {
        let windows = vec!["06:00".to_string(), "18:00".to_string()];
        // The scheduler fires at :15 — fifteen minutes into the window's hour.
        assert_eq!(
            matching_window(&windows, 6 * 60 + 15, 45).as_deref(),
            Some("06:00")
        );
        assert_eq!(
            matching_window(&windows, 18 * 60 + 15, 45).as_deref(),
            Some("18:00")
        );
        // 00:15 and 12:15 are far from either window: the run skips.
        assert!(matching_window(&windows, 15, 45).is_none());
        assert!(matching_window(&windows, 12 * 60 + 15, 45).is_none());
    }

    #[test]
    fn window_parsing_rejects_junk() {
        assert_eq!(parse_window("06:00"), Some(360));
        assert_eq!(parse_window(" 18:30 "), Some(1110));
        assert_eq!(parse_window("24:00"), None);
        assert_eq!(parse_window("06:99"), None);
        assert_eq!(parse_window("nope"), None);
    }

    #[test]
    fn a_window_near_midnight_wraps_around() {
        let windows = vec!["23:50".to_string()];
        // 00:05 the next day is fifteen minutes after the window.
        assert!(matching_window(&windows, 5, 45).is_some());
    }

    #[test]
    fn humanizes_the_niche_into_a_query() {
        assert_eq!(humanize_niche("science-curiosities"), "science curiosities");
        assert_eq!(humanize_niche("astrology_horoscope"), "astrology horoscope");
        assert_eq!(humanize_niche("  spaced  out "), "spaced out");
    }

    #[test]
    fn resolve_channels_keeps_enabled_and_applies_the_filter() {
        let persona = persona();
        assert_eq!(
            resolve_channels(&persona, None),
            vec![Platform::Bluesky, Platform::X]
        );
        assert_eq!(
            resolve_channels(&persona, Some(Platform::X)),
            vec![Platform::X]
        );
        assert!(resolve_channels(&persona, Some(Platform::Instagram)).is_empty());
    }

    #[test]
    fn select_topic_respects_the_rubric_floor() {
        let shortlist = vec![
            TopicScore {
                title: "good".into(),
                url: String::new(),
                score: 0.9,
                reason: String::new(),
            },
            TopicScore {
                title: "borderline".into(),
                url: String::new(),
                score: 0.8,
                reason: String::new(),
            },
        ];
        assert_eq!(
            select_topic(&shortlist, 0.75).map(|topic| topic.title.as_str()),
            Some("good")
        );
        assert!(select_topic(&shortlist, 0.95).is_none());
    }

    #[test]
    fn partition_variants_splits_on_the_guardrail() {
        let persona = persona();
        let variants = vec![
            variant("A", "O cérebro gasta energia. #ciencia"),
            variant("B", "Sobre o terraplanismo moderno..."),
        ];
        let (approved, rejected) = partition_variants(&persona, Platform::Bluesky, &variants);
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].label, "A");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].0.label, "B");
        assert!(!rejected[0].1.approved);
    }

    #[test]
    fn card_attachment_modes_decide_on_the_image_prompt() {
        assert_eq!(CardAttachment::default(), CardAttachment::Off);
        assert!(!CardAttachment::Off.should_attach("anything"));
        assert!(!CardAttachment::WhenPrompted.should_attach(""));
        assert!(!CardAttachment::WhenPrompted.should_attach("   "));
        assert!(CardAttachment::WhenPrompted.should_attach("a terminal card"));
        assert!(CardAttachment::Always.should_attach(""));
        assert_eq!(CardAttachment::WhenPrompted.as_str(), "prompted");
    }

    #[test]
    fn guardrail_sees_the_polished_text_not_the_original() {
        let persona = persona();
        let originals = vec![variant("A", "Não acredite: o cérebro gasta energia.")];

        // The first draft carries a banned phrase, so the guardrail refuses it.
        let (_, rejected_before) = partition_variants(&persona, Platform::Bluesky, &originals);
        assert_eq!(rejected_before.len(), 1);

        // After the review, the guardrail partitions the polished text instead.
        let edits = vec![EditedVariant {
            label: "A".into(),
            text: "O cérebro gasta ~20% da energia do corpo em repouso.".into(),
            changes: vec!["removed the templated opener".into()],
            rejected_reason: String::new(),
        }];
        let kept = kept_variants(review_or_keep(&originals, Some(&edits)));
        let (approved, rejected) = partition_variants(&persona, Platform::Bluesky, &kept);

        assert!(
            rejected.is_empty(),
            "the polished text clears the guardrail"
        );
        assert_eq!(approved.len(), 1);
        assert_eq!(
            approved[0].text,
            "O cérebro gasta ~20% da energia do corpo em repouso."
        );
    }

    #[tokio::test]
    async fn editor_failure_keeps_the_original_drafts_and_journals_it() {
        let journal_path = std::env::temp_dir().join(format!(
            "autopilot-editor-degrade-{}.jsonl",
            std::process::id()
        ));
        let _ = tokio::fs::remove_file(&journal_path).await;

        let journal = Journal::new("run-test", "demo", journal_path.clone());
        let mut report = RunReport {
            run_id: "run-test".into(),
            persona: "demo".into(),
            dry_run: false,
            journal: journal_path.clone(),
            stages: Vec::new(),
            published: Vec::new(),
            failed: false,
        };
        // An empty chain fails immediately, so the degradation path is exercised
        // without a network call: the editor is simply unavailable.
        let registry = autopilot_providers::ProvidersConfig {
            providers: Vec::new(),
        };
        let drafts = vec![(
            Platform::Bluesky,
            vec![variant("A", "O cérebro gasta energia.")],
        )];

        let polished = stage_editor(
            &mut report,
            &journal,
            &persona(),
            &drafts,
            &registry,
            "o cérebro",
        )
        .await
        .expect("an unavailable editor is not an error");

        assert_eq!(polished.len(), 1);
        assert_eq!(polished[0].1[0].text, "O cérebro gasta energia.");
        assert!(
            !report.failed,
            "a transient editor failure must not fail the run"
        );

        let raw = tokio::fs::read_to_string(&journal_path)
            .await
            .expect("journal written");
        let event: serde_json::Value =
            serde_json::from_str(raw.lines().next().unwrap_or("{}")).expect("valid journal JSON");
        assert_eq!(event["stage"], "editor");
        assert_eq!(event["status"], "skipped");
        assert!(
            event["detail"]
                .as_str()
                .unwrap_or_default()
                .starts_with("skipped ("),
            "the journal must say why the review was skipped: {event}"
        );
        assert!(
            event.get("changes").is_none(),
            "a skipped review carries no change list: {event}"
        );

        tokio::fs::remove_file(&journal_path).await.ok();
    }
}
