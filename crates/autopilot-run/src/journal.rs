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
use serde::Serialize;

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
        };
        append_line(&self.path, &event, "journal").await
    }
}
