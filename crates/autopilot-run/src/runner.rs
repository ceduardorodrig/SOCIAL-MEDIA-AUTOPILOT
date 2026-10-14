//! The orchestrator: chain the stage crates for exactly one persona pack.
//!
//! The data flow is a single pass: harvest → triage → creation → editorial
//! review → guardrail → publish. Each stage records a [`StageOutcome`] that is
//! both printed by the caller and appended to the run's [`Journal`]. The pure
//! decisions (channel resolution, topic selection, editor fold, guardrail
//! partition) are separated from the effectful calls so they can be unit-tested
//! without any network.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use autopilot_core::{
    Format, FormatAvailability, FormatBudgets, PersonaPack, Platform, PublishingPolicy,
    SelectionRule, load_persona, now_unix, utc_day,
};
use autopilot_guardrail::{Verdict, evaluate};
use autopilot_harvest::{
    ARTICLE_TIMEOUT, MAX_SOURCE_CHARS, SearxngClient, Topic, fetch_article_text,
};
use autopilot_media::{
    ImageSource, MediaConfig, PlaceholderImageSource, RenderOptions, SceneImageDir,
    VERTICAL_HEIGHT, VERTICAL_WIDTH, VideoPlan, render as render_media,
};
use autopilot_pipeline::{
    EditorOutcome, Source, TopicScore, Variant, build_creation_prompt_with_policy,
    build_editor_prompt, build_grounding_prompt, build_triage_prompt, kept_variants,
    parse_editor_response, parse_grounding, parse_shortlist, parse_variants, review_or_keep,
    supported_labels,
};
use autopilot_providers::{JobKind, complete_with_chain, load_config};
use autopilot_publish::{
    BlueskyCredentials, CARD_HEIGHT, CARD_MIME_TYPE, CARD_WIDTH, CardIdentity, CardSpec,
    CardTemplate, DEFAULT_BLUESKY_BASE_URL, DEFAULT_RSVG_BINARY, DEFAULT_VIDEO_BASE_URL,
    DEFAULT_VIDEO_POLL_INTERVAL, PostImage, PostVideo, Session, VIDEO_MIME_TYPE, alt_text,
    await_video_job, build_image_embed, build_post_record_with_embed, build_video_embed,
    create_post, create_session, now_rfc3339_millis, post_url, render_png_with, resolve_pool,
    template_for_identity_in_pool, upload_blob, upload_video, validate_video,
};

use crate::journal::{Journal, PublishedEntry, read_published};

/// How long to wait for one LLM completion.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(300);

/// How far from a declared publishing window a run may still publish: the
/// scheduler fires a few minutes into the window's hour.
const WINDOW_TOLERANCE_MINUTES: i64 = 45;

/// How long to wait for one publishing API call.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(30);

/// Where generated cards are kept (gitignored, alongside the run journal).
const DEFAULT_CARD_DIR: &str = "var/cards";

/// Where rendered short videos are kept (gitignored, under `var/`).
const DEFAULT_VIDEO_DIR: &str = "var/media";

/// How long to wait for one video-service request (the upload or a job poll).
const VIDEO_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for a whole video processing job to complete.
const VIDEO_JOB_TIMEOUT: Duration = Duration::from_secs(600);

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
    /// The persona's persisted memory proposal to read the "avoid" warnings
    /// from (defaults to `var/learn/<persona>.json`, where `autopilot learn run
    /// --write` puts it). The creation stage is warned off claims the [3.7]
    /// reviewer already refused (HB-046).
    pub memory_path: Option<PathBuf>,
    /// When set, no LLM call, no GPU work, no network and no publish happen.
    pub dry_run: bool,
    /// Whether a generated post card is attached to the published post.
    pub card: CardAttachment,
    /// Media configuration (Piper binary/voice, motion, scene images), read only
    /// in a live run that the policy lets publish a short video. A missing file
    /// falls back to the safe defaults, so text and cards never need it.
    pub media_path: PathBuf,
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
    /// The AT URI of a published post, when the stage is `publish` (additive).
    /// The web URL is not enough to collect metrics later: the API needs
    /// `at://did/app.bsky.feed.post/<rkey>`.
    pub uri: Option<String>,
    /// The unsupported claim, when the stage is `grounding` and it rejected a
    /// draft (additive). The learning stage reads it as a *kind of claim* to
    /// avoid (HB-046).
    pub reason: Option<String>,
    /// The format actually published, when the stage is `publish` (additive):
    /// `text`, `card` or `video`. Recorded so the weekly per-format budget is
    /// counted back from the journal (HB-034).
    pub format: Option<String>,
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
            uri: None,
            reason: None,
            format: None,
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

    fn with_uri(mut self, uri: &str) -> Self {
        self.uri = Some(uri.to_string());
        self
    }

    fn with_reason(mut self, reason: &str) -> Self {
        self.reason = Some(reason.to_string());
        self
    }

    fn with_format(mut self, format: Format) -> Self {
        self.format = Some(format.as_str().to_string());
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

/// The harvest query for this slot, walked through the pack's rotation (HB-033).
///
/// The rotation is the same deterministic per-slot offset the variant selection
/// uses, so consecutive slots walk the query list instead of repeating it. An
/// empty list yields `None` (the caller then falls back to the humanised niche).
pub fn select_harvest_query(queries: &[String], rotation: usize) -> Option<&str> {
    if queries.is_empty() {
        return None;
    }
    Some(queries[rotation % queries.len()].as_str())
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

/// Resolve the harvested candidate a triage selection points at.
///
/// HB-057: the triage model frequently paraphrases the title it echoes back, so
/// matching the harvest candidate by exact title alone resolved to **no**
/// candidate — the [3.7] gate then had no source and withheld the whole slot
/// (fail-closed, HB-041), starving the pipeline of every paraphrased title. The
/// candidate URL travels through the shortlist and is echoed verbatim far more
/// often, so it is a second key into the same list.
pub fn resolve_candidate<'a>(topics: &'a [Topic], title: &str, url: &str) -> Option<&'a Topic> {
    topics
        .iter()
        .find(|candidate| candidate.title == title || (!url.is_empty() && candidate.url == url))
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
///
/// The one local-clock conversion lives in `autopilot_core::time` (HB-058): the
/// publishing windows are local too, so both stages must read the same clock.
fn local_minutes_now() -> i64 {
    i64::from(autopilot_core::local_minute_of_day(now_unix()))
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

/// The format a slot intends to publish, before the frequency policy runs.
///
/// The creation stage fills `video_plan` when the topic's angle suits a short
/// video (`content-formats.md` §2, HB-035), and `image_prompt` when it suits a
/// card; an empty prompt is a text post. The video plan wins: a topic the model
/// shaped as a video is a video.
pub fn intended_format(
    image_prompt: &str,
    video_plan: Option<&VideoPlan>,
    card: CardAttachment,
) -> Format {
    if video_plan.is_some() {
        Format::Video
    } else if card.should_attach(image_prompt) {
        Format::Card
    } else {
        Format::Text
    }
}

/// The warming-ramp week (1-based) for an account `age_days` old.
///
/// `content-formats.md` §1: week 1 is text-only, week 2 adds the card, week 3+
/// is steady state. A future or just-started account (age <= 0) is week 1.
pub fn warming_week(age_days: i64) -> u8 {
    (age_days.max(0) / 7 + 1).clamp(1, 3) as u8
}

/// The daily posting ceiling the ramp imposes on its week.
///
/// Week 1 holds a new account to one post a day; weeks 2+ allow two. The
/// declared `posts_per_day` is the upper bound, so the ramp only ever tightens.
pub fn warming_ceiling(age_days: i64) -> u8 {
    match warming_week(age_days) {
        1 => 1,
        _ => 2,
    }
}

/// The formats a ramp week releases.
pub fn warming_releases(age_days: i64) -> &'static [Format] {
    const WEEK_1: [Format; 1] = [Format::Text];
    const WEEK_2: [Format; 2] = [Format::Text, Format::Card];
    const STEADY: [Format; 3] = [Format::Text, Format::Card, Format::Video];
    match warming_week(age_days) {
        1 => &WEEK_1,
        2 => &WEEK_2,
        _ => &STEADY,
    }
}

/// The account's **birth day** (UTC, days since the epoch) as the earliest
/// publication recorded in the run journals (HB-034).
///
/// The warming ramp is autonomous: it does not depend on an operator filling in
/// `started_on`. The first publication the engine ever recorded is the account's
/// birth — the journals (`var/runs`) are the primary source of truth for "what
/// happened in a run", and the metrics store is a secondary one. `None` when no
/// publication has ever been recorded (an empty base), which the ramp reads as
/// "no ramp" — a mature account, never an eternal week 1.
pub fn detected_start_day(published: &[PublishedEntry]) -> Option<i64> {
    published
        .iter()
        .map(|entry| utc_day(entry.published_at_unix))
        .min()
}

/// Whether a format may be released this week: the warming ramp has opened it
/// **and** its weekly budget is not spent.
///
/// This is the single predicate the prompt's availability and the publish gate's
/// [`effective_format`] both read, so the model is told exactly what the engine
/// will enforce (HB-034).
pub fn format_available(
    format: Format,
    age_days: Option<i64>,
    budgets: &FormatBudgets,
    counts: &FormatCounts,
) -> bool {
    let released = age_days.is_none_or(|age| warming_releases(age).contains(&format));
    released && counts.count(format) < budgets.budget(format)
}

/// The formats a slot may release this week, after the ramp and the budget.
///
/// Handed to the creation prompt so the **model** chooses the format among the
/// available ones; the engine keeps only [`effective_format`] as a deterministic
/// safety net.
pub fn format_availability(
    age_days: Option<i64>,
    budgets: &FormatBudgets,
    counts: &FormatCounts,
) -> FormatAvailability {
    FormatAvailability {
        text: format_available(Format::Text, age_days, budgets, counts),
        card: format_available(Format::Card, age_days, budgets, counts),
        video: format_available(Format::Video, age_days, budgets, counts),
    }
}

/// How many posts this persona and platform released on a given UTC day.
pub fn published_on(
    published: &[PublishedEntry],
    persona: &str,
    platform: &str,
    day: i64,
) -> usize {
    published
        .iter()
        .filter(|entry| {
            entry.persona == persona
                && entry.platform == platform
                && utc_day(entry.published_at_unix) == day
        })
        .count()
}

/// The Monday (UTC) that starts the week containing `day`.
///
/// 1970-01-01 was a Thursday, so a `+3` offset lands Monday on a multiple of
/// seven.
pub fn week_start(day: i64) -> i64 {
    day - (day + 3).rem_euclid(7)
}

/// How many posts of each format a channel already released this week.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FormatCounts {
    pub text: u16,
    pub card: u16,
    pub video: u16,
}

