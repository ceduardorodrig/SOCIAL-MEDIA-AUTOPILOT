//! The engagement run: gather candidates, plan under the caps, then act slowly.
//!
//! Interactions run on their own schedule (never with publishing), skip some
//! runs on purpose, and space every action with jitter. A `--dry-run` performs
//! no network call and no write: it prints exactly what the run would do.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use autopilot_core::{PersonaPack, load_persona, now_unix};
use autopilot_providers::{JobKind, ProvidersConfig, complete_with_chain, load_config};
use autopilot_publish::{BlueskyCredentials, DEFAULT_BLUESKY_BASE_URL, Session, create_session};
use serde::Deserialize;

use crate::bluesky::{
    DEFAULT_BLUESKY_READ_URL, DEFAULT_ENGAGE_TIMEOUT, create_follow, create_like, create_reply,
    get_author_feed, get_interest_feed, get_post_thread, get_profile, list_notifications,
};
use crate::candidates::{
    Candidate, CandidateKind, FollowTarget, InterestFeed, Notification, Profile, Thread, combine,
    follow_candidates, mention_candidates, own_post_reply_candidates, parse_feed_posts_value,
    parse_thread_value, proactive_candidates,
};
use crate::config::EngageConfig;
use crate::jitter::{Rng, jitter_secs};
use crate::planner::{Action, Plan, PlanInput, plan};
use crate::prompt::{build_reply_prompt, parse_reply, screen_reply};
use crate::quality::screen_proactive_reply;
use crate::records::StrongRef;
use crate::review::{ReviewOutcome, build_review_prompt, parse_review};
use crate::state::{ActionKind, ActionLog, StateEntry, append_entry};

/// How long to wait for one LLM completion.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(300);

/// Options for one `autopilot engage run`.
#[derive(Debug, Clone)]
pub struct EngageOptions {
    pub persona_path: PathBuf,
    pub config_path: PathBuf,
    pub state_path: PathBuf,
    pub providers_path: PathBuf,
    /// Offline fixture for a dry run (no network): notifications, threads and
    /// profiles. Ignored in a live run.
    pub fixture: Option<PathBuf>,
    pub dry_run: bool,
}

/// One action that was attempted.
#[derive(Debug, Clone)]
pub struct ExecutedAction {
    pub action: String,
    pub target: String,
    pub status: String,
    pub detail: String,
}

impl ExecutedAction {
    fn ok(action: &str, target: &str, detail: impl Into<String>) -> Self {
        Self {
            action: action.to_string(),
            target: target.to_string(),
            status: "ok".to_string(),
            detail: detail.into(),
        }
    }

    fn rejected(action: &str, target: &str, detail: impl Into<String>) -> Self {
        Self {
            action: action.to_string(),
            target: target.to_string(),
            status: "rejected".to_string(),
            detail: detail.into(),
        }
    }

    fn error(action: &str, target: &str, detail: impl Into<String>) -> Self {
        Self {
            action: action.to_string(),
            target: target.to_string(),
            status: "error".to_string(),
            detail: detail.into(),
        }
    }
}

/// The result of one engagement run.
#[derive(Debug, Clone)]
pub struct EngageReport {
    pub persona: String,
    pub dry_run: bool,
    pub kill_switch: bool,
    pub in_window: bool,
    pub skipped_run: bool,
    pub notes: Vec<String>,
    pub planned: Vec<String>,
    pub executed: Vec<ExecutedAction>,
    pub state_path: PathBuf,
    pub failed: bool,
}

