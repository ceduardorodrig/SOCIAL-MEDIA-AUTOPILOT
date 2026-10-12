//! Local JSONL persistence for metrics samples.
//!
//! The store is intentionally dependency-light: one JSON object per line under
//! `var/` (gitignored), so the learning stage can replay history and the file
//! stays human-readable.

use std::path::Path;

use anyhow::{Context, Result};
use autopilot_core::{Platform, store};
use serde::{Deserialize, Serialize};

use crate::source::PostMetrics;

/// The A/B context that ties a metrics sample to its experiment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordContext {
    /// When the sample was collected (Unix seconds).
    pub captured_at_unix: u64,
    /// Persona slug that produced the post.
    pub persona: String,
    /// Variant label from the creation stage (`A`, `B`, ...).
    pub variant: String,
    /// Topic the post was written about.
    pub topic: String,
    /// When the post was published (Unix seconds), when known.
    pub published_at_unix: Option<u64>,
}

/// One persisted metrics sample for a published post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsRecord {
    pub captured_at_unix: u64,
    pub platform: Platform,
    pub persona: String,
    pub variant: String,
    #[serde(default)]
    pub topic: String,
    pub uri: String,
    #[serde(default)]
    pub text: String,
    pub likes: u64,
    pub replies: u64,
    pub reposts: u64,
    pub quotes: u64,
    #[serde(default)]
    pub published_at_unix: Option<u64>,
}

impl MetricsRecord {
    /// Build a record from a parsed post plus its A/B context.
    pub fn from_post(platform: Platform, post: &PostMetrics, context: RecordContext) -> Self {
        Self {
            captured_at_unix: context.captured_at_unix,
            platform,
            persona: context.persona,
            variant: context.variant,
            topic: context.topic,
            uri: post.uri.clone(),
            text: post.text.clone(),
            likes: post.likes,
            replies: post.replies,
            reposts: post.reposts,
            quotes: post.quotes,
            published_at_unix: context.published_at_unix,
        }
    }

    /// Raw interaction count, without the A/B weighting.
    pub fn total_engagement(&self) -> u64 {
        self.likes + self.replies + self.reposts + self.quotes
    }
}

/// Append one metrics sample to the JSONL store.
pub async fn append_metrics(path: &Path, record: &MetricsRecord) -> Result<()> {
    store::append_line(path, record, "metrics").await
}

/// Read every metrics sample from a JSONL store (a missing file is empty).
pub async fn read_metrics(path: &Path) -> Result<Vec<MetricsRecord>> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("reading metrics store {}", path.display()));
        }
    };

    let mut records = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: MetricsRecord = serde_json::from_str(line)
            .with_context(|| format!("invalid metrics JSON at {}:{}", path.display(), index + 1))?;
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(variant: &str) -> MetricsRecord {
        MetricsRecord::from_post(
            Platform::Bluesky,
            &PostMetrics {
                uri: "at://did:plc:abc/app.bsky.feed.post/1".to_string(),
                text: "corpo".to_string(),
                likes: 10,
                replies: 2,
                reposts: 1,
                quotes: 0,
            },
            RecordContext {
                captured_at_unix: 1_700_000_000,
                persona: "ciencia".to_string(),
                variant: variant.to_string(),
                topic: "cérebro".to_string(),
                published_at_unix: Some(1_699_999_000),
            },
        )
    }

    /// A unique temporary directory per test process and run.
    fn temp_dir() -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("autopilot-metrics-{}-{stamp}", std::process::id()))
    }

    #[tokio::test]
    async fn store_round_trips_records() {
        let dir = temp_dir();
        let path = dir.join("metrics.jsonl");
        let first = sample("A");
        let second = sample("B");

        append_metrics(&path, &first).await.expect("append first");
        append_metrics(&path, &second).await.expect("append second");

        let read = read_metrics(&path).await.expect("read");
        assert_eq!(read, vec![first, second]);

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }

    #[tokio::test]
    async fn missing_store_reads_as_empty() {
        let path = temp_dir().join("does-not-exist.jsonl");
        let read = read_metrics(&path).await.expect("missing file is empty");
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn malformed_line_reports_its_position() {
        let dir = temp_dir();
        let path = dir.join("metrics.jsonl");
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        tokio::fs::write(&path, "{\"not\":\"a record\"}\n")
            .await
            .expect("write");

        let err = read_metrics(&path).await.expect_err("bad record fails");
        assert!(err.to_string().contains(":1"), "unexpected error: {err}");

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }
}
