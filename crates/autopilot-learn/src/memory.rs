//! Persona memory proposals built from an A/B outcome.
//!
//! The engine never rewrites a persona pack silently: it emits this proposal and
//! the write happens only when the operator asks for it (`--write`).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use autopilot_core::Platform;
use autopilot_metrics::MetricsRecord;
use serde::{Deserialize, Serialize};

use crate::score::{AbOutcome, VariantStats, weighted_score};

/// How many winning posts to carry forward as few-shot examples.
pub const DEFAULT_MAX_FEW_SHOT: usize = 3;

/// Seconds in a day, used to derive the UTC hour of a publish timestamp.
const SECONDS_PER_DAY: u64 = 86_400;

/// Seconds in an hour.
const SECONDS_PER_HOUR: u64 = 3_600;

/// A proposed update to a persona pack's memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryUpdate {
    pub persona: String,
    pub generated_at_unix: u64,
    pub platform: Platform,
    /// Winning variant label, or `None` when the comparison was inconclusive.
    pub winner: Option<String>,
    pub min_samples: usize,
    /// Per-variant statistics, best first.
    pub variants: Vec<VariantStats>,
    /// Winning posts, to reuse as few-shot examples.
    pub few_shot: Vec<String>,
    /// Human-readable schedule hints derived from the winning samples.
    pub schedule_hints: Vec<String>,
    /// Why this update was proposed (or why it is inconclusive).
    pub reason: String,
}

/// The strongest posting hour (UTC) for one variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScheduleHint {
    pub hour_utc: u8,
    pub mean_score: f64,
    pub samples: usize,
}

/// Build a memory proposal from the metrics and the A/B outcome.
///
/// `generated_at_unix` is passed in so the function stays pure and testable.
pub fn propose_memory(
    records: &[MetricsRecord],
    persona: &str,
    platform: Platform,
    outcome: &AbOutcome,
    max_few_shot: usize,
    generated_at_unix: u64,
) -> MemoryUpdate {
    let few_shot = outcome
        .winner
        .as_deref()
        .map(|winner| winning_texts(records, winner, max_few_shot))
        .unwrap_or_default();

    let schedule_hints = outcome
        .winner
        .as_deref()
        .and_then(|winner| best_hour(records, winner))
        .map(|hint| {
            vec![format!(
                "best hour (UTC): {:02}:00 — mean weighted engagement {:.2} over {} sample(s)",
                hint.hour_utc, hint.mean_score, hint.samples
            )]
        })
        .unwrap_or_default();

    MemoryUpdate {
        persona: persona.to_string(),
        generated_at_unix,
        platform,
        winner: outcome.winner.clone(),
        min_samples: outcome.min_samples,
        variants: outcome.variants.clone(),
        few_shot,
        schedule_hints,
        reason: outcome.reason.clone(),
    }
}

/// The UTC hour with the highest mean weighted engagement for `variant`.
pub fn best_hour(records: &[MetricsRecord], variant: &str) -> Option<ScheduleHint> {
    let mut buckets: BTreeMap<u8, (f64, usize)> = BTreeMap::new();
    for record in records.iter().filter(|record| record.variant == variant) {
        let Some(published) = record.published_at_unix else {
            continue;
        };
        let hour = ((published % SECONDS_PER_DAY) / SECONDS_PER_HOUR) as u8;
        let bucket = buckets.entry(hour).or_insert((0.0, 0));
        bucket.0 += weighted_score(record);
        bucket.1 += 1;
    }

    buckets
        .into_iter()
        .map(|(hour_utc, (sum, samples))| ScheduleHint {
            hour_utc,
            mean_score: sum / samples as f64,
            samples,
        })
        .max_by(|left, right| {
            left.mean_score
                .total_cmp(&right.mean_score)
                .then_with(|| right.hour_utc.cmp(&left.hour_utc))
        })
}

/// Render the proposal as YAML.
pub fn render_memory(update: &MemoryUpdate) -> Result<String> {
    serde_yaml_ng::to_string(update).context("serializing the memory update")
}

/// Persist the proposal (explicit opt-in; the engine never does this silently).
pub async fn write_memory(path: &Path, update: &MemoryUpdate) -> Result<()> {
    let body = render_memory(update)?;
    autopilot_core::store::write_text(path, &body, "memory").await
}