/// Execute one engagement run for a persona.
pub async fn run(options: EngageOptions) -> Result<EngageReport> {
    let config = EngageConfig::load(&options.config_path).await?;
    config.validate()?;
    let persona = load_persona(&options.persona_path).await?;
    let state = ActionLog::load(&options.state_path).await?;

    let now = now_unix();
    let kill_switch = config.kill_switch_active();
    let in_window = config.in_window(now);

    let mut report = EngageReport {
        persona: persona.persona.clone(),
        dry_run: options.dry_run,
        kill_switch,
        in_window,
        skipped_run: false,
        notes: Vec::new(),
        planned: Vec::new(),
        executed: Vec::new(),
        state_path: options.state_path.clone(),
        failed: false,
    };

    if options.dry_run {
        dry_run(&options, &config, &state, now, &mut report).await?;
        return Ok(report);
    }

    // A live run obeys the kill switch and the windows before doing anything.
    if kill_switch {
        report.notes.push(format!(
            "kill switch `{}` is engaged — stopping without any action",
            config.kill_switch_env
        ));
        return Ok(report);
    }
    if !in_window {
        report.notes.push(format!(
            "outside the engagement windows {:?} — waiting for the next one",
            config.windows
        ));
        return Ok(report);
    }

    let credentials = BlueskyCredentials::from_env()?;
    let session = match create_session(
        DEFAULT_BLUESKY_BASE_URL,
        &credentials,
        DEFAULT_ENGAGE_TIMEOUT,
    )
    .await
    {
        Ok(session) => session,
        Err(err) => {
            report.failed = true;
            report
                .notes
                .push(format!("bluesky authentication failed: {err:#}"));
            return Ok(report);
        }
    };

    let (candidates, follows) =
        gather_live(&config, &session, &state, now, &mut report.notes).await?;

    let mut rng = Rng::from_clock();
    let chosen = plan(
        &PlanInput {
            candidates: &candidates,
            follows: &follows,
            state: &state,
            config: &config,
            now_unix: now,
        },
        &mut rng,
    );
    report.skipped_run = chosen.skipped_run;
    report.planned = chosen.actions.iter().map(Action::describe).collect();

    if chosen.skipped_run || chosen.actions.is_empty() {
        report.notes.push(chosen.reason.clone());
        return Ok(report);
    }

    let registry = load_config(&options.providers_path).await?;
    let our_tokens = our_account_tokens(&persona, &session);
    let mut execution = Execution {
        persona: &persona,
        session: &session,
        registry: &registry,
        config: &config,
        state_path: &options.state_path,
        our_tokens: &our_tokens,
        rng: &mut rng,
    };
    report.executed = execution.perform_all(&chosen).await;
    Ok(report)
}

/// The strings a proactive reply must never contain: our handles, DID, display
/// name and persona slug. Used by the deterministic quality bar.
fn our_account_tokens(persona: &PersonaPack, session: &Session) -> Vec<String> {
    let mut tokens = vec![
        session.did.clone(),
        session.handle.clone(),
        persona.display_name.clone(),
        persona.persona.clone(),
    ];
    for channel in &persona.channels {
        if !channel.handle.trim().is_empty() {
            tokens.push(channel.handle.clone());
        }
    }
    tokens.retain(|token| token.trim().chars().count() >= 4);
    tokens.sort();
    tokens.dedup();
    tokens
}

/// Describe the plan without any network call or write.
async fn dry_run(
    options: &EngageOptions,
    config: &EngageConfig,
    state: &ActionLog,
    now: u64,
    report: &mut EngageReport,
) -> Result<()> {
    report
        .notes
        .push("dry run — no network call and no write".to_string());
    if report.kill_switch {
        report.notes.push(format!(
            "kill switch `{}` is engaged — a live run would abort",
            config.kill_switch_env
        ));
    }
    if !report.in_window {
        report.notes.push(format!(
            "outside the engagement windows {:?} — a live run would wait",
            config.windows
        ));
    }

    let (candidates, follows) = match &options.fixture {
        Some(path) => gather_offline(path, config, state, now, "").await?,
        None => {
            report.notes.push(format!(
                "would fetch up to {} notifications and scan {} of our own posts for comments",
                config.notification_limit, config.own_posts_to_scan
            ));
            if config.proactive.enabled {
                report.notes.push(format!(
                    "would read the recent posts of up to {} curated accounts (newer than {}h)",
                    config.proactive.accounts_per_run, config.proactive.max_post_age_hours
                ));
            }
            (Vec::new(), Vec::new())
        }
    };

    let mut rng = Rng::from_clock();
    let chosen = plan(
        &PlanInput {
            candidates: &candidates,
            follows: &follows,
            state,
            config,
            now_unix: now,
        },
        &mut rng,
    );
    report.skipped_run = chosen.skipped_run;
    report.planned = describe_plan(&chosen, config, &mut rng);
    report.notes.push(format!(
        "caps: {} replies, {} likes, {} follows/day ({} /week); at most {} writes per run",
        config.caps.replies_per_day,
        config.caps.likes_per_day,
        config.caps.follows_per_day,
        config.caps.follows_per_week,
        config.caps.max_actions_per_run,
    ));
    if config.proactive.enabled {
        report.notes.push(format!(
            "proactive: ≤{} reply/day, ≤{} per account/week, ≥{}h gap before the same account again",
            config.proactive.replies_per_day,
            config.proactive.replies_per_account_per_week,
            config.proactive.min_gap_hours,
        ));
    }
    Ok(())
}

