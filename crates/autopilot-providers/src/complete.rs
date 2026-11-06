//! Text completion across the provider chain.
//!
//! Each provider kind has its own transport:
//! - `opencode` — the headless CLI (`run --format json`), text extracted from events
//! - `gemini`   — the Developer API (`generateContent`)
//! - `ollama`   — the local server (`/api/generate`)

use std::fmt;
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

/// Short, bounded pause before re-calling a provider that answered 5xx.
///
/// One retry only: a transient upstream fault is worth a second look, a second
/// failure is not — the chain exists so the run moves on instead of hammering.
const SERVER_ERROR_BACKOFF: Duration = Duration::from_millis(250);

/// Total attempts for a 5xx-classified failure (the first call plus one retry).
const SERVER_ERROR_MAX_ATTEMPTS: u32 = 2;

/// Error-event text is operator detail, never a payload: keep it readable.
const ERROR_MESSAGE_LIMIT: usize = 300;

/// How a provider call failed, at the granularity the fallback chain needs.
///
/// The distinction is operational, not cosmetic: a quota cap means "this
/// provider is spent until its window resets", a 5xx means "try once more", a
/// 401 means "the key is wrong, no retry will fix it". Collapsing them into one
/// `exit status: 1` is what let a rate limit look like a crash for 60 hours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Quota / rate cap: `provider.quota` or HTTP 429. Advance immediately.
    Quota,
    /// Upstream fault: HTTP 5xx. Retry the same provider once, then advance.
    Server,
    /// Credentials refused: HTTP 401/403. Advance; the next provider has its own key.
    Auth,
    /// The process died without an error event: nothing to classify.
    Crash,
    /// Anything else.
    Other,
}

impl FailureClass {
    /// Classify by HTTP status, the strongest signal when one is present.
    pub fn from_status(status: u16) -> Self {
        match status {
            429 => Self::Quota,
            401 | 403 => Self::Auth,
            500..=599 => Self::Server,
            _ => Self::Other,
        }
    }

    /// Classify by the provider's own error `type` when no status came with it.
    pub fn from_error_type(error_type: &str) -> Self {
        let lowered = error_type.to_ascii_lowercase();
        if lowered.contains("quota") || lowered.contains("rate") || lowered.contains("limit") {
            Self::Quota
        } else if lowered.contains("auth")
            || lowered.contains("forbidden")
            || lowered.contains("credential")
        {
            Self::Auth
        } else if lowered.contains("server")
            || lowered.contains("unavailable")
            || lowered.contains("overload")
        {
            Self::Server
        } else {
            Self::Other
        }
    }

    /// Delay before re-calling the *same* provider; `None` means hop now.
    pub fn retry_delay(self) -> Option<Duration> {
        match self {
            Self::Server => Some(SERVER_ERROR_BACKOFF),
            _ => None,
        }
    }

    /// Headline that makes a 429, a 401 and a crash read differently.
    pub fn headline(self) -> &'static str {
        match self {
            Self::Quota => "quota limit",
            Self::Server => "server error",
            Self::Auth => "credentials rejected",
            Self::Crash => "crashed",
            Self::Other => "call failed",
        }
    }

    /// What the chain did with this failure, for the aggregate journal detail.
    pub fn action_note(self) -> &'static str {
        match self {
            Self::Quota => "quota: advanced immediately, no retry",
            Self::Server => "5xx: retried once after the bounded backoff, then advanced",
            Self::Auth => "auth: advanced, each provider has its own credentials",
            Self::Crash => "crash: advanced",
            Self::Other => "advanced",
        }
    }
}

/// A provider failure carrying everything the journal needs to tell a quota cap
/// from a bad key from a crash: `kind`, HTTP status, error type and message.
#[derive(Debug, Clone)]
pub struct ProviderFailure {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub class: FailureClass,
    pub status: Option<u16>,
    pub error_type: Option<String>,
    pub message: String,
}

