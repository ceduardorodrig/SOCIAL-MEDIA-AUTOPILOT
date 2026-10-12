//! LLM provider layer: data-driven registry + availability probes.
//!
//! The fallback chain is (by default) OpenCode -> Gemini -> Ollama. Because the
//! OpenCode model catalogue changes often, the model id is **data** (YAML), not
//! code: swapping a model is a one-line config edit.

pub mod chain;
pub mod complete;
pub mod config;
pub mod gateway;
pub mod probe;
pub mod quota;

pub use chain::resolve_chain;
pub use complete::{Completion, complete, complete_with_chain, extract_opencode_text};
pub use config::{ProviderConfig, ProviderKind, ProvidersConfig, load_config};
pub use gateway::{
    DEFAULT_GATEWAY_URL, GATEWAY_URL_ENV, GatewayClient, Job, JobKind, Lease,
    estimate_required_bytes,
};
pub use probe::{ProbeOutcome, probe};
pub use quota::{
    BudgetCheck, LedgerEntry, ModelUsage, UsageDelta, append_ledger, check_budget, delta,
    ledger_entry, opencode_usage, read_baseline, usage_for, write_baseline,
};
