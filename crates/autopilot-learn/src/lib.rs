//! Stage [6] — learning: turn collected metrics into a persona memory proposal.
//!
//! Three deterministic pieces live here: the **performance score** (weighted
//! interactions normalised against the persona's own rolling median), the
//! **craft score** (persona rubric minus named slop penalties) and the memory
//! proposal that only promotes a winner whose craft score also holds up.
//!
//! Everything is pure and unit-tested. The stage never rewrites a persona pack on
//! its own: it emits a [`MemoryUpdate`] proposal that is only persisted when the
//! operator passes `--write`.

pub mod craft;
pub mod memory;
pub mod score;

#[cfg(test)]
pub(crate) mod test_support;

pub use craft::{
    AvoidEntry, CraftConfig, CraftPenalty, CraftScore, DEFAULT_CRAFT_FLOOR, DEFAULT_MAX_AVOID,
    avoid_entries, avoid_entries_with_rejections, craft_score, craft_score_guarded,
    guardrail_penalty,
};
pub use memory::{
    DEFAULT_MAX_FEW_SHOT, MemoryUpdate, ProposalContext, ScheduleHint, best_hour, propose_memory,
    propose_memory_with_config, propose_memory_with_context, render_memory, write_memory,
};
pub use score::{
    AbOutcome, DEFAULT_BASELINE_WINDOW, DEFAULT_MIN_SAMPLES, PerformanceScore, VariantStats,
    compare_variants, performance_score, rolling_baseline, summarise, weighted_interactions,
    weighted_score,
};
