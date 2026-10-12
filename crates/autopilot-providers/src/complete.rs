//! Text completion across the provider chain.
//!
//! Each provider kind has its own transport:
//! - `opencode` — the headless CLI (`run --format json`), text extracted from events
//! - `gemini`   — the Developer API (`generateContent`)
//! - `ollama`   — the local server (`/api/generate`)

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::config::{
    DEFAULT_GEMINI_ENDPOINT, DEFAULT_OLLAMA_ENDPOINT, ProviderConfig, ProviderKind, ProvidersConfig,
};
use crate::gateway::{DEFAULT_GATEWAY_TIMEOUT, GatewayClient, JobKind, estimate_required_bytes};

/// A successful completion, tagged with the provider that produced it.
#[derive(Debug, Clone)]
pub struct Completion {
    pub provider_id: String,
    pub model: String,
    pub text: String,
}

/// Call a single provider.
///
/// `kind` is the autopilot stage, forwarded to the GPU gateway when the local
/// provider requests a VRAM lease. It is ignored by the cloud providers.
pub async fn complete(
    cfg: &ProviderConfig,
    prompt: &str,
    kind: JobKind,
    timeout: Duration,
) -> Result<String> {
    tokio::time::timeout(timeout, complete_inner(cfg, prompt, kind))
        .await
        .with_context(|| format!("provider `{}` timed out after {timeout:?}", cfg.id))?
}

async fn complete_inner(cfg: &ProviderConfig, prompt: &str, kind: JobKind) -> Result<String> {
    match cfg.kind {
        ProviderKind::Opencode => complete_opencode(cfg, prompt).await,
        ProviderKind::Gemini => complete_gemini(cfg, prompt).await,
        ProviderKind::Ollama => complete_ollama(cfg, prompt, kind).await,
    }
}

/// Try every provider in priority order, returning the first success.
///
/// Errors from earlier providers are collected and reported only if the whole
/// chain fails, so a single down provider never blocks the pipeline.
pub async fn complete_with_chain(
    registry: &ProvidersConfig,
    prompt: &str,
    kind: JobKind,
    timeout: Duration,
) -> Result<Completion> {
    let mut providers = registry.providers.clone();
    providers.sort_by_key(|provider| provider.priority);

    let mut failures: Vec<String> = Vec::new();
    for provider in providers {
        match complete(&provider, prompt, kind, timeout).await {
            Ok(text) => {
                return Ok(Completion {
                    provider_id: provider.id,
                    model: provider.model,
                    text,
                });
            }
            Err(err) => failures.push(format!("{}: {err:#}", provider.id)),
        }
    }

    bail!("every provider failed -> {}", failures.join(" | "))
}

/// Extract the assistant text from the event stream emitted by `opencode run`.
pub fn extract_opencode_text(raw: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if event.get("type").and_then(serde_json::Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = event
            .get("part")
            .and_then(|part| part.get("text"))
            .and_then(serde_json::Value::as_str)
        {
            parts.push(text.to_string());
        }
    }
    parts.join("")
}

async fn complete_opencode(cfg: &ProviderConfig, prompt: &str) -> Result<String> {
    let output = Command::new("opencode")
        .args(["run", "--format", "json", "-m", &cfg.model, prompt])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to spawn `opencode`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("opencode exited with {}: {}", output.status, stderr.trim());
    }

    let text = extract_opencode_text(&String::from_utf8_lossy(&output.stdout));
    if text.trim().is_empty() {
        bail!("opencode returned no text");
    }
    Ok(text)
}

async fn complete_gemini(cfg: &ProviderConfig, prompt: &str) -> Result<String> {
    let key_env = cfg.api_key_env.as_deref().unwrap_or("GEMINI_API_KEY");
    let key = std::env::var(key_env)
        .with_context(|| format!("missing environment variable {key_env}"))?;
    let endpoint = cfg.endpoint_or_default(DEFAULT_GEMINI_ENDPOINT);
    let url = format!(
        "{endpoint}/v1beta/models/{}:generateContent?key={key}",
        cfg.model
    );

    let body = serde_json::json!({
        "contents": [{ "parts": [{ "text": prompt }] }]
    });

    let response = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("gemini request failed")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.context("invalid gemini JSON")?;
    if !status.is_success() {
        bail!(
            "gemini returned HTTP {status}: {}",
            truncate(&payload.to_string(), 200)
        );
    }

    let text = payload
        .pointer("/candidates/0/content/parts")
        .and_then(serde_json::Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    if text.trim().is_empty() {
        bail!("gemini returned no text");
    }
    Ok(text)
}

/// Local GPU completion, gated by a VRAM lease from the `hl-infer` gateway.
///
/// Homelab policy: local GPU work is only allowed through the gateway, so an
/// unreachable gateway is a hard failure — never a silent bypass. The lease is
/// released on success and on error (the gateway TTL is only a backstop).
async fn complete_ollama(cfg: &ProviderConfig, prompt: &str, kind: JobKind) -> Result<String> {
    let gateway = GatewayClient::from_env(DEFAULT_GATEWAY_TIMEOUT);
    let required_bytes = estimate_required_bytes(&cfg.model);
    let label = format!("ollama:{}", cfg.model);

    let lease = gateway
        .lease(kind, required_bytes, &label)
        .await
        .with_context(|| {
            format!(
                "local GPU provider `{}` could not obtain a VRAM lease from the hl-infer gateway",
                cfg.id
            )
        })?;

    let outcome = generate_ollama(cfg, prompt).await;
    match lease.release().await {
        Ok(()) => outcome,
        Err(release_err) => match outcome {
            Ok(_) => Err(release_err.context("failed to release the VRAM lease after generation")),
            Err(gen_err) => Err(gen_err.context(format!(
                "and the VRAM lease also failed to release: {release_err:#}"
            ))),
        },
    }
}

async fn generate_ollama(cfg: &ProviderConfig, prompt: &str) -> Result<String> {
    let endpoint = cfg.endpoint_or_default(DEFAULT_OLLAMA_ENDPOINT);
    let url = format!("{endpoint}/api/generate");

    // keep_alive=0 releases the GPU as soon as the job finishes (VRAM is scarce).
    let body = serde_json::json!({
        "model": cfg.model,
        "prompt": prompt,
        "stream": false,
        "keep_alive": 0
    });

    let response = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("ollama request failed")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.context("invalid ollama JSON")?;
    if !status.is_success() {
        bail!(
            "ollama returned HTTP {status}: {}",
            truncate(&payload.to_string(), 200)
        );
    }

    let text = payload
        .get("response")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    if text.trim().is_empty() {
        bail!("ollama returned no text");
    }
    Ok(text)
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
    use super::*;

    #[test]
    fn extracts_text_from_opencode_events() {
        let raw = concat!(
            r#"{"type":"step_start","part":{"id":"x"}}"#,
            "\n",
            r#"{"type":"text","part":{"type":"text","text":"hello "}}"#,
            "\n",
            r#"{"type":"text","part":{"type":"text","text":"world"}}"#,
        );
        assert_eq!(extract_opencode_text(raw), "hello world");
    }

    #[test]
    fn ignores_non_text_and_invalid_lines() {
        let raw = "not json\n{}\n{\"type\":\"step_begin\"}\n";
        assert_eq!(extract_opencode_text(raw), "");
    }
}