/// Render a plan, showing the jittered delay that would precede each action.
fn describe_plan(chosen: &Plan, config: &EngageConfig, rng: &mut Rng) -> Vec<String> {
    if chosen.skipped_run {
        return vec![chosen.reason.clone()];
    }

    let mut lines = Vec::new();
    for (index, action) in chosen.actions.iter().enumerate() {
        let delay = if index == 0 {
            0
        } else {
            delay_for(config, action, rng)
        };
        if delay == 0 {
            lines.push(format!("{}: {}", action.kind().as_str(), action.describe()));
        } else {
            lines.push(format!(
                "after ~{delay}s — {}: {}",
                action.kind().as_str(),
                action.describe()
            ));
        }
    }
    if lines.is_empty() {
        lines.push(chosen.reason.clone());
    }
    lines
}

/// The jittered delay before an action, by type.
fn delay_for(config: &EngageConfig, action: &Action, rng: &mut Rng) -> u64 {
    let pacing = &config.pacing;
    match action {
        Action::Follow(_) => jitter_secs(
            rng,
            pacing.follow_delay_min_secs,
            pacing.follow_delay_max_secs,
        ),
        _ => jitter_secs(rng, pacing.delay_min_secs, pacing.delay_max_secs),
    }
}

/// Read candidates from an offline fixture (no network).
async fn gather_offline(
    path: &Path,
    config: &EngageConfig,
    state: &ActionLog,
    now_unix: u64,
    our_did: &str,
) -> Result<(Vec<Candidate>, Vec<FollowTarget>)> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading engagement fixture {}", path.display()))?;
    let fixture: Fixture = serde_json::from_str(&raw).context("parsing engagement fixture")?;

    let mentions = mention_candidates(&fixture.notifications, our_did);
    let threads: Vec<Thread> = fixture
        .threads
        .iter()
        .map(parse_thread_value)
        .collect::<Result<Vec<_>>>()?;
    let replies = own_post_reply_candidates(&threads, our_did);
    // Follows are driven by people who talked to us, never by cold proactive
    // replies — a stranger's post is not an interaction with us.
    let interactions = combine(mentions, replies);
    let follows = follow_candidates(&interactions, &fixture.profiles, our_did);

    let mut proactive = Vec::new();
    if config.proactive.enabled {
        for feed in &fixture.feeds {
            let posts = parse_feed_posts_value(&feed.response)?;
            proactive.extend(proactive_candidates(
                &InterestFeed {
                    did: &feed.profile.did,
                    handle: &feed.profile.handle,
                    posts: &posts,
                },
                now_unix,
                config.proactive.max_age_secs(),
                state,
                config.proactive.candidates_per_account_per_run,
            ));
        }
    }
    let candidates = combine(interactions, proactive);
    Ok((candidates, follows))
}

