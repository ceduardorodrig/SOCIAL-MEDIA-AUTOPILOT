//! Shared test fixtures for the learning stage.

use autopilot_core::Platform;
use autopilot_metrics::{MetricsRecord, PostMetrics, RecordContext};

/// Build a Bluesky metrics record with the given engagement counters.
pub fn record(variant: &str, likes: u64, replies: u64, reposts: u64, quotes: u64) -> MetricsRecord {
    MetricsRecord::from_post(
        Platform::Bluesky,
        &PostMetrics {
            uri: format!("at://did:plc:abc/app.bsky.feed.post/{variant}"),
            text: format!("corpo {variant}"),
            likes,
            replies,
            reposts,
            quotes,
        },
        RecordContext {
            captured_at_unix: 1_700_000_000,
            persona: "ciencia".to_string(),
            variant: variant.to_string(),
            topic: "cérebro".to_string(),
            published_at_unix: None,
        },
    )
}

/// Same as [`record`] but pinned to a UTC hour (used by schedule tests).
pub fn record_at(
    variant: &str,
    hour_utc: u8,
    likes: u64,
    replies: u64,
    reposts: u64,
    quotes: u64,
) -> MetricsRecord {
    let mut record = record(variant, likes, replies, reposts, quotes);
    // 2024-01-01T00:00:00Z is a multiple of 86400; add the hour offset.
    record.published_at_unix = Some(1_704_067_200 + u64::from(hour_utc) * 3_600);
    record
}

/// Same as [`record`] but with an explicit post body (used by craft tests).
pub fn record_text(
    variant: &str,
    text: &str,
    likes: u64,
    replies: u64,
    reposts: u64,
    quotes: u64,
) -> MetricsRecord {
    let mut record = record(variant, likes, replies, reposts, quotes);
    record.text = text.to_string();
    record
}
