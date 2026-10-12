//! Bluesky (AT Protocol) publisher.
//!
//! Credentials come from the environment (never from the repository):
//! `BLUESKY_HANDLE` and `BLUESKY_APP_PASSWORD` (an app password, not the account
//! password).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Hosted Bluesky entry point.
pub const DEFAULT_BLUESKY_BASE_URL: &str = "https://bsky.social";

const COLLECTION: &str = "app.bsky.feed.post";

/// Credentials read from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlueskyCredentials {
    pub handle: String,
    pub app_password: String,
}

impl BlueskyCredentials {
    /// Read `BLUESKY_HANDLE` / `BLUESKY_APP_PASSWORD` from the environment.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            handle: crate::env_var("BLUESKY_HANDLE")?,
            app_password: crate::env_var("BLUESKY_APP_PASSWORD")?,
        })
    }
}

/// An authenticated session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub did: String,
    pub handle: String,
    #[serde(rename = "accessJwt")]
    pub access_jwt: String,
}

/// Current time as the RFC 3339 string the AT Protocol expects.
pub fn now_rfc3339_millis() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Build the `app.bsky.feed.post` record body.
pub fn build_post_record(text: &str, created_at: &str) -> serde_json::Value {
    serde_json::json!({
        "$type": COLLECTION,
        "text": text,
        "createdAt": created_at,
    })
}

/// Authenticate and return a session.
pub async fn create_session(
    base_url: &str,
    credentials: &BlueskyCredentials,
    timeout: Duration,
) -> Result<Session> {
    let url = format!("{base_url}/xrpc/com.atproto.server.createSession");
    let body = serde_json::json!({
        "identifier": credentials.handle,
        "password": credentials.app_password,
    });

    let response = tokio::time::timeout(
        timeout,
        reqwest::Client::new().post(&url).json(&body).send(),
    )
    .await
    .context("bluesky session request timed out")?
    .context("bluesky session request failed")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.context("invalid bluesky JSON")?;
    if !status.is_success() {
        let message = payload
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        bail!("bluesky authentication failed (HTTP {status}): {message}");
    }

    serde_json::from_value(payload).context("unexpected bluesky session payload")
}

/// Publish a post record, returning its AT-URI.
pub async fn create_post(
    base_url: &str,
    session: &Session,
    record: &serde_json::Value,
    timeout: Duration,
) -> Result<String> {
    let url = format!("{base_url}/xrpc/com.atproto.repo.createRecord");
    let body = serde_json::json!({
        "repo": session.did,
        "collection": COLLECTION,
        "record": record,
    });

    let response = tokio::time::timeout(
        timeout,
        reqwest::Client::new()
            .post(&url)
            .bearer_auth(&session.access_jwt)
            .json(&body)
            .send(),
    )
    .await
    .context("bluesky post request timed out")?
    .context("bluesky post request failed")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.context("invalid bluesky JSON")?;
    if !status.is_success() {
        let message = payload
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        bail!("bluesky rejected the post (HTTP {status}): {message}");
    }

    payload
        .get("uri")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .context("bluesky did not return a post uri")
}

/// Human-readable URL for a post AT-URI.
pub fn post_url(handle: &str, at_uri: &str) -> String {
    let rkey = at_uri.rsplit('/').next().unwrap_or_default();
    format!("https://bsky.app/profile/{handle}/post/{rkey}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_has_type_text_and_timestamp() {
        let record = build_post_record("olá", "2026-10-12T20:00:00.000Z");
        assert_eq!(record["$type"], "app.bsky.feed.post");
        assert_eq!(record["text"], "olá");
        assert_eq!(record["createdAt"], "2026-10-12T20:00:00.000Z");
    }

    #[test]
    fn timestamp_is_rfc3339_with_millis() {
        let stamp = now_rfc3339_millis();
        assert!(stamp.ends_with('Z'), "expected UTC marker in {stamp}");
        assert!(
            stamp.contains('T'),
            "expected date/time separator in {stamp}"
        );

        let (_, fraction) = stamp.split_once('.').expect("fractional seconds present");
        assert_eq!(fraction.len(), 4, "expected 3 digits + Z, got {fraction}");
        assert!(fraction[..3].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn post_url_is_derived_from_at_uri() {
        let at_uri = "at://did:plc:abc123/app.bsky.feed.post/3kxyz";
        assert_eq!(
            post_url("user.bsky.social", at_uri),
            "https://bsky.app/profile/user.bsky.social/post/3kxyz"
        );
    }
}
