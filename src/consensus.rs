use std::collections::HashMap;

use chrono::Utc;
use rust_decimal::Decimal;

use crate::{
    Error, Result,
    domain::{ConsensusPrice, FairQuote, SourceFamily, SourceQuote},
};

pub fn remove_vig(quote: &SourceQuote) -> Result<FairQuote> {
    if quote.decimal_odds_a <= Decimal::ONE || quote.decimal_odds_b <= Decimal::ONE {
        return Err(Error::InvalidData(format!(
            "{} supplied invalid decimal odds",
            quote.source_id
        )));
    }

    let inverse_a = Decimal::ONE / quote.decimal_odds_a;
    let inverse_b = Decimal::ONE / quote.decimal_odds_b;
    let inverse_neutral = match quote.decimal_odds_neutral {
        Some(value) if value > Decimal::ONE => Decimal::ONE / value,
        Some(_) => {
            return Err(Error::InvalidData(format!(
                "{} supplied invalid neutral odds",
                quote.source_id
            )));
        }
        None => Decimal::ZERO,
    };
    let total = inverse_a + inverse_b + inverse_neutral;
    if total <= Decimal::ZERO {
        return Err(Error::InvalidData("implied probability sum is zero".into()));
    }

    let win_a = inverse_a / total;
    let neutral = inverse_neutral / total;

    let probability_a = win_a + neutral / Decimal::TWO;
    Ok(FairQuote {
        source_id: quote.source_id.clone(),
        family: quote.family.clone(),
        // A neutral settlement pays both sides 0.50.
        probability_a,
        probability_b: Decimal::ONE - probability_a,
        probability_neutral: neutral,
        source_timestamp: quote.source_timestamp,
    })
}

/// Builds a robust consensus from quotes observed within `max_quote_age_seconds`.
/// Freshness is judged by `fetched_at` (when we last saw the book display the
/// line); `source_timestamp` is when the book last moved it and only breaks
/// ties, because an unmoved line is still a live price.
pub fn build_consensus(
    quotes: &[SourceQuote],
    max_quote_age_seconds: i64,
) -> Result<ConsensusPrice> {
    let now = Utc::now();
    let cutoff = now - chrono::Duration::seconds(max_quote_age_seconds);
    let future_limit = now + chrono::Duration::seconds(30);
    let mut newest_by_family: HashMap<SourceFamily, &SourceQuote> = HashMap::new();
    for quote in quotes.iter().filter(|quote| {
        !quote.validation_only
            && quote.fetched_at >= cutoff
            && quote.fetched_at <= future_limit
            && quote.source_timestamp <= future_limit
    }) {
        newest_by_family
            .entry(quote.family.clone())
            .and_modify(|current| {
                if (quote.fetched_at, quote.source_timestamp)
                    > (current.fetched_at, current.source_timestamp)
                {
                    *current = quote;
                }
            })
            .or_insert(quote);
    }

    let mut fair = newest_by_family
        .values()
        .map(|quote| remove_vig(quote))
        .collect::<Result<Vec<_>>>()?;
    if fair.is_empty() {
        return Err(Error::InvalidData("no fresh independent quotes".into()));
    }

    let center = median(fair.iter().map(|quote| quote.probability_a).collect())?;
    let initial_mad = median(
        fair.iter()
            .map(|quote| (quote.probability_a - center).abs())
            .collect(),
    )?;
    let outlier_limit = (initial_mad * Decimal::new(3, 0)).max(Decimal::new(25, 3));
    fair.retain(|quote| (quote.probability_a - center).abs() <= outlier_limit);
    if fair.is_empty() {
        return Err(Error::InvalidData("all quotes rejected as outliers".into()));
    }

    let probability_a = median(fair.iter().map(|quote| quote.probability_a).collect())?;
    let probability_b = median(fair.iter().map(|quote| quote.probability_b).collect())?;
    let probability_neutral = median(fair.iter().map(|quote| quote.probability_neutral).collect())?;
    let dispersion_a = median(
        fair.iter()
            .map(|quote| (quote.probability_a - probability_a).abs())
            .collect(),
    )?;
    let newest_source_timestamp = fair
        .iter()
        .map(|quote| quote.source_timestamp)
        .max()
        .ok_or_else(|| Error::InvalidData("consensus has no timestamp".into()))?;
    let has_reference = fair.iter().any(|quote| quote.family.is_reference());
    let mut source_ids = fair
        .iter()
        .map(|quote| quote.source_id.clone())
        .collect::<Vec<_>>();
    source_ids.sort();

    Ok(ConsensusPrice {
        probability_a,
        probability_b,
        probability_neutral,
        dispersion_a,
        source_count: source_ids.len(),
        family_count: fair.len(),
        has_reference,
        source_ids,
        newest_source_timestamp,
    })
}