impl FormatCounts {
    /// The count for one format.
    pub fn count(&self, format: Format) -> u16 {
        match format {
            Format::Text => self.text,
            Format::Card => self.card,
            Format::Video => self.video,
        }
    }
}

/// The per-format counts a channel released in the week starting `week_start_day`.
///
/// The format comes from the publish journal line, so the accounting reads back
/// exactly what went out (HB-034). An unlabelled older line counts as text.
pub fn week_counts(
    published: &[PublishedEntry],
    persona: &str,
    platform: &str,
    week_start_day: i64,
) -> FormatCounts {
    let mut counts = FormatCounts::default();
    for entry in published {
        if entry.persona != persona || entry.platform != platform {
            continue;
        }
        if utc_day(entry.published_at_unix) < week_start_day {
            continue;
        }
        let slot = match Format::from_label(entry.format.as_deref()) {
            Format::Text => &mut counts.text,
            Format::Card => &mut counts.card,
            Format::Video => &mut counts.video,
        };
        *slot = slot.saturating_add(1);
    }
    counts
}

/// The format a slot may actually publish — the deterministic **safety net**.
///
/// `content-formats.md` §3: the model chooses the format from the availability
/// it was given ([`format_availability`]); this function only guarantees the
/// engine never violates the policy when the model's choice conflicts with it.
/// A card (or video) that would exceed the week's budget ships as text (the
/// baseline) instead of being lost — never dropped without the embed it asked
/// for. Only a slot whose text budget is *also* spent waits (`None`).
pub fn effective_format(
    intended: Format,
    age_days: Option<i64>,
    budgets: &FormatBudgets,
    counts: &FormatCounts,
) -> Option<Format> {
    if format_available(intended, age_days, budgets, counts) {
        return Some(intended);
    }
    if intended != Format::Text && format_available(Format::Text, age_days, budgets, counts) {
        return Some(Format::Text);
    }
    None
}

/// The weekly budgets for one platform, after the learned adjustment (HB-034).
///
/// The learned movement is scoped **per persona × platform**: the memory
/// proposal was learned for one platform, so it only adjusts that platform's
/// budget; every other channel keeps the pack's declared budgets.
pub fn budgets_for_platform(
    base: &FormatBudgets,
    memory: Option<&autopilot_learn::MemoryUpdate>,
    platform: Platform,
) -> FormatBudgets {
    match memory {
        Some(update) if update.platform == platform => {
            autopilot_learn::apply_budget_adjustments(base, &update.budget_adjustments)
        }
        _ => *base,
    }
}

/// The day's posting ceiling for one platform, tightened by the warming ramp.
pub fn channel_ceiling(persona: &PersonaPack, platform: Platform, age_days: Option<i64>) -> u8 {
    let declared = persona
        .channels
        .iter()
        .filter(|channel| channel.enabled && channel.platform == platform)
        .map(|channel| channel.posts_per_day)
        .max()
        .unwrap_or(0);
    match age_days {
        Some(age) => declared.min(warming_ceiling(age)),
        None => declared,
    }
}

