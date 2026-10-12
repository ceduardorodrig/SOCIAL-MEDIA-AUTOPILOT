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
    get_author_feed, get_post_thread, get_profile, list_notifications,
};
use crate::candidates::{
    Candidate, FollowTarget, Notification, Profile, Thread, combine, follow_candidates,
    mention_candidates, own_post_reply_candidates, parse_thread_value,
};
use crate::config::EngageConfig;
use crate::jitter::{Rng, jitter_secs};
use crate::planner::{Action, Plan, PlanInput, plan};
use crate::prompt::{build_reply_prompt, parse_reply, screen_reply};
use crate::records::StrongRef;
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

    let (candidates, follows) = gather_live(&config, &session, &mut report.notes).await?;

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
    let mut execution = Execution {
        persona: &persona,
        session: &session,
        registry: &registry,
        config: &config,
        state_path: &options.state_path,
        rng: &mut rng,
    };
    report.executed = execution.perform_all(&chosen).await;
    Ok(report)
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
        Some(path) => gather_offline(path, "").await?,
        None => {
            report.notes.push(format!(
                "would fetch up to {} notifications and scan {} of our own posts for comments",
                config.notification_limit, config.own_posts_to_scan
            ));
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
async fn gather_offline(path: &Path, our_did: &str) -> Result<(Vec<Candidate>, Vec<FollowTarget>)> {
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
    let candidates = combine(mentions, replies);
    let follows = follow_candidates(&candidates, &fixture.profiles, our_did);
    Ok((candidates, follows))
}

/// Read candidates from the live AppView.
async fn gather_live(
    config: &EngageConfig,
    session: &Session,
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
    let candidates = combine(mentions, replies);

    // Resolve only the accounts of interest we might actually follow.
    let probe = (config.caps.follows_per_day.max(1) as usize) * 2;
    let mut interests = Vec::new();
    for handle in config.interests.accounts.iter().take(probe) {
        match get_profile(DEFAULT_BLUESKY_READ_URL, handle, DEFAULT_ENGAGE_TIMEOUT).await {
            Ok(profile) => interests.push(profile),
            Err(err) => notes.push(format!("could not resolve interest `{handle}`: {err:#}")),
        }
    }
    let follows = follow_candidates(&candidates, &interests, &session.did);
    Ok((candidates, follows))
}

/// The effectful execution of a plan: generate, screen, write, remember.
struct Execution<'a> {
    persona: &'a PersonaPack,
    session: &'a Session,
    registry: &'a ProvidersConfig,
    config: &'a EngageConfig,
    state_path: &'a Path,
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
        let verdict = screen_reply(self.persona, &text);
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

    async fn like(&mut self, subject: &StrongRef) -> Result<ExecutedAction> {
        let uri = create_like(
            DEFAULT_BLUESKY_BASE_URL,
            self.session,
            subject,
            DEFAULT_ENGAGE_TIMEOUT,
        )
        .await?;
        self.record(ActionKind::Like, &subject.uri, &uri).await?;
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
    async fn record(&self, kind: ActionKind, target: &str, detail: &str) -> Result<()> {
        let entry = StateEntry {
            at_unix: now_unix(),
            action: kind,
            target: target.to_string(),
            persona: self.persona.persona.clone(),
            detail: detail.to_string(),
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

        let (candidates, follows) = gather_offline(&path, "").await.expect("fixture reads");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].author_handle, "them.bsky.social");
        assert_eq!(follows.len(), 2);
        assert_eq!(follows[0].reason, "interacted with us");
        assert_eq!(follows[1].handle, "gnome.org");

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