fn median(mut values: Vec<Decimal>) -> Result<Decimal> {
    if values.is_empty() {
        return Err(Error::InvalidData(
            "cannot calculate an empty median".into(),
        ));
    }
    values.sort();
    let middle = values.len() / 2;
    Ok(if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / Decimal::TWO
    } else {
        values[middle]
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use rust_decimal::Decimal;

    use super::*;
    use crate::domain::Sport;

    fn quote(source: &str, family: SourceFamily, a: i64, b: i64) -> SourceQuote {
        SourceQuote {
            source_id: source.into(),
            family,
            sport: Sport::Nfl,
            event_id: "event".into(),
            participant_a: "A".into(),
            participant_b: "B".into(),
            participant_a_provider_ids: BTreeMap::new(),
            participant_b_provider_ids: BTreeMap::new(),
            start_time: Utc::now(),
            start_time_tolerance_minutes: 15,
            decimal_odds_a: Decimal::new(a, 2),
            decimal_odds_b: Decimal::new(b, 2),
            decimal_odds_neutral: None,
            source_timestamp: Utc::now(),
            fetched_at: Utc::now(),
            parser_version: "test".into(),
            validation_only: false,
        }
    }

    #[test]
    fn proportional_de_vig_sums_to_one() {
        let fair = remove_vig(&quote("a", SourceFamily::Circa, 180, 220)).unwrap();
        assert_eq!(fair.probability_a + fair.probability_b, Decimal::ONE);
    }

    #[test]
    fn neutral_outcome_is_split_between_contract_sides() {
        let mut value = quote("a", SourceFamily::Circa, 200, 200);
        value.decimal_odds_neutral = Some(Decimal::new(1000, 2));
        let fair = remove_vig(&value).unwrap();
        assert_eq!(fair.probability_a + fair.probability_b, Decimal::ONE);
        assert!(fair.probability_neutral > Decimal::ZERO);
    }

    #[test]
    fn consensus_deduplicates_families_and_rejects_outlier() {
        let quotes = vec![
            quote("circa", SourceFamily::Circa, 190, 200),
            quote("kambi-a", SourceFamily::Kambi, 195, 195),
            quote("kambi-b", SourceFamily::Kambi, 194, 196),
            quote("fanduel", SourceFamily::FanDuel, 192, 198),
            quote("bad", SourceFamily::Validation("bad".into()), 101, 900),
        ];
        let result = build_consensus(&quotes, 600).unwrap();
        assert_eq!(result.family_count, 3);
        assert!(result.has_reference);
        assert!(result.probability_a < Decimal::new(60, 2));
    }

    #[test]
    fn consensus_excludes_stale_and_validation_only_quotes() {
        let mut stale = quote("stale", SourceFamily::Circa, 190, 200);
        stale.fetched_at = Utc::now() - chrono::Duration::minutes(20);
        let mut validator = quote(
            "validator",
            SourceFamily::Validation("validator".into()),
            190,
            200,
        );
        validator.validation_only = true;
        let result = build_consensus(
            &[
                stale,
                validator,
                quote("pinnacle", SourceFamily::Pinnacle, 190, 200),
            ],
            300,
        )
        .unwrap();
        assert_eq!(result.family_count, 1);
        assert_eq!(result.source_ids, vec!["pinnacle"]);
    }
}