/// Read candidates from the live AppView.
async fn gather_live(
    config: &EngageConfig,
    session: &Session,
    state: &ActionLog,
    now_unix: u64,
    notes: &mut Vec<String>,
) -> Result<(Vec<Candidate>, Vec<FollowTarget>)> {
    let notifications = list_notifications(
        DEFAULT_BLUESKY_BASE_URL,
        session,
        &config.notification_reasons,
        config.notification_limit,
        DEFAULT_ENGAGE_TIMEOUT,
    )
    .await?;
    let mentions = mention_candidates(&notifications, &session.did);

    let mut threads = Vec::new();
    match get_author_feed(
        DEFAULT_BLUESKY_READ_URL,
        &session.did,
        config.own_posts_to_scan,
        DEFAULT_ENGAGE_TIMEOUT,
    )
    .await
    {
        Ok(posts) => {
            for post in &posts {
                match get_post_thread(
                    DEFAULT_BLUESKY_READ_URL,
                    &post.uri,
                    config.thread_depth,
                    DEFAULT_ENGAGE_TIMEOUT,
                )
                .await
                {
                    Ok(thread) => threads.push(thread),
                    Err(err) => notes.push(format!(
                        "could not read the thread of {}: {err:#}",
                        post.uri
                    )),
                }
            }
        }
        Err(err) => notes.push(format!("could not read our own feed: {err:#}")),
    }
    let replies = own_post_reply_candidates(&threads, &session.did);
    let interactions = combine(mentions, replies);

    // Resolve the curated accounts once: the same profiles drive slow follows
    // and the proactive feed reads.
    let follow_probe = (config.caps.follows_per_day.max(1) as usize) * 2;
    let probe = if config.proactive.enabled {
        follow_probe.max(config.proactive.accounts_per_run)
    } else {
        follow_probe
    };
    let mut interests = Vec::new();
    for handle in config.interests.accounts.iter().take(probe) {
        match get_profile(DEFAULT_BLUESKY_READ_URL, handle, DEFAULT_ENGAGE_TIMEOUT).await {
            Ok(profile) => interests.push(profile),
            Err(err) => notes.push(format!("could not resolve interest `{handle}`: {err:#}")),
        }
    }
    let follows = follow_candidates(&interactions, &interests, &session.did);

    let mut proactive = Vec::new();
    if config.proactive.enabled {
        let max_age = config.proactive.max_age_secs();
        for profile in &interests {
            match get_interest_feed(
                DEFAULT_BLUESKY_READ_URL,
                &profile.handle,
                config.proactive.posts_per_account,
                DEFAULT_ENGAGE_TIMEOUT,
            )
            .await
            {
                Ok(posts) => proactive.extend(proactive_candidates(
                    &InterestFeed {
                        did: &profile.did,
                        handle: &profile.handle,
                        posts: &posts,
                    },
                    now_unix,
                    max_age,
                    state,
                    config.proactive.candidates_per_account_per_run,
                )),
                Err(err) => notes.push(format!(
                    "could not read the feed of `{}`: {err:#}",
                    profile.handle
                )),
            }
        }
    }
    let candidates = combine(interactions, proactive);
    Ok((candidates, follows))
}

/// The effectful execution of a plan: generate, screen, write, remember.
struct Execution<'a> {
    persona: &'a PersonaPack,
    session: &'a Session,
    registry: &'a ProvidersConfig,
    config: &'a EngageConfig,
    state_path: &'a Path,
    /// Our handles/DID/name: a proactive reply must never name any of them.
    our_tokens: &'a [String],
    rng: &'a mut Rng,
}