/// Whether **any** targeted channel still has room under the day's ceiling.
///
/// This is the "slot is an opportunity, not an obligation" gate: when every
/// channel already met its declared `posts_per_day` (tightened by the ramp), the
/// slot is skipped before any LLM call, so an extra scheduler fire costs nothing.
pub fn day_has_room(
    persona: &PersonaPack,
    platforms: &[Platform],
    published: &[PublishedEntry],
    day: i64,
    age_days: Option<i64>,
) -> bool {
    platforms.iter().any(|platform| {
        let ceiling = channel_ceiling(persona, *platform, age_days) as usize;
        published_on(published, &persona.persona, platform.as_str(), day) < ceiling
    })
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

/// Choose which approved variants a slot publishes.
///
/// A slot releases at most `per_platform` variants per platform, starting at
/// `rotation` (for [`SelectionRule::Alternate`]) or the first variant (for
/// [`SelectionRule::First`]), and wrapping. With the default policy — one
/// variant, alternating — consecutive slots exercise both styles over time
/// while no single moment shows two near-identical posts (sequential A/B,
/// HB-040). Both knobs are persona data ([`PublishingPolicy`]), not constants.
pub fn select_variants<'a>(
    approved: &'a [(Platform, &'a Variant)],
    per_platform: usize,
    rotation: usize,
    rule: SelectionRule,
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
        let start = match rule {
            SelectionRule::Alternate => rotation % group.len(),
            SelectionRule::First => 0,
        };
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

    // HB-032: the persona's card templates are scoped once, up front. An empty
    // pool inherits the whole catalogue; a pack naming an unknown template fails
    // the whole run here rather than silently widening back to every template
    // (which is how two personas come to share a look).
    let card_pool = resolve_pool(&persona.visual_templates())?;

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
    //
    // The day gate (below) is the second half of the same contract: a slot is an
    // *opportunity*, not an obligation. Once every targeted channel has met its
    // declared `posts_per_day` (tightened by the warming ramp for a new
    // account), the slot is skipped before any LLM call, so an extra scheduler
    // fire costs nothing. A missed slot is never compensated with a burst.
    let mut published: Vec<PublishedEntry> = Vec::new();
    let mut age_days: Option<i64> = None;
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

        let runs_dir = report
            .journal
            .parent()
            .unwrap_or_else(|| Path::new("var/runs"));
        published = read_published(runs_dir).await?;
        let day = utc_day(now_unix());
        // HB-034: the ramp is autonomous. The explicit `started_on` is an
        // override; otherwise the engine detects the account's birth from the
        // first publication it ever recorded in the journals. An empty base (no
        // record, no override) is a mature account with **no ramp** — never an
        // eternal week 1.
        age_days = persona
            .publishing
            .warming
            .resolve_start_day(detected_start_day(&published))
            .map(|start| day - start);

        if !day_has_room(&persona, &channels, &published, day, age_days) {
            let reached = channels
                .iter()
                .map(|platform| {
                    format!(
                        "{} {}/{}",
                        platform.as_str(),
                        published_on(&published, &persona.persona, platform.as_str(), day),
                        channel_ceiling(&persona, *platform, age_days)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            record(
                &mut report,
                &journal,
                StageOutcome::new(
                    "run",
                    StageStatus::Skipped,
                    format!(
                        "the day's ceiling is reached ({reached}) — a slot is an opportunity, not an obligation"
                    ),
                ),
            )
            .await?;
            return Ok(report);
        }
    }

    // HB-033: the harvest subject belongs to the persona, not to its niche
    // alone. When the pack declares `harvest.queries` the stage walks the list
    // in rotation (the same per-slot offset the variants use); a pack that
    // declares none keeps the humanised-niche fallback. An explicit `--query`
    // still overrides the pack.
    let query = match &options.query {
        Some(query) => query.clone(),
        None => {
            let queries = persona.harvest_queries();
            select_harvest_query(&queries, rotation_offset(&report.run_id))
                .unwrap_or_default()
                .to_string()
        }
    };

    let topics = stage_harvest(&mut report, &journal, &options, &query).await?;

    // A dry run never calls an LLM, the GPU, the network or a publisher: it only
    // describes the decisions the live run would take.
    if options.dry_run {
        stage_dry_run_plan(&mut report, &journal, &persona, &channels, options.card).await?;
        return Ok(report);
    }

    let registry = load_config(&options.config_path).await?;
    let Some(selected) = stage_triage(&mut report, &journal, &persona, &topics, &registry).await?
    else {
        return Ok(report);
    };
    let topic_title = selected.title.clone();

    // The source travels with the topic. The harvest snippet is one or two
    // sentences — too thin to write against, which made the [3.7] review refuse
    // every draft (verified 2026-10-13: nothing was published) — so fetch the
    // **article body** once, for the selected topic only, and fall back to the
    // snippet when the fetch fails.
    //
    // HB-057: the triage model paraphrases the title it echoes back, so a
    // title-only match resolved to no candidate and the gate withheld the slot
    // for lack of a source. The URL the shortlist carries is a second key.
    let candidate = resolve_candidate(&topics, &topic_title, &selected.url);
    let source_text = match candidate {
        Some(candidate) => {
            match fetch_article_text(&candidate.url, ARTICLE_TIMEOUT, MAX_SOURCE_CHARS).await {
                Ok(article) => {
                    record(
                        &mut report,
                        &journal,
                        StageOutcome::new(
                            "source",
                            StageStatus::Ok,
                            format!(
                                "fetched {} chars from {} (the snippet was {})",
                                article.chars().count(),
                                candidate.url,
                                candidate.content.chars().count()
                            ),
                        ),
                    )
                    .await?;
                    article
                }
                Err(err) => {
                    record(
                        &mut report,
                        &journal,
                        StageOutcome::new(
                            "source",
                            StageStatus::Skipped,
                            format!("article fetch failed — using the snippet: {err:#}"),
                        ),
                    )
                    .await?;
                    candidate.content.clone()
                }
            }
        }
        None => String::new(),
    };
    let source = candidate.map(|candidate| Source {
        url: &candidate.url,
        content: &source_text,
    });

    // HB-046: the [3.7] reviewer's refusals are learned, not just gated. Load
    // the persona's memory so the creation stage is warned off a claim that was
    // already refused — and so the learned per-format budget (HB-034) reaches
    // the slot's format policy instead of being rediscovered every week.
    let memory = load_memory(
        &mut report,
        &journal,
        &persona,
        options.memory_path.as_deref(),
    )
    .await?;
    let avoid = memory
        .as_ref()
        .map(autopilot_learn::avoid_warnings)
        .unwrap_or_default();

    let drafts = stage_create(
        &mut report,
        &journal,
        &persona,
        &channels,
        &registry,
        CreationInput {
            topic: &topic_title,
            source,
            avoid: &avoid,
            published: &published,
            age_days,
            memory: memory.as_ref(),
        },
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

    // Stage [3.7] — the claim-vs-source review. The guardrail is deterministic:
    // it can refuse a banned phrase, but it cannot know whether a claim is true.
    let approved = stage_grounding(&mut report, &journal, &approved, source, &registry).await?;
    if approved.is_empty() {
        record(
            &mut report,
            &journal,
            StageOutcome::new(
                "publish",
                StageStatus::Skipped,
                "no draft was grounded in its source — nothing was published",
            ),
        )
        .await?;
        return Ok(report);
    }

    let media = MediaConfig::load_or_default(&options.media_path).await?;

    stage_publish(
        &mut report,
        &journal,
        &options,
        &persona.publishing,
        &topic_title,
        &approved,
        &SlotState {
            published: &published,
            age_days,
            media: &media,
            memory: memory.as_ref(),
            card_pool: &card_pool,
        },
    )
    .await?;
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

    // HB-035: the video enlace is described too — it is gated by the policy
    // (weekly budget + warming ramp), so it only fires when a pack releases it.
    record(
        report,
        journal,
        StageOutcome::new(
            "media",
            StageStatus::DryRun,
            "would render and upload a short video when the policy releases the format \
             (weekly budget + warming ramp); otherwise the slot stays text or card",
        ),
    )
    .await?;

    Ok(())
}

/// Stage [2] — triage. Returns the chosen topic (its title and URL), or `None`
/// to stop. The URL travels with the title so the create path can resolve the
/// harvest candidate by either key (HB-057).
async fn stage_triage(
    report: &mut RunReport,
    journal: &Journal,
    persona: &PersonaPack,
    topics: &[Topic],
    registry: &autopilot_providers::ProvidersConfig,
) -> Result<Option<TopicScore>> {
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

    let selected = topic.clone();
    record(
        report,
        journal,
        StageOutcome::new(
            "triage",
            StageStatus::Ok,
            format!(
                "selected \"{}\" (score {:.2}) via {} ({})",
                selected.title, selected.score, completion.provider_id, completion.model
            ),
        ),
    )
    .await?;
    Ok(Some(selected))
}

/// The memory path for a persona: the explicit override, or the default the
/// scheduled learning stage writes (`var/learn/<slug>.json`).
fn memory_path(persona: &str, override_path: Option<&Path>) -> PathBuf {
    override_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("var/learn/{persona}.json")))
}

/// The persona's persisted memory proposal, or `None` when nothing has been
/// learned yet.
///
/// HB-046: the [3.7] grounding reviewer's refusals must teach, not just gate.
/// The persisted memory carries them (as *kinds of claim*), and the creation
/// stage is warned off them here so the same unsupported claim is not
/// rediscovered every slot. HB-034: the same file carries the learned
/// per-format budget movements the slot applies. A missing file is normal
/// (nothing learned yet); an unreadable one is journalled but never blocks the
/// run — this memory is advisory, unlike the fail-closed grounding review itself.
async fn load_memory(
    report: &mut RunReport,
    journal: &Journal,
    persona: &PersonaPack,
    override_path: Option<&Path>,
) -> Result<Option<autopilot_learn::MemoryUpdate>> {
    let path = memory_path(&persona.persona, override_path);

    let (update, outcome) = match autopilot_learn::read_memory(&path).await {
        Ok(Some(update)) => {
            let detail = format!(
                "loaded from {} — {} avoid warning(s), {} budget adjustment(s)",
                path.display(),
                update.avoid.len(),
                update.budget_adjustments.len()
            );
            (
                Some(update),
                StageOutcome::new("memory", StageStatus::Ok, detail),
            )
        }
        Ok(None) => (
            None,
            StageOutcome::new(
                "memory",
                StageStatus::Skipped,
                format!(
                    "no memory at {} — the creation stage starts clean",
                    path.display()
                ),
            ),
        ),
        Err(err) => (
            None,
            StageOutcome::new(
                "memory",
                StageStatus::Error,
                format!(
                    "memory at {} unreadable — continuing without it: {err:#}",
                    path.display()
                ),
            ),
        ),
    };

    record(report, journal, outcome).await?;
    Ok(update)
}

