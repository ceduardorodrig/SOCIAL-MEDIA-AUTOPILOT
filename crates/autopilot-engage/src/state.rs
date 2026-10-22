//! Engagement state: an append-only JSONL log under `var/` (gitignored).
//!
//! Every action that actually happened is recorded, so the stage never answers,
//! likes or follows the same target twice. The log is also the source of truth
//! for the per-day and per-week caps, which keeps the pacing honest across runs.

use std::path::Path;

use anyhow::{Context, Result};
use autopilot_core::store::append_line;
use serde::{Deserialize, Serialize};

/// The three action types the stage may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Reply,
    Like,
    Follow,
}

impl ActionKind {
    /// Stable lower-case label used in the state file and the console.
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Reply => "reply",
            ActionKind::Like => "like",
            ActionKind::Follow => "follow",
        }
    }
}

/// One recorded action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateEntry {
    pub at_unix: u64,
    pub action: ActionKind,
    /// AT-URI (reply/like) or DID (follow) that was acted on.
    pub target: String,
    pub persona: String,
    #[serde(default)]
    pub detail: String,
    /// Author DID of the post this action touched (empty for follows/likes).
    /// It is what makes the per-account caps possible.
    #[serde(default)]
    pub author_did: String,
    /// Where the action came from: `mention`, `reply-to-us`, `comment`,
    /// `proactive`, `contextual-like` or `interest`. Used to cap proactive
    /// replies separately from the answers to people who talked to us.
    #[serde(default)]
    pub source: String,
}

/// The in-memory view of the log.
#[derive(Debug, Clone, Default)]
pub struct ActionLog {
    entries: Vec<StateEntry>,
}

impl ActionLog {
    /// Build a log from already-loaded entries (tests and fixtures).
    pub fn from_entries(entries: Vec<StateEntry>) -> Self {
        Self { entries }
    }

    /// Load the log; a missing file is an empty log.
    pub async fn load(path: &Path) -> Result<Self> {
        let raw = match tokio::fs::read_to_string(path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading engagement state {}", path.display()));
            }
        };

        let mut entries = Vec::new();
        for (index, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: StateEntry = serde_json::from_str(line).with_context(|| {
                format!(
                    "invalid engagement state at {}:{}",
                    path.display(),
                    index + 1
                )
            })?;
            entries.push(entry);
        }
        Ok(Self { entries })
    }

    /// Every recorded entry, in file order.
    pub fn entries(&self) -> &[StateEntry] {
        &self.entries
    }

    /// Whether an action of `kind` was already taken on `target`.
    pub fn has(&self, kind: ActionKind, target: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.action == kind && entry.target == target)
    }

    /// Number of `kind` actions recorded at or after `since_unix`.
    pub fn count_since(&self, kind: ActionKind, since_unix: u64) -> u32 {
        self.entries
            .iter()
            .filter(|entry| entry.action == kind && entry.at_unix >= since_unix)
            .count() as u32
    }

    /// Number of `kind` actions recorded today (UTC).
    pub fn count_today(&self, kind: ActionKind, now_unix: u64) -> u32 {
        self.count_since(kind, day_start_unix(now_unix))
    }

    /// Number of `kind` actions recorded this week (UTC, fixed 7-day bucket).
    pub fn count_this_week(&self, kind: ActionKind, now_unix: u64) -> u32 {
        self.count_since(kind, week_start_unix(now_unix))
    }

    /// Number of replies recorded to `author_did` at or after `since_unix`.
    pub fn count_replies_to_author_since(&self, author_did: &str, since_unix: u64) -> u32 {
        self.entries
            .iter()
            .filter(|entry| {
                entry.action == ActionKind::Reply
                    && entry.author_did == author_did
                    && entry.at_unix >= since_unix
            })
            .count() as u32
    }

    /// When we last replied to `author_did`, if ever.
    pub fn last_reply_to_author(&self, author_did: &str) -> Option<u64> {
        self.entries
            .iter()
            .filter(|entry| entry.action == ActionKind::Reply && entry.author_did == author_did)
            .map(|entry| entry.at_unix)
            .max()
    }

    /// Number of actions of any kind recorded today whose `source` matches.
    pub fn count_source_today(&self, source: &str, now_unix: u64) -> u32 {
        let since = day_start_unix(now_unix);
        self.entries
            .iter()
            .filter(|entry| entry.source == source && entry.at_unix >= since)
            .count() as u32
    }
}

/// Start of the UTC day containing `now_unix`.
pub fn day_start_unix(now_unix: u64) -> u64 {
    (now_unix / 86_400) * 86_400
}

