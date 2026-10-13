//! Learned per-format weekly-budget adjustment (HB-034 §4).
//!
//! `content-formats.md` §4 asks the scorer a direct question: *does a card
//! perform better than text for this persona?* The metrics store is keyed by post
//! URI and does **not** carry the format; the run journal does, so the caller
//! joins the two (a post URI maps to the format it was published with) and this
//! module aggregates the weighted engagement **per format**, normalised against
//! the persona's own rolling median.
//!
//! The movement is deliberately conservative: a format only moves once it has a
//! minimum number of samples, and then by exactly one post a week. The engine
//! never rewrites a persona pack — the adjustment is a [`BudgetAdjustment`]
//! carried by the memory proposal (`var/learn/<slug>.json`) and consumed by the
//! next run.

use std::collections::{BTreeMap, HashMap};

use autopilot_core::{Format, FormatBudgets};
use autopilot_metrics::MetricsRecord;
use serde::{Deserialize, Serialize};

use crate::score::{rolling_baseline, weighted_score};

/// Minimum samples a format needs before its weekly budget may move at all.
///
/// Below this the sample is noise: a couple of posts cannot tell a persona
/// anything durable, so the budget holds.
pub const DEFAULT_FORMAT_MIN_SAMPLES: usize = 8;

/// The conservative weekly step: a format's budget moves by at most one post.
pub const FORMAT_BUDGET_STEP: i16 = 1;

/// Scores closer than this are treated as a tie, so the budget holds.
const TIE_EPSILON: f64 = 1e-9;

/// The weighted-engagement performance of one format across its samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatStat {
    pub format: Format,
    pub samples: usize,
    /// Mean weighted engagement per post of this format.
    pub mean_score: f64,
    /// Median weighted engagement of the persona's recent samples (the baseline).
    pub baseline: f64,
    /// Signed deviation from the baseline (`mean_score - baseline`).
    pub delta: f64,
}

/// One proposed change to a format's weekly budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetAdjustment {
    pub format: Format,
    /// The signed weekly step applied to the budget (`-1`, `0` or `+1`).
    pub step: i16,
    pub samples: usize,
    pub mean_score: f64,
    pub baseline: f64,
    /// Why the budget moved (or held), for the operator reading the proposal.
    pub reason: String,
}

/// The per-format performance of a persona's posts.
///
/// `format_of` maps a post URI to the format it was published with (built from
/// the run journal); a record whose URI is unknown is skipped — an unlabelled
/// older post cannot be attributed to a format. The baseline is the persona's
/// own rolling median over `window` recent samples, so a format is compared to
/// the account's norm, never to an absolute target.
pub fn format_performance(
    records: &[MetricsRecord],
    format_of: &HashMap<String, Format>,
    window: usize,
) -> Vec<FormatStat> {
    let (baseline, _) = rolling_baseline(records, window);

    let mut buckets: BTreeMap<Format, (f64, usize)> = BTreeMap::new();
    for record in records {
        let Some(format) = format_of.get(&record.uri) else {
            continue;
        };
        let bucket = buckets.entry(*format).or_insert((0.0, 0));
        bucket.0 += weighted_score(record);
        bucket.1 += 1;
    }

    buckets
        .into_iter()
        .map(|(format, (sum, samples))| {
            let mean_score = sum / samples as f64;
            FormatStat {
                format,
                samples,
                mean_score,
                baseline,
                delta: mean_score - baseline,
            }
        })
        .collect()
}

/// The proposed per-format budget movements, guarded by a minimum sample.
///
/// A format with fewer than `min_samples` samples never moves. Above the guard,
/// a format performing **above** the persona's own median gains one post a week;
/// below it loses one; a tie leaves it. The step is always exactly
/// [`FORMAT_BUDGET_STEP`] — the movement is conservative because the budget only
/// exists to avoid monotony, never to chase noise.
pub fn budget_adjustments(stats: &[FormatStat], min_samples: usize) -> Vec<BudgetAdjustment> {
    stats
        .iter()
        .map(|stat| {
            let (step, reason) = if stat.samples < min_samples {
                (
                    0,
                    format!(
                        "{} has {} sample(s), below the minimum of {min_samples} — holding the budget",
                        stat.format.as_str(),
                        stat.samples
                    ),
                )
            } else if stat.delta > TIE_EPSILON {
                (
                    FORMAT_BUDGET_STEP,
                    format!(
                        "{} scores {:.2} above the persona median {:.2} over {} sample(s) — adding one post a week",
                        stat.format.as_str(),
                        stat.delta,
                        stat.baseline,
                        stat.samples
                    ),
                )
            } else if stat.delta < -TIE_EPSILON {
                (
                    -FORMAT_BUDGET_STEP,
                    format!(
                        "{} scores {:.2} below the persona median {:.2} over {} sample(s) — removing one post a week",
                        stat.format.as_str(),
                        stat.delta,
                        stat.baseline,
                        stat.samples
                    ),
                )
            } else {
                (
                    0,
                    format!(
                        "{} matches the persona median over {} sample(s) — holding the budget",
                        stat.format.as_str(),
                        stat.samples
                    ),
                )
            };

            BudgetAdjustment {
                format: stat.format,
                step,
                samples: stat.samples,
                mean_score: stat.mean_score,
                baseline: stat.baseline,
                reason,
            }
        })
        .collect()
}