impl Execution<'_> {
    /// Perform every planned action, jittered and state-recorded.
    async fn perform_all(&mut self, chosen: &Plan) -> Vec<ExecutedAction> {
        let mut executed = Vec::new();
        for (index, action) in chosen.actions.iter().enumerate() {
            if index > 0 {
                let delay = delay_for(self.config, action, self.rng);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            match self.perform(action).await {
                Ok(outcome) => executed.push(outcome),
                Err(err) => executed.push(ExecutedAction::error(
                    action.kind().as_str(),
                    action.target(),
                    format!("{err:#}"),
                )),
            }
        }
        executed
    }

    async fn perform(&mut self, action: &Action) -> Result<ExecutedAction> {
        match action {
            Action::Reply(candidate) => self.reply(candidate).await,
            Action::Like(subject) => self.like(subject).await,
            Action::Follow(target) => self.follow(target).await,
        }
    }

    /// Generate an in-context reply, screen it, write it and remember it.
    async fn reply(&mut self, candidate: &Candidate) -> Result<ExecutedAction> {
        let prompt = build_reply_prompt(self.persona, candidate);
        let completion =
            match complete_with_chain(self.registry, &prompt, JobKind::Create, COMPLETION_TIMEOUT)
                .await
            {
                Ok(completion) => completion,
                Err(err) => {
                    return Ok(ExecutedAction::error(
                        "reply",
                        &candidate.parent.uri,
                        format!("provider chain failed: {err:#}"),
                    ));
                }
            };

        let text = match parse_reply(&completion.text) {
            Ok(text) => text,
            Err(err) => {
                return Ok(ExecutedAction::error(
                    "reply",
                    &candidate.parent.uri,
                    format!("unusable reply: {err:#}"),
                ));
            }
        };

        // A reply is content: the deterministic guardrail is mandatory.
        let mut verdict = screen_reply(self.persona, &text);
        // A proactive reply carries the extra quality bar: it must contribute,
        // never flatter, link or name our own account.
        if candidate.kind == CandidateKind::Proactive {
            let quality = screen_proactive_reply(
                &text,
                self.our_tokens,
                self.config.proactive.min_chars,
                self.config.proactive.min_words,
            );
            if !quality.approved {
                verdict.approved = false;
                verdict.issues.extend(quality.issues);
            }
        }
        if !verdict.approved {
            let issues = verdict
                .issues
                .iter()
                .map(|issue| format!("{}: {}", issue.rule, issue.detail))
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(ExecutedAction::rejected(
                "reply",
                &candidate.parent.uri,
                format!("guardrail refused the reply — {issues}"),
            ));
        }

        // The generic layer: before speaking to a stranger, a sceptical reviewer
        // decides whether the reply is grounded in the post. Fail-closed — if the
        // review cannot run, the reply is withheld rather than risked.
        if candidate.kind == CandidateKind::Proactive {
            match self.review_grounding(candidate, &text).await {
                Ok(outcome) if outcome.approved => {}
                Ok(outcome) => {
                    return Ok(ExecutedAction::rejected(
                        "reply",
                        &candidate.parent.uri,
                        format!("grounding review refused the reply — {}", outcome.reason),
                    ));
                }
                Err(err) => {
                    return Ok(ExecutedAction::rejected(
                        "reply",
                        &candidate.parent.uri,
                        format!("grounding review unavailable — reply withheld: {err:#}"),
                    ));
                }
            }
        }

        let uri = create_reply(
            DEFAULT_BLUESKY_BASE_URL,
            self.session,
            &text,
            &candidate.root,
            &candidate.parent,
            DEFAULT_ENGAGE_TIMEOUT,
        )
        .await?;
        self.record(
            ActionKind::Reply,
            &candidate.parent.uri,
            &candidate.author_did,
            candidate.kind.as_str(),
            &format!("{uri} :: {text}"),
        )
        .await?;
        Ok(ExecutedAction::ok(
            "reply",
            &candidate.parent.uri,
            format!(
                "replied via {} ({})",
                completion.provider_id, completion.model
            ),
        ))
    }

    /// Ask the grounding reviewer whether a proactive reply is safe to send.
    ///
    /// A different role on the same chain: it judges the claim, not the wording,
    /// so it catches an invention no word list could enumerate.
    async fn review_grounding(&self, candidate: &Candidate, reply: &str) -> Result<ReviewOutcome> {
        let prompt = build_review_prompt(&candidate.text, reply);
        let completion =
            complete_with_chain(self.registry, &prompt, JobKind::Other, COMPLETION_TIMEOUT).await?;
        parse_review(&completion.text)
    }

    async fn like(&mut self, subject: &StrongRef) -> Result<ExecutedAction> {
        let uri = create_like(
            DEFAULT_BLUESKY_BASE_URL,
            self.session,
            subject,
            DEFAULT_ENGAGE_TIMEOUT,
        )
        .await?;
        self.record(ActionKind::Like, &subject.uri, "", "contextual-like", &uri)
            .await?;
        Ok(ExecutedAction::ok(
            "like",
            &subject.uri,
            "liked the post we replied to",
        ))
    }

    async fn follow(&mut self, target: &FollowTarget) -> Result<ExecutedAction> {
        let uri = create_follow(
            DEFAULT_BLUESKY_BASE_URL,
            self.session,
            &target.did,
            DEFAULT_ENGAGE_TIMEOUT,
        )
        .await?;
        self.record(
            ActionKind::Follow,
            &target.did,
            &target.did,
            "interest",
            &format!("{} :: {uri}", target.handle),
        )
        .await?;
        Ok(ExecutedAction::ok(
            "follow",
            &target.did,
            format!("followed {} ({})", target.handle, target.reason),
        ))
    }

    /// Append an action to the JSONL state so it is never repeated.
    async fn record(
        &self,
        kind: ActionKind,
        target: &str,
        author_did: &str,
        source: &str,
        detail: &str,
    ) -> Result<()> {
        let entry = StateEntry {
            at_unix: now_unix(),
            action: kind,
            target: target.to_string(),
            persona: self.persona.persona.clone(),
            detail: detail.to_string(),
            author_did: author_did.to_string(),
            source: source.to_string(),
        };
        append_entry(self.state_path, &entry).await
    }
}

