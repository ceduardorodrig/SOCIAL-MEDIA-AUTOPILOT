//! Client for the centralized GPU gateway (`hl-infer`).
//!
//! Homelab policy: local GPU work is only allowed through the gateway. The
//! Ollama provider must hold a VRAM **lease** before it runs and must release
//! it afterwards, even when generation fails. The gateway also enforces a TTL
//! as a backstop, but an explicit release frees VRAM immediately.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Default gateway base URL, overridable with the `HL_INFER_URL` environment variable.
pub const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:9095";

/// Environment variable holding the gateway base URL.
pub const GATEWAY_URL_ENV: &str = "HL_INFER_URL";

/// Default timeout for a single gateway call.
pub const DEFAULT_GATEWAY_TIMEOUT: Duration = Duration::from_secs(30);

/// Fallback VRAM estimate (bytes) when the model size cannot be inferred.
pub const DEFAULT_REQUIRED_BYTES: u64 = 4_000_000_000;

/// Numerator of the assumed VRAM cost per parameter.
///
/// Local Ollama models normally run 4-bit quantized (about 0.5 byte per
/// parameter); we budget 0.75 byte per parameter (3/4) to leave headroom for
/// the KV cache and activations. This is only a hint: the gateway measures the
/// real device itself, so no device size is hardcoded here.
const VRAM_BYTES_PER_PARAM_NUM: u64 = 3;

/// Denominator of the assumed VRAM cost per parameter (see the numerator).
const VRAM_BYTES_PER_PARAM_DEN: u64 = 4;

/// Gateway job kind, mirroring the `POST /v1/jobs` contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Triage,
    Create,
    ImageGen,
    #[default]
    Other,
}

impl JobKind {
    /// Wire representation used by the gateway.
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Triage => "triage",
            JobKind::Create => "create",
            JobKind::ImageGen => "image_gen",
            JobKind::Other => "other",
        }
    }
}

/// A lease (job) as returned by the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub required_bytes: u64,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub state: String,
}

/// Request body for `POST /v1/jobs`.
#[derive(Debug, Serialize)]
struct LeaseRequest<'a> {
    kind: JobKind,
    required_bytes: u64,
    label: &'a str,
}

/// Minimal client for the `hl-infer` GPU gateway.
#[derive(Debug, Clone)]
pub struct GatewayClient {
    base_url: String,
    timeout: Duration,
    /// How long the gateway may block waiting for admission. The waiting policy
    /// is centralized in the gateway; consumers only declare their patience.
    wait_secs: u64,
    http: reqwest::Client,
}

/// Default patience when the gateway has no free VRAM yet (seconds).
pub const DEFAULT_WAIT_SECS: u64 = 600;

/// Environment variable that overrides [`DEFAULT_WAIT_SECS`].
pub const WAIT_ENV: &str = "HL_INFER_WAIT_SECS";

