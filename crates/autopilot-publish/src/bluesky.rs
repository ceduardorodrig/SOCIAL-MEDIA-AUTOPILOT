//! Bluesky (AT Protocol) publisher.
//!
//! Credentials come from the environment (never from the repository):
//! `BLUESKY_HANDLE` and `BLUESKY_APP_PASSWORD` (an app password, not the account
//! password).
//!
//! Text-only posts and image posts share the same `app.bsky.feed.post` record:
//! an image is first uploaded as a blob with `com.atproto.repo.uploadBlob` and
//! then referenced from an `app.bsky.embed.images` embed. Alt text is mandatory —
//! the Lexicon marks `alt` as required and it is Bluesky's accessibility rule.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Hosted Bluesky entry point.
pub const DEFAULT_BLUESKY_BASE_URL: &str = "https://bsky.social";

const COLLECTION: &str = "app.bsky.feed.post";

/// `app.bsky.embed.images` Lexicon id.
const EMBED_IMAGES: &str = "app.bsky.embed.images";

/// Blob upload endpoint (`com.atproto.repo.uploadBlob`).
pub const UPLOAD_BLOB_PATH: &str = "/xrpc/com.atproto.repo.uploadBlob";

/// Maximum size of one image blob.
///
/// From `app.bsky.embed.images#image`: `"accept": ["image/*"], "maxSize": 2000000`
/// (2 MB — the earlier 1 MB limit was raised; the AT Protocol docs still mention
/// the old value in places, the Lexicon is authoritative).
pub const MAX_IMAGE_BYTES: usize = 2_000_000;

/// Maximum images in one embed: `app.bsky.embed.images` `images.maxLength` is 4.
pub const MAX_IMAGES_PER_POST: usize = 4;

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

/// Content reference returned by `com.atproto.repo.uploadBlob`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    #[serde(rename = "$type")]
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: BlobLink,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub size: u64,
}

/// The `{"$link": "<cid>"}` inside a [`BlobRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobLink {
    #[serde(rename = "$link")]
    pub link: String,
}

/// An uploaded image ready to be embedded, with its mandatory alt text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostImage {
    pub alt: String,
    pub blob: BlobRef,
    pub width: u32,
    pub height: u32,
}

/// Reject an image the Bluesky Lexicon would refuse, before spending the upload.
///
/// Checks the two blob constraints of `app.bsky.embed.images#image`: the declared
/// type must match `image/*` and the size must be at most [`MAX_IMAGE_BYTES`].
pub fn validate_image(len: usize, mime_type: &str) -> Result<()> {
    if len == 0 {
        bail!("refusing to upload an empty image");
    }
    if len > MAX_IMAGE_BYTES {
        bail!(
            "image is {len} bytes; the Bluesky embed limit is {MAX_IMAGE_BYTES} bytes \
             (app.bsky.embed.images#image maxSize)"
        );
    }
    let mime = mime_type.trim();
    if mime.is_empty() || !mime.starts_with("image/") {
        bail!("`{mime_type}` is not an image MIME type; app.bsky.embed.images accepts image/*");
    }
    Ok(())
}

/// Alt text is a required field of `app.bsky.embed.images#image` and Bluesky's
/// accessibility rule: an image is never posted without it.
pub fn validate_alt(alt: &str) -> Result<()> {
    if alt.trim().is_empty() {
        bail!("an embedded image must carry alt text (app.bsky.embed.images requires `alt`)");
    }
    Ok(())
}

/// Build an `app.bsky.embed.images` embed from uploaded images.
pub fn build_image_embed(images: &[PostImage]) -> Result<serde_json::Value> {
    if images.is_empty() {
        bail!("an image embed needs at least one image");
    }
    if images.len() > MAX_IMAGES_PER_POST {
        bail!(
            "{} images given; a Bluesky post carries at most {MAX_IMAGES_PER_POST}",
            images.len()
        );
    }

    let mut encoded = Vec::with_capacity(images.len());
    for image in images {
        validate_alt(&image.alt)?;
        if image.width == 0 || image.height == 0 {
            bail!("an aspect ratio needs positive width and height");
        }
        encoded.push(serde_json::json!({
            "alt": image.alt,
            "image": image.blob,
            "aspectRatio": { "width": image.width, "height": image.height },
        }));
    }

    Ok(serde_json::json!({
        "$type": EMBED_IMAGES,
        "images": encoded,
    }))
}

/// Build a post record, attaching an image embed when images are given.
///
/// With an empty slice this is exactly [`build_post_record`], so the text-only
/// path is untouched.
pub fn build_post_record_with_images(
    text: &str,
    created_at: &str,
    images: &[PostImage],
) -> Result<serde_json::Value> {
    let mut record = build_post_record(text, created_at);
    if !images.is_empty() {
        record["embed"] = build_image_embed(images)?;
    }
    Ok(record)
}

/// Extract the message from an AT Protocol error payload.
fn api_error_message(payload: &serde_json::Value) -> &str {
    payload
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown error")
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
        bail!(
            "bluesky authentication failed (HTTP {status}): {}",
            api_error_message(&payload)
        );
    }

    serde_json::from_value(payload).context("unexpected bluesky session payload")
}