impl ProviderFailure {
    fn new(cfg: &ProviderConfig, class: FailureClass, message: String) -> Self {
        Self {
            provider_id: cfg.id.clone(),
            kind: cfg.kind,
            class,
            status: None,
            error_type: None,
            message,
        }
    }

    /// Wrap an error raised by a transport that does not classify itself
    /// (gateway lease, missing env var, malformed JSON…).
    fn wrap(cfg: &ProviderConfig, err: anyhow::Error) -> Self {
        Self::new(
            cfg,
            FailureClass::Other,
            truncate(&format!("{err:#}"), ERROR_MESSAGE_LIMIT),
        )
    }

    fn timeout(cfg: &ProviderConfig, timeout: Duration) -> Self {
        Self::new(
            cfg,
            FailureClass::Other,
            format!("timed out after {timeout:?}"),
        )
    }

    /// An HTTP failure: the status is the classification as well as the detail.
    fn http(cfg: &ProviderConfig, status: u16, body: &str) -> Self {
        Self {
            status: Some(status),
            ..Self::new(
                cfg,
                FailureClass::from_status(status),
                truncate(body, ERROR_MESSAGE_LIMIT),
            )
        }
    }
}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} — kind={}",
            self.provider_id,
            self.class.headline(),
            self.kind.as_str()
        )?;
        if let Some(status) = self.status {
            write!(f, ", HTTP {status}")?;
        }
        if let Some(error_type) = &self.error_type {
            write!(f, ", error type `{error_type}`")?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for ProviderFailure {}

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
    complete_detailed(cfg, prompt, kind, timeout)
        .await
        .map_err(anyhow::Error::new)
}

/// One provider call, with the failure classified instead of flattened.
async fn complete_detailed(
    cfg: &ProviderConfig,
    prompt: &str,
    kind: JobKind,
    timeout: Duration,
) -> std::result::Result<String, ProviderFailure> {
    match tokio::time::timeout(timeout, complete_inner(cfg, prompt, kind)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(ProviderFailure::timeout(cfg, timeout)),
    }
}

async fn complete_inner(
    cfg: &ProviderConfig,
    prompt: &str,
    kind: JobKind,
) -> std::result::Result<String, ProviderFailure> {
    match cfg.kind {
        ProviderKind::Opencode => complete_opencode(cfg, prompt).await,
        ProviderKind::Gemini => complete_gemini(cfg, prompt).await,
        ProviderKind::Ollama => complete_ollama(cfg, prompt, kind).await,
    }
}

/// Try every provider in priority order, returning the first success.
///
/// Errors from earlier providers are collected and reported only if the whole
/// chain fails, so a single down provider never blocks the pipeline. The class
/// of the failure decides how the chain moves on: a quota cap (429) hops
/// immediately, a 5xx gets one retry after a short bounded backoff, everything
/// else hops at once.
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
        let mut attempt: u32 = 1;
        loop {
            match complete_detailed(&provider, prompt, kind, timeout).await {
                Ok(text) => {
                    return Ok(Completion {
                        provider_id: provider.id,
                        model: provider.model,
                        text,
                    });
                }
                Err(failure) => {
                    if attempt < SERVER_ERROR_MAX_ATTEMPTS
                        && let Some(delay) = failure.class.retry_delay()
                    {
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    failures.push(format!("{failure} [{}]", failure.class.action_note()));
                    break;
                }
            }
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

/// The error event `opencode run` writes to stdout just before it exits
/// non-zero. Before this was parsed, the event was thrown away and the journal
/// kept only `exit status: 1:` with an empty detail.
#[derive(Debug, Default, PartialEq, Eq)]
struct OpencodeErrorEvent {
    error_type: Option<String>,
    message: String,
    status: Option<u16>,
}

/// Find the first `{"type":"error",…}` line in the stdout event stream.
///
/// Shapes seen in the wild: `error.type` / `error.message` / `error.status`,
/// with the status also nested under `error.response.status`. Anything else on
/// the stream (text, step events, junk) is skipped, exactly like the happy-path
/// parser does.
fn find_opencode_error_event(raw: &str) -> Option<OpencodeErrorEvent> {
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let is_error_event = event.get("type").and_then(serde_json::Value::as_str) == Some("error")
            || event.get("error").is_some_and(serde_json::Value::is_object);
        if !is_error_event {
            continue;
        }

        let error = event.get("error");
        let error_type = error
            .and_then(|value| value.get("type"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let message = error
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| {
                        value
                            .pointer("/data/message")
                            .and_then(serde_json::Value::as_str)
                    })
            })
            .unwrap_or_default()
            .to_string();
        let status = error.and_then(|value| {
            value
                .get("status")
                .and_then(json_status)
                .or_else(|| value.pointer("/response/status").and_then(json_status))
        });

        return Some(OpencodeErrorEvent {
            error_type,
            message,
            status,
        });
    }
    None
}

