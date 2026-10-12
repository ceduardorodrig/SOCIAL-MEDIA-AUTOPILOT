//! OpenCode usage / quota instrumentation.
//!
//! The OpenCode CLI does not expose token usage per call, but it does expose
//! per-model aggregates via `opencode stats --json --models`. We snapshot that
//! and attribute consumption to our model, so the free-tier budget can be
//! measured before the autonomous loop is enabled.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Aggregated usage for one model over a time window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub provider: String,
    pub steps: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
}

impl ModelUsage {
    /// `<provider>/<model>` identifier used in the provider registry.
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.reasoning_tokens + self.cache_read_tokens
    }
}

/// Budget assessment for a single model.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetCheck {
    pub model_id: String,
    pub used_tokens: u64,
    pub budget_tokens: Option<u64>,
    pub within_budget: bool,
}

/// A single line of the local quota ledger (append-only JSONL).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub recorded_at_unix: u64,
    pub model_id: String,
    pub window_days: u32,
    pub steps: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
}

/// Fetch per-model usage from `opencode stats`.
pub async fn opencode_usage(days: u32, timeout: Duration) -> Result<Vec<ModelUsage>> {
    let output = tokio::time::timeout(
        timeout,
        Command::new("opencode")
            .args(["stats", "--json", "--models", "--days", &days.to_string()])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("`opencode stats` timed out")?
    .context("failed to spawn `opencode stats`")?;

    if !output.status.success() {
        bail!("`opencode stats` exited with {}", output.status);
    }

    parse_opencode_usage(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the JSON emitted by `opencode stats --json --models`.
pub fn parse_opencode_usage(raw: &str) -> Result<Vec<ModelUsage>> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("invalid `opencode stats` JSON")?;
    let entries = value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut usages = Vec::new();
    for entry in entries {
        let model = entry
            .get("model")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let tokens = entry
            .get("tokens")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        usages.push(ModelUsage {
            model: string_field(&model, "id"),
            provider: string_field(&model, "providerID"),
            steps: entry
                .get("steps")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            input_tokens: u64_field(&tokens, "input"),
            output_tokens: u64_field(&tokens, "output"),
            reasoning_tokens: u64_field(&tokens, "reasoning"),
            cache_read_tokens: tokens
                .get("cache")
                .map(|cache| u64_field(cache, "read"))
                .unwrap_or(0),
            cost_usd: entry
                .get("cost")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
        });
    }
    Ok(usages)
}

/// Find the usage entry matching a registry model id (`provider/model` or bare model).
pub fn usage_for<'a>(usages: &'a [ModelUsage], model_id: &str) -> Option<&'a ModelUsage> {
    usages
        .iter()
        .find(|usage| model_id == usage.id() || model_id == usage.model)
}

/// Compare a usage window against an optional token budget.
pub fn check_budget(usage: &ModelUsage, budget_tokens: Option<u64>) -> BudgetCheck {
    let used_tokens = usage.total_tokens();
    BudgetCheck {
        model_id: usage.id(),
        used_tokens,
        budget_tokens,
        within_budget: budget_tokens.is_none_or(|budget| used_tokens <= budget),
    }
}

/// Append a snapshot to the append-only quota ledger.
pub async fn append_ledger(path: &Path, entry: &LedgerEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating ledger directory {}", parent.display()))?;
    }

    let mut line = serde_json::to_string(entry).context("serializing ledger entry")?;
    line.push('\n');

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("opening ledger {}", path.display()))?;
    file.write_all(line.as_bytes())
        .await
        .context("writing ledger entry")?;
    Ok(())
}

/// Build a ledger entry from a usage window.
pub fn ledger_entry(usage: &ModelUsage, window_days: u32) -> LedgerEntry {
    let recorded_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);

    LedgerEntry {
        recorded_at_unix,
        model_id: usage.id(),
        window_days,
        steps: usage.steps,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cost_usd: usage.cost_usd,
    }
}

fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn u64_field(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "models": [
        {"model": {"id": "mimo-v2.6-flash-free", "providerID": "opencode"},
         "steps": 40,
         "tokens": {"input": 144750, "output": 22335, "reasoning": 17392,
                    "cache": {"read": 1942400, "write": 0}},
         "cost": 0},
        {"model": {"id": "other-model", "providerID": "vendor"},
         "steps": 3,
         "tokens": {"input": 10, "output": 20, "reasoning": 0,
                    "cache": {"read": 0}},
         "cost": 1.5}
      ]
    }"#;

    #[test]
    fn parses_per_model_usage() {
        let usages = parse_opencode_usage(FIXTURE).expect("fixture parses");
        assert_eq!(usages.len(), 2);
        let mimo = usages.first().expect("first entry");
        assert_eq!(mimo.id(), "opencode/mimo-v2.6-flash-free");
        assert_eq!(mimo.input_tokens, 144_750);
        assert_eq!(mimo.cache_read_tokens, 1_942_400);
        assert!(mimo.cost_usd < f64::EPSILON);
    }

    #[test]
    fn finds_usage_by_registry_id() {
        let usages = parse_opencode_usage(FIXTURE).expect("fixture parses");
        let found = usage_for(&usages, "opencode/mimo-v2.6-flash-free").expect("found");
        assert_eq!(found.model, "mimo-v2.6-flash-free");
        assert!(usage_for(&usages, "nope/nope").is_none());
    }

    #[test]
    fn budget_flags_overconsumption() {
        let usages = parse_opencode_usage(FIXTURE).expect("fixture parses");
        let mimo = usages.first().expect("first entry");
        let total = mimo.total_tokens();
        assert!(check_budget(mimo, Some(total)).within_budget);
        assert!(check_budget(mimo, Some(total + 1)).within_budget);
        assert!(!check_budget(mimo, Some(total - 1)).within_budget);
        assert!(check_budget(mimo, None).within_budget);
    }

    #[test]
    fn ledger_entry_carries_totals() {
        let usages = parse_opencode_usage(FIXTURE).expect("fixture parses");
        let mimo = usages.first().expect("first entry");
        let entry = ledger_entry(mimo, 0);
        assert_eq!(entry.model_id, "opencode/mimo-v2.6-flash-free");
        assert_eq!(mimo.total_tokens(), 144_750 + 22_335 + 17_392 + 1_942_400);
    }
}
