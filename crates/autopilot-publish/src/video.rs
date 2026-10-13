//! Bluesky video publisher: the `app.bsky.video` side-car.
//!
//! An image travels through the PDS as a `com.atproto.repo.uploadBlob` and is
//! referenced from an `embed`. A video does **not**: it is uploaded to the
//! **video processing service** (`video.bsky.app`) with
//! `app.bsky.video.uploadVideo`, which returns a processing **job**; the client
//! then polls `app.bsky.video.getJobStatus` until the job completes, and only the
//! blob the completed job returns may be embedded via `app.bsky.embed.video`.
//!
//! The Lexicon facts this module encodes (`app.bsky.embed.video`):
//! * `accept: ["video/mp4"]` — MP4 only;
//! * `maxSize: 300000000` (300 MB);
//! * `alt` is **optional** (`required: ["video"]`) — unlike
//!   `app.bsky.embed.images#image`, where it is required;
//! * `aspectRatio {width, height}`; and
//! * there is **no duration** in the Lexicon (the 10-minute cap is enforced
//!   server-side).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::bluesky::{BlobRef, Session, api_error_message};

/// Hosted Bluesky video processing service (the `video.bsky.app` side-car).
pub const DEFAULT_VIDEO_BASE_URL: &str = "https://video.bsky.app";

/// `app.bsky.video.uploadVideo` procedure path.
pub const UPLOAD_VIDEO_PATH: &str = "/xrpc/app.bsky.video.uploadVideo";

/// `app.bsky.video.getJobStatus` query path.
pub const GET_JOB_STATUS_PATH: &str = "/xrpc/app.bsky.video.getJobStatus";

/// The only accepted video MIME type (`accept: ["video/mp4"]`).
pub const VIDEO_MIME_TYPE: &str = "video/mp4";

/// `app.bsky.embed.video#video` `maxSize` (300 MB).
pub const MAX_VIDEO_BYTES: usize = 300_000_000;

/// How long to wait between two `getJobStatus` polls.
pub const DEFAULT_VIDEO_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// A finished video ready to be embedded, with its (optional) alt text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostVideo {
    /// Alt text. Optional for video, unlike an image, but set when known.
    pub alt: Option<String>,
    pub blob: BlobRef,
    pub width: u32,
    pub height: u32,
    /// Optional presentation hint (`default` or `gif`).
    pub presentation: Option<String>,
}

/// The state of a video processing job (`app.bsky.video.defs#jobStatus`).
///
/// Only `JOB_STATE_COMPLETED` yields a blob; every other known state means the
/// job is still in process, and `JOB_STATE_FAILED` is terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoJobStatus {
    #[serde(rename = "jobId")]
    pub job_id: String,
    pub did: String,
    pub state: String,
    #[serde(default)]
    pub progress: Option<u32>,
    /// The uploaded blob, present once the job completes.
    #[serde(default)]
    pub blob: Option<BlobRef>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(rename = "failureCode", default)]
    pub failure_code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl VideoJobStatus {
    /// Whether the job finished and its blob may be embedded.
    pub fn is_completed(&self) -> bool {
        self.state == "JOB_STATE_COMPLETED"
    }

    /// Whether the job failed permanently.
    pub fn is_failed(&self) -> bool {
        self.state == "JOB_STATE_FAILED"
    }

    /// A one-line reason for a failed job, for the journal.
    pub fn failure_reason(&self) -> String {
        let code = self.failure_code.as_deref().unwrap_or("unknown");
        let message = self
            .message
            .as_deref()
            .or(self.error.as_deref())
            .unwrap_or("no detail");
        format!("{code}: {message}")
    }
}

/// Reject a video the Bluesky Lexicon would refuse, before spending the upload.
///
/// Checks the two blob constraints of `app.bsky.embed.video#video`: the declared
/// type must be `video/mp4` and the size must be at most [`MAX_VIDEO_BYTES`].
pub fn validate_video(len: usize, mime_type: &str) -> Result<()> {
    if len == 0 {
        bail!("refusing to upload an empty video");
    }
    if len > MAX_VIDEO_BYTES {
        bail!(
            "video is {len} bytes; the Bluesky video limit is {MAX_VIDEO_BYTES} bytes \
             (app.bsky.embed.video#video maxSize)"
        );
    }
    let mime = mime_type.trim();
    if mime != VIDEO_MIME_TYPE {
        bail!("`{mime_type}` is not accepted; app.bsky.embed.video accepts video/mp4 only");
    }
    Ok(())
}

