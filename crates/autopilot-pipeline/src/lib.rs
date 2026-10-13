//! Pipeline stages [2] triage, [3] creation, [3.2] editorial review and the
//! claim-vs-source grounding review.
//!
//! Each stage is split into a pure part (prompt building + response parsing)
//! and an effectful part (the LLM call, which lives in `autopilot-providers`).
//! That keeps the fragile contract — the JSON shape we ask the model for —
//! fully unit-tested without ever calling an LLM in tests.

pub mod create;
pub mod editor;
pub mod grounding;
pub mod json;
pub mod prompt;
pub mod triage;

#[cfg(test)]
pub(crate) mod test_support;

pub use create::{Variant, build_creation_prompt, parse_variants};
pub use editor::{
    EditedVariant, EditorOutcome, build_editor_prompt, kept_variants, parse_editor_response,
    review_or_keep,
};
pub use grounding::{
    GroundingVerdict, Source, build_grounding_prompt, parse_grounding, supported_labels,
};
pub use json::extract_json;
pub use triage::{TopicScore, build_triage_prompt, parse_shortlist};
