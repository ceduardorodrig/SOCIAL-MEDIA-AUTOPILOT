//! Pure A/B scoring and comparison.
//!
//! Two numbers live here: the **performance score** (weighted interactions,
//! normalised against the persona's own rolling median so a brand-new account
//! is not compared to an absolute target) and the A/B ranking that feeds the
//! memory proposal. Craft scoring lives in [`crate::craft`].

use std::cmp::Ordering;

use autopilot_metrics::MetricsRecord;
use serde::{Deserialize, Serialize};

/// Weight applied to each engagement signal when building the A/B score.
///
/// The weights come from `scoring-model.md` §1: interactions do not weigh the
/// same because costly signals reach further than a cheap like. The weights are
/// relative, which makes the score comparable between variants of the same
/// platform but not across platforms.
///
/// | signal | weight | why |
/// |---|---|---|
/// | like | 1 | cheap, a thumb swipe |
/// | reply | 3 | costs time, starts a conversation |
/// | repost | 4 | the author stakes their reputation on it |
/// | quote | 4 | same, with a comment |
/// | follow gained | 5 | the most durable effect |
pub const LIKE_WEIGHT: f64 = 1.0;
pub const REPLY_WEIGHT: f64 = 3.0;
pub const REPOST_WEIGHT: f64 = 4.0;
pub const QUOTE_WEIGHT: f64 = 4.0;
pub const FOLLOW_WEIGHT: f64 = 5.0;

/// Minimum samples a variant needs before a comparison is conclusive.
pub const DEFAULT_MIN_SAMPLES: usize = 1;

/// How many recent samples form the persona's rolling baseline.
pub const DEFAULT_BASELINE_WINDOW: usize = 20;

/// Scores closer than this are treated as a tie before tie-breaking.
const TIE_EPSILON: f64 = 1e-9;

/// Weighted engagement of raw interaction counts.
///
/// `follows` is the number of new followers gained in the measurement window.
/// The metrics store does not carry it yet, so [`weighted_score`] passes zero;
/// exposing the parameter keeps the documented weight usable the moment the
/// collector starts recording follows.
pub fn weighted_interactions(
    likes: u64,
    replies: u64,
    reposts: u64,
    quotes: u64,
    follows: u64,
) -> f64 {
    likes as f64 * LIKE_WEIGHT
        + replies as f64 * REPLY_WEIGHT
        + reposts as f64 * REPOST_WEIGHT
        + quotes as f64 * QUOTE_WEIGHT
        + follows as f64 * FOLLOW_WEIGHT
}

/// Weighted engagement of a single sample.
pub fn weighted_score(record: &MetricsRecord) -> f64 {
    weighted_interactions(
        record.likes,
        record.replies,
        record.reposts,
        record.quotes,
        0,
    )
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
    /// Median weighted engagement of the persona's recent samples (the baseline).
    pub baseline: f64,
    /// How many recent samples the baseline was computed from.
    pub baseline_samples: usize,
    /// Signed deviation from the baseline (`score - baseline`).
    pub delta: f64,
    /// Relative deviation from the baseline, or 0 when the baseline is zero.
    pub relative: f64,
}

impl VariantStats {
    /// Raw interactions summed across the variant's samples.
    pub fn total_engagement(&self) -> u64 {
        self.likes + self.replies + self.reposts + self.quotes
    }

    /// True when the variant performed above the persona's own baseline.
    pub fn is_above_baseline(&self) -> bool {
        self.delta > 0.0
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
    let (baseline, baseline_samples) = rolling_baseline(records, DEFAULT_BASELINE_WINDOW);

    VariantStats {
        variant: variant.to_string(),
        samples,
        likes: matching.iter().map(|record| record.likes).sum(),
        replies: matching.iter().map(|record| record.replies).sum(),
        reposts: matching.iter().map(|record| record.reposts).sum(),
        quotes: matching.iter().map(|record| record.quotes).sum(),
        score,
        baseline,
        baseline_samples,
        delta: score - baseline,
        relative: relative_delta(score, baseline),
    }
}

/// The persona's own rolling baseline: the median weighted engagement of the
/// most recent `window` samples.
///
/// Comparing to an absolute target would be unfair to a brand-new account, so
/// the reference is the account's own recent median (`scoring-model.md` §1).
/// Returns `(median, samples)`; `(0.0, 0)` when there is nothing to compare to.
pub fn rolling_baseline(records: &[MetricsRecord], window: usize) -> (f64, usize) {
    if window == 0 || records.is_empty() {
        return (0.0, 0);
    }
    let mut recent: Vec<&MetricsRecord> = records.iter().collect();
    recent.sort_by_key(|record| std::cmp::Reverse(record.captured_at_unix));
    recent.truncate(window);

    let samples = recent.len();
    let values: Vec<f64> = recent.iter().map(|record| weighted_score(record)).collect();
    (median(values), samples)
}

/// The signed performance score of one variant against a custom baseline window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerformanceScore {
    pub variant: String,
    /// Samples of this variant the score was computed from.
    pub samples: usize,
    /// Mean weighted engagement of this variant.
    pub value: f64,
    /// Median weighted engagement of the persona's recent samples.
    pub baseline: f64,
    /// How many recent samples the baseline was computed from.
    pub baseline_samples: usize,
    /// Signed deviation from the baseline: positive is above the persona's norm.
    pub delta: f64,
    /// Relative deviation, or 0 when the baseline is zero.
    pub relative: f64,
}

