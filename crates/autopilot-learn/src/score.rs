//! Pure A/B scoring and comparison.

use std::cmp::Ordering;

use autopilot_metrics::MetricsRecord;
use serde::{Deserialize, Serialize};

/// Weight applied to each engagement signal when building the A/B score.
///
/// A like is cheap; a reply or a repost costs the reader more and reaches
/// further, so they weigh more. The weights are relative, which makes the score
/// comparable between variants of the same platform but not across platforms.
pub const LIKE_WEIGHT: f64 = 1.0;
pub const REPLY_WEIGHT: f64 = 2.0;
pub const REPOST_WEIGHT: f64 = 3.0;
pub const QUOTE_WEIGHT: f64 = 3.0;

/// Minimum samples a variant needs before a comparison is conclusive.
pub const DEFAULT_MIN_SAMPLES: usize = 1;

/// Scores closer than this are treated as a tie before tie-breaking.
const TIE_EPSILON: f64 = 1e-9;

/// Weighted engagement of a single sample.
pub fn weighted_score(record: &MetricsRecord) -> f64 {
    record.likes as f64 * LIKE_WEIGHT
        + record.replies as f64 * REPLY_WEIGHT
        + record.reposts as f64 * REPOST_WEIGHT
        + record.quotes as f64 * QUOTE_WEIGHT
}

/// Aggregate statistics for one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantStats {
    pub variant: String,
    pub samples: usize,
    pub likes: u64,
    pub replies: u64,
    pub reposts: u64,
    pub quotes: u64,
    /// Mean weighted engagement per post.
    pub score: f64,
}

impl VariantStats {
    /// Raw interactions summed across the variant's samples.
    pub fn total_engagement(&self) -> u64 {
        self.likes + self.replies + self.reposts + self.quotes
    }
}

/// The result of an A/B comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbOutcome {
    /// Winning variant label, or `None` when the result is inconclusive.
    pub winner: Option<String>,
    pub min_samples: usize,
    /// Every variant found, best first.
    pub variants: Vec<VariantStats>,
    /// Why the outcome was reached (or why it is inconclusive).
    pub reason: String,
}

/// Summarise one variant across its samples.
pub fn summarise(records: &[MetricsRecord], variant: &str) -> VariantStats {
    let matching: Vec<&MetricsRecord> = records
        .iter()
        .filter(|record| record.variant == variant)
        .collect();
    let samples = matching.len();
    let score = if samples == 0 {
        0.0
    } else {
        matching
            .iter()
            .map(|record| weighted_score(record))
            .sum::<f64>()
            / samples as f64
    };

    VariantStats {
        variant: variant.to_string(),
        samples,
        likes: matching.iter().map(|record| record.likes).sum(),
        replies: matching.iter().map(|record| record.replies).sum(),
        reposts: matching.iter().map(|record| record.reposts).sum(),
        quotes: matching.iter().map(|record| record.quotes).sum(),
        score,
    }
}

/// Compare the A/B variants found in `records`.
///
/// Ranking is deterministic: highest mean weighted engagement first, then the
/// most samples, then the most raw interactions, then the label alphabetically.
/// A variant with fewer than `min_samples` samples makes the whole comparison
/// inconclusive — a single lucky post is not evidence.
pub fn compare_variants(records: &[MetricsRecord], min_samples: usize) -> AbOutcome {
    let mut labels: Vec<String> = records.iter().map(|r| r.variant.clone()).collect();
    labels.sort();
    labels.dedup();

    let mut variants: Vec<VariantStats> = labels
        .iter()
        .map(|label| summarise(records, label))
        .collect();
    variants.sort_by(rank);

    if variants.len() < 2 {
        return AbOutcome {
            winner: None,
            min_samples,
            variants,
            reason: "need at least two variants to compare".to_string(),
        };
    }

    if let Some(short) = variants
        .iter()
        .find(|stats| stats.samples < min_samples)
        .cloned()
    {
        return AbOutcome {
            winner: None,
            min_samples,
            variants,
            reason: format!(
                "variant {} has {} sample(s), below the minimum of {min_samples}",
                short.variant, short.samples
            ),
        };
    }

    let winner = variants[0].variant.clone();
    let reason = match variants.get(1) {
        Some(runner_up) if (variants[0].score - runner_up.score).abs() < TIE_EPSILON => format!(
            "{winner} and {} tie on score; {winner} wins the deterministic tie-break",
            runner_up.variant
        ),
        Some(runner_up) => format!(
            "{winner} leads {} by {:.2} weighted engagement per post",
            runner_up.variant,
            variants[0].score - runner_up.score
        ),
        None => format!("{winner} is the only conclusive variant"),
    };

    AbOutcome {
        winner: Some(winner),
        min_samples,
        variants,
        reason,
    }
}

