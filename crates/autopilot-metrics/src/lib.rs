//! Stage [5] — metrics collection and local persistence.
//!
//! One trait ([`MetricsSource`]) with one adapter per platform (Bluesky first)
//! and a single shared, effectful driver. Samples are appended to a JSONL store
//! under `var/` so the learning stage reads them back with no database.

pub mod bluesky;
pub mod source;
pub mod store;

pub use bluesky::{BlueskyMetrics, DEFAULT_BLUESKY_READ_URL, parse_post_views};
pub use source::{DEFAULT_METRICS_TIMEOUT, MetricsRequest, MetricsSource, PostMetrics, collect};
pub use store::{MetricsRecord, RecordContext, append_metrics, read_metrics};