impl PerformanceScore {
    /// True when the variant performed above the persona's own baseline.
    pub fn is_above_baseline(&self) -> bool {
        self.delta > 0.0
    }
}

/// Score one variant against the persona's rolling baseline with a custom window.
pub fn performance_score(
    records: &[MetricsRecord],
    variant: &str,
    window: usize,
) -> PerformanceScore {
    let stats = summarise(records, variant);
    let (baseline, baseline_samples) = rolling_baseline(records, window);

    PerformanceScore {
        variant: variant.to_string(),
        samples: stats.samples,
        value: stats.score,
        baseline,
        baseline_samples,
        delta: stats.score - baseline,
        relative: relative_delta(stats.score, baseline),
    }
}

/// Relative deviation from a baseline, guarding against division by zero.
fn relative_delta(value: f64, baseline: f64) -> f64 {
    if baseline > 0.0 {
        (value - baseline) / baseline
    } else {
        0.0
    }
}

/// Median of a set of values (0.0 for an empty set).
fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
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
        assert_eq!(weighted_score(&reply), 3.0);
        assert_eq!(weighted_score(&repost), 4.0);
    }

    #[test]
    fn follow_gained_is_the_heaviest_signal() {
        assert_eq!(weighted_interactions(0, 0, 0, 0, 1), FOLLOW_WEIGHT);
        assert!(weighted_interactions(0, 0, 0, 0, 1) > weighted_interactions(0, 0, 1, 0, 0));
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
        // Equal mean score (4 likes vs 1 repost = 4.0) and sample count; A has
        // more raw interactions.
        let records = vec![record("A", 4, 0, 0, 0), record("B", 0, 0, 1, 0)];
        let outcome = compare_variants(&records, DEFAULT_MIN_SAMPLES);
        assert_eq!(outcome.winner.as_deref(), Some("A"));
    }

    #[test]
    fn performance_score_is_signed_against_the_persona_baseline() {
        // Recent baseline: median of [10, 20, 30] = 20.
        let records = vec![
            record("A", 10, 0, 0, 0),
            record("A", 20, 0, 0, 0),
            record("A", 30, 0, 0, 0),
        ];
        let score = performance_score(&records, "A", 3);
        assert_eq!(score.samples, 3);
        assert_eq!(score.value, 20.0);
        assert_eq!(score.baseline, 20.0);
        assert_eq!(score.baseline_samples, 3);
        assert_eq!(score.delta, 0.0);
        assert!(!score.is_above_baseline());
    }

    #[test]
    fn a_strong_post_scores_above_its_own_baseline() {
        // Baseline window of 2 sees the most recent samples: [50, 50] -> 50.
        let records = vec![
            record("A", 50, 0, 0, 0),
            record("A", 50, 0, 0, 0),
            record("B", 200, 0, 0, 0),
        ];
        let score = performance_score(&records, "B", 2);
        assert_eq!(score.baseline, 50.0);
        assert_eq!(score.value, 200.0);
        assert_eq!(score.delta, 150.0);
        assert_eq!(score.relative, 3.0);
        assert!(score.is_above_baseline());
    }

    #[test]
    fn baseline_is_zero_without_history() {
        let records = vec![record("A", 10, 0, 0, 0)];
        let score = performance_score(&records, "A", 0);
        assert_eq!(score.baseline, 0.0);
        assert_eq!(score.baseline_samples, 0);
        // Division by zero must not happen: relative falls back to 0.
        assert_eq!(score.relative, 0.0);
        assert!(score.is_above_baseline());
    }

    #[test]
    fn summarise_carries_the_default_window_baseline() {
        let records = vec![record("A", 10, 0, 0, 0), record("A", 30, 0, 0, 0)];
        let stats = summarise(&records, "A");
        assert_eq!(stats.baseline, 20.0);
        assert_eq!(stats.baseline_samples, 2);
        assert_eq!(stats.delta, 0.0);
    }
}