/// A status may arrive as a number or as a numeric string.
fn json_status(value: &serde_json::Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|number| u16::try_from(number).ok())
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// Turn a failed `opencode` exit into a failure the operator can act on.
///
/// stdout is consulted first because that is where the CLI puts the error
/// event; stderr is usually empty, which is why the old message said nothing.
pub(crate) fn opencode_failure(
    cfg: &ProviderConfig,
    exit: &std::process::ExitStatus,
    stdout: &str,
    stderr: &str,
) -> ProviderFailure {
    if let Some(event) = find_opencode_error_event(stdout) {
        let class = event
            .status
            .map(FailureClass::from_status)
            .filter(|class| *class != FailureClass::Other)
            .or_else(|| {
                event
                    .error_type
                    .as_deref()
                    .map(FailureClass::from_error_type)
            })
            .unwrap_or(FailureClass::Other);
        let message = if event.message.trim().is_empty() {
            "error event carried no message".to_string()
        } else {
            truncate(event.message.trim(), ERROR_MESSAGE_LIMIT)
        };
        return ProviderFailure {
            status: event.status,
            error_type: event.error_type,
            ..ProviderFailure::new(cfg, class, message)
        };
    }

    let detail = stderr.trim();
    let stderr_detail = if detail.is_empty() {
        "stderr empty".to_string()
    } else {
        format!("stderr: {}", truncate(detail, ERROR_MESSAGE_LIMIT))
    };
    ProviderFailure::new(
        cfg,
        FailureClass::Crash,
        format!("{exit}, no error event on stdout, {stderr_detail}"),
    )
}

async fn complete_opencode(
    cfg: &ProviderConfig,
    prompt: &str,
) -> std::result::Result<String, ProviderFailure> {
    run_opencode("opencode", cfg, prompt).await
}

/// Spawn the CLI and turn its output into either text or a classified failure.
///
/// `bin` is a test seam: production always passes `"opencode"`, and a test can
/// point at a stub script instead of reaching the network.
async fn run_opencode(
    bin: &str,
    cfg: &ProviderConfig,
    prompt: &str,
) -> std::result::Result<String, ProviderFailure> {
    let output = Command::new(bin)
        .args(["run", "--format", "json", "-m", &cfg.model, prompt])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| {
            ProviderFailure::new(
                cfg,
                FailureClass::Other,
                format!("failed to spawn `{bin}`: {err}"),
            )
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(opencode_failure(
            cfg,
            &output.status,
            &stdout,
            &String::from_utf8_lossy(&output.stderr),
        ));
    }

    let text = extract_opencode_text(&stdout);
    if text.trim().is_empty() {
        return Err(ProviderFailure::new(
            cfg,
            FailureClass::Other,
            "returned no text".to_string(),
        ));
    }
    Ok(text)
}

