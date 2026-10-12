//! Bluesky reads and writes for the engagement stage.
//!
//! Public reads (`getPostThread`, `getAuthorFeed`, `getProfile`) go to the
//! public AppView; the authenticated notification read and every write go to the
//! session's host. The session and its credential path are the publication
//! adapter's, reused as-is, so the repository keeps a single login path.
//!
//! Note: `app.bsky.feed.getReplies` no longer exists on the AppView (it answers
//! `501 MethodNotImplemented`); the supported way to read a post's replies is
//! `app.bsky.feed.getPostThread`, which this module uses.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use autopilot_publish::{Session, create_record, now_rfc3339_millis};

use crate::candidates::{
    Notification, OwnPost, Profile, Thread, parse_author_feed, parse_notifications, parse_profile,
    parse_thread,
};
use crate::records::{
    FOLLOW_COLLECTION, LIKE_COLLECTION, POST_COLLECTION, StrongRef, build_follow_record,
    build_like_record, build_reply_record,
};

/// Public AppView used for unauthenticated reads.
pub const DEFAULT_BLUESKY_READ_URL: &str = "https://public.api.bsky.app";

/// Default timeout for one engagement API call.
pub const DEFAULT_ENGAGE_TIMEOUT: Duration = Duration::from_secs(30);

const LIST_NOTIFICATIONS: &str = "app.bsky.notification.listNotifications";
const GET_POST_THREAD: &str = "app.bsky.feed.getPostThread";
const GET_AUTHOR_FEED: &str = "app.bsky.feed.getAuthorFeed";
const GET_PROFILE: &str = "app.bsky.actor.getProfile";

/// Read one XRPC query and return the raw JSON body.
async fn get_text(
    base_url: &str,
    method: &str,
    query: &[(String, String)],
    bearer: Option<&str>,
    timeout: Duration,
) -> Result<String> {
    let url = format!("{}/xrpc/{method}", base_url.trim_end_matches('/'));
    let mut request = reqwest::Client::new().get(&url).query(query);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }

    let response = tokio::time::timeout(timeout, request.send())
        .await
        .with_context(|| format!("bluesky `{method}` read timed out"))?
        .with_context(|| format!("bluesky `{method}` read failed"))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("reading the bluesky response")?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        bail!("bluesky `{method}` returned HTTP {status}: {snippet}");
    }
    Ok(body)
}

/// Read mentions and replies addressed to us (authenticated).
pub async fn list_notifications(
    base_url: &str,
    session: &Session,
    reasons: &[String],
    limit: usize,
    timeout: Duration,
) -> Result<Vec<Notification>> {
    let mut query: Vec<(String, String)> = reasons
        .iter()
        .map(|reason| ("reasons".to_string(), reason.clone()))
        .collect();
    query.push(("limit".to_string(), limit.to_string()));

    let raw = get_text(
        base_url,
        LIST_NOTIFICATIONS,
        &query,
        Some(&session.access_jwt),
        timeout,
    )
    .await?;
    parse_notifications(&raw)
}

/// Read a post's thread (public); the replies are comments on that post.
pub async fn get_post_thread(
    base_url: &str,
    uri: &str,
    depth: u32,
    timeout: Duration,
) -> Result<Thread> {
    let query = vec![
        ("uri".to_string(), uri.to_string()),
        ("depth".to_string(), depth.to_string()),
    ];
    let raw = get_text(base_url, GET_POST_THREAD, &query, None, timeout).await?;
    parse_thread(&raw)
}