/// Non-empty, de-duplicated texts of the winning variant (order preserved).
fn winning_texts(records: &[MetricsRecord], winner: &str, max: usize) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();
    for record in records.iter().filter(|record| record.variant == winner) {
        let text = record.text.trim();
        if text.is_empty() || texts.iter().any(|kept| kept == text) {
            continue;
        }
        texts.push(text.to_string());
        if texts.len() >= max {
            break;
        }
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::{DEFAULT_MIN_SAMPLES, compare_variants};
    use crate::test_support::{record, record_at};

    fn outcome_for(records: &[MetricsRecord]) -> AbOutcome {
        compare_variants(records, DEFAULT_MIN_SAMPLES)
    }

    #[test]
    fn proposal_carries_the_winner_and_its_text() {
        let records = vec![record("A", 5, 0, 0, 0), record("B", 40, 0, 0, 0)];
        let outcome = outcome_for(&records);
        let update = propose_memory(
            &records,
            "ciencia-curiosidades",
            Platform::Bluesky,
            &outcome,
            DEFAULT_MAX_FEW_SHOT,
            1_700_000_000,
        );

        assert_eq!(update.winner.as_deref(), Some("B"));
        assert_eq!(update.few_shot, vec!["corpo B".to_string()]);
        assert_eq!(update.persona, "ciencia-curiosidades");
        assert_eq!(update.generated_at_unix, 1_700_000_000);
    }

    #[test]
    fn few_shot_is_deduplicated_and_capped() {
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(record("A", 0, 0, 1, 0));
        }
        records.push(record("B", 0, 0, 0, 0));
        let outcome = outcome_for(&records);
        let update = propose_memory(
            &records,
            "p",
            Platform::Bluesky,
            &outcome,
            DEFAULT_MAX_FEW_SHOT,
            0,
        );

        // All winning texts are identical here, so dedup leaves exactly one.
        assert_eq!(update.few_shot, vec!["corpo A".to_string()]);
    }

    #[test]
    fn inconclusive_outcome_yields_no_hints() {
        let records = vec![record("A", 5, 0, 0, 0)];
        let outcome = outcome_for(&records);
        let update = propose_memory(
            &records,
            "p",
            Platform::Bluesky,
            &outcome,
            DEFAULT_MAX_FEW_SHOT,
            0,
        );

        assert_eq!(update.winner, None);
        assert!(update.few_shot.is_empty());
        assert!(update.schedule_hints.is_empty());
    }

    #[test]
    fn best_hour_picks_the_strongest_bucket() {
        let records = vec![
            record_at("A", 6, 1, 0, 0, 0),
            record_at("A", 18, 10, 0, 0, 0),
            record_at("A", 12, 5, 0, 0, 0),
        ];
        let hint = best_hour(&records, "A").expect("a hint");
        assert_eq!(hint.hour_utc, 18);
        assert_eq!(hint.samples, 1);
        assert_eq!(hint.mean_score, 10.0);
    }

    #[test]
    fn best_hour_is_none_without_timestamps() {
        let records = vec![record("A", 10, 0, 0, 0)];
        assert!(best_hour(&records, "A").is_none());
    }

    #[test]
    fn schedule_hint_is_rendered_when_timestamps_exist() {
        let records = vec![
            record_at("A", 6, 1, 0, 0, 0),
            record_at("B", 18, 10, 0, 0, 0),
        ];
        let outcome = outcome_for(&records);
        let update = propose_memory(
            &records,
            "p",
            Platform::Bluesky,
            &outcome,
            DEFAULT_MAX_FEW_SHOT,
            0,
        );
        assert_eq!(update.winner.as_deref(), Some("B"));
        assert_eq!(update.schedule_hints.len(), 1);
        assert!(update.schedule_hints[0].contains("18:00"));
    }

    #[test]
    fn render_is_yaml_with_the_winner() {
        let records = vec![record("A", 1, 0, 0, 0), record("B", 9, 0, 0, 0)];
        let outcome = outcome_for(&records);
        let update = propose_memory(
            &records,
            "p",
            Platform::Bluesky,
            &outcome,
            DEFAULT_MAX_FEW_SHOT,
            0,
        );

        let yaml = render_memory(&update).expect("renders");
        assert!(yaml.contains("winner: B"), "yaml was: {yaml}");
        assert!(yaml.contains("persona: p"));
    }
}
