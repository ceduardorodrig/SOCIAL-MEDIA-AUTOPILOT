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
    /// The format the slot actually published (`text`, `card`, `video`). It is
    /// the publish step's own record, so the weekly per-format budget can be
    /// counted back from the journal (HB-034).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<&'a str>,
    /// The unsupported claim a rejection named (additive: only the `grounding`
    /// stage). Kept as a field, not buried in `detail`, so the learning stage
    /// can read the *claim* rather than parse a sentence (HB-046).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
}

/// One published post as recorded in a run journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedEntry {
    pub persona: String,
    pub platform: String,
    pub variant: String,
    pub uri: String,
    /// The format actually published (`text`, `card`, `video`), when recorded.
    pub format: Option<String>,
    /// When the journal recorded the publication (Unix seconds).
    pub published_at_unix: u64,
}

/// A grounding refusal recovered from a run journal.
///
/// The [3.7] reviewer refused a draft because the source did not support a
/// claim. The refusal is a *kind of claim* the engine must learn to avoid, not
/// just a gate that cost one slot (HB-046).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundingRejection {
    pub persona: String,
    pub platform: String,
    pub variant: String,
    /// The unsupported claim the reviewer named.
    pub reason: String,
    /// When the journal recorded the refusal (Unix seconds).
    pub at_unix: u64,
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
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Every parsable journal line under `runs_dir`.
///
/// The journal is the source of truth for *what happened in a run*, and the
/// engine owns that format — so the readers live here rather than in a caller.
/// A missing directory (no run yet) and a malformed line are both skipped: one
/// bad line must never hide the rest of the history.
async fn read_lines(runs_dir: &Path) -> Result<Vec<JournalLine>> {
    let mut lines = Vec::new();
    let Ok(mut dir) = tokio::fs::read_dir(runs_dir).await else {
        return Ok(lines);
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
            if let Ok(event) = serde_json::from_str::<JournalLine>(line) {
                lines.push(event);
            }
        }
    }

    Ok(lines)
}

/// Every published post recorded under `runs_dir`, newest first.
pub async fn read_published(runs_dir: &Path) -> Result<Vec<PublishedEntry>> {
    let mut entries = Vec::new();
    for event in read_lines(runs_dir).await? {
        if event.stage != "publish" || event.status != "published" {
            continue;
        }
        let (Some(uri), Some(platform), Some(variant)) = (event.uri, event.platform, event.variant)
        else {
            continue;
        };
        entries.push(PublishedEntry {
            persona: event.persona,
            platform,
            variant,
            uri,
            format: event.format,
            published_at_unix: event.at_unix,
        });
    }

    entries.sort_by_key(|entry| std::cmp::Reverse(entry.published_at_unix));
    Ok(entries)
}

/// Every grounding refusal recorded under `runs_dir`, newest first.
///
/// This is what turns the [3.7] reviewer's verdict into teaching signal: the
/// learning stage reads the *claim* that was refused and warns the creation
/// stage off it (HB-046). A rejection without a named claim is skipped — it
/// carries nothing to learn from.
pub async fn read_grounding_rejections(runs_dir: &Path) -> Result<Vec<GroundingRejection>> {
    let mut rejections = Vec::new();
    for event in read_lines(runs_dir).await? {
        if event.stage != "grounding" || event.status != "rejected" {
            continue;
        }
        let (Some(platform), Some(variant), Some(reason)) =
            (event.platform, event.variant, event.reason)
        else {
            continue;
        };
        if reason.trim().is_empty() {
            continue;
        }
        rejections.push(GroundingRejection {
            persona: event.persona,
            platform,
            variant,
            reason,
            at_unix: event.at_unix,
        });
    }

    rejections.sort_by_key(|rejection| std::cmp::Reverse(rejection.at_unix));
    Ok(rejections)
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
            format: outcome.format.as_deref(),
            reason: outcome.reason.as_deref(),
        };
        append_line(&self.path, &event, "journal").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_runs_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "autopilot-journal-{tag}-{}-{stamp}",
            std::process::id()
        ));
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        dir
    }

    #[tokio::test]
    async fn grounding_rejections_are_recovered_from_the_journal() {
        let dir = temp_runs_dir("grounding").await;
        let path = dir.join("tech-curiosities-1.jsonl");
        let claim = "Claims the agent is 'inspectable, not someone's cloud API' — the \
                     source never says whether KylinBot/Xiao K is local/inspectable or cloud-based.";
        let rejected = serde_json::json!({
            "at_unix": 10,
            "run_id": "tech-curiosities-1",
            "persona": "tech-curiosities",
            "stage": "grounding",
            "status": "rejected",
            "platform": "bluesky",
            "variant": "B",
            "reason": claim,
        });
        // A refusal without a named claim carries nothing to learn from, and a
        // publish line is not a refusal: both must be ignored.
        let no_reason = serde_json::json!({
            "at_unix": 9,
            "persona": "tech-curiosities",
            "stage": "grounding",
            "status": "rejected",
            "platform": "bluesky",
            "variant": "A",
        });
        let published = serde_json::json!({
            "at_unix": 8,
            "persona": "tech-curiosities",
            "stage": "publish",
            "status": "published",
            "platform": "bluesky",
            "variant": "A",
            "uri": "at://did:plc:abc/app.bsky.feed.post/1",
        });
        let body = format!("{rejected}\n{no_reason}\n{published}\nnot json\n");
        tokio::fs::write(&path, body).await.expect("write journal");

        let rejections = read_grounding_rejections(&dir).await.expect("reads");
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].persona, "tech-curiosities");
        assert_eq!(rejections[0].platform, "bluesky");
        assert_eq!(rejections[0].variant, "B");
        assert_eq!(rejections[0].reason, claim);
        assert_eq!(rejections[0].at_unix, 10);

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }

    #[tokio::test]
    async fn a_missing_runs_dir_yields_no_rejections() {
        let missing = std::env::temp_dir().join("autopilot-journal-does-not-exist");
        let rejections = read_grounding_rejections(&missing).await.expect("reads");
        assert!(rejections.is_empty());
    }

    #[tokio::test]
    async fn the_published_format_is_recovered_from_the_journal() {
        // HB-034: the weekly per-format budget is counted back from the journal,
        // so the publish line must carry the format it actually released.
        let dir = temp_runs_dir("format").await;
        let path = dir.join("demo-1.jsonl");
        let card = serde_json::json!({
            "at_unix": 100,
            "run_id": "demo-1",
            "persona": "demo",
            "stage": "publish",
            "status": "published",
            "platform": "bluesky",
            "variant": "A",
            "uri": "at://did:plc:abc/app.bsky.feed.post/1",
            "format": "card",
        });
        let text = serde_json::json!({
            "at_unix": 90,
            "run_id": "demo-1",
            "persona": "demo",
            "stage": "publish",
            "status": "published",
            "platform": "bluesky",
            "variant": "B",
            "uri": "at://did:plc:abc/app.bsky.feed.post/2",
        });
        tokio::fs::write(&path, format!("{card}\n{text}\n"))
            .await
            .expect("write journal");

        let published = read_published(&dir).await.expect("reads");
        assert_eq!(published.len(), 2);
        assert_eq!(published[0].variant, "A");
        assert_eq!(published[0].format.as_deref(), Some("card"));
        // An older line without a format reads back as unknown (None), not lost.
        assert_eq!(published[1].format, None);

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }
}
