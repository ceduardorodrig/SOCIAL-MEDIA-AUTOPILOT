//! Stage [4] — publication. One adapter per platform.
//!
//! Bluesky is the first adapter: an open API with no app review, which makes it
//! the shortest path to closing the loop.

pub mod bluesky;

pub use bluesky::{
    BlueskyCredentials, DEFAULT_BLUESKY_BASE_URL, Session, build_post_record, create_post,
    create_session, now_rfc3339_millis, post_url,
};
