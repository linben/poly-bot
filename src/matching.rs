use std::collections::BTreeMap;

use crate::domain::{DEFAULT_START_TIME_TOLERANCE_MINUTES, SourceQuote, UsMoneylineMarket};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteOrientation {
    Direct,
    Swapped,
}

pub fn match_quote(market: &UsMoneylineMarket, quote: &SourceQuote) -> Option<QuoteOrientation> {
    if market.sport != quote.sport {
        return None;
    }
    let tolerance = quote
        .start_time_tolerance_minutes
        .max(DEFAULT_START_TIME_TOLERANCE_MINUTES);
    let start_delta = (market.start_time - quote.start_time).num_minutes().abs();
    if start_delta > tolerance {
        return None;
    }

    let long_ids = &market.long_participant.provider_ids;
    let short_ids = &market.short_participant.provider_ids;
    if provider_overlap(long_ids, &quote.participant_a_provider_ids)
        && provider_overlap(short_ids, &quote.participant_b_provider_ids)
    {
        return Some(QuoteOrientation::Direct);
    }
    if provider_overlap(long_ids, &quote.participant_b_provider_ids)
        && provider_overlap(short_ids, &quote.participant_a_provider_ids)
    {
        return Some(QuoteOrientation::Swapped);
    }
    let comparable_provider_ids = has_shared_provider(long_ids, &quote.participant_a_provider_ids)
        || has_shared_provider(long_ids, &quote.participant_b_provider_ids)
        || has_shared_provider(short_ids, &quote.participant_a_provider_ids)
        || has_shared_provider(short_ids, &quote.participant_b_provider_ids);
    if comparable_provider_ids {
        return None;
    }

    let long = normalize_participant(&market.long_participant.name);
    let short = normalize_participant(&market.short_participant.name);
    let a = normalize_participant(&quote.participant_a);
    let b = normalize_participant(&quote.participant_b);
    let direct = names_match(&long, &a) && names_match(&short, &b);
    let swapped = names_match(&long, &b) && names_match(&short, &a);
    match (direct, swapped) {
        (true, false) => Some(QuoteOrientation::Direct),
        (false, true) => Some(QuoteOrientation::Swapped),
        // Ambiguous (e.g. "chicago" against Cubs and White Sox) or no match.
        _ => None,
    }
}

/// Exact normalized equality, or the quote name is a city/short-form prefix
/// of the market name ("kansascity" -> "kansascityroyals", "losangelesr" ->
/// "losangelesrams"). The prefix rule is only safe because the caller demands
/// that both participants resolve to distinct sides.
fn names_match(market_name: &str, quote_name: &str) -> bool {
    const MINIMUM_PREFIX: usize = 5;
    market_name == quote_name
        || (quote_name.len() >= MINIMUM_PREFIX && market_name.starts_with(quote_name))
}

pub fn orient_quote(quote: &SourceQuote, orientation: QuoteOrientation) -> SourceQuote {
    if orientation == QuoteOrientation::Direct {
        return quote.clone();
    }
    let mut oriented = quote.clone();
    std::mem::swap(&mut oriented.participant_a, &mut oriented.participant_b);
    std::mem::swap(
        &mut oriented.participant_a_provider_ids,
        &mut oriented.participant_b_provider_ids,
    );
    std::mem::swap(&mut oriented.decimal_odds_a, &mut oriented.decimal_odds_b);
    oriented
}

fn provider_overlap(left: &BTreeMap<String, String>, right: &BTreeMap<String, String>) -> bool {
    left.iter()
        .any(|(provider, id)| right.get(provider).is_some_and(|other| other == id))
}

fn has_shared_provider(left: &BTreeMap<String, String>, right: &BTreeMap<String, String>) -> bool {
    left.keys().any(|provider| right.contains_key(provider))
}

