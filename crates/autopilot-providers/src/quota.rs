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
///
/// `project` scopes the statistics to one project (`.` resolves against the
/// current working directory). Scoping to the repository directory isolates the
/// pipeline's own consumption from the operator's interactive sessions.
pub async fn opencode_usage(
    days: u32,
    project: Option<&str>,
    timeout: Duration,
) -> Result<Vec<ModelUsage>> {
    let args = stats_args(days, project);
    let output = tokio::time::timeout(
        timeout,
        Command::new("opencode")
            .args(&args)
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

/// Build the `opencode stats` argument list.
fn stats_args(days: u32, project: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["stats", "--json", "--models", "--days"]
        .iter()
        .map(|value| (*value).to_string())
        .collect();
    args.push(days.to_string());
    if let Some(project) = project {
        args.push("--project".to_string());
        args.push(project.to_string());
    }
    args
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
    autopilot_core::store::append_line(path, entry, "ledger").await
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

/// Consumption since a previously recorded baseline (same window).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageDelta {
    pub model_id: String,
    pub steps: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
}

impl UsageDelta {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.reasoning_tokens + self.cache_read_tokens
    }
}

/// Consumption between a baseline snapshot and the current usage.
///
/// Counters are cumulative inside the stats window, so a baseline is only
/// meaningful within the same window (`window_days`). When the window rolls over
/// the counters reset and the delta saturates to zero rather than going negative.
pub fn delta(before: &LedgerEntry, after: &ModelUsage) -> UsageDelta {
    UsageDelta {
        model_id: after.id(),
        steps: after.steps.saturating_sub(before.steps),
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        reasoning_tokens: after
            .reasoning_tokens
            .saturating_sub(before.reasoning_tokens),
        cache_read_tokens: after
            .cache_read_tokens
            .saturating_sub(before.cache_read_tokens),
        cost_usd: (after.cost_usd - before.cost_usd).max(0.0),
    }
}

/// Read a baseline snapshot, returning `None` when the file does not exist.
pub async fn read_baseline(path: &Path) -> Result<Option<LedgerEntry>> {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => {
            let entry: LedgerEntry = serde_json::from_str(&raw)
                .with_context(|| format!("invalid baseline JSON in {}", path.display()))?;
            Ok(Some(entry))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading baseline {}", path.display())),
    }
}

/// Persist the current usage as the new baseline.
pub async fn write_baseline(path: &Path, entry: &LedgerEntry) -> Result<()> {
    let body = serde_json::to_string_pretty(entry).context("serializing baseline")?;
    autopilot_core::store::write_text(path, &body, "baseline").await
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

    #[test]
    fn stats_args_include_project_only_when_scoped() {
        let expected_global: Vec<String> = ["stats", "--json", "--models", "--days", "0"]
            .iter()
            .map(|value| (*value).to_string())
            .collect();
        assert_eq!(stats_args(0, None), expected_global);

        let expected_scoped: Vec<String> = [
            "stats",
            "--json",
            "--models",
            "--days",
            "7",
            "--project",
            ".",
        ]
        .iter()
        .map(|value| (*value).to_string())
        .collect();
        assert_eq!(stats_args(7, Some(".")), expected_scoped);
    }

    #[test]
    fn delta_measures_consumption_since_baseline() {
        let usages = parse_opencode_usage(FIXTURE).expect("fixture parses");
        let mimo = usages.first().expect("first entry");

        let mut baseline = ledger_entry(mimo, 0);
        baseline.input_tokens = mimo.input_tokens - 1_000;
        baseline.output_tokens = mimo.output_tokens - 500;
        baseline.reasoning_tokens = mimo.reasoning_tokens;
        baseline.cache_read_tokens = mimo.cache_read_tokens;
        baseline.steps = mimo.steps - 2;

        let delta = delta(&baseline, mimo);
        assert_eq!(delta.steps, 2);
        assert_eq!(delta.input_tokens, 1_000);
        assert_eq!(delta.output_tokens, 500);
        assert_eq!(delta.reasoning_tokens, 0);
        assert_eq!(delta.total_tokens(), 1_500);
    }

    #[test]
    fn delta_saturates_when_window_rolls_over() {
        let usages = parse_opencode_usage(FIXTURE).expect("fixture parses");
        let mimo = usages.first().expect("first entry");

        let mut baseline = ledger_entry(mimo, 0);
        baseline.input_tokens = mimo.input_tokens + 10_000;

        let delta = delta(&baseline, mimo);
        assert_eq!(delta.input_tokens, 0);
        assert_eq!(delta.steps, 0);
    }
}
