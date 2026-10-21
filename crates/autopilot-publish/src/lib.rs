//! Stage [4] — publication. One adapter per platform.
//!
//! Bluesky is the first adapter: an open API with no app review, which makes it
//! the shortest path to closing the loop. X (Twitter) follows on the same shape,
//! signing its requests with OAuth 1.0a.

use anyhow::{Context, Result, bail};

pub mod bluesky;
pub mod card;
pub mod video;
pub mod x;

pub use bluesky::{
    BlobLink, BlobRef, BlueskyCredentials, DEFAULT_BLUESKY_BASE_URL, MAX_IMAGE_BYTES,
    MAX_IMAGES_PER_POST, PostImage, Session, build_image_embed, build_post_record,
    build_post_record_with_embed, build_post_record_with_images, create_post, create_record,
    create_session, create_session_for_channel, now_rfc3339_millis, post_url, upload_blob,
    validate_alt, validate_image, validate_session_handle,
};
pub use card::{
    CARD_HEIGHT, CARD_MIME_TYPE, CARD_TEMPLATE_VERSION, CARD_TEMPLATES, CARD_WIDTH, CardIdentity,
    CardSpec, CardTemplate, DEFAULT_RSVG_BINARY, alt_text, default_template, render_png,
    render_png_for, render_png_in_pool, render_png_with, render_svg, render_svg_for,
    render_svg_in_pool, render_svg_with, resolve_pool, template_for, template_for_identity_in_pool,
    template_for_key, template_for_spec, template_for_spec_in_pool, template_index, template_names,
};
// X shares the `create_post` / `post_url` names with Bluesky, so those two stay
// module-qualified (`x::create_post`); the rest are re-exported for symmetry.
pub use video::{
    DEFAULT_VIDEO_BASE_URL, DEFAULT_VIDEO_POLL_INTERVAL, GET_JOB_STATUS_PATH, MAX_VIDEO_BYTES,
    PostVideo, UPLOAD_VIDEO_PATH, VIDEO_MIME_TYPE, VideoJobStatus, await_video_job,
    build_video_embed, get_job_status, upload_video, validate_video,
};
pub use x::{
    DEFAULT_X_API_BASE_URL, PublishedPost, X_ENV_VARS, XCredentials, authorization_header,
    build_tweet_body, missing_env_vars, missing_env_vars_for, post_endpoint,
};

/// Read a required, non-empty environment variable.
pub(crate) fn env_var(name: &str) -> Result<String> {
    let value =
        std::env::var(name).with_context(|| format!("missing environment variable {name}"))?;
    if value.trim().is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(value)
}