pub fn normalize_participant(value: &str) -> String {
    let replacements = [
        ("football club", ""),
        ("baseball club", ""),
        ("basketball club", ""),
        ("the ", ""),
        ("fc", ""),
    ];
    let mut normalized = value.to_lowercase();
    for (from, to) in replacements {
        normalized = normalized.replace(from, to);
    }
    normalized
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '/')
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rust_decimal::Decimal;

    use super::*;
    use crate::domain::{MarketParticipant, SourceFamily, Sport, UsMoneylineMarket};

    fn market(long: &str, short: &str) -> UsMoneylineMarket {
        UsMoneylineMarket {
            event_id: "event".into(),
            game_id: None,
            sportradar_game_id: None,
            sport: Sport::Tennis,
            start_time: Utc::now() + chrono::Duration::hours(2),
            market_id: "market".into(),
            market_slug: "market".into(),
            description: "walkover settles to $0.50".into(),
            sports_market_type: "tennis_match_winner".into(),
            long_participant: MarketParticipant {
                side_id: "a".into(),
                name: long.into(),
                long: true,
                team_id: None,
                provider_ids: BTreeMap::new(),
            },
            short_participant: MarketParticipant {
                side_id: "b".into(),
                name: short.into(),
                long: false,
                team_id: None,
                provider_ids: BTreeMap::new(),
            },
            tick_size: Decimal::new(1, 2),
            minimum_quantity: Decimal::ONE,
            fee_coefficient: Decimal::ZERO,
            ep3_status: "OPEN".into(),
            ep3_synced_at: None,
        }
    }

    fn quote(a: &str, b: &str) -> SourceQuote {
        SourceQuote {
            source_id: "source".into(),
            family: SourceFamily::Pinnacle,
            sport: Sport::Tennis,
            event_id: "other".into(),
            participant_a: a.into(),
            participant_b: b.into(),
            participant_a_provider_ids: BTreeMap::new(),
            participant_b_provider_ids: BTreeMap::new(),
            start_time: Utc::now() + chrono::Duration::hours(2),
            start_time_tolerance_minutes: 15,
            decimal_odds_a: Decimal::new(19, 1),
            decimal_odds_b: Decimal::new(2, 0),
            decimal_odds_neutral: None,
            source_timestamp: Utc::now(),
            fetched_at: Utc::now(),
            parser_version: "test".into(),
            validation_only: false,
        }
    }

    #[test]
    fn normalization_preserves_doubles_separator() {
        assert_eq!(normalize_participant("Ruehl / Veldheer"), "ruehl/veldheer");
    }

    #[test]
    fn both_participants_are_required() {
        assert_eq!(
            match_quote(&market("Bad Luck", "Yawara"), &quote("Players", "Bad Luck")),
            None
        );
    }

    #[test]
    fn provider_ids_take_precedence_over_different_names() {
        let mut market = market("LA", "New York");
        market
            .long_participant
            .provider_ids
            .insert("sportradar".into(), "team-1".into());
        market
            .short_participant
            .provider_ids
            .insert("sportradar".into(), "team-2".into());
        let mut quote = quote("Los Angeles", "NY");
        quote
            .participant_a_provider_ids
            .insert("sportradar".into(), "team-1".into());
        quote
            .participant_b_provider_ids
            .insert("sportradar".into(), "team-2".into());
        assert_eq!(match_quote(&market, &quote), Some(QuoteOrientation::Direct));
    }

    #[test]
    fn conflicting_provider_ids_reject_matching_names() {
        let mut market = market("LA", "New York");
        market
            .long_participant
            .provider_ids
            .insert("sportradar".into(), "team-1".into());
        market
            .short_participant
            .provider_ids
            .insert("sportradar".into(), "team-2".into());
        let mut quote = quote("LA", "New York");
        quote
            .participant_a_provider_ids
            .insert("sportradar".into(), "different-1".into());
        quote
            .participant_b_provider_ids
            .insert("sportradar".into(), "different-2".into());
        assert_eq!(match_quote(&market, &quote), None);
    }

    #[test]
    fn tennis_doubles_match_in_swapped_orientation() {
        let market = market("Ruehl / Veldheer", "Jones / Smith");
        let quote = quote("Jones/Smith", "Ruehl/Veldheer");
        assert_eq!(
            match_quote(&market, &quote),
            Some(QuoteOrientation::Swapped)
        );
    }

    #[test]
    fn city_prefix_matches_when_both_sides_are_distinct() {
        let mut market = market("Kansas City Royals", "Boston Red Sox");
        market.sport = Sport::Mlb;
        let mut quote = quote("Boston", "Kansas City");
        quote.sport = Sport::Mlb;
        assert_eq!(
            match_quote(&market, &quote),
            Some(QuoteOrientation::Swapped)
        );
    }

    #[test]
    fn ambiguous_prefix_is_rejected() {
        let mut market = market("Chicago Cubs", "Chicago White Sox");
        market.sport = Sport::Mlb;
        let mut quote = quote("Chicago", "Chicago");
        quote.sport = Sport::Mlb;
        assert_eq!(match_quote(&market, &quote), None);
    }

    #[test]
    fn date_only_quotes_use_their_wider_tolerance() {
        let market = market("Ruehl / Veldheer", "Jones / Smith");
        let mut quote = quote("Ruehl/Veldheer", "Jones/Smith");
        quote.start_time = market.start_time - chrono::Duration::hours(9);
        assert_eq!(match_quote(&market, &quote), None);
        quote.start_time_tolerance_minutes = 14 * 60;
        assert_eq!(match_quote(&market, &quote), Some(QuoteOrientation::Direct));
    }
}
