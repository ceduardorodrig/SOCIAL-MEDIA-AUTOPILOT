//! Pipeline stages [2] triage and [3] creation.
//!
//! Both stages are split into a pure part (prompt building + response parsing)
//! and an effectful part (the LLM call, which lives in `autopilot-providers`).
//! That keeps the fragile contract — the JSON shape we ask the model for —
//! fully unit-tested without ever calling an LLM in tests.

pub mod create;
pub mod json;
pub mod triage;

pub use create::{Variant, build_creation_prompt, parse_variants};
pub use json::extract_json;
pub use triage::{TopicScore, build_triage_prompt, parse_shortlist};
