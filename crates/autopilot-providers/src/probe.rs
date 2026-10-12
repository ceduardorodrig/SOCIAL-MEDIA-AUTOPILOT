//! Availability probes for each provider kind.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::config::{
    DEFAULT_GEMINI_ENDPOINT, DEFAULT_OLLAMA_ENDPOINT, ProviderConfig, ProviderKind,
};

/// Result of a single availability probe.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub id: String,
    pub kind: ProviderKind,
    pub model: String,
    pub priority: u32,
    pub available: bool,
    pub latency_ms: u128,
    pub detail: String,
}

/// Probe one provider, enforcing its configured timeout.
pub async fn probe(cfg: &ProviderConfig) -> ProbeOutcome {
    let started = Instant::now();
    let timeout = Duration::from_millis(cfg.timeout_ms);

    let (available, detail) = match tokio::time::timeout(timeout, probe_inner(cfg)).await {
        Ok(Ok(detail)) => (true, detail),
        Ok(Err(err)) => (false, format!("{err:#}")),
        Err(_) => (false, format!("timed out after {} ms", cfg.timeout_ms)),
    };

    ProbeOutcome {
        id: cfg.id.clone(),
        kind: cfg.kind,
        model: cfg.model.clone(),
        priority: cfg.priority,
        available,
        latency_ms: started.elapsed().as_millis(),
        detail,
    }
}

async fn probe_inner(cfg: &ProviderConfig) -> Result<String> {
    match cfg.kind {
        ProviderKind::Opencode => probe_opencode(cfg).await,
        ProviderKind::Gemini => probe_gemini(cfg).await,
        ProviderKind::Ollama => probe_ollama(cfg).await,
    }
}

/// Headless OpenCode call: validates that the CLI can reach the model.
async fn probe_opencode(cfg: &ProviderConfig) -> Result<String> {
    let output = Command::new("opencode")
        .args([
            "run",
            "--format",
            "json",
            "-m",
            &cfg.model,
            "Reply with exactly: ok",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to spawn `opencode` CLI")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(format!("cli ok ({})", truncate(stdout.trim(), 80)))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "opencode exited with {}: {}",
            output.status,
            truncate(stderr.trim(), 160)
        )
    }
}

/// Gemini Developer API: validates that the key can actually **generate**.
///
/// Listing models is not enough — a project with a depleted prepay balance
/// answers `402` to generation while the key still lists models, so a shallow
/// probe would advertise a fail-safe that cannot fail over.
async fn probe_gemini(cfg: &ProviderConfig) -> Result<String> {
    let key_env = cfg.api_key_env.as_deref().unwrap_or("GEMINI_API_KEY");
    let key = std::env::var(key_env)
        .with_context(|| format!("missing environment variable {key_env}"))?;
    let endpoint = cfg.endpoint_or_default(DEFAULT_GEMINI_ENDPOINT);
    let url = format!(
        "{endpoint}/v1beta/models/{}:generateContent?key={key}",
        cfg.model
    );
    let body = serde_json::json!({
        "contents": [{ "parts": [{ "text": "ok" }] }]
    });

    let response = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("gemini request failed")?;

    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .context("invalid gemini JSON response")?;

    if status.is_success() {
        Ok(format!("generation ok (HTTP {status})"))
    } else {
        let message = payload
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        bail!(
            "gemini cannot generate (HTTP {status}): {}",
            truncate(message, 140)
        )
    }
}

/// Ollama: server must answer and the configured model must be installed.
async fn probe_ollama(cfg: &ProviderConfig) -> Result<String> {
    let endpoint = cfg.endpoint_or_default(DEFAULT_OLLAMA_ENDPOINT);
    let url = format!("{endpoint}/api/tags");

    let response = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .context("ollama request failed")?;

    let status = response.status();
    if !status.is_success() {
        bail!("ollama returned HTTP {status}");
    }

    let body: serde_json::Value = response.json().await.context("invalid ollama JSON")?;
    let models: Vec<String> = body
        .get("models")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|m| m.get("name").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let wanted = &cfg.model;
    let present = models
        .iter()
        .any(|m| m == wanted || m.starts_with(&format!("{wanted}:")));

    if present {
        Ok(format!("server up, model '{wanted}' installed"))
    } else {
        bail!(
            "server up but model '{wanted}' is not installed (found {} models)",
            models.len()
        )
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let cut: String = value.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("ok", 10), "ok");
    }

    #[test]
    fn truncate_cuts_long_strings() {
        let result = truncate("abcdefghij", 5);
        assert_eq!(result, "abcde…");
    }
}