/// Apply learned adjustments to a base budget, clamped at zero.
///
/// The result is what the next run uses for the week: the pack's declared budget
/// plus one conservative step per format. A format can never go below zero, and
/// a base budget of zero plus a positive step is how a format first enters the
/// rotation.
pub fn apply_budget_adjustments(
    base: &FormatBudgets,
    adjustments: &[BudgetAdjustment],
) -> FormatBudgets {
    let mut budgets = *base;
    for adjustment in adjustments {
        let next = (i32::from(budgets.budget(adjustment.format)) + i32::from(adjustment.step))
            .max(0) as u16;
        match adjustment.format {
            Format::Text => budgets.text = next,
            Format::Card => budgets.card = next,
            Format::Video => budgets.video = next,
        }
    }
    budgets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{record, record_text};

    fn formats(entries: &[(&str, Format)]) -> HashMap<String, Format> {
        entries
            .iter()
            .map(|(uri, format)| ((*uri).to_string(), *format))
            .collect()
    }

    #[test]
    fn performance_is_aggregated_per_format_and_normalised_to_the_median() {
        // A has two strong card posts, B one weak text post. The persona median
        // over all three is the reference (never an absolute target).
        let records = vec![
            record_text("A", "card one", 40, 0, 0, 0),
            record_text("A", "card two", 20, 0, 0, 0),
            record_text("B", "text one", 0, 0, 0, 0),
        ];
        let map = formats(&[
            ("at://did:plc:abc/app.bsky.feed.post/A", Format::Card),
            ("at://did:plc:abc/app.bsky.feed.post/B", Format::Text),
        ]);
        let stats = format_performance(&records, &map, 20);

        let card = stats
            .iter()
            .find(|stat| stat.format == Format::Card)
            .expect("the card format is scored");
        assert_eq!(card.samples, 2);
        assert_eq!(card.mean_score, 30.0);
        assert_eq!(card.baseline, 20.0, "median of [40, 20, 0]");
        assert!(card.delta > 0.0);

        let text = stats
            .iter()
            .find(|stat| stat.format == Format::Text)
            .expect("the text format is scored");
        assert_eq!(text.samples, 1);
        assert_eq!(text.mean_score, 0.0);
        assert!(text.delta < 0.0);
    }

    #[test]
    fn a_record_with_no_journalled_format_is_skipped() {
        let records = vec![record("A", 10, 0, 0, 0), record("B", 10, 0, 0, 0)];
        // Only A has a format; B is unattributable and must not skew the stats.
        let map = formats(&[("at://did:plc:abc/app.bsky.feed.post/A", Format::Card)]);
        let stats = format_performance(&records, &map, 20);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].format, Format::Card);
        assert_eq!(stats[0].samples, 1);
    }

    #[test]
    fn the_minimum_sample_guard_holds_a_thin_format() {
        let stats = vec![FormatStat {
            format: Format::Video,
            samples: 3,
            mean_score: 100.0,
            baseline: 5.0,
            delta: 95.0,
        }];
        let adjustments = budget_adjustments(&stats, DEFAULT_FORMAT_MIN_SAMPLES);
        assert_eq!(adjustments.len(), 1);
        assert_eq!(
            adjustments[0].step, 0,
            "three samples is noise, not evidence"
        );
        assert!(adjustments[0].reason.contains("below the minimum"));
    }

    #[test]
    fn a_well_sampled_format_moves_by_exactly_one_step() {
        let stats = vec![
            FormatStat {
                format: Format::Card,
                samples: 10,
                mean_score: 40.0,
                baseline: 10.0,
                delta: 30.0,
            },
            FormatStat {
                format: Format::Text,
                samples: 12,
                mean_score: 2.0,
                baseline: 10.0,
                delta: -8.0,
            },
            FormatStat {
                format: Format::Video,
                samples: 8,
                mean_score: 10.0,
                baseline: 10.0,
                delta: 0.0,
            },
        ];
        let adjustments = budget_adjustments(&stats, DEFAULT_FORMAT_MIN_SAMPLES);
        let step = |format: Format| {
            adjustments
                .iter()
                .find(|adjustment| adjustment.format == format)
                .map(|adjustment| adjustment.step)
        };
        assert_eq!(step(Format::Card), Some(FORMAT_BUDGET_STEP));
        assert_eq!(step(Format::Text), Some(-FORMAT_BUDGET_STEP));
        assert_eq!(step(Format::Video), Some(0));
    }

    #[test]
    fn adjustments_apply_to_the_base_budget_and_clamp_at_zero() {
        let base = FormatBudgets {
            text: 1,
            card: 6,
            video: 0,
        };
        let adjustments = vec![
            BudgetAdjustment {
                format: Format::Text,
                step: -1,
                samples: 9,
                mean_score: 0.0,
                baseline: 5.0,
                reason: String::new(),
            },
            BudgetAdjustment {
                format: Format::Card,
                step: 1,
                samples: 9,
                mean_score: 9.0,
                baseline: 5.0,
                reason: String::new(),
            },
            BudgetAdjustment {
                format: Format::Video,
                step: 1,
                samples: 9,
                mean_score: 9.0,
                baseline: 5.0,
                reason: String::new(),
            },
        ];
        let adjusted = apply_budget_adjustments(&base, &adjustments);
        assert_eq!(adjusted.text, 0);
        assert_eq!(adjusted.card, 7);
        assert_eq!(
            adjusted.video, 1,
            "a zero base plus a step brings a format in"
        );

        // A further negative step cannot push a budget below zero.
        let floor = apply_budget_adjustments(
            &adjusted,
            &[BudgetAdjustment {
                format: Format::Text,
                step: -1,
                samples: 9,
                mean_score: 0.0,
                baseline: 5.0,
                reason: String::new(),
            }],
        );
        assert_eq!(floor.text, 0);
    }
}
