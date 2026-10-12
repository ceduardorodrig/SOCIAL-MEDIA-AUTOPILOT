//! Stage [4] — publication. One adapter per platform.
//!
//! Bluesky is the first adapter: an open API with no app review, which makes it
//! the shortest path to closing the loop. X (Twitter) follows on the same shape,
//! signing its requests with OAuth 1.0a.

use anyhow::{Context, Result, bail};

pub mod bluesky;
pub mod x;

pub use bluesky::{
    BlueskyCredentials, DEFAULT_BLUESKY_BASE_URL, Session, build_post_record, create_post,
    create_session, now_rfc3339_millis, post_url,
};
// X shares the `create_post` / `post_url` names with Bluesky, so those two stay
// module-qualified (`x::create_post`); the rest are re-exported for symmetry.
pub use x::{
    DEFAULT_X_API_BASE_URL, PublishedPost, X_ENV_VARS, XCredentials, authorization_header,
    build_tweet_body, missing_env_vars, post_endpoint,
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
