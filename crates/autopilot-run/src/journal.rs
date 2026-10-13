//! Per-run JSONL journal.
//!
//! Every decision of a run is appended, in order, to one file under `var/`
//! (gitignored). The journal is what makes an autonomous run auditable after
//! the fact: which persona ran, which stage decided what, and what was
//! published. It reuses the shared JSONL store from `autopilot-core`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use autopilot_core::now_unix;
use autopilot_core::store::append_line;
use serde::{Deserialize, Serialize};

use crate::runner::StageOutcome;

/// One journal line: the run context plus a single stage decision.
#[derive(Debug, Serialize)]
pub struct JournalEvent<'a> {
    pub at_unix: u64,
    pub run_id: &'a str,
    pub persona: &'a str,
    pub stage: &'a str,
    pub status: &'a str,
    pub detail: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<&'a str>,
    /// What the editorial review changed (additive: only the `editor` stage
    /// carries it, so older readers see exactly the previous shape).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<&'a [String]>,
    /// The AT URI of a published post (additive: only the `publish` stage).
    /// The web URL in `detail` is for humans; the API needs `at://…`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<&'a str>,
}

/// One published post as recorded in a run journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedEntry {
    pub persona: String,
    pub platform: String,
    pub variant: String,
    pub uri: String,
    /// When the journal recorded the publication (Unix seconds).
    pub published_at_unix: u64,
}

/// The journal fields the published-post reader needs.
#[derive(Deserialize)]
struct JournalLine {
    at_unix: u64,
    persona: String,
    stage: String,
    status: String,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    uri: Option<String>,
}

/// Every published post recorded under `runs_dir`, newest first.
///
/// The journal is the source of truth for *what went out and when*, and the
/// engine owns that format — so the reader lives here rather than in a caller.
/// A missing directory (no run yet) and a malformed line are both skipped: one
/// bad line must never hide the rest of the history.
pub async fn read_published(runs_dir: &Path) -> Result<Vec<PublishedEntry>> {
    let mut entries = Vec::new();
    let Ok(mut dir) = tokio::fs::read_dir(runs_dir).await else {
        return Ok(entries);
    };

    while let Some(item) = dir.next_entry().await? {
        let path = item.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let raw = tokio::fs::read_to_string(&path).await?;
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<JournalLine>(line) else {
                continue;
            };
            if event.stage != "publish" || event.status != "published" {
                continue;
            }
            let (Some(uri), Some(platform), Some(variant)) =
                (event.uri, event.platform, event.variant)
            else {
                continue;
            };
            entries.push(PublishedEntry {
                persona: event.persona,
                platform,
                variant,
                uri,
                published_at_unix: event.at_unix,
            });
        }
    }

    entries.sort_by_key(|entry| std::cmp::Reverse(entry.published_at_unix));
    Ok(entries)
}

/// Append-only writer bound to one run.
#[derive(Debug, Clone)]
pub struct Journal {
    run_id: String,
    persona: String,
    path: PathBuf,
}

impl Journal {
    /// Bind a journal to a run id, a persona slug and a file path.
    pub fn new(
        run_id: impl Into<String>,
        persona: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            persona: persona.into(),
            path: path.into(),
        }
    }

    /// The journal file path (for the console and for auditing).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one stage decision to the journal.
    pub async fn record(&self, outcome: &StageOutcome) -> Result<()> {
        let event = JournalEvent {
            at_unix: now_unix(),
            run_id: &self.run_id,
            persona: &self.persona,
            stage: &outcome.stage,
            status: outcome.status.as_str(),
            detail: &outcome.detail,
            platform: outcome.platform.as_deref(),
            variant: outcome.variant.as_deref(),
            changes: outcome.changes.as_deref(),
            uri: outcome.uri.as_deref(),
        };
        append_line(&self.path, &event, "journal").await
    }
}