/// Build an `app.bsky.embed.video` embed from an uploaded video.
///
/// `alt` is included only when non-empty: the Lexicon marks it optional (unlike
/// an image embed, where it is required), so a missing description is legal.
pub fn build_video_embed(video: &PostVideo) -> Result<serde_json::Value> {
    if video.width == 0 || video.height == 0 {
        bail!("a video aspect ratio needs positive width and height");
    }

    let mut embed = serde_json::json!({
        "$type": "app.bsky.embed.video",
        "video": video.blob,
        "aspectRatio": { "width": video.width, "height": video.height },
    });
    if let Some(alt) = video
        .alt
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        embed["alt"] = serde_json::Value::String(alt.to_string());
    }
    if let Some(kind) = video
        .presentation
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        embed["presentation"] = serde_json::Value::String(kind.to_string());
    }
    Ok(embed)
}

/// Upload a video to the processing service, returning its initial job status.
///
/// `app.bsky.video.uploadVideo` is a procedure with a `video/mp4` body; the
/// response carries the `jobStatus` to poll with [`get_job_status`].
pub async fn upload_video(
    base_url: &str,
    session: &Session,
    data: &[u8],
    timeout: Duration,
) -> Result<VideoJobStatus> {
    validate_video(data.len(), VIDEO_MIME_TYPE)?;

    let url = format!("{}{UPLOAD_VIDEO_PATH}", base_url.trim_end_matches('/'));
    let response = tokio::time::timeout(
        timeout,
        reqwest::Client::new()
            .post(&url)
            .bearer_auth(&session.access_jwt)
            .header(reqwest::header::CONTENT_TYPE, VIDEO_MIME_TYPE)
            .body(data.to_vec())
            .send(),
    )
    .await
    .context("bluesky video upload timed out")?
    .context("bluesky video upload failed")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.context("invalid bluesky JSON")?;
    decode_job_response(status, &payload)
}

/// Fetch the status of a video processing job by id.
pub async fn get_job_status(
    base_url: &str,
    session: &Session,
    job_id: &str,
    timeout: Duration,
) -> Result<VideoJobStatus> {
    let url = format!("{}{GET_JOB_STATUS_PATH}", base_url.trim_end_matches('/'));
    let response = tokio::time::timeout(
        timeout,
        reqwest::Client::new()
            .get(&url)
            .bearer_auth(&session.access_jwt)
            .query(&[("jobId", job_id)])
            .send(),
    )
    .await
    .context("bluesky video job poll timed out")?
    .context("bluesky video job poll failed")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.context("invalid bluesky JSON")?;
    decode_job_response(status, &payload)
}

/// Poll a job until it completes or fails, or the overall wait elapses.
///
/// The upload response is often only `JOB_STATE_CREATED`: processing is
/// asynchronous, so the caller must wait for `JOB_STATE_COMPLETED` before the
/// blob exists. A failed job stops immediately with its reason.
pub async fn await_video_job(
    base_url: &str,
    session: &Session,
    initial: VideoJobStatus,
    request_timeout: Duration,
    poll_interval: Duration,
    total_timeout: Duration,
) -> Result<VideoJobStatus> {
    let deadline = tokio::time::Instant::now() + total_timeout;
    let mut job = initial;
    loop {
        if job.is_completed() {
            return Ok(job);
        }
        if job.is_failed() {
            bail!(
                "bluesky video job {} failed: {}",
                job.job_id,
                job.failure_reason()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "bluesky video job {} still `{}` after {}s",
                job.job_id,
                job.state,
                total_timeout.as_secs()
            );
        }
        tokio::time::sleep(poll_interval).await;
        job = get_job_status(base_url, session, &job.job_id, request_timeout).await?;
    }
}