/// The offline fixture shape accepted by `--fixture`.
#[derive(Debug, Default, Deserialize)]
struct Fixture {
    #[serde(default)]
    notifications: Vec<Notification>,
    #[serde(default)]
    threads: Vec<serde_json::Value>,
    #[serde(default)]
    profiles: Vec<Profile>,
    /// Curated accounts with a canned `getAuthorFeed` response each.
    #[serde(default)]
    feeds: Vec<FixtureFeed>,
}

/// One curated account's recent feed in an offline fixture.
#[derive(Debug, Deserialize)]
struct FixtureFeed {
    profile: Profile,
    /// A full `app.bsky.feed.getAuthorFeed` response (`{"feed": [...]}`).
    #[serde(default)]
    response: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "autopilot-engage-fixture-{}-{stamp}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn offline_fixture_becomes_candidates_and_follows() {
        let dir = fixture_dir();
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let path = dir.join("fixture.json");
        tokio::fs::write(
            &path,
            r#"{
              "notifications": [{
                "uri": "at://did:plc:them/app.bsky.feed.post/1",
                "cid": "c1",
                "reason": "mention",
                "author": {"did": "did:plc:them", "handle": "them.bsky.social"},
                "record": {"text": "oi"}
              }],
              "threads": [],
              "profiles": [{"did": "did:plc:gnome", "handle": "gnome.org"}]
            }"#,
        )
        .await
        .expect("write fixture");

        let config = EngageConfig::parse_yaml("{}").expect("defaults");
        let (candidates, follows) =
            gather_offline(&path, &config, &ActionLog::default(), 1_791_795_600, "")
                .await
                .expect("fixture reads");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].author_handle, "them.bsky.social");
        assert_eq!(follows.len(), 2);
        assert_eq!(follows[0].reason, "interacted with us");
        assert_eq!(follows[1].handle, "gnome.org");

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }

    #[tokio::test]
    async fn offline_feed_yields_a_proactive_candidate() {
        let dir = fixture_dir();
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let path = dir.join("fixture.json");
        tokio::fs::write(
            &path,
            r#"{
              "feeds": [{
                "profile": {"did": "did:plc:famous", "handle": "famous.bsky.social"},
                "response": {"feed": [{
                  "post": {
                    "uri": "at://did:plc:famous/app.bsky.feed.post/1",
                    "cid": "c1",
                    "author": {"did": "did:plc:famous", "handle": "famous.bsky.social"},
                    "record": {"text": "the new scheduler is here", "createdAt": "2026-10-12T08:00:00.000Z"},
                    "indexedAt": "2026-10-12T08:00:01.000Z"
                  }
                }]}
              }]
            }"#,
        )
        .await
        .expect("write fixture");

        let config = EngageConfig::parse_yaml("{}").expect("defaults");
        let now = 1_791_795_600; // 2026-10-12T09:00:00Z
        let (candidates, follows) =
            gather_offline(&path, &config, &ActionLog::default(), now, "did:plc:our")
                .await
                .expect("fixture reads");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, CandidateKind::Proactive);
        assert_eq!(candidates[0].author_handle, "famous.bsky.social");
        // A cold proactive reply must NOT turn the stranger into a follow target.
        assert!(follows.is_empty());

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }

    #[tokio::test]
    async fn a_dry_run_prints_the_proactive_plan_without_the_network() {
        let dir = fixture_dir();
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let path = dir.join("fixture.json");
        tokio::fs::write(
            &path,
            r#"{
              "feeds": [{
                "profile": {"did": "did:plc:famous", "handle": "famous.bsky.social"},
                "response": {"feed": [
                  {"post": {"uri": "at://did:plc:famous/app.bsky.feed.post/1", "cid": "c1",
                            "author": {"did": "did:plc:famous", "handle": "famous.bsky.social"},
                            "record": {"text": "the new scheduler is here", "createdAt": "2026-10-12T08:30:00.000Z"},
                            "indexedAt": "2026-10-12T08:30:01.000Z"}},
                  {"post": {"uri": "at://did:plc:famous/app.bsky.feed.post/2", "cid": "c2",
                            "author": {"did": "did:plc:famous", "handle": "famous.bsky.social"},
                            "record": {"text": "another fresh claim", "createdAt": "2026-10-12T08:00:00.000Z"},
                            "indexedAt": "2026-10-12T08:00:01.000Z"}}
                ]}
              }]
            }"#,
        )
        .await
        .expect("write fixture");

        // A window that always matches, no sporadic skip: the plan is visible.
        let config_path = dir.join("engage.yaml");
        tokio::fs::write(
            &config_path,
            "windows: [\"00:00\"]\nwindow_minutes: 1440\npacing: {run_skip_probability: 0.0}\n",
        )
        .await
        .expect("write config");

        let options = EngageOptions {
            persona_path: dir.join("persona.yaml"),
            config_path,
            state_path: dir.join("state.jsonl"),
            providers_path: dir.join("providers.yaml"),
            fixture: Some(path),
            dry_run: true,
        };
        tokio::fs::write(
            &options.persona_path,
            r#"
persona: tech-curiosities
display_name: Tech Curiosities
niche: computing-technology-ai-linux-foss
language: en
voice: {tone: curious, audience: developers}
rubric: {min_score: 0.75}
channels:
  - platform: bluesky
    handle: example.bsky.social
"#,
        )
        .await
        .expect("write persona");

        let report = run(options).await.expect("dry run");
        assert!(report.dry_run);
        assert!(!report.failed);
        // The newest post per account becomes a planned proactive reply.
        assert!(
            report
                .planned
                .iter()
                .any(|line| line.contains("proactive") && line.contains("famous.bsky.social")),
            "planned: {:?}",
            report.planned
        );
        println!("--- dry run plan ---");
        for note in &report.notes {
            println!("note    : {note}");
        }
        for line in &report.planned {
            println!("planned : {line}");
        }

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }

    #[test]
    fn describe_plan_shows_the_delay_and_the_target() {
        let config = EngageConfig::parse_yaml("{}").expect("defaults");
        let candidate = Candidate {
            kind: crate::candidates::CandidateKind::Mention,
            parent: StrongRef::new("at://1", "c1"),
            root: StrongRef::new("at://1", "c1"),
            author_did: "did:plc:them".into(),
            author_handle: "them.bsky.social".into(),
            text: "oi".into(),
            score: 30,
        };
        let chosen = Plan {
            actions: vec![
                Action::Reply(candidate),
                Action::Like(StrongRef::new("at://1", "c1")),
            ],
            skipped_run: false,
            reason: String::new(),
        };
        let lines = describe_plan(&chosen, &config, &mut Rng::from_seed(1));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("reply:"));
        assert!(lines[1].contains("after ~"));
        assert!(lines[1].contains("like"));
    }
}
