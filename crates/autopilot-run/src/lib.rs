//! Orchestration for one full pipeline run (`autopilot run`).
//!
//! The stage order is **harvest → triage → creation → guardrail → publish**.
//! Metrics ([5]) and learning ([6]) are scheduled separately, so this crate
//! stops after publishing.
//!
//! The orchestrator only **chains** the stage crates' public APIs — it never
//! reimplements a stage (the repository enforces DRY). Every prompt, parser and
//! adapter comes from its own crate.
//!
//! GPU policy: local inference is reached through `autopilot-providers`, which
//! requests an `hl-infer` VRAM lease for each local job. A lease that is not
//! admitted is an error, never a green light, so a queued local job aborts the
//! stage instead of running unarbitrated — this crate relies on that contract
//! rather than re-implementing it.

pub mod journal;
pub mod runner;

pub use journal::{Journal, JournalEvent};
pub use runner::{
    CardAttachment, PublishedPost, RunOptions, RunReport, StageOutcome, StageStatus,
    humanize_niche, partition_variants, resolve_channels, run, select_topic,
};