/// Check an HTTP status and extract the `jobStatus` from the response payload.
fn decode_job_response(
    status: reqwest::StatusCode,
    payload: &serde_json::Value,
) -> Result<VideoJobStatus> {
    if !status.is_success() {
        bail!(
            "bluesky video service returned HTTP {status}: {}",
            api_error_message(payload)
        );
    }
    let job = payload
        .get("jobStatus")
        .cloned()
        .context("bluesky did not return a jobStatus")?;
    serde_json::from_value(job).context("unexpected bluesky jobStatus payload")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluesky::BlobLink;
    use axum::Json;
    use axum::Router;
    use axum::extract::Query;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn session() -> Session {
        Session {
            did: "did:plc:abc123".into(),
            handle: "demo.bsky.social".into(),
            access_jwt: "jwt-token".into(),
        }
    }

    fn blob(cid: &str) -> BlobRef {
        BlobRef {
            kind: "blob".into(),
            reference: BlobLink { link: cid.into() },
            mime_type: "video/mp4".into(),
            size: 4_500_000,
        }
    }

    fn status_payload(state: &str, with_blob: bool) -> serde_json::Value {
        let mut job = json!({
            "jobId": "job-1",
            "did": "did:plc:abc123",
            "state": state,
            "progress": 50,
        });
        if with_blob {
            job["blob"] = json!({
                "$type": "blob",
                "ref": { "$link": "bafkreivideo" },
                "mimeType": "video/mp4",
                "size": 4500000,
            });
        }
        json!({ "jobStatus": job })
    }

    /// Serve a mock video service that reports `ENCODING` first and
    /// `COMPLETED` (with a blob) afterwards, counting the polls.
    async fn spawn_video_service() -> (String, Arc<AtomicUsize>) {
        let polls = Arc::new(AtomicUsize::new(0));
        let counter = polls.clone();
        let app = Router::new()
            .route(
                UPLOAD_VIDEO_PATH,
                post(|| async { Json(status_payload("JOB_STATE_ENCODING", false)) }),
            )
            .route(
                GET_JOB_STATUS_PATH,
                get(move |Query(params): Query<HashMap<String, String>>| {
                    let counter = counter.clone();
                    async move {
                        assert_eq!(params.get("jobId").map(String::as_str), Some("job-1"));
                        let seen = counter.fetch_add(1, Ordering::SeqCst);
                        if seen == 0 {
                            Json(status_payload("JOB_STATE_ENCODING", false))
                        } else {
                            Json(status_payload("JOB_STATE_COMPLETED", true))
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock video service");
        let addr = listener.local_addr().expect("mock address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock");
        });
        (format!("http://{addr}"), polls)
    }

    #[test]
    fn video_validation_matches_the_lexicon_limits() {
        assert!(validate_video(1, "video/mp4").is_ok());
        assert!(validate_video(MAX_VIDEO_BYTES, "video/mp4").is_ok());
        assert!(validate_video(MAX_VIDEO_BYTES + 1, "video/mp4").is_err());
        assert!(validate_video(0, "video/mp4").is_err());
        // Only MP4 is accepted; an image MIME is refused.
        assert!(validate_video(10, "image/png").is_err());
        assert!(validate_video(10, "  ").is_err());
    }

    #[test]
    fn a_video_embed_carries_the_blob_and_aspect_ratio() {
        let embed = build_video_embed(&PostVideo {
            alt: Some("a short narrated demo".into()),
            blob: blob("bafkreivideo"),
            width: 1080,
            height: 1920,
            presentation: None,
        })
        .expect("embed builds");

        assert_eq!(embed["$type"], "app.bsky.embed.video");
        assert_eq!(embed["video"]["mimeType"], "video/mp4");
        assert_eq!(embed["video"]["ref"]["$link"], "bafkreivideo");
        assert_eq!(embed["aspectRatio"]["width"], 1080);
        assert_eq!(embed["aspectRatio"]["height"], 1920);
        assert_eq!(embed["alt"], "a short narrated demo");
        assert!(embed.get("presentation").is_none());
    }

    #[test]
    fn alt_is_optional_for_a_video_but_required_for_an_image() {
        // The Lexicon marks `alt` optional for `embed.video` (required only for
        // `embed.images`), so an empty alt is simply omitted, never an error.
        for alt in [None, Some("".to_string()), Some("   ".to_string())] {
            let embed = build_video_embed(&PostVideo {
                alt,
                blob: blob("bafkrei"),
                width: 1080,
                height: 1920,
                presentation: None,
            })
            .expect("a video embed builds without alt");
            assert!(embed.get("alt").is_none());
        }
    }

    #[test]
    fn a_video_embed_needs_positive_dimensions() {
        let video = PostVideo {
            alt: None,
            blob: blob("bafkrei"),
            width: 0,
            height: 1920,
            presentation: None,
        };
        assert!(build_video_embed(&video).is_err());
    }

    #[test]
    fn a_presentation_hint_is_carried_when_set() {
        let embed = build_video_embed(&PostVideo {
            alt: None,
            blob: blob("bafkrei"),
            width: 1080,
            height: 1920,
            presentation: Some("gif".into()),
        })
        .expect("embed builds");
        assert_eq!(embed["presentation"], "gif");
    }

    #[test]
    fn a_job_status_deserializes_from_the_lexicon_shape() {
        let payload = status_payload("JOB_STATE_COMPLETED", true);
        let job: VideoJobStatus =
            serde_json::from_value(payload["jobStatus"].clone()).expect("job shape");
        assert_eq!(job.job_id, "job-1");
        assert_eq!(job.did, "did:plc:abc123");
        assert!(job.is_completed());
        assert!(!job.is_failed());
        assert_eq!(
            job.blob.as_ref().map(|b| b.mime_type.as_str()),
            Some("video/mp4")
        );
    }

    #[test]
    fn a_failed_job_reports_its_code_and_message() {
        let payload = json!({
            "jobId": "job-9",
            "did": "did:plc:abc",
            "state": "JOB_STATE_FAILED",
            "failureCode": "encoding_failure",
            "message": "could not decode",
        });
        let job: VideoJobStatus = serde_json::from_value(payload).expect("job shape");
        assert!(job.is_failed());
        assert!(job.failure_reason().contains("encoding_failure"));
        assert!(job.failure_reason().contains("could not decode"));
    }

    #[tokio::test]
    async fn upload_and_poll_until_the_job_completes() {
        let (base, polls) = spawn_video_service().await;
        let bytes = vec![0u8; 1024];

        let job = upload_video(&base, &session(), &bytes, Duration::from_secs(5))
            .await
            .expect("upload succeeds");
        assert_eq!(job.state, "JOB_STATE_ENCODING");

        let done = await_video_job(
            &base,
            &session(),
            job,
            Duration::from_secs(5),
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .await
        .expect("the job completes");
        assert!(done.is_completed());
        assert_eq!(
            done.blob.as_ref().map(|b| b.reference.link.as_str()),
            Some("bafkreivideo")
        );
        assert!(
            polls.load(Ordering::SeqCst) >= 2,
            "the client must poll until the job leaves ENCODING"
        );

        // The completed blob is exactly what the embed references.
        let embed = build_video_embed(&PostVideo {
            alt: Some("demo".into()),
            blob: done.blob.clone().expect("blob present"),
            width: 1080,
            height: 1920,
            presentation: None,
        })
        .expect("embed builds");
        assert_eq!(embed["video"]["ref"]["$link"], "bafkreivideo");
    }

    #[tokio::test]
    async fn an_upload_error_is_reported() {
        let app = Router::new().route(
            UPLOAD_VIDEO_PATH,
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"message": "boom"})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let err = upload_video(
            &format!("http://{addr}"),
            &session(),
            &[1, 2, 3],
            Duration::from_secs(5),
        )
        .await
        .expect_err("HTTP 500 must fail");
        assert!(err.to_string().contains("500"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn a_refused_upload_never_reaches_the_network() {
        // An oversize payload is rejected before any request is made.
        let huge = vec![0u8; MAX_VIDEO_BYTES + 1];
        let err = upload_video(
            "http://127.0.0.1:9",
            &session(),
            &huge,
            Duration::from_millis(200),
        )
        .await
        .expect_err("an oversize video is refused");
        assert!(err.to_string().contains("limit"), "unexpected error: {err}");
    }
}