/// Upload an image blob, returning the reference to embed in a post record.
///
/// `com.atproto.repo.uploadBlob` is called with the declared `Content-Type`
/// (the AT Protocol asks clients to set it) and the raw bytes; the response's
/// `blob` object is what a post record references.
pub async fn upload_blob(
    base_url: &str,
    session: &Session,
    data: &[u8],
    mime_type: &str,
    timeout: Duration,
) -> Result<BlobRef> {
    validate_image(data.len(), mime_type)?;

    let url = format!("{base_url}{UPLOAD_BLOB_PATH}");
    let response = tokio::time::timeout(
        timeout,
        reqwest::Client::new()
            .post(&url)
            .bearer_auth(&session.access_jwt)
            .header(reqwest::header::CONTENT_TYPE, mime_type.trim())
            .body(data.to_vec())
            .send(),
    )
    .await
    .context("bluesky blob upload timed out")?
    .context("bluesky blob upload failed")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.context("invalid bluesky JSON")?;
    if !status.is_success() {
        bail!(
            "bluesky rejected the blob (HTTP {status}): {}",
            api_error_message(&payload)
        );
    }

    let blob = payload
        .get("blob")
        .cloned()
        .context("bluesky did not return a blob reference")?;
    serde_json::from_value(blob).context("unexpected bluesky blob payload")
}

/// Publish a post record, returning its AT-URI.
pub async fn create_post(
    base_url: &str,
    session: &Session,
    record: &serde_json::Value,
    timeout: Duration,
) -> Result<String> {
    create_record(base_url, session, COLLECTION, record, timeout).await
}

/// Create any `app.bsky.*` record in the session's repo, returning its AT-URI.
///
/// The publication stage (posts) and the engagement stage (replies, likes,
/// follows) share this single XRPC transport, so no adapter duplicates it.
pub async fn create_record(
    base_url: &str,
    session: &Session,
    collection: &str,
    record: &serde_json::Value,
    timeout: Duration,
) -> Result<String> {
    let url = format!("{base_url}/xrpc/com.atproto.repo.createRecord");
    let body = serde_json::json!({
        "repo": session.did,
        "collection": collection,
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
    .context("bluesky write request timed out")?
    .context("bluesky write request failed")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.context("invalid bluesky JSON")?;
    if !status.is_success() {
        bail!(
            "bluesky rejected the `{collection}` write (HTTP {status}): {}",
            api_error_message(&payload)
        );
    }

    payload
        .get("uri")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .context("bluesky did not return a record uri")
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

    fn blob(cid: &str) -> BlobRef {
        BlobRef {
            kind: "blob".into(),
            reference: BlobLink { link: cid.into() },
            mime_type: "image/png".into(),
            size: 1234,
        }
    }

    fn image(alt: &str) -> PostImage {
        PostImage {
            alt: alt.into(),
            blob: blob("bafkrei"),
            width: 1200,
            height: 1200,
        }
    }

    #[test]
    fn text_only_record_has_no_embed() {
        let record = build_post_record("só texto", "2026-10-12T20:00:00.000Z");
        assert!(record.get("embed").is_none());
        let with_empty = build_post_record_with_images("só texto", "2026-10-12T20:00:00.000Z", &[])
            .expect("empty images keeps the text-only shape");
        assert_eq!(with_empty, record);
    }

    #[test]
    fn image_record_carries_the_images_embed_and_alt() {
        let record = build_post_record_with_images(
            "com card",
            "2026-10-12T20:00:00.000Z",
            &[image("um card de terminal")],
        )
        .expect("embed builds");

        assert_eq!(record["embed"]["$type"], "app.bsky.embed.images");
        let images = &record["embed"]["images"];
        assert_eq!(images.as_array().map(Vec::len), Some(1));
        assert_eq!(images[0]["alt"], "um card de terminal");
        assert_eq!(images[0]["image"]["mimeType"], "image/png");
        assert_eq!(images[0]["image"]["ref"]["$link"], "bafkrei");
        assert_eq!(images[0]["aspectRatio"]["width"], 1200);
        assert_eq!(images[0]["aspectRatio"]["height"], 1200);
    }

    #[test]
    fn an_embed_without_alt_is_refused() {
        assert!(build_image_embed(&[image("  ")]).is_err());
        assert!(validate_alt("").is_err());
        assert!(validate_alt("ok").is_ok());
    }

    #[test]
    fn an_embed_without_images_is_refused() {
        assert!(build_image_embed(&[]).is_err());
    }

    #[test]
    fn an_embed_with_more_than_four_images_is_refused() {
        let images = vec![image("a"), image("b"), image("c"), image("d"), image("e")];
        assert!(build_image_embed(&images).is_err());
        assert!(build_image_embed(&images[..MAX_IMAGES_PER_POST]).is_ok());
    }

    #[test]
    fn an_embed_needs_positive_dimensions() {
        let mut broken = image("a");
        broken.width = 0;
        assert!(build_image_embed(&[broken]).is_err());
    }

    #[test]
    fn image_validation_matches_the_lexicon_limits() {
        assert!(validate_image(1, "image/png").is_ok());
        assert!(validate_image(MAX_IMAGE_BYTES, "image/png").is_ok());
        assert!(validate_image(MAX_IMAGE_BYTES + 1, "image/png").is_err());
        assert!(validate_image(0, "image/png").is_err());
        assert!(validate_image(10, "application/pdf").is_err());
        assert!(validate_image(10, "  ").is_err());
    }

    #[test]
    fn blob_ref_deserializes_from_the_upload_response_shape() {
        let payload = serde_json::json!({
            "blob": {
                "$type": "blob",
                "ref": { "$link": "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku" },
                "mimeType": "image/png",
                "size": 760898,
            }
        });
        let blob: BlobRef = serde_json::from_value(payload["blob"].clone()).expect("blob shape");
        assert_eq!(blob.kind, "blob");
        assert_eq!(blob.mime_type, "image/png");
        assert_eq!(blob.size, 760898);
        assert!(blob.reference.link.starts_with("bafkrei"));
    }
}