async fn complete_gemini(
    cfg: &ProviderConfig,
    prompt: &str,
) -> std::result::Result<String, ProviderFailure> {
    let key_env = cfg.api_key_env.as_deref().unwrap_or("GEMINI_API_KEY");
    let key = std::env::var(key_env).map_err(|err| {
        ProviderFailure::new(
            cfg,
            FailureClass::Other,
            format!("missing environment variable {key_env}: {err}"),
        )
    })?;
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
        .map_err(|err| {
            ProviderFailure::new(
                cfg,
                FailureClass::Other,
                format!("gemini request failed: {err}"),
            )
        })?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.map_err(|err| {
        ProviderFailure::new(
            cfg,
            FailureClass::Other,
            format!("invalid gemini JSON: {err}"),
        )
    })?;
    if !status.is_success() {
        return Err(ProviderFailure::http(
            cfg,
            status.as_u16(),
            &payload.to_string(),
        ));
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
        return Err(ProviderFailure::new(
            cfg,
            FailureClass::Other,
            "returned no text".to_string(),
        ));
    }
    Ok(text)
}

/// Local GPU completion, gated by a VRAM lease from the `hl-infer` gateway.
///
/// Homelab policy: local GPU work is only allowed through the gateway, so an
/// unreachable gateway is a hard failure — never a silent bypass. The lease is
/// released on success and on error (the gateway TTL is only a backstop).
async fn complete_ollama(
    cfg: &ProviderConfig,
    prompt: &str,
    kind: JobKind,
) -> std::result::Result<String, ProviderFailure> {
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
        })
        .map_err(|err| ProviderFailure::wrap(cfg, err))?;

    let outcome = generate_ollama(cfg, prompt).await;
    match lease.release().await {
        Ok(()) => outcome,
        Err(release_err) => match outcome {
            Ok(_) => Err(ProviderFailure::wrap(
                cfg,
                release_err.context("failed to release the VRAM lease after generation"),
            )),
            Err(generation_failure) => Err(ProviderFailure {
                message: format!(
                    "{} (and the VRAM lease also failed to release: {release_err:#})",
                    generation_failure.message
                ),
                ..generation_failure
            }),
        },
    }
}