/// Start of the UTC week (a fixed 7-day bucket) containing `now_unix`.
pub fn week_start_unix(now_unix: u64) -> u64 {
    const WEEK: u64 = 86_400 * 7;
    (now_unix / WEEK) * WEEK
}

/// Append one action to the JSONL log, creating `var/` when needed.
pub async fn append_entry(path: &Path, entry: &StateEntry) -> Result<()> {
    append_line(path, entry, "engagement state").await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at_unix: u64, action: ActionKind, target: &str) -> StateEntry {
        StateEntry {
            at_unix,
            action,
            target: target.to_string(),
            persona: "tech-curiosities".to_string(),
            detail: String::new(),
            author_did: String::new(),
            source: String::new(),
        }
    }

    fn reply_to(at_unix: u64, author_did: &str, source: &str) -> StateEntry {
        StateEntry {
            at_unix,
            action: ActionKind::Reply,
            target: format!("at://{author_did}/post"),
            persona: "tech-curiosities".to_string(),
            detail: String::new(),
            author_did: author_did.to_string(),
            source: source.to_string(),
        }
    }

    /// A unique temporary directory per test process and run.
    ///
    /// The nanosecond stamp alone is not enough: two tests can start in the
    /// same clock tick on a fast runner, get the same directory and delete each
    /// other's `state.jsonl` (the CI flake fixed on 2026-10-21). The atomic
    /// sequence makes the directory unique even when the stamp collides.
    fn temp_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "autopilot-engage-{}-{stamp}-{seq}",
            std::process::id()
        ))
    }

    #[test]
    fn dedup_and_counts_respect_kind_and_target() {
        let now = 1_700_000_000;
        let log = ActionLog::from_entries(vec![
            entry(now, ActionKind::Reply, "at://post/1"),
            entry(now, ActionKind::Like, "at://post/1"),
            entry(now - 200_000, ActionKind::Reply, "at://post/2"),
        ]);

        assert!(log.has(ActionKind::Reply, "at://post/1"));
        assert!(log.has(ActionKind::Like, "at://post/1"));
        assert!(!log.has(ActionKind::Follow, "at://post/1"));
        assert!(!log.has(ActionKind::Reply, "at://post/9"));

        // The old reply falls outside today's window.
        assert_eq!(log.count_today(ActionKind::Reply, now), 1);
        assert_eq!(log.count_today(ActionKind::Like, now), 1);
        assert_eq!(log.count_today(ActionKind::Reply, now + 86_400), 0);
    }

    #[test]
    fn per_author_caps_and_sources_are_tracked() {
        let now = 1_700_000_000;
        let log = ActionLog::from_entries(vec![
            reply_to(now - 3_600, "did:plc:famous", "proactive"),
            reply_to(now - 86_400 * 2, "did:plc:famous", "proactive"),
            reply_to(now - 86_400 * 9, "did:plc:famous", "proactive"),
            reply_to(now - 100, "did:plc:friend", "mention"),
        ]);

        // The two recent replies fall inside the 7-day bucket; the old one does not.
        assert_eq!(
            log.count_replies_to_author_since("did:plc:famous", week_start_unix(now)),
            2
        );
        assert_eq!(
            log.count_replies_to_author_since("did:plc:friend", week_start_unix(now)),
            1
        );
        assert_eq!(
            log.last_reply_to_author("did:plc:famous"),
            Some(now - 3_600)
        );
        assert_eq!(log.last_reply_to_author("did:plc:nobody"), None);

        // Only the proactive reply happened today.
        assert_eq!(log.count_source_today("proactive", now), 1);
        assert_eq!(log.count_source_today("mention", now), 1);
    }

    #[test]
    fn week_bucket_covers_seven_days() {
        let now = 1_700_000_000;
        let start = week_start_unix(now);
        assert!(now - start < 86_400 * 7);
        assert_eq!(day_start_unix(start), start);
    }

    #[tokio::test]
    async fn store_round_trips_and_missing_file_is_empty() {
        let dir = temp_dir();
        let path = dir.join("state.jsonl");

        let empty = ActionLog::load(&path).await.expect("missing is empty");
        assert!(empty.entries().is_empty());

        let first = entry(1, ActionKind::Follow, "did:plc:abc");
        append_entry(&path, &first).await.expect("append");
        let loaded = ActionLog::load(&path).await.expect("load");
        assert_eq!(loaded.entries(), std::slice::from_ref(&first));

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }

    #[tokio::test]
    async fn malformed_line_reports_its_position() {
        let dir = temp_dir();
        let path = dir.join("state.jsonl");
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        tokio::fs::write(&path, "{\"nope\":true}\n")
            .await
            .expect("write");

        let err = ActionLog::load(&path).await.expect_err("bad record fails");
        assert!(err.to_string().contains(":1"), "unexpected error: {err}");

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }
}