impl GatewayClient {
    /// Build a client for `base_url` with a per-call timeout.
    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timeout,
            wait_secs: DEFAULT_WAIT_SECS,
            http: reqwest::Client::new(),
        }
    }

    /// Override how long the gateway may block waiting for a lease.
    pub fn with_wait_secs(mut self, wait_secs: u64) -> Self {
        self.wait_secs = wait_secs;
        self
    }

    /// Build a client from `HL_INFER_URL`, falling back to the local default.
    pub fn from_env(timeout: Duration) -> Self {
        let base_url =
            std::env::var(GATEWAY_URL_ENV).unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_string());
        let wait_secs = std::env::var(WAIT_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_WAIT_SECS);
        Self::new(base_url, timeout).with_wait_secs(wait_secs)
    }

    /// Base URL this client targets.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Request a VRAM lease and return the created job.
    ///
    /// The gateway blocks until the job is admitted (or its wait expires), so the
    /// "wait for VRAM" policy lives in one place instead of in every consumer. A
    /// job still `queued` after the wait is an **error**: local GPU work must
    /// never proceed unarbitrated.
    pub async fn submit(&self, kind: JobKind, required_bytes: u64, label: &str) -> Result<Job> {
        let url = format!("{}/v1/jobs?wait_secs={}", self.base_url, self.wait_secs);
        let body = LeaseRequest {
            kind,
            required_bytes,
            label,
        };

        let response = tokio::time::timeout(self.timeout, self.http.post(&url).json(&body).send())
            .await
            .with_context(|| format!("GPU gateway timed out after {:?} at {url}", self.timeout))?
            .with_context(|| format!("GPU gateway unreachable at {url} (set {GATEWAY_URL_ENV})"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("reading the GPU gateway response")?;
        if !status.is_success() {
            let snippet: String = text.chars().take(200).collect();
            bail!("GPU gateway rejected the lease with HTTP {status}: {snippet}");
        }

        let job: Job =
            serde_json::from_str(&text).context("invalid job JSON from the GPU gateway")?;
        if job.state != "admitted" {
            bail!(
                "GPU gateway did not admit the lease (state `{}`) after waiting {}s: the GPU is busy or the batch window is closed",
                job.state,
                self.wait_secs
            );
        }
        Ok(job)
    }

    /// Request a VRAM lease and return a handle that releases it on drop.
    pub async fn lease(&self, kind: JobKind, required_bytes: u64, label: &str) -> Result<Lease> {
        let job = self.submit(kind, required_bytes, label).await?;
        Ok(Lease {
            client: self.clone(),
            job,
            released: false,
        })
    }

    /// Release a lease so its VRAM becomes available again.
    pub async fn release(&self, id: &str) -> Result<()> {
        let url = format!("{}/v1/jobs/{id}/release", self.base_url);

        let response = tokio::time::timeout(self.timeout, self.http.post(&url).send())
            .await
            .with_context(|| {
                format!(
                    "GPU gateway timed out after {:?} releasing lease {id}",
                    self.timeout
                )
            })?
            .with_context(|| {
                format!("GPU gateway unreachable at {url} while releasing lease {id}")
            })?;

        let status = response.status();
        if !status.is_success() {
            bail!("GPU gateway failed to release lease {id} (HTTP {status})");
        }
        Ok(())
    }
}

/// A held VRAM lease.
///
/// Prefer the explicit [`Lease::release`]. If the lease is dropped without an
/// explicit release — for example when an outer timeout cancels the future —
/// the `Drop` implementation still schedules a best-effort release so a lease
/// cannot leak. The gateway TTL remains the final backstop for process crashes.
#[derive(Debug)]
pub struct Lease {
    client: GatewayClient,
    job: Job,
    released: bool,
}

impl Lease {
    /// The gateway job id backing this lease.
    pub fn id(&self) -> &str {
        &self.job.id
    }

    /// The full job record returned by the gateway.
    pub fn job(&self) -> &Job {
        &self.job
    }

    /// Release the lease and return the gateway outcome.
    pub async fn release(mut self) -> Result<()> {
        self.released = true;
        self.client.release(&self.job.id).await
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        let id = self.job.id.clone();
        handle.spawn(async move {
            let _ = client.release(&id).await;
        });
    }
}

/// Estimate the VRAM a model needs, in bytes.
///
/// The value is a hint for the gateway scheduler, derived from the parameter
/// count in the model tag (for example `llama3:8b`). It is never a device size:
/// the gateway measures the real device itself. Unknown sizes fall back to
/// [`DEFAULT_REQUIRED_BYTES`].
pub fn estimate_required_bytes(model: &str) -> u64 {
    parse_billions(model).map_or(DEFAULT_REQUIRED_BYTES, |billions| {
        billions
            .saturating_mul(1_000_000_000)
            .saturating_mul(VRAM_BYTES_PER_PARAM_NUM)
            / VRAM_BYTES_PER_PARAM_DEN
    })
}

/// Parse the `<n>b` parameter count from a model tag, in billions.
fn parse_billions(model: &str) -> Option<u64> {
    let lower = model.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if bytes.get(index) == Some(&b'b')
            && let Ok(value) = lower[start..index].parse::<u64>()
        {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Json;
    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::post;
    use serde_json::json;

    use super::*;

    /// Start an in-process mock gateway on a random local port.
    async fn spawn(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock gateway");
        let addr = listener.local_addr().expect("mock gateway address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve mock gateway");
        });
        format!("http://{addr}")
    }

    /// Mock gateway that never admits: the job stays queued.
    async fn queued_submit(
        Json(_body): Json<serde_json::Value>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        (
            StatusCode::ACCEPTED,
            Json(json!({
                "id": "job-000099",
                "kind": "other",
                "required_bytes": 1,
                "label": "",
                "state": "queued",
                "seq": 1,
                "enqueued_at_unix": 0
            })),
        )
    }

    #[tokio::test]
    async fn a_queued_lease_is_an_error() {
        let base = spawn(Router::new().route("/v1/jobs", post(queued_submit))).await;
        let client = GatewayClient::new(base, Duration::from_secs(5)).with_wait_secs(0);
        let result = client.submit(JobKind::Create, 1_000_000, "test").await;
        assert!(
            result.is_err(),
            "a queued lease must never be accepted — GPU work cannot proceed unarbitrated"
        );
    }

    async fn echo_submit(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "id": "job-000042",
            "kind": body.get("kind").cloned().unwrap_or(json!("other")),
            "required_bytes": body.get("required_bytes").cloned().unwrap_or(json!(0)),
            "label": body.get("label").cloned().unwrap_or(json!("")),
            "state": "admitted"
        }))
    }

    async fn ok_release() -> StatusCode {
        StatusCode::OK
    }

    async fn record_release(
        State(releases): State<Arc<Mutex<Vec<String>>>>,
        Path(id): Path<String>,
    ) -> StatusCode {
        releases.lock().expect("lock releases").push(id);
        StatusCode::OK
    }

    async fn failing_release() -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }

    async fn slow_submit() -> StatusCode {
        tokio::time::sleep(Duration::from_secs(30)).await;
        StatusCode::OK
    }

    #[tokio::test]
    async fn submit_maps_request_and_parses_job() {
        let base = spawn(Router::new().route("/v1/jobs", post(echo_submit))).await;
        let client = GatewayClient::new(base, Duration::from_secs(2));

        let job = client
            .submit(JobKind::Create, 3_000_000_000, "ollama:llama3:8b")
            .await
            .expect("submit should succeed");

        assert_eq!(job.id, "job-000042");
        assert_eq!(job.kind, "create");
        assert_eq!(job.required_bytes, 3_000_000_000);
        assert_eq!(job.label, "ollama:llama3:8b");
        assert_eq!(job.state, "admitted");
    }

    #[tokio::test]
    async fn release_succeeds_on_success_status() {
        let app = Router::new().route("/v1/jobs/{id}/release", post(ok_release));
        let base = spawn(app).await;
        let client = GatewayClient::new(base, Duration::from_secs(2));

        client
            .release("job-000042")
            .await
            .expect("release should succeed");
    }

    #[tokio::test]
    async fn dropping_a_lease_releases_it() {
        let releases: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/jobs", post(echo_submit))
            .route("/v1/jobs/{id}/release", post(record_release))
            .with_state(releases.clone());
        let base = spawn(app).await;
        let client = GatewayClient::new(base, Duration::from_secs(2));

        let lease = client
            .lease(JobKind::Triage, 1_000_000_000, "ollama:llama3:8b")
            .await
            .expect("lease should be created");
        assert_eq!(lease.id(), "job-000042");
        drop(lease);

        // Give the detached release task a moment to run.
        for _ in 0..100 {
            if !releases.lock().expect("lock releases").is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            releases.lock().expect("lock releases").as_slice(),
            ["job-000042"]
        );
    }

    #[tokio::test]
    async fn release_reports_http_errors() {
        let app = Router::new().route("/v1/jobs/{id}/release", post(failing_release));
        let base = spawn(app).await;
        let client = GatewayClient::new(base, Duration::from_secs(2));

        let err = client
            .release("job-000042")
            .await
            .expect_err("release must fail");
        assert!(
            err.to_string().contains("release"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn submit_fails_when_gateway_is_unreachable() {
        // Bind and drop to obtain a port that nothing is listening on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind free port");
        let addr = listener.local_addr().expect("free port address");
        drop(listener);

        let client = GatewayClient::new(format!("http://{addr}"), Duration::from_secs(2));
        let err = client
            .submit(JobKind::Other, 1, "probe")
            .await
            .expect_err("an unreachable gateway must fail");
        assert!(
            err.to_string().contains("unreachable"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn submit_times_out_with_the_configured_timeout() {
        let base = spawn(Router::new().route("/v1/jobs", post(slow_submit))).await;
        let client = GatewayClient::new(base, Duration::from_millis(100));

        let err = client
            .submit(JobKind::Other, 1, "probe")
            .await
            .expect_err("a slow gateway must time out");
        assert!(
            err.to_string().contains("timed out"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn job_kind_serializes_to_the_gateway_contract() {
        assert_eq!(
            serde_json::to_string(&JobKind::Triage).expect("serialize"),
            "\"triage\""
        );
        assert_eq!(
            serde_json::to_string(&JobKind::Create).expect("serialize"),
            "\"create\""
        );
        assert_eq!(
            serde_json::to_string(&JobKind::ImageGen).expect("serialize"),
            "\"image_gen\""
        );
        assert_eq!(
            serde_json::to_string(&JobKind::Other).expect("serialize"),
            "\"other\""
        );
    }

    #[test]
    fn estimates_bytes_from_model_size_and_falls_back() {
        assert_eq!(estimate_required_bytes("llama3:8b"), 6_000_000_000);
        assert_eq!(estimate_required_bytes("qwen2.5:7b"), 5_250_000_000);
        assert_eq!(estimate_required_bytes("phi4:14b"), 10_500_000_000);
        assert_eq!(
            estimate_required_bytes("mistral:latest"),
            DEFAULT_REQUIRED_BYTES
        );
    }
}
