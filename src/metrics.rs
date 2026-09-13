//! Evaluation arithmetic shared by the backtest report: Brier score,
//! reliability buckets, closing-line value summaries, and drawdown. Pure
//! Decimal functions over graded rows; nothing here reads a store.

use chrono::{DateTime, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Serialize;

/// Mean squared error between a probability and the realized payout
/// (0, 1, or 0.5 for a push). Lower is better; a coin flip scores 0.25.
pub fn brier(pairs: &[(Decimal, Decimal)]) -> Option<Decimal> {
    if pairs.is_empty() {
        return None;
    }
    let total: Decimal = pairs
        .iter()
        .map(|(probability, outcome)| (probability - outcome) * (probability - outcome))
        .sum();
    Some((total / Decimal::from(pairs.len())).round_dp(6))
}

/// One probability bucket of a reliability diagram. A calibrated source has
/// `mean_outcome` close to `mean_probability` in every populated bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReliabilityBucket {
    #[serde(with = "rust_decimal::serde::str")]
    pub lower: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub upper: Decimal,
    pub count: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub mean_probability: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub mean_outcome: Decimal,
}

/// Equal-width buckets over `[0, 1]`; probabilities of exactly 1 land in
/// the top bucket. Empty buckets are omitted.
pub fn reliability(pairs: &[(Decimal, Decimal)], buckets: usize) -> Vec<ReliabilityBucket> {
    if buckets == 0 {
        return Vec::new();
    }
    let width = Decimal::ONE / Decimal::from(buckets);
    let mut sums = vec![(0usize, Decimal::ZERO, Decimal::ZERO); buckets];
    for (probability, outcome) in pairs {
        let index = (*probability / width)
            .floor()
            .to_usize()
            .unwrap_or(0)
            .min(buckets - 1);
        let entry = &mut sums[index];
        entry.0 += 1;
        entry.1 += probability;
        entry.2 += outcome;
    }
    sums.into_iter()
        .enumerate()
        .filter(|(_, (count, _, _))| *count > 0)
        .map(|(index, (count, probability, outcome))| ReliabilityBucket {
            lower: width * Decimal::from(index),
            upper: width * Decimal::from(index + 1),
            count,
            mean_probability: (probability / Decimal::from(count)).round_dp(4),
            mean_outcome: (outcome / Decimal::from(count)).round_dp(4),
        })
        .collect()
}

/// Count, mean, median, and the share of positive values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub count: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub mean: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub median: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total: Decimal,
    /// Fraction strictly greater than zero.
    #[serde(with = "rust_decimal::serde::str")]
    pub positive_share: Decimal,
}

pub fn summary(values: &[Decimal]) -> Option<Summary> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort();
    let count = Decimal::from(sorted.len());
    let total: Decimal = sorted.iter().sum();
    let middle = sorted.len() / 2;
    let median = if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / Decimal::TWO
    } else {
        sorted[middle]
    };
    let positive = sorted
        .iter()
        .filter(|value| **value > Decimal::ZERO)
        .count();
    Some(Summary {
        count: sorted.len(),
        mean: (total / count).round_dp(6),
        median: median.round_dp(6),
        total: total.round_dp(6),
        positive_share: (Decimal::from(positive) / count).round_dp(4),
    })
}

/// Largest peak-to-trough fall of an equity series, in currency, with the
/// timestamps that bound it. Zero for a monotone series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Drawdown {
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub peak_at: Option<DateTime<Utc>>,
    pub trough_at: Option<DateTime<Utc>>,
}

pub fn max_drawdown(equity: &[(DateTime<Utc>, Decimal)]) -> Drawdown {
    let mut peak: Option<(DateTime<Utc>, Decimal)> = None;
    let mut worst = Drawdown {
        amount: Decimal::ZERO,
        peak_at: None,
        trough_at: None,
    };
    for (at, value) in equity {
        match peak {
            Some((_, high)) if *value <= high => {
                let fall = high - value;
                if fall > worst.amount {
                    worst = Drawdown {
                        amount: fall,
                        peak_at: peak.map(|(when, _)| when),
                        trough_at: Some(*at),
                    };
                }
            }
            _ => peak = Some((*at, *value)),
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(value: &str) -> Decimal {
        value.parse().unwrap()
    }

    #[test]
    fn brier_scores_certainty_and_coin_flips() {
        assert_eq!(brier(&[]), None);
        assert_eq!(
            brier(&[(Decimal::ONE, Decimal::ONE), (Decimal::ZERO, Decimal::ZERO)]),
            Some(Decimal::ZERO)
        );
        assert_eq!(
            brier(&[(d("0.5"), Decimal::ONE), (d("0.5"), Decimal::ZERO)]),
            Some(d("0.25"))
        );
    }

    #[test]
    fn reliability_buckets_group_by_probability_and_skip_empty() {
        let pairs = [
            (d("0.05"), Decimal::ZERO),
            (d("0.15"), Decimal::ZERO),
            (d("0.95"), Decimal::ONE),
            (Decimal::ONE, Decimal::ONE),
        ];
        let buckets = reliability(&pairs, 10);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].count, 1);
        assert_eq!(buckets[0].lower, Decimal::ZERO);
        assert_eq!(buckets[2].count, 2, "probability 1 lands in the top bucket");
        assert_eq!(buckets[2].mean_outcome, Decimal::ONE);
    }

    #[test]
    fn summary_reports_median_and_positive_share() {
        let result = summary(&[d("-1"), d("2"), d("3"), d("4")]).unwrap();
        assert_eq!(result.count, 4);
        assert_eq!(result.median, d("2.5"));
        assert_eq!(result.total, d("8"));
        assert_eq!(result.positive_share, d("0.75"));
        assert_eq!(summary(&[]), None);
    }

    #[test]
    fn drawdown_is_peak_to_trough() {
        let t = |seconds: i64| DateTime::<Utc>::from_timestamp(seconds, 0).unwrap();
        let equity = [
            (t(0), d("100")),
            (t(1), d("105")),
            (t(2), d("101")),
            (t(3), d("99")),
            (t(4), d("110")),
            (t(5), d("108")),
        ];
        let result = max_drawdown(&equity);
        assert_eq!(result.amount, d("6"));
        assert_eq!(result.peak_at, Some(t(1)));
        assert_eq!(result.trough_at, Some(t(3)));
        assert_eq!(max_drawdown(&[]).amount, Decimal::ZERO);
    }
}