/// The inputs the creation stage writes against: the topic, the source it may
/// cite, the avoid memory it must respect (HB-046) and the slot's format policy
/// — the live counts, the account age and the learned budget (HB-034).
struct CreationInput<'a> {
    topic: &'a str,
    source: Option<Source<'a>>,
    avoid: &'a [String],
    published: &'a [PublishedEntry],
    age_days: Option<i64>,
    memory: Option<&'a autopilot_learn::MemoryUpdate>,
}

/// Stage [3] — creation. Returns the drafts grouped by platform.
async fn stage_create(
    report: &mut RunReport,
    journal: &Journal,
    persona: &PersonaPack,
    channels: &[Platform],
    registry: &autopilot_providers::ProvidersConfig,
    input: CreationInput<'_>,
) -> Result<Vec<(Platform, Vec<Variant>)>> {
    let mut drafts = Vec::new();
    let day = utc_day(now_unix());
    for platform in channels {
        // HB-034: the slot's real format availability (warming ramp + weekly
        // per-format budget + learned adjustment) is handed to the model, which
        // then chooses the format for the topic. The engine's deterministic gate
        // (`effective_format`) stays as the safety net, not the decider.
        let counts = week_counts(
            input.published,
            &persona.persona,
            platform.as_str(),
            week_start(day),
        );
        let budgets = budgets_for_platform(&persona.publishing.formats, input.memory, *platform);
        let availability = format_availability(input.age_days, &budgets, &counts);
        let prompt = build_creation_prompt_with_policy(
            persona,
            *platform,
            input.topic,
            input.source,
            input.avoid,
            &availability,
        );
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

/// Stage [3.7] — ground every approved draft in the source it came from.
///
/// The guardrail is deterministic: it can refuse a banned phrase, but it cannot
/// know whether a claim is *true*. This layer asks a sceptical reviewer — a
/// different role on the same chain, never the writer — whether each draft is
/// supported by the source the harvest read, and **fails closed**: an unsupported
/// draft is dropped, and a review that cannot run drops them all. Silence costs
/// one slot; a fabrication costs credibility.
async fn stage_grounding<'a>(
    report: &mut RunReport,
    journal: &Journal,
    approved: &'a [(Platform, &'a Variant)],
    source: Option<Source<'_>>,
    registry: &autopilot_providers::ProvidersConfig,
) -> Result<Vec<(Platform, &'a Variant)>> {
    // Fail-closed, exactly like a review that cannot run: without a source there
    // is nothing to verify the drafts against, so releasing them would be the
    // very fabrication this gate exists to stop. Silence costs one slot; a
    // fabrication costs credibility — the promise is "no source, no publish".
    let Some(source) = source else {
        report.failed = true;
        record(
            report,
            journal,
            StageOutcome::new(
                "grounding",
                StageStatus::Error,
                "no source text for this topic — every draft withheld (fail-closed)",
            ),
        )
        .await?;
        return Ok(Vec::new());
    };

    let mut platforms: Vec<Platform> = Vec::new();
    for (platform, _) in approved {
        if !platforms.contains(platform) {
            platforms.push(*platform);
        }
    }

    let mut kept = Vec::new();
    for platform in platforms {
        let variants: Vec<Variant> = approved
            .iter()
            .filter(|(candidate, _)| *candidate == platform)
            .map(|(_, variant)| (*variant).clone())
            .collect();

        let prompt = build_grounding_prompt(source, &variants);
        let reviewed = match complete_with_chain(
            registry,
            &prompt,
            JobKind::Other,
            COMPLETION_TIMEOUT,
        )
        .await
        {
            Ok(completion) => parse_grounding(&completion.text),
            Err(err) => Err(anyhow::anyhow!("{err:#}")),
        };

        let verdicts = match reviewed {
            Ok(verdicts) => verdicts,
            Err(err) => {
                // Fail-closed: a review that cannot run publishes nothing.
                report.failed = true;
                record(
                    report,
                    journal,
                    StageOutcome::new(
                        "grounding",
                        StageStatus::Error,
                        format!("grounding review unavailable — every draft withheld: {err:#}"),
                    )
                    .with_platform(platform),
                )
                .await?;
                continue;
            }
        };

        let supported = supported_labels(&verdicts);
        for (candidate, variant) in approved
            .iter()
            .filter(|(candidate, _)| *candidate == platform)
        {
            if supported.contains(&variant.label.as_str()) {
                kept.push((*candidate, *variant));
                continue;
            }
            let reason = verdicts
                .iter()
                .find(|verdict| verdict.label == variant.label)
                .map(|verdict| verdict.reason.clone())
                .unwrap_or_else(|| "the reviewer did not vouch for this draft".to_string());
            record(
                report,
                journal,
                StageOutcome::new(
                    "grounding",
                    StageStatus::Rejected,
                    format!(
                        "variant {} not supported by the source — {reason}",
                        variant.label
                    ),
                )
                .with_platform(*candidate)
                .with_variant(&variant.label)
                .with_reason(&reason),
            )
            .await?;
        }

        record(
            report,
            journal,
            StageOutcome::new(
                "grounding",
                StageStatus::Ok,
                format!(
                    "{} of {} draft(s) grounded in the source",
                    kept.len(),
                    approved.len()
                ),
            )
            .with_platform(platform),
        )
        .await?;
    }

    Ok(kept)
}

/// The frequency-policy state resolved once per run (HB-034).
///
/// The journal read, the account age and the learned budget are shared by the
/// day gate and by every platform's publish step, so they travel together
/// instead of as loose arguments. The media configuration travels with them: it
/// is resolved once for the run and consumed only when the policy releases the
/// video format.
struct SlotState<'a> {
    /// The posts already published (the journal, read once before the gates).
    published: &'a [PublishedEntry],
    /// The account's age in days, or `None` for a mature account (ramp inert).
    age_days: Option<i64>,
    /// Media configuration (Piper binary/voice, motion, scene images).
    media: &'a MediaConfig,
    /// The persona's memory proposal, carrying the learned per-format budget
    /// (HB-034) the publish gate applies.
    memory: Option<&'a autopilot_learn::MemoryUpdate>,
    /// The persona's card-template pool (HB-032): this persona's cards are drawn
    /// only from these templates, so the run never widens back to the whole
    /// catalogue on a typo.
    card_pool: &'a [&'static CardTemplate],
}