async fn generate_ollama(
    cfg: &ProviderConfig,
    prompt: &str,
) -> std::result::Result<String, ProviderFailure> {
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
        .map_err(|err| {
            ProviderFailure::new(
                cfg,
                FailureClass::Other,
                format!("ollama request failed: {err}"),
            )
        })?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.map_err(|err| {
        ProviderFailure::new(
            cfg,
            FailureClass::Other,
            format!("invalid ollama JSON: {err}"),
        )
    })?;
    if !status.is_success() {
        return Err(ProviderFailure::http(
            cfg,
            status.as_u16(),
            &payload.to_string(),
        ));
    }

    let text = payload
        .get("response")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    if text.trim().is_empty() {
        return Err(ProviderFailure::new(
            cfg,
            FailureClass::Other,
            "returned no text".to_string(),
        ));
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

    const TEXT_EVENT: &str = r#"{"type":"text","part":{"type":"text","text":"hello "}}"#;
    const QUOTA_EVENT: &str = concat!(
        r#"{"type":"error","error":{"type":"provider.quota","#,
        r#""message":"Free usage limit reached — subscribe to OpenCode Go","#,
        r#""status":429,"response":{"status":429,"body":"FreeUsageLimitError"}}}"#
    );
    const AUTH_EVENT: &str = concat!(
        r#"{"type":"error","error":{"type":"provider.auth","#,
        r#""message":"invalid api key","status":401}}"#
    );

    fn provider(kind: ProviderKind, id: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.to_string(),
            kind,
            model: "test/model".to_string(),
            priority: 100,
            endpoint: None,
            api_key_env: None,
            timeout_ms: 1_000,
            role: String::new(),
        }
    }

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

    #[test]
    fn the_error_event_is_found_among_text_events() {
        let stream = format!("{TEXT_EVENT}\n{QUOTA_EVENT}\n");
        let event = find_opencode_error_event(&stream).expect("the error event is on stdout");
        assert_eq!(event.error_type.as_deref(), Some("provider.quota"));
        assert_eq!(event.status, Some(429));
        assert!(event.message.contains("Free usage limit"));
        assert!(find_opencode_error_event(TEXT_EVENT).is_none());
    }

    #[test]
    fn quota_advances_at_once_and_5xx_retries_once() {
        assert_eq!(FailureClass::from_status(429), FailureClass::Quota);
        assert_eq!(FailureClass::from_status(401), FailureClass::Auth);
        assert_eq!(FailureClass::from_status(403), FailureClass::Auth);
        assert_eq!(FailureClass::from_status(503), FailureClass::Server);
        assert_eq!(
            FailureClass::from_error_type("provider.quota"),
            FailureClass::Quota
        );

        // quota / auth / crash hop straight to the next provider
        assert_eq!(FailureClass::Quota.retry_delay(), None);
        assert_eq!(FailureClass::Auth.retry_delay(), None);
        assert_eq!(FailureClass::Crash.retry_delay(), None);
        // 5xx gets one retry after the short bounded backoff
        assert_eq!(
            FailureClass::Server.retry_delay(),
            Some(SERVER_ERROR_BACKOFF)
        );
        assert_eq!(SERVER_ERROR_MAX_ATTEMPTS, 2);
    }

    #[tokio::test]
    async fn the_chain_advances_past_a_failed_provider() {
        let first_key = "AUTOPILOT_TEST_UNSET_KEY_FIRST";
        let second_key = "AUTOPILOT_TEST_UNSET_KEY_SECOND";
        assert!(
            std::env::var(first_key).is_err(),
            "{first_key} must stay unset"
        );
        assert!(
            std::env::var(second_key).is_err(),
            "{second_key} must stay unset"
        );

        let registry = ProvidersConfig {
            providers: vec![
                ProviderConfig {
                    priority: 10,
                    api_key_env: Some(first_key.to_string()),
                    ..provider(ProviderKind::Gemini, "gemini-a")
                },
                ProviderConfig {
                    priority: 20,
                    api_key_env: Some(second_key.to_string()),
                    ..provider(ProviderKind::Gemini, "gemini-b")
                },
            ],
        };

        let err = complete_with_chain(&registry, "hi", JobKind::Triage, Duration::from_secs(1))
            .await
            .expect_err("both providers fail");
        let detail = format!("{err:#}");
        assert!(detail.contains("every provider failed"), "{detail}");
        assert!(
            detail.contains("gemini-a"),
            "first provider missing: {detail}"
        );
        assert!(
            detail.contains("gemini-b"),
            "chain stopped at the first provider: {detail}"
        );
    }

    #[cfg(unix)]
    mod opencode_cli {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        /// A throwaway `opencode` stand-in, so the child-process handling is
        /// exercised for real (exit code, stdout bytes, stderr) with no network.
        struct StubCli {
            dir: PathBuf,
        }

        impl StubCli {
            fn new(name: &str, body: &str) -> Self {
                let dir = std::env::temp_dir().join(format!(
                    "autopilot-opencode-stub-{}-{name}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&dir).expect("create the stub directory");
                let path = dir.join("opencode");
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))
                    .expect("write the stub script");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("make the stub executable");
                Self { dir }
            }

            fn path(&self) -> PathBuf {
                self.dir.join("opencode")
            }
        }

        impl Drop for StubCli {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        async fn run(name: &str, body: &str) -> std::result::Result<String, ProviderFailure> {
            let stub = StubCli::new(name, body);
            let cfg = provider(ProviderKind::Opencode, "opencode");
            let bin = stub.path().to_str().expect("utf-8 stub path").to_string();

            // Harness-only race: another test thread can fork while the stub is
            // open for writing, which makes exec fail with ETXTBSY. Retry
            // briefly — production execs an installed binary nobody writes to.
            let mut attempt = 0;
            loop {
                attempt += 1;
                match run_opencode(&bin, &cfg, "hi").await {
                    Err(failure) if failure.message.contains("Text file busy") && attempt < 20 => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    other => return other,
                }
            }
        }

        #[tokio::test]
        async fn surfaces_the_quota_event_the_old_code_dropped() {
            let failure = run("quota", &format!("echo '{QUOTA_EVENT}'\nexit 1"))
                .await
                .expect_err("a non-zero exit is a failure");

            assert_eq!(failure.class, FailureClass::Quota);
            assert_eq!(failure.status, Some(429));
            assert_eq!(failure.error_type.as_deref(), Some("provider.quota"));
            assert_eq!(failure.kind, ProviderKind::Opencode);

            let detail = failure.to_string();
            assert!(detail.contains("HTTP 429"), "status missing: {detail}");
            assert!(
                detail.contains("provider.quota"),
                "error type missing: {detail}"
            );
            assert!(
                detail.contains("Free usage limit reached"),
                "message missing: {detail}"
            );
            assert!(detail.contains("kind=opencode"), "kind missing: {detail}");
            assert!(detail.contains("quota limit"), "class missing: {detail}");
            // a quota cap must not look like a crash
            assert!(!detail.contains("crashed"), "{detail}");
            assert_eq!(failure.class.retry_delay(), None);
        }

        #[tokio::test]
        async fn a_rejected_key_reads_differently_from_a_quota_cap() {
            let failure = run("auth", &format!("echo '{AUTH_EVENT}'\nexit 1"))
                .await
                .expect_err("a non-zero exit is a failure");

            assert_eq!(failure.class, FailureClass::Auth);
            assert_eq!(failure.status, Some(401));
            let detail = failure.to_string();
            assert!(detail.contains("HTTP 401"), "{detail}");
            assert!(detail.contains("credentials rejected"), "{detail}");
            assert!(!detail.contains("quota"), "{detail}");
            assert!(!detail.contains("crashed"), "{detail}");
        }

        #[tokio::test]
        async fn a_crash_with_empty_stdout_stays_honest_about_having_no_detail() {
            let failure = run("silent", "exit 1")
                .await
                .expect_err("a non-zero exit is a failure");

            assert_eq!(failure.class, FailureClass::Crash);
            assert_eq!(failure.status, None);
            let detail = failure.to_string();
            assert!(detail.contains("exit status"), "{detail}");
            assert!(
                detail.contains("no error event on stdout"),
                "the absence of a detail must be stated: {detail}"
            );
            assert!(detail.contains("stderr empty"), "{detail}");
            assert!(!detail.contains("HTTP"), "no invented status: {detail}");
        }

        #[tokio::test]
        async fn a_crash_keeps_the_stderr_it_has() {
            let failure = run("stderr", "echo 'flamegraph panic' >&2\nexit 2")
                .await
                .expect_err("a non-zero exit is a failure");

            let detail = failure.to_string();
            assert_eq!(failure.class, FailureClass::Crash);
            assert!(detail.contains("exit status: 2"), "{detail}");
            assert!(detail.contains("flamegraph panic"), "{detail}");
        }

        #[tokio::test]
        async fn a_long_error_message_is_truncated() {
            let event = serde_json::json!({
                "type": "error",
                "error": {
                    "type": "provider.quota",
                    "status": 429,
                    "message": "x".repeat(600),
                }
            })
            .to_string();
            let failure = run("long", &format!("echo '{event}'\nexit 1"))
                .await
                .expect_err("a non-zero exit is a failure");

            assert!(failure.message.chars().count() <= ERROR_MESSAGE_LIMIT + 1);
            assert!(failure.message.ends_with('…'), "{}", failure.message);
        }

        #[tokio::test]
        async fn a_successful_run_is_parsed_exactly_as_before() {
            let text = run(
                "happy",
                concat!(
                    r#"printf '%s\n' '{"type":"step_start","part":{"id":"x"}}' "#,
                    r#"'{"type":"text","part":{"type":"text","text":"hello "}}' "#,
                    r#"'{"type":"text","part":{"type":"text","text":"world"}}'"#,
                    "\nexit 0"
                ),
            )
            .await
            .expect("the happy path is unchanged");
            assert_eq!(text, "hello world");
        }
    }
}
