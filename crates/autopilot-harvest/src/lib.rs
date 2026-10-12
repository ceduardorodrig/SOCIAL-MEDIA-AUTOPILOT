//! Harvest stage: trend collection from SearXNG's JSON API.
//!
//! SearXNG runs on kavure and exposes `?format=json`, which is what the n8n
//! pipelines already consume. This crate is the harness-side client.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default SearXNG endpoint (kavure, tailnet).
pub const DEFAULT_SEARXNG_ENDPOINT: &str = "http://100.124.146.77:8080";

fn default_engine() -> String {
    "unknown".to_string()
}

/// A harvested trend candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topic {
    pub title: String,
    pub url: String,
    pub content: String,
    pub engine: String,
}

#[derive(Debug, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Debug, Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
    #[serde(default = "default_engine")]
    engine: String,
}

/// Client for the SearXNG JSON API.
#[derive(Debug, Clone)]
pub struct SearxngClient {
    endpoint: String,
    client: reqwest::Client,
}

impl SearxngClient {
    /// Create a client for the given endpoint (trailing slash tolerated).
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        Self {
            endpoint,
            client: reqwest::Client::new(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Run a search and return at most `limit` topics.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Topic>> {
        let url = format!("{}/search", self.endpoint);
        let response = self
            .client
            .get(&url)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await
            .with_context(|| format!("searxng request failed ({url})"))?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("searxng returned HTTP {status}");
        }

        let body = response
            .text()
            .await
            .context("reading searxng response body")?;

        parse_topics(&body, limit)
    }
}

/// Parse a raw SearXNG JSON payload into topics, dropping entries without a URL.
pub fn parse_topics(raw: &str, limit: usize) -> Result<Vec<Topic>> {
    let response: SearxngResponse =
        serde_json::from_str(raw).context("invalid searxng JSON response")?;

    Ok(response
        .results
        .into_iter()
        .filter(|result| !result.url.trim().is_empty())
        .take(limit)
        .map(|result| Topic {
            title: result.title,
            url: result.url,
            content: result.content,
            engine: result.engine,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> SearxngClient {
        SearxngClient::new("http://example.invalid:8080/")
    }

    #[test]
    fn strips_trailing_slash() {
        assert_eq!(client().endpoint(), "http://example.invalid:8080");
    }

    #[test]
    fn parses_and_limits_results() {
        let topics = parse_topics(
            r#"{
              "results": [
                {"title": "A", "url": "https://a.example", "content": "aa", "engine": "wikipedia"},
                {"title": "B", "url": "https://b.example", "content": "bb", "engine": "duckduckgo"},
                {"title": "C", "url": "https://c.example", "content": "cc", "engine": "google"}
              ]
            }"#,
            2,
        )
        .expect("fixture parses");

        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].title, "A");
        assert_eq!(topics[1].engine, "duckduckgo");
    }

    #[test]
    fn drops_entries_without_url() {
        let topics = parse_topics(
            r#"{"results": [
                 {"title": "no url", "url": "", "content": "x"},
                 {"title": "ok", "url": "https://ok.example"}
               ]}"#,
            10,
        )
        .expect("fixture parses");

        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].title, "ok");
        assert_eq!(topics[0].engine, "unknown");
    }
}
