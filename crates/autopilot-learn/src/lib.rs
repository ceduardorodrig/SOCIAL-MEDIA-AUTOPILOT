//! Stage [6] — learning: turn collected metrics into a persona memory proposal.
//!
//! The scoring and comparison are pure and unit-tested. The stage never rewrites
//! a persona pack on its own: it emits a [`MemoryUpdate`] proposal that is only
//! persisted when the operator passes `--write`.

pub mod memory;
pub mod score;

#[cfg(test)]
pub(crate) mod test_support;

pub use memory::{
    DEFAULT_MAX_FEW_SHOT, MemoryUpdate, ScheduleHint, best_hour, propose_memory, render_memory,
    write_memory,
};
pub use score::{
    AbOutcome, DEFAULT_MIN_SAMPLES, VariantStats, compare_variants, summarise, weighted_score,
};