/// Deterministic ranking: score, then samples, then raw interactions, then label.
fn rank(left: &VariantStats, right: &VariantStats) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| right.samples.cmp(&left.samples))
        .then_with(|| right.total_engagement().cmp(&left.total_engagement()))
        .then_with(|| left.variant.cmp(&right.variant))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::record;

    #[test]
    fn stronger_signals_score_higher() {
        let like = record("A", 1, 0, 0, 0);
        let reply = record("A", 0, 1, 0, 0);
        let repost = record("A", 0, 0, 1, 0);

        assert!(weighted_score(&reply) > weighted_score(&like));
        assert!(weighted_score(&repost) > weighted_score(&reply));
        assert_eq!(weighted_score(&like), 1.0);
        assert_eq!(weighted_score(&repost), 3.0);
    }

    #[test]
    fn summarise_averages_weighted_score() {
        let records = vec![record("A", 10, 0, 0, 0), record("A", 20, 0, 0, 0)];
        let stats = summarise(&records, "A");
        assert_eq!(stats.samples, 2);
        assert_eq!(stats.likes, 30);
        assert_eq!(stats.score, 15.0);
        assert_eq!(stats.total_engagement(), 30);
    }

    #[test]
    fn higher_score_wins() {
        let records = vec![
            record("A", 10, 0, 0, 0),
            record("B", 50, 0, 0, 0),
            record("A", 12, 0, 0, 0),
            record("B", 40, 0, 0, 0),
        ];
        let outcome = compare_variants(&records, DEFAULT_MIN_SAMPLES);
        assert_eq!(outcome.winner.as_deref(), Some("B"));
        assert_eq!(outcome.variants[0].variant, "B");
        assert!(outcome.reason.contains("leads"));
    }

    #[test]
    fn ties_break_on_samples_then_label() {
        // Same mean score, but A has more samples -> A wins.
        let records = vec![
            record("A", 10, 0, 0, 0),
            record("A", 10, 0, 0, 0),
            record("B", 10, 0, 0, 0),
        ];
        let outcome = compare_variants(&records, DEFAULT_MIN_SAMPLES);
        assert_eq!(outcome.winner.as_deref(), Some("A"));
        assert!(outcome.reason.contains("tie"));

        // Same score and same samples -> the label breaks the tie (A < B).
        let records = vec![record("B", 10, 0, 0, 0), record("A", 10, 0, 0, 0)];
        let outcome = compare_variants(&records, DEFAULT_MIN_SAMPLES);
        assert_eq!(outcome.winner.as_deref(), Some("A"));
    }

    #[test]
    fn minimum_sample_guard_makes_it_inconclusive() {
        let records = vec![
            record("A", 10, 0, 0, 0),
            record("A", 12, 0, 0, 0),
            record("B", 50, 0, 0, 0),
        ];
        let outcome = compare_variants(&records, 2);
        assert_eq!(outcome.winner, None);
        assert!(outcome.reason.contains("below the minimum"));
    }

    #[test]
    fn a_single_variant_is_inconclusive() {
        let records = vec![record("A", 10, 0, 0, 0)];
        let outcome = compare_variants(&records, DEFAULT_MIN_SAMPLES);
        assert_eq!(outcome.winner, None);
        assert!(outcome.reason.contains("two variants"));
    }

    #[test]
    fn raw_engagement_breaks_a_score_tie() {
        // Equal mean score and sample count; A has more raw interactions.
        let records = vec![record("A", 3, 0, 0, 0), record("B", 0, 0, 1, 0)];
        let outcome = compare_variants(&records, DEFAULT_MIN_SAMPLES);
        assert_eq!(outcome.winner.as_deref(), Some("A"));
    }
}