/// Stage [4] — publish. Only the platforms with an adapter are served.
async fn stage_publish(
    report: &mut RunReport,
    journal: &Journal,
    options: &RunOptions,
    policy: &PublishingPolicy,
    topic: &str,
    approved: &[(Platform, &Variant)],
    slot: &SlotState<'_>,
) -> Result<()> {
    let mut session: Option<Session> = None;
    let mut handle = String::new();
    let run_id = report.run_id.clone();

    // One slot, one variant per platform by default: publishing two
    // near-identical variants at the same moment reads as spam, so the A/B
    // comparison is sequential and the rotation alternates which variant each
    // slot releases. Both knobs are persona data (HB-040), not constants.
    let selection = select_variants(
        approved,
        policy.variants_per_slot,
        rotation_offset(&run_id),
        policy.selection,
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

        // Frequency policy (HB-034): the format the creation stage chose must fit
        // the warming ramp and the week's per-format budget. The model was told
        // the availability and chose accordingly; this is the deterministic
        // safety net — a card that would overshoot the budget ships as text (the
        // baseline), and only a slot whose text budget is *also* spent waits.
        let intended = intended_format(
            &variant.image_prompt,
            variant.video_plan.as_ref(),
            options.card,
        );
        let budgets = budgets_for_platform(&policy.formats, slot.memory, *platform);
        let counts = week_counts(
            slot.published,
            &report.persona,
            platform.as_str(),
            week_start(utc_day(now_unix())),
        );
        let Some(format) = effective_format(intended, slot.age_days, &budgets, &counts) else {
            record(
                report,
                journal,
                StageOutcome::new(
                    "publish",
                    StageStatus::Skipped,
                    "the week's format budget is spent — waiting for the next slot",
                )
                .with_platform(*platform)
                .with_variant(&variant.label),
            )
            .await?;
            continue;
        };
        if format != intended {
            record(
                report,
                journal,
                StageOutcome::new(
                    "slot",
                    StageStatus::Ok,
                    format!(
                        "{} is not available this slot — publishing as {} instead",
                        intended.as_str(),
                        format.as_str()
                    ),
                )
                .with_platform(*platform)
                .with_variant(&variant.label),
            )
            .await?;
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

        // One format per slot (`content-formats.md` §2): the format the frequency
        // policy resolved decides whether the post carries a card or a short
        // video. The guardrail already approved the text; the media is a
        // rendering of it, never a second claim.
        let embed = match format {
            Format::Video => {
                match stage_media(
                    report, journal, format, slot.media, &run_id, variant, current,
                )
                .await?
                {
                    Some(video) => Some(build_video_embed(&video)?),
                    None => None,
                }
            }
            _ => {
                match stage_card_image(
                    report,
                    journal,
                    format,
                    topic,
                    variant,
                    current,
                    slot.card_pool,
                )
                .await?
                {
                    Some(image) => Some(build_image_embed(&[image])?),
                    None => None,
                }
            }
        };

        let post = build_post_record_with_embed(&variant.text, &now_rfc3339_millis(), embed);
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
                    .with_variant(&variant.label)
                    .with_uri(&uri)
                    .with_format(format),
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
    format: Format,
    topic: &str,
    variant: &Variant,
    session: &Session,
    pool: &[&'static CardTemplate],
) -> Result<Option<PostImage>> {
    if format != Format::Card {
        return Ok(None);
    }

    // The card states the information; the post text introduces it. Rendering
    // the post text into the image — what happened until 2026-10-13 — made the
    // picture repeat the post. Fall back to the old shape when no card was sent.
    let spec = match variant.card.as_ref().filter(|card| card.is_usable()) {
        Some(card) => CardSpec::from_post(&card.title, &card.body, &session.handle),
        None => CardSpec::from_post(topic, &variant.text, &session.handle),
    };
    // HB-032: the template is chosen inside the persona's own pool, so the
    // rotation still varies per post while two personas never share a look. The
    // identity is the run, the variant and the persona — not the text — so a
    // re-run of the same post reproduces the same card.
    let identity = CardIdentity::new(report.run_id.as_str(), &variant.label, &session.handle);
    let template = template_for_identity_in_pool(&identity, pool);
    let png =
        PathBuf::from(DEFAULT_CARD_DIR).join(format!("{}-{}.png", report.run_id, variant.label));
    let result = async {
        let svg = render_png_with(&spec, template, &png, DEFAULT_RSVG_BINARY).await?;
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
                        "attached {} card {}x{} ({} bytes) from {} and {}",
                        template.name,
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

/// Render and upload a short video for a slot, or `None` when not requested.
///
/// The counterpart of [`stage_card_image`] for the video format (HB-035): render
/// the variant's plan through `autopilot-media` (Piper + ffmpeg), upload the MP4
/// to the Bluesky video service and return the ready [`PostVideo`]. Unlike an
/// image (an `uploadBlob` on the PDS), the video goes through the `app.bsky.video`
/// side-car and a polling job. Every failure — no plan, a render that cannot run
/// (no `piper-tts`, no stills), a refused upload — degrades the slot to a text
/// post, journaling why and marking the run failed, so a degraded slot is never
/// mistaken for a clean one.
async fn stage_media(
    report: &mut RunReport,
    journal: &Journal,
    format: Format,
    media: &MediaConfig,
    run_id: &str,
    variant: &Variant,
    session: &Session,
) -> Result<Option<PostVideo>> {
    if format != Format::Video {
        return Ok(None);
    }

    let Some(plan) = variant.video_plan.as_ref() else {
        record(
            report,
            journal,
            StageOutcome::new(
                "media",
                StageStatus::Skipped,
                "the video format has no plan for this variant — publishing text",
            )
            .with_platform(Platform::Bluesky)
            .with_variant(&variant.label),
        )
        .await?;
        return Ok(None);
    };

    let path = match render_video(plan, media, run_id, variant).await {
        Ok(path) => path,
        Err(err) => {
            report.failed = true;
            record(
                report,
                journal,
                StageOutcome::new(
                    "media",
                    StageStatus::Error,
                    format!("video render failed — publishing text only: {err:#}"),
                )
                .with_platform(Platform::Bluesky)
                .with_variant(&variant.label),
            )
            .await?;
            return Ok(None);
        }
    };

    let result = async {
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading the rendered video {}", path.display()))?;
        validate_video(bytes.len(), VIDEO_MIME_TYPE)?;
        let job = upload_video(
            DEFAULT_VIDEO_BASE_URL,
            session,
            &bytes,
            VIDEO_REQUEST_TIMEOUT,
        )
        .await?;
        let job = await_video_job(
            DEFAULT_VIDEO_BASE_URL,
            session,
            job,
            VIDEO_REQUEST_TIMEOUT,
            DEFAULT_VIDEO_POLL_INTERVAL,
            VIDEO_JOB_TIMEOUT,
        )
        .await?;
        let blob = job
            .blob
            .clone()
            .context("the completed video job carried no blob")?;
        Ok::<_, anyhow::Error>((bytes.len(), job, blob))
    }
    .await;

    match result {
        Ok((len, job, blob)) => {
            record(
                report,
                journal,
                StageOutcome::new(
                    "media",
                    StageStatus::Ok,
                    format!(
                        "attached {VIDEO_MIME_TYPE} ({len} bytes) {VERTICAL_WIDTH}x{VERTICAL_HEIGHT} — job {} {}",
                        job.job_id, job.state
                    ),
                )
                .with_platform(Platform::Bluesky)
                .with_variant(&variant.label),
            )
            .await?;
            Ok(Some(PostVideo {
                alt: video_alt(variant),
                blob,
                width: VERTICAL_WIDTH,
                height: VERTICAL_HEIGHT,
                presentation: None,
            }))
        }
        Err(err) => {
            report.failed = true;
            record(
                report,
                journal,
                StageOutcome::new(
                    "media",
                    StageStatus::Error,
                    format!("video upload failed — publishing text only: {err:#}"),
                )
                .with_platform(Platform::Bluesky)
                .with_variant(&variant.label),
            )
            .await?;
            Ok(None)
        }
    }
}

/// Render a plan to a vertical MP4 through `autopilot-media` (Piper + ffmpeg).
///
/// This is the one effectful, external-binary-dependent step of the video enlace:
/// it spawns `piper-tts` and `ffmpeg` and consumes real still images (resolved
/// behind the [`ImageSource`] seam). It is deliberately outside the pure
/// decision helpers, and every failure is handled by the caller.
async fn render_video(
    plan: &VideoPlan,
    media: &MediaConfig,
    run_id: &str,
    variant: &Variant,
) -> Result<PathBuf> {
    let work_dir = PathBuf::from(DEFAULT_VIDEO_DIR).join(format!("{run_id}-{}", variant.label));
    let images: Box<dyn ImageSource> = match &media.scene_images {
        Some(dir) => Box::new(SceneImageDir::new(dir.clone())),
        None => Box::new(PlaceholderImageSource),
    };
    let options = RenderOptions::new(work_dir, media.piper.clone(), media.motion, false);
    let output = render_media(plan, images.as_ref(), &options).await?;
    Ok(output.video)
}

/// Alt text for a video, derived from the caption.
///
/// `app.bsky.embed.video` marks `alt` optional (unlike an image embed), but a
/// described video is still better than an undescribed one.
fn video_alt(variant: &Variant) -> Option<String> {
    let text = variant.text.trim();
    (!text.is_empty()).then(|| text.to_string())
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
    use autopilot_core::{Channel, HarvestPolicy, PublishingPolicy, Rubric, Voice};
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
            publishing: PublishingPolicy::default(),
            harvest: HarvestPolicy::default(),
            visual: None,
        }
    }

    fn variant(label: &str, text: &str) -> Variant {
        Variant {
            label: label.into(),
            text: text.into(),
            hashtags: vec![],
            image_prompt: String::new(),
            card: None,
            video_plan: None,
        }
    }

    #[test]
    fn a_slot_publishes_one_variant_per_platform_and_alternates() {
        let a = variant("A", "texto a");
        let b = variant("B", "texto b");
        let approved = vec![(Platform::Bluesky, &a), (Platform::Bluesky, &b)];

        // Rotation 0 → A, rotation 1 → B: consecutive slots alternate, so no
        // moment ever shows two near-identical posts.
        let first = select_variants(&approved, 1, 0, SelectionRule::Alternate);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1.label, "A");

        let second = select_variants(&approved, 1, 1, SelectionRule::Alternate);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].1.label, "B");
    }

    #[test]
    fn every_platform_still_gets_its_variant_in_the_same_slot() {
        let a = variant("A", "texto a");
        let b = variant("B", "texto b");
        let approved = vec![(Platform::Bluesky, &a), (Platform::X, &b)];
        let selected = select_variants(&approved, 1, 0, SelectionRule::Alternate);
        assert_eq!(selected.len(), 2);
        assert!(selected.iter().any(|(p, _)| *p == Platform::Bluesky));
        assert!(selected.iter().any(|(p, _)| *p == Platform::X));
    }

    #[test]
    fn the_policy_decides_how_many_variants_a_slot_releases() {
        // The count is persona data, not a constant: a policy of two releases
        // both (the deliberate opt-in), and the default releases one.
        let a = variant("A", "texto a");
        let b = variant("B", "texto b");
        let approved = vec![(Platform::Bluesky, &a), (Platform::Bluesky, &b)];

        let both = select_variants(&approved, 2, 0, SelectionRule::Alternate);
        assert_eq!(both.len(), 2);

        let policy = PublishingPolicy::default();
        let sequential = select_variants(&approved, policy.variants_per_slot, 0, policy.selection);
        assert_eq!(sequential.len(), 1);
    }

    #[test]
    fn the_first_selection_rule_ignores_the_rotation() {
        let a = variant("A", "texto a");
        let b = variant("B", "texto b");
        let approved = vec![(Platform::Bluesky, &a), (Platform::Bluesky, &b)];

        // `First` is deterministic: every slot takes A, whatever the run id.
        for rotation in [0, 1, 2, 7] {
            let selected = select_variants(&approved, 1, rotation, SelectionRule::First);
            assert_eq!(selected[0].1.label, "A");
        }
    }

    #[test]
    fn the_rotation_comes_from_the_run_id_digits() {
        assert_eq!(rotation_offset("tech-curiosities-1791849666"), 6);
        assert_eq!(rotation_offset("persona-1791849661"), 1);
        assert_eq!(rotation_offset("no-digits"), 0);
    }

    #[test]
    fn the_memory_defaults_to_where_learn_writes_it() {
        assert_eq!(
            memory_path("tech-curiosities", None),
            PathBuf::from("var/learn/tech-curiosities.json")
        );
        let explicit = PathBuf::from("/tmp/memory.yaml");
        assert_eq!(memory_path("anything", Some(&explicit)), explicit);
    }

    #[test]
    fn a_refused_claim_in_the_memory_reaches_the_creation_prompt() {
        // The end-to-end link HB-046 promises: a claim the [3.7] reviewer
        // refused is read back from the memory proposal and warns the next
        // creation, so the same assertion is not rediscovered every slot.
        let claim = "Claims the agent is 'inspectable, not someone's cloud API' — the source \
                     never says whether KylinBot/Xiao K is local/inspectable or cloud-based.";
        let update = autopilot_learn::MemoryUpdate {
            persona: "tech-curiosities".into(),
            generated_at_unix: 0,
            platform: Platform::Bluesky,
            winner: None,
            min_samples: 1,
            variants: vec![],
            few_shot: vec![],
            avoid: vec![autopilot_learn::AvoidEntry {
                rule: "grounding-rejected".into(),
                penalty: autopilot_learn::PENALTY_GROUNDING_REJECTED,
                evidence: claim.into(),
            }],
            budget_adjustments: vec![],
            schedule_hints: vec![],
            reason: "test".into(),
        };

        let warnings = autopilot_learn::avoid_warnings(&update);
        assert!(warnings[0].contains("grounding-rejected"));
        let prompt = autopilot_pipeline::build_creation_prompt_with_avoid(
            &persona(),
            Platform::Bluesky,
            "a topic",
            None,
            &warnings,
        );
        assert!(prompt.contains(claim), "the claim must reach the prompt");
        assert!(prompt.contains("never repeat them"));
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
    fn the_harvest_queries_rotate_across_slots() {
        // HB-033: consecutive slots walk the pack's list, so two slots never
        // harvest the same subject; a single-query pack is unchanged.
        let queries = vec!["a".to_string(), "b".to_string()];
        assert_eq!(select_harvest_query(&queries, 0), Some("a"));
        assert_eq!(select_harvest_query(&queries, 1), Some("b"));
        assert_eq!(select_harvest_query(&queries, 2), Some("a"));
        assert_eq!(select_harvest_query(&["only".to_string()], 7), Some("only"));
    }

    #[test]
    fn an_empty_query_list_has_no_selection() {
        assert_eq!(select_harvest_query(&[], 3), None);
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
    fn a_paraphrased_triage_title_still_resolves_to_its_source_by_url() {
        // HB-057: the triage model echoes the title back paraphrased. Matching
        // the harvest candidate by exact title alone resolved to *no* source, so
        // the [3.7] gate withheld the whole slot (fail-closed, HB-041) and the
        // pipeline starved. The URL the shortlist carries is a second key.
        let topics = vec![
            Topic {
                title: "Descoberta sobre o cérebro".into(),
                url: "https://a.example/artigo".into(),
                content: "o cérebro consome glicose".into(),
                engine: "google".into(),
            },
            Topic {
                title: "Outro tema".into(),
                url: "https://b.example/outro".into(),
                content: "irrelevante".into(),
                engine: "duckduckgo".into(),
            },
        ];

        let resolved = resolve_candidate(
            &topics,
            "Como o cérebro gasta energia",
            "https://a.example/artigo",
        )
        .expect("a paraphrased title still resolves through the URL");
        assert_eq!(resolved.url, "https://a.example/artigo");
        assert_eq!(resolved.content, "o cérebro consome glicose");
    }

    #[test]
    fn a_paraphrased_title_without_its_url_resolves_to_no_source() {
        // The counterpart of the fix: if neither key matches, resolution stays
        // empty — the source is absent, and stage_grounding fail-closes exactly
        // as HB-041 requires (no source, no publish).
        let topics = vec![Topic {
            title: "Descoberta sobre o cérebro".into(),
            url: "https://a.example/artigo".into(),
            content: "o cérebro consome glicose".into(),
            engine: "google".into(),
        }];

        assert!(
            resolve_candidate(&topics, "Como o cérebro gasta energia", "").is_none(),
            "a paraphrased title with no URL has no source"
        );
        assert!(
            resolve_candidate(
                &topics,
                "Como o cérebro gasta energia",
                "https://outro.example"
            )
            .is_none(),
            "a URL that matches no candidate is not a source"
        );
        // An exact title still resolves without needing the URL.
        assert!(resolve_candidate(&topics, "Descoberta sobre o cérebro", "").is_some());
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

    #[tokio::test]
    async fn the_create_path_withholds_every_draft_when_there_is_no_source() {
        // HB-041: the documented contract is fail-closed — "no review, no
        // publish". A topic the harvest could not resolve to a source has
        // nothing to verify against, so it must be withheld, never released
        // unverified (the fabrication this gate exists to stop).
        let journal_path = std::env::temp_dir().join(format!(
            "autopilot-grounding-nosource-{}.jsonl",
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
        // An empty chain would fail if the gate tried to call it; with no source
        // it must short-circuit and withhold without a provider at all.
        let registry = autopilot_providers::ProvidersConfig {
            providers: Vec::new(),
        };
        let a = variant("A", "O cérebro gasta 20% da energia do corpo em repouso.");
        let approved = vec![(Platform::Bluesky, &a)];

        let kept = stage_grounding(&mut report, &journal, &approved, None, &registry)
            .await
            .expect("no source is a clean, fail-closed stop, not an error");

        assert!(
            kept.is_empty(),
            "fail-closed: nothing is published without a source"
        );
        assert!(
            report.failed,
            "a withheld publish must not look like a clean run"
        );

        let raw = tokio::fs::read_to_string(&journal_path)
            .await
            .expect("journal written");
        let event: serde_json::Value =
            serde_json::from_str(raw.lines().next().unwrap_or("{}")).expect("valid journal JSON");
        assert_eq!(event["stage"], "grounding");
        assert_eq!(event["status"], "error");
        assert!(
            event["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("fail-closed"),
            "the journal must say the drafts were withheld: {event}"
        );

        tokio::fs::remove_file(&journal_path).await.ok();
    }

    /// A published entry on a given UTC day (`day` = days since the epoch).
    fn entry(persona: &str, platform: &str, day: i64, format: Option<Format>) -> PublishedEntry {
        PublishedEntry {
            persona: persona.into(),
            platform: platform.into(),
            variant: "A".into(),
            uri: format!("at://{persona}/{day}"),
            format: format.map(|value| value.as_str().to_string()),
            published_at_unix: (day as u64) * 86_400,
        }
    }

    #[test]
    fn the_warming_ramp_opens_one_week_at_a_time() {
        // content-formats.md §1: week 1 text-only, week 2 adds the card, week 3+
        // adds the video. The week is 1-based and clamps at steady state.
        assert_eq!(warming_week(0), 1);
        assert_eq!(warming_week(6), 1);
        assert_eq!(warming_week(7), 2);
        assert_eq!(warming_week(13), 2);
        assert_eq!(warming_week(14), 3);
        assert_eq!(warming_week(400), 3);
        // A start date in the future is still week 1, never a negative week.
        assert_eq!(warming_week(-5), 1);

        assert_eq!(warming_releases(0), [Format::Text]);
        assert!(warming_releases(7).contains(&Format::Card));
        assert!(!warming_releases(7).contains(&Format::Video));
        assert!(warming_releases(14).contains(&Format::Video));

        assert_eq!(warming_ceiling(0), 1);
        assert_eq!(warming_ceiling(7), 2);
        assert_eq!(warming_ceiling(14), 2);
    }

    #[test]
    fn the_week_starts_on_monday() {
        // 1970-01-01 was a Thursday (day 0), so its Monday is three days back.
        assert_eq!(week_start(0), -3);
        assert_eq!(week_start(3), -3);
        assert_eq!(week_start(4), 4);
        // 1970-01-08 (Thursday, day 7) belongs to the week of Monday the 5th.
        assert_eq!(week_start(7), 4);
    }

    #[test]
    fn published_on_counts_only_that_day_and_channel() {
        let history = vec![
            entry("demo", "bluesky", 10, Some(Format::Text)),
            entry("demo", "bluesky", 10, Some(Format::Card)),
            entry("demo", "bluesky", 9, Some(Format::Text)),
            entry("outra", "bluesky", 10, Some(Format::Text)),
            entry("demo", "x", 10, Some(Format::Text)),
        ];
        assert_eq!(published_on(&history, "demo", "bluesky", 10), 2);
        assert_eq!(published_on(&history, "demo", "bluesky", 9), 1);
        assert_eq!(published_on(&history, "demo", "x", 10), 1);
        assert_eq!(published_on(&history, "demo", "bluesky", 11), 0);
    }

    #[test]
    fn week_counts_totals_each_format_and_ignores_earlier_weeks() {
        // Day 10 is a Saturday; its week starts Monday day 4.
        let history = vec![
            entry("demo", "bluesky", 10, Some(Format::Card)),
            entry("demo", "bluesky", 9, Some(Format::Text)),
            entry("demo", "bluesky", 4, Some(Format::Text)),
            entry("demo", "bluesky", 3, Some(Format::Card)), // previous week
            entry("outra", "bluesky", 10, Some(Format::Card)),
        ];
        let counts = week_counts(&history, "demo", "bluesky", week_start(10));
        assert_eq!(counts.text, 2);
        assert_eq!(counts.card, 1);
        assert_eq!(counts.video, 0);
        // An unlabelled older line counts as the baseline format, never lost.
        let legacy = vec![entry("demo", "bluesky", 10, None)];
        assert_eq!(week_counts(&legacy, "demo", "bluesky", 4).text, 1);
    }

    #[test]
    fn a_card_over_budget_becomes_text_but_a_full_text_budget_waits() {
        let budgets = FormatBudgets::default();
        let mut counts = FormatCounts::default();
        assert_eq!(
            effective_format(Format::Card, None, &budgets, &counts),
            Some(Format::Card)
        );

        // The card budget is spent, so the card ships as text (content commands).
        counts.card = budgets.card;
        assert_eq!(
            effective_format(Format::Card, None, &budgets, &counts),
            Some(Format::Text)
        );

        // The text budget is *also* spent: the slot waits for the next one.
        counts.text = budgets.text;
        assert_eq!(
            effective_format(Format::Card, None, &budgets, &counts),
            None
        );
        assert_eq!(
            effective_format(Format::Text, None, &budgets, &counts),
            None
        );
    }

    #[test]
    fn the_ramp_holds_a_new_account_to_text_even_with_budget_left() {
        let budgets = FormatBudgets::default();
        let counts = FormatCounts::default();
        // Week 1: a card the LLM wanted becomes text, because the ramp releases
        // no card yet — not because a budget ran out.
        assert_eq!(
            effective_format(Format::Card, Some(0), &budgets, &counts),
            Some(Format::Text)
        );
        // Week 2 releases the card, and it has budget.
        assert_eq!(
            effective_format(Format::Card, Some(7), &budgets, &counts),
            Some(Format::Card)
        );
    }

    #[test]
    fn the_daily_ceiling_is_declared_and_tightened_by_the_ramp() {
        let mut persona = persona();
        persona.channels[0].posts_per_day = 2; // bluesky
        assert_eq!(channel_ceiling(&persona, Platform::Bluesky, None), 2);
        assert_eq!(channel_ceiling(&persona, Platform::Bluesky, Some(0)), 1);
        assert_eq!(channel_ceiling(&persona, Platform::Bluesky, Some(7)), 2);
    }

    #[test]
    fn a_slot_is_an_opportunity_not_an_obligation() {
        // One post a day is declared; the journal already holds today's post, so
        // the slot has no room and the run would skip before any LLM call.
        let persona = persona();
        let day = 20_000;
        let history = vec![entry("demo", "bluesky", day, Some(Format::Text))];
        assert!(!day_has_room(
            &persona,
            &[Platform::Bluesky],
            &history,
            day,
            None
        ));
        // Yesterday's post does not fill today's slot.
        assert!(day_has_room(
            &persona,
            &[Platform::Bluesky],
            &history,
            day + 1,
            None
        ));

        // The run proceeds while *any* targeted channel still has room.
        let history = vec![entry("demo", "bluesky", day, Some(Format::Text))];
        assert!(day_has_room(
            &persona,
            &[Platform::Bluesky, Platform::X],
            &history,
            day,
            None
        ));
    }

    #[test]
    fn the_intended_format_follows_the_card_mode_and_the_prompt() {
        assert_eq!(
            intended_format("a terminal", None, CardAttachment::Off),
            Format::Text
        );
        assert_eq!(
            intended_format("a terminal", None, CardAttachment::WhenPrompted),
            Format::Card
        );
        assert_eq!(
            intended_format("   ", None, CardAttachment::WhenPrompted),
            Format::Text
        );
        assert_eq!(
            intended_format("", None, CardAttachment::Always),
            Format::Card
        );
    }

    fn video_plan() -> VideoPlan {
        VideoPlan {
            hook: "um gancho falado".into(),
            scenes: vec![autopilot_media::Scene {
                narration: "uma cena".into(),
                image_prompt: "um cérebro".into(),
            }],
            call_to_action: "siga".into(),
        }
    }

    #[test]
    fn a_video_plan_wins_over_the_card_prompt() {
        // HB-035: a topic the model shaped as a short video is a video, whatever
        // the image prompt says — the plan is the stronger signal.
        let plan = video_plan();
        assert_eq!(
            intended_format("a terminal", Some(&plan), CardAttachment::WhenPrompted),
            Format::Video
        );
        assert_eq!(
            intended_format("", Some(&plan), CardAttachment::Off),
            Format::Video
        );
    }

    #[test]
    fn the_video_format_respects_the_budget_and_the_warming_ramp() {
        // content-formats.md §3: the default video budget is 0, so a video the
        // model wanted ships as text — the format is gated by the policy.
        let budgets = FormatBudgets::default();
        let counts = FormatCounts::default();
        assert_eq!(budgets.budget(Format::Video), 0);
        assert_eq!(
            effective_format(Format::Video, None, &budgets, &counts),
            Some(Format::Text)
        );

        // A pack that budgets one video a week and is past its ramp releases it.
        let budgets = FormatBudgets {
            video: 1,
            ..FormatBudgets::default()
        };
        assert_eq!(
            effective_format(Format::Video, Some(14), &budgets, &counts),
            Some(Format::Video)
        );
        // Week 1 and week 2 do not release the video: the ramp tightens it back
        // to text even though the budget is available.
        assert_eq!(
            effective_format(Format::Video, Some(0), &budgets, &counts),
            Some(Format::Text)
        );
        assert_eq!(
            effective_format(Format::Video, Some(7), &budgets, &counts),
            Some(Format::Text)
        );
        // The budget spent → text; text spent too → the slot waits.
        let spent_video = FormatCounts {
            video: 1,
            ..FormatCounts::default()
        };
        assert_eq!(
            effective_format(Format::Video, Some(14), &budgets, &spent_video),
            Some(Format::Text)
        );
        let spent_all = FormatCounts {
            video: 1,
            text: budgets.text,
            ..FormatCounts::default()
        };
        assert_eq!(
            effective_format(Format::Video, Some(14), &budgets, &spent_all),
            None
        );
    }

    #[test]
    fn the_ramp_birth_is_detected_from_the_first_published_post() {
        // HB-034: the ramp is autonomous — the birth is the earliest
        // publication ever recorded, not a field someone fills in by hand.
        assert_eq!(detected_start_day(&[]), None, "an empty base has no birth");

        let one = vec![entry("demo", "bluesky", 20_000, Some(Format::Text))];
        assert_eq!(detected_start_day(&one), Some(20_000));

        let history = vec![
            entry("demo", "bluesky", 20_010, Some(Format::Card)),
            entry("demo", "bluesky", 20_000, Some(Format::Text)),
            entry("demo", "bluesky", 20_005, Some(Format::Text)),
        ];
        assert_eq!(detected_start_day(&history), Some(20_000));
    }

    #[test]
    fn a_brand_new_account_ramps_from_its_first_post_and_an_old_one_does_not() {
        // One post, published today: the account is in week 1 (text only).
        let day = 20_000;
        let one = vec![entry("demo", "bluesky", day, Some(Format::Text))];
        let start = detected_start_day(&one).expect("a birth");
        let age = day - start;
        assert_eq!(warming_week(age), 1);
        assert_eq!(warming_ceiling(age), 1);
        assert!(!warming_releases(age).contains(&Format::Card));

        // The same account a month later is at steady state — the ramp opened
        // on its own, with no `started_on` ever written.
        let old = vec![entry("demo", "bluesky", day - 30, Some(Format::Text))];
        let start = detected_start_day(&old).expect("a birth");
        let age = day - start;
        assert_eq!(warming_week(age), 3);
        assert!(warming_releases(age).contains(&Format::Video));
    }

    #[test]
    fn the_format_availability_mirrors_the_ramp_and_the_budget() {
        let budgets = FormatBudgets::default();
        let counts = FormatCounts::default();
        // Week 1 releases text only.
        assert_eq!(
            format_availability(Some(0), &budgets, &counts),
            FormatAvailability {
                text: true,
                card: false,
                video: false,
            }
        );
        // Steady state releases text and card; the default video budget is 0, so
        // the video stays unavailable even after the ramp.
        assert_eq!(
            format_availability(Some(21), &budgets, &counts),
            FormatAvailability {
                text: true,
                card: true,
                video: false,
            }
        );
        // A spent card budget removes the card from the availability.
        let spent_card = FormatCounts {
            card: budgets.card,
            ..FormatCounts::default()
        };
        let limited = format_availability(None, &budgets, &spent_card);
        assert!(!limited.allows(Format::Card));
        assert!(limited.allows(Format::Text));
    }

    #[test]
    fn the_model_choice_is_degraded_by_the_safety_net_never_dropped_embedless() {
        // The model chose a card; the card budget is spent. The deterministic
        // fallback ships the post as text — never a card without budget, and
        // never a post that promised an embed and lost it silently.
        let budgets = FormatBudgets::default();
        let spent_card = FormatCounts {
            card: budgets.card,
            ..FormatCounts::default()
        };
        assert_eq!(
            effective_format(Format::Card, None, &budgets, &spent_card),
            Some(Format::Text)
        );
        // Text is spent too: the slot waits for the next one.
        let spent_all = FormatCounts {
            card: budgets.card,
            text: budgets.text,
            ..FormatCounts::default()
        };
        assert_eq!(
            effective_format(Format::Card, None, &budgets, &spent_all),
            None
        );
    }

    #[test]
    fn the_learned_budget_only_moves_the_platform_it_was_learned_for() {
        // HB-034: the learned adjustment is scoped per persona × platform, so a
        // proposal learned on bluesky never changes the X budget.
        let base = FormatBudgets::default();
        let update = autopilot_learn::MemoryUpdate {
            persona: "demo".into(),
            generated_at_unix: 0,
            platform: Platform::Bluesky,
            winner: None,
            min_samples: 1,
            variants: vec![],
            few_shot: vec![],
            avoid: vec![],
            budget_adjustments: vec![autopilot_learn::BudgetAdjustment {
                format: Format::Card,
                step: 1,
                samples: 10,
                mean_score: 30.0,
                baseline: 10.0,
                reason: "card beats the median".into(),
            }],
            schedule_hints: vec![],
            reason: "test".into(),
        };
        let bluesky = budgets_for_platform(&base, Some(&update), Platform::Bluesky);
        assert_eq!(bluesky.card, base.card + 1);
        let x = budgets_for_platform(&base, Some(&update), Platform::X);
        assert_eq!(x, base, "the X budget is untouched");
        // No memory at all: the pack's declared budget.
        assert_eq!(budgets_for_platform(&base, None, Platform::Bluesky), base);
    }

    #[tokio::test]
    async fn the_media_stage_is_a_no_op_for_a_non_video_slot() {
        // The enlace decides inside `stage_media`: a text or card slot never
        // renders, never uploads and never touches the network.
        let journal_path =
            std::env::temp_dir().join(format!("autopilot-media-skip-{}.jsonl", std::process::id()));
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
        // A session that would be refused if a request were attempted: the point
        // is that none is.
        let session = Session {
            did: "did:plc:abc".into(),
            handle: "demo.bsky.social".into(),
            access_jwt: "irrelevant".into(),
        };

        let media = MediaConfig::default();
        for format in [Format::Text, Format::Card] {
            let video = stage_media(
                &mut report,
                &journal,
                format,
                &media,
                "run-test",
                &variant("A", "texto"),
                &session,
            )
            .await
            .expect("a non-video slot is a clean no-op");
            assert!(video.is_none());
        }
        assert!(report.stages.is_empty(), "nothing is even journalled");
        assert!(!report.failed);

        tokio::fs::remove_file(&journal_path).await.ok();
    }

    #[tokio::test]
    async fn a_video_render_failure_degrades_to_text_and_fails_loudly() {
        // The render needs `piper-tts` and real stills; with the default config
        // and no scene images it cannot run, so the slot must degrade to a text
        // post, journal the reason and mark the run failed (never a silent drop).
        let journal_path = std::env::temp_dir().join(format!(
            "autopilot-media-degrade-{}.jsonl",
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
        let session = Session {
            did: "did:plc:abc".into(),
            handle: "demo.bsky.social".into(),
            access_jwt: "irrelevant".into(),
        };
        let mut with_video = variant("A", "um vídeo sobre o cérebro");
        with_video.video_plan = Some(video_plan());

        let video = stage_media(
            &mut report,
            &journal,
            Format::Video,
            &MediaConfig::default(),
            "run-test",
            &with_video,
            &session,
        )
        .await
        .expect("the failure is handled, not returned");
        assert!(video.is_none(), "the post falls back to text");
        assert!(report.failed, "a degraded slot is never a clean run");

        let raw = tokio::fs::read_to_string(&journal_path)
            .await
            .expect("journal written");
        let event: serde_json::Value =
            serde_json::from_str(raw.lines().next().unwrap_or("{}")).expect("valid journal JSON");
        assert_eq!(event["stage"], "media");
        assert_eq!(event["status"], "error");
        assert!(
            event["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("publishing text only"),
            "the journal must explain the fallback: {event}"
        );

        tokio::fs::remove_file(&journal_path).await.ok();
    }
}
