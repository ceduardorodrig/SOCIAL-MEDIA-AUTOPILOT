//! The platform-agnostic metrics collection contract.
//!
//! Adapters are split into a pure part (build the read request, parse the
//! payload) and one shared effectful driver ([`collect`]). That keeps every
//! payload parser unit-testable with fixtures and never calls the network in a
//! test, while a new platform only implements two methods.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use autopilot_core::Platform;
use serde::{Deserialize, Serialize};

/// Default timeout for a single metrics request.
pub const DEFAULT_METRICS_TIMEOUT: Duration = Duration::from_secs(30);

/// Platform-neutral engagement counters for one published post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostMetrics {
    /// Platform post identifier (for Bluesky, the post AT-URI).
    pub uri: String,
    /// Post body as returned by the platform, when available.
    #[serde(default)]
    pub text: String,
    pub likes: u64,
    pub replies: u64,
    pub reposts: u64,
    pub quotes: u64,
}

impl PostMetrics {
    /// Raw interaction count, without the A/B weighting (that lives in `learn`).
    pub fn total_engagement(&self) -> u64 {
        self.likes + self.replies + self.reposts + self.quotes
    }
}

/// A read-only metrics request: where to ask and with which query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsRequest {
    pub url: String,
    pub query: Vec<(String, String)>,
}

/// A platform adapter able to read engagement for published posts.
///
/// The trait is deliberately pure so it stays object-safe and offline-testable:
/// it only builds the request and parses the payload. The single effectful
/// driver [`collect`] performs the HTTP call, so every adapter shares one
/// transport.
pub trait MetricsSource {
    /// Platform this adapter speaks for.
    fn platform(&self) -> Platform;
    /// Build the read-only request for a batch of post identifiers.
    fn request(&self, ids: &[String]) -> Result<MetricsRequest>;
    /// Parse a raw platform payload into neutral metrics.
    fn parse(&self, raw: &str) -> Result<Vec<PostMetrics>>;
}

/// Fetch metrics for `ids` through any adapter, with one shared transport.
pub async fn collect(
    source: &dyn MetricsSource,
    ids: &[String],
    timeout: Duration,
) -> Result<Vec<PostMetrics>> {
    let request = source.request(ids)?;

    let response = tokio::time::timeout(
        timeout,
        reqwest::Client::new()
            .get(&request.url)
            .query(&request.query)
            .send(),
    )
    .await
    .with_context(|| format!("metrics request to {} timed out", request.url))?
    .with_context(|| format!("metrics request to {} failed", request.url))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("reading the metrics response")?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        bail!("metrics source returned HTTP {status}: {snippet}");
    }

    source.parse(&body)
}

#[cfg(test)]
mod tests {
    use axum::Json;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use serde_json::json;

    use super::*;
    use crate::bluesky::BlueskyMetrics;

    /// Start a mock `getPosts` endpoint on a random local port.
    async fn spawn(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock source");
        let addr = listener.local_addr().expect("mock source address");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve mock source");
        });
        format!("http://{addr}")
    }

    async fn posts() -> Json<serde_json::Value> {
        Json(
            json!({"posts": [{"uri": "at://did:plc:abc/app.bsky.feed.post/1",
                                "record": {"text": "olá"},
                                "likeCount": 7, "replyCount": 2,
                                "repostCount": 1, "quoteCount": 0}]}),
        )
    }

    async fn boom() -> (StatusCode, &'static str) {
        (StatusCode::INTERNAL_SERVER_ERROR, "boom")
    }

    #[tokio::test]
    async fn collect_fetches_and_parses_over_http() {
        let base = spawn(Router::new().route("/xrpc/app.bsky.feed.getPosts", get(posts))).await;
        let source = BlueskyMetrics::new(base);
        let ids = vec!["at://did:plc:abc/app.bsky.feed.post/1".to_string()];

        let metrics = collect(&source, &ids, DEFAULT_METRICS_TIMEOUT)
            .await
            .expect("collect should succeed");

        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].likes, 7);
        assert_eq!(metrics[0].text, "olá");
    }

    #[tokio::test]
    async fn collect_reports_http_errors() {
        let base = spawn(Router::new().route("/xrpc/app.bsky.feed.getPosts", get(boom))).await;
        let source = BlueskyMetrics::new(base);
        let ids = vec!["at://x".to_string()];

        let err = collect(&source, &ids, DEFAULT_METRICS_TIMEOUT)
            .await
            .expect_err("HTTP 500 must fail");
        assert!(err.to_string().contains("500"), "unexpected error: {err}");
    }
}
