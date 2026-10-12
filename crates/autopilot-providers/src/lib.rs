//! LLM provider layer: data-driven registry + availability probes.
//!
//! The fallback chain is (by default) OpenCode -> Gemini -> Ollama. Because the
//! OpenCode model catalogue changes often, the model id is **data** (YAML), not
//! code: swapping a model is a one-line config edit.

pub mod chain;
pub mod config;
pub mod probe;

pub use chain::resolve_chain;
pub use config::{ProviderConfig, ProviderKind, ProvidersConfig, load_config};
pub use probe::{ProbeOutcome, probe};