/// Read our own recent posts (public), to look for comments on them.
pub async fn get_author_feed(
    base_url: &str,
    actor: &str,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<OwnPost>> {
    let query = vec![
        ("actor".to_string(), actor.to_string()),
        ("limit".to_string(), limit.to_string()),
    ];
    let raw = get_text(base_url, GET_AUTHOR_FEED, &query, None, timeout).await?;
    parse_author_feed(&raw)
}

/// Resolve an account of interest to a profile (public).
pub async fn get_profile(base_url: &str, actor: &str, timeout: Duration) -> Result<Profile> {
    let query = vec![("actor".to_string(), actor.to_string())];
    let raw = get_text(base_url, GET_PROFILE, &query, None, timeout).await?;
    parse_profile(&raw)
}

/// Post a reply to `parent`, anchored at the thread `root`.
pub async fn create_reply(
    base_url: &str,
    session: &Session,
    text: &str,
    root: &StrongRef,
    parent: &StrongRef,
    timeout: Duration,
) -> Result<String> {
    let record = build_reply_record(text, &now_rfc3339_millis(), root, parent);
    create_record(base_url, session, POST_COLLECTION, &record, timeout).await
}

/// Like a post (only ever the parent of a reply we are making).
pub async fn create_like(
    base_url: &str,
    session: &Session,
    subject: &StrongRef,
    timeout: Duration,
) -> Result<String> {
    let record = build_like_record(subject, &now_rfc3339_millis());
    create_record(base_url, session, LIKE_COLLECTION, &record, timeout).await
}

/// Follow an account (slowly; there is no unfollow counterpart).
pub async fn create_follow(
    base_url: &str,
    session: &Session,
    subject_did: &str,
    timeout: Duration,
) -> Result<String> {
    let record = build_follow_record(subject_did, &now_rfc3339_millis());
    create_record(base_url, session, FOLLOW_COLLECTION, &record, timeout).await
}

#[cfg(test)]
mod tests {
    use axum::Json;
    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use serde_json::json;

    use super::*;

    async fn spawn(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock bluesky");
        let addr = listener.local_addr().expect("mock bluesky address");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve mock bluesky");
        });
        format!("http://{addr}")
    }

    fn session() -> Session {
        Session {
            did: "did:plc:our".to_string(),
            handle: "example.bsky.social".to_string(),
            access_jwt: "test-jwt".to_string(),
        }
    }

    async fn notifications(headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
        if headers.get("authorization").is_none() {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(Json(json!({
            "notifications": [{
                "uri": "at://did:plc:them/app.bsky.feed.post/1",
                "cid": "cid-1",
                "reason": "mention",
                "author": {"did": "did:plc:them", "handle": "them.bsky.social"},
                "record": {"text": "oi"}
            }]
        })))
    }

    async fn create_record(
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        if headers.get("authorization").is_none() {
            return Err(StatusCode::UNAUTHORIZED);
        }
        if body["collection"] != "app.bsky.graph.follow" {
            return Err(StatusCode::BAD_REQUEST);
        }
        Ok(Json(
            json!({"uri": "at://did:plc:our/app.bsky.graph.follow/1"}),
        ))
    }

    #[tokio::test]
    async fn list_notifications_sends_bearer_and_parses() {
        let base = spawn(Router::new().route(
            "/xrpc/app.bsky.notification.listNotifications",
            get(notifications),
        ))
        .await;

        let reasons = vec!["mention".to_string()];
        let found = list_notifications(&base, &session(), &reasons, 10, DEFAULT_ENGAGE_TIMEOUT)
            .await
            .expect("notifications read");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].author.handle, "them.bsky.social");
    }

    #[tokio::test]
    async fn create_follow_writes_the_follow_record() {
        let base =
            spawn(Router::new().route("/xrpc/com.atproto.repo.createRecord", post(create_record)))
                .await;

        let uri = create_follow(&base, &session(), "did:plc:gnome", DEFAULT_ENGAGE_TIMEOUT)
            .await
            .expect("follow write");
        assert!(uri.contains("app.bsky.graph.follow"));
    }

    #[tokio::test]
    async fn a_public_read_reports_http_errors() {
        async fn boom() -> StatusCode {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        let base = spawn(Router::new().route("/xrpc/app.bsky.feed.getPostThread", get(boom))).await;

        let err = get_post_thread(&base, "at://x", 2, DEFAULT_ENGAGE_TIMEOUT)
            .await
            .expect_err("HTTP 500 must fail");
        assert!(err.to_string().contains("500"), "unexpected error: {err}");
    }
}
