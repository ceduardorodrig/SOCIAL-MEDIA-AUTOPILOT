//! Bluesky (AT Protocol) metrics adapter.
//!
//! Reads engagement from the public AppView query `app.bsky.feed.getPosts`,
//! which returns hydrated `postView`s (like/reply/repost/quote counts) and needs
//! no authentication. No credential is involved in this stage at all.

use anyhow::{Context, Result, bail};
use autopilot_core::Platform;
use serde::Deserialize;

use crate::source::{MetricsRequest, MetricsSource, PostMetrics};

/// Public AppView host that serves the read queries.
pub const DEFAULT_BLUESKY_READ_URL: &str = "https://public.api.bsky.app";

/// XRPC query that hydrates post views by AT-URI.
const GET_POSTS: &str = "app.bsky.feed.getPosts";

/// Metrics adapter for Bluesky's public read API.
#[derive(Debug, Clone)]
pub struct BlueskyMetrics {
    base_url: String,
}

impl BlueskyMetrics {
    /// Build an adapter for `base_url` (trailing slash tolerated).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Base URL this adapter targets.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl MetricsSource for BlueskyMetrics {
    fn platform(&self) -> Platform {
        Platform::Bluesky
    }

    fn request(&self, ids: &[String]) -> Result<MetricsRequest> {
        if ids.is_empty() {
            bail!("no post identifiers provided");
        }
        Ok(MetricsRequest {
            url: format!("{}/xrpc/{GET_POSTS}", self.base_url),
            query: ids
                .iter()
                .map(|id| ("uris".to_string(), id.clone()))
                .collect(),
        })
    }

    fn parse(&self, raw: &str) -> Result<Vec<PostMetrics>> {
        parse_post_views(raw)
    }
}

/// Response of `app.bsky.feed.getPosts`.
#[derive(Debug, Deserialize)]
struct PostsResponse {
    #[serde(default)]
    posts: Vec<PostView>,
}

/// One hydrated `postView` (only the fields the metrics stage needs).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostView {
    #[serde(default)]
    uri: String,
    /// The post record (`unknown` in the lexicon); the body text lives here.
    #[serde(default)]
    record: serde_json::Value,
    #[serde(default)]
    like_count: u64,
    #[serde(default)]
    reply_count: u64,
    #[serde(default)]
    repost_count: u64,
    #[serde(default)]
    quote_count: u64,
}

/// Parse an `app.bsky.feed.getPosts` payload into neutral metrics.
pub fn parse_post_views(raw: &str) -> Result<Vec<PostMetrics>> {
    let response: PostsResponse =
        serde_json::from_str(raw).context("invalid bluesky getPosts JSON")?;
    Ok(response.posts.into_iter().map(post_metrics).collect())
}

fn post_metrics(view: PostView) -> PostMetrics {
    PostMetrics {
        uri: view.uri,
        text: view
            .record
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        likes: view.like_count,
        replies: view.reply_count,
        reposts: view.repost_count,
        quotes: view.quote_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic slice of an `app.bsky.feed.getPosts` response.
    const FIXTURE: &str = r#"{
      "posts": [
        {
          "uri": "at://did:plc:abc123/app.bsky.feed.post/3kxyz",
          "cid": "bafyrei...",
          "author": {"did": "did:plc:abc123", "handle": "ciencia.bsky.social"},
          "record": {"$type": "app.bsky.feed.post", "text": "O cérebro gasta 20%.",
                     "createdAt": "2026-10-12T18:00:00.000Z"},
          "replyCount": 3,
          "repostCount": 5,
          "likeCount": 42,
          "quoteCount": 1,
          "indexedAt": "2026-10-12T18:00:01.000Z"
        },
        {
          "uri": "at://did:plc:abc123/app.bsky.feed.post/3kabc",
          "record": {"text": "Segunda variante."},
          "likeCount": 7
        }
      ]
    }"#;

    #[test]
    fn parses_counts_text_and_uri() {
        let metrics = parse_post_views(FIXTURE).expect("fixture parses");
        assert_eq!(metrics.len(), 2);
        assert_eq!(
            metrics[0].uri,
            "at://did:plc:abc123/app.bsky.feed.post/3kxyz"
        );
        assert_eq!(metrics[0].likes, 42);
        assert_eq!(metrics[0].replies, 3);
        assert_eq!(metrics[0].reposts, 5);
        assert_eq!(metrics[0].quotes, 1);
        assert_eq!(metrics[0].text, "O cérebro gasta 20%.");
        assert_eq!(metrics[0].total_engagement(), 51);
    }

    #[test]
    fn missing_counters_default_to_zero() {
        let metrics = parse_post_views(FIXTURE).expect("fixture parses");
        assert_eq!(metrics[1].likes, 7);
        assert_eq!(metrics[1].replies, 0);
        assert_eq!(metrics[1].reposts, 0);
        assert_eq!(metrics[1].quotes, 0);
        assert_eq!(metrics[1].text, "Segunda variante.");
    }

    #[test]
    fn empty_posts_array_is_valid() {
        let metrics = parse_post_views(r#"{"posts": []}"#).expect("empty parses");
        assert!(metrics.is_empty());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse_post_views("not json").is_err());
    }

    #[test]
    fn request_repeats_uris_and_strips_trailing_slash() {
        let adapter = BlueskyMetrics::new("https://public.api.bsky.app/");
        assert_eq!(adapter.base_url(), "https://public.api.bsky.app");

        let request = adapter
            .request(&["at://a".to_string(), "at://b".to_string()])
            .expect("request builds");
        assert_eq!(
            request.url,
            "https://public.api.bsky.app/xrpc/app.bsky.feed.getPosts"
        );
        assert_eq!(
            request.query,
            vec![
                ("uris".to_string(), "at://a".to_string()),
                ("uris".to_string(), "at://b".to_string()),
            ]
        );
    }

    #[test]
    fn request_without_ids_is_an_error() {
        let adapter = BlueskyMetrics::new(DEFAULT_BLUESKY_READ_URL);
        assert!(adapter.request(&[]).is_err());
    }
}
