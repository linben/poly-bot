use std::{collections::BTreeMap, fmt};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sport {
    Nfl,
    Nba,
    Wnba,
    Mlb,
    Tennis,
}

impl Sport {
    pub const ALL: [Self; 5] = [Self::Nfl, Self::Nba, Self::Wnba, Self::Mlb, Self::Tennis];

    pub fn discovery_path(self) -> &'static str {
        match self {
            Self::Nfl => "/v2/leagues/nfl/events",
            Self::Nba => "/v2/leagues/nba/events",
            Self::Wnba => "/v2/leagues/wnba/events",
            Self::Mlb => "/v2/leagues/mlb/events",
            Self::Tennis => "/v2/sports/tennis/events",
        }
    }

    pub fn moneyline_type(self) -> &'static str {
        match self {
            Self::Nfl => "football_team_full_game_winner",
            Self::Nba | Self::Wnba => "basketball_team_full_game_winner",
            Self::Mlb => "baseball_team_full_game_winner",
            Self::Tennis => "tennis_match_winner",
        }
    }
}

impl fmt::Display for Sport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", format!("{self:?}").to_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventKey {
    pub sport: Sport,
    pub participant_a: String,
    pub participant_b: String,
    pub start_time: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFamily {
    Pinnacle,
    Circa,
    Bookmaker,
    BetOnline,
    Bet365,
    DraftKings,
    FanDuel,
    Caesars,
    BetMgm,
    Fanatics,
    Kambi,
    HardRock,
    Penn,
    Bovada,
    LowVig,
    /// Kalshi regulated exchange; independent order flow, not a sportsbook.
    Kalshi,
    /// Polymarket global CLOB; separate participant pool from Polymarket US.
    PolymarketGlobal,
    /// A real book or exchange without a dedicated variant. Counts as its own
    /// family but can never be a reference book.
    Other(String),
    Validation(String),
}

impl SourceFamily {
    pub fn is_reference(&self) -> bool {
        matches!(
            self,
            Self::Pinnacle | Self::Circa | Self::Bookmaker | Self::BetOnline
        )
    }

    /// Maps a The Odds API bookmaker key onto the family that owns it so
    /// skins of one operator are never counted twice.
    pub fn from_odds_api_key(key: &str) -> Self {
        match key {
            "pinnacle" => Self::Pinnacle,
            "circasports" => Self::Circa,
            "bookmaker" => Self::Bookmaker,
            "betonlineag" => Self::BetOnline,
            "bet365" | "bet365_au" => Self::Bet365,
            "draftkings" => Self::DraftKings,
            "fanduel" => Self::FanDuel,
            "williamhill_us" | "williamhill" | "caesars" => Self::Caesars,
            "betmgm" | "betmgm_uk" => Self::BetMgm,
            "fanatics" => Self::Fanatics,
            "betrivers" | "unibet_us" | "unibet_uk" | "unibet_eu" => Self::Kambi,
            "hardrockbet" | "hardrockbet_az" | "hardrockbet_fl" | "hardrockbet_oh" => {
                Self::HardRock
            }
            "espnbet" | "thescorebet" => Self::Penn,
            "bovada" => Self::Bovada,
            "lowvig" => Self::LowVig,
            "kalshi" => Self::Kalshi,
            "polymarket" => Self::PolymarketGlobal,
            other => Self::Other(other.to_string()),
        }
    }
}

pub const DEFAULT_START_TIME_TOLERANCE_MINUTES: i64 = 15;

fn default_start_time_tolerance() -> i64 {
    DEFAULT_START_TIME_TOLERANCE_MINUTES
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceQuote {
    pub source_id: String,
    pub family: SourceFamily,
    pub sport: Sport,
    pub event_id: String,
    pub participant_a: String,
    pub participant_b: String,
    pub participant_a_provider_ids: BTreeMap<String, String>,
    pub participant_b_provider_ids: BTreeMap<String, String>,
    pub start_time: DateTime<Utc>,
    /// Widest start-time mismatch the matcher may accept. Sources that only
    /// publish a calendar date set this large enough to cover the day.
    #[serde(default = "default_start_time_tolerance")]
    pub start_time_tolerance_minutes: i64,
    #[serde(with = "rust_decimal::serde::str")]
    pub decimal_odds_a: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub decimal_odds_b: Decimal,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub decimal_odds_neutral: Option<Decimal>,
    pub source_timestamp: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
    pub parser_version: String,
    pub validation_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FairQuote {
    pub source_id: String,
    pub family: SourceFamily,
    #[serde(with = "rust_decimal::serde::str")]
    pub probability_a: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub probability_b: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub probability_neutral: Decimal,
    pub source_timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusPrice {
    #[serde(with = "rust_decimal::serde::str")]
    pub probability_a: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub probability_b: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub probability_neutral: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub dispersion_a: Decimal,
    pub source_count: usize,
    pub family_count: usize,
    pub has_reference: bool,
    pub source_ids: Vec<String>,
    pub newest_source_timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeSide {
    Long,
    Short,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookLevel {
    #[serde(with = "rust_decimal::serde::str")]
    pub yes_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketBook {
    pub market_slug: String,
    pub bids: Vec<BookLevel>,
    pub offers: Vec<BookLevel>,
    pub state: String,
    /// Last time the venue changed this book. An unchanged book is still live.
    pub transact_time: DateTime<Utc>,
    /// When this snapshot was retrieved; freshness is judged against this.
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketParticipant {
    pub side_id: String,
    pub name: String,
    pub long: bool,
    pub team_id: Option<i64>,
    pub provider_ids: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsMoneylineMarket {
    pub event_id: String,
    pub game_id: Option<i64>,
    pub sportradar_game_id: Option<String>,
    pub sport: Sport,
    pub start_time: DateTime<Utc>,
    pub market_id: String,
    pub market_slug: String,
    pub description: String,
    pub sports_market_type: String,
    pub long_participant: MarketParticipant,
    pub short_participant: MarketParticipant,
    #[serde(with = "rust_decimal::serde::str")]
    pub tick_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub minimum_quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fee_coefficient: Decimal,
    pub ep3_status: String,
    pub ep3_synced_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationClass {
    Actionable,
    Watchlist,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Opportunity {
    pub id: Uuid,
    pub generated_at: DateTime<Utc>,
    pub class: RecommendationClass,
    pub sport: Sport,
    pub event_id: String,
    pub market_id: String,
    pub market_slug: String,
    pub participant: String,
    pub side: OutcomeSide,
    #[serde(with = "rust_decimal::serde::str")]
    pub fair_probability: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub conservative_probability: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub executable_price: Decimal,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub maker_price: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    pub raw_edge: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub net_edge: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub maximum_loss: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub estimated_fee: Decimal,
    pub source_count: usize,
    pub family_count: usize,
    pub source_ids: Vec<String>,
    /// Scheduled start of the underlying game or match.
    pub start_time: DateTime<Utc>,
    pub book_time: DateTime<Utc>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperPortfolio {
    #[serde(with = "rust_decimal::serde::str")]
    pub bankroll: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub open_exposure: Decimal,
    pub open_positions: Vec<PaperPosition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperPosition {
    pub opportunity_id: Uuid,
    pub event_id: String,
    pub market_id: String,
    pub side: OutcomeSide,
    #[serde(with = "rust_decimal::serde::str")]
    pub maximum_loss: Decimal,
    pub opened_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceHealth {
    pub source_id: String,
    pub family: SourceFamily,
    pub checked_at: DateTime<Utc>,
    pub reachable: bool,
    pub odds_found: usize,
    pub latency_ms: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsEvidence {
    pub opportunity_id: Uuid,
    pub generated_at: DateTime<Utc>,
    pub summary: String,
    pub confidence_effect: String,
    pub manual_review: bool,
    pub citations: Vec<NewsCitation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsCitation {
    pub title: String,
    pub url: String,
    pub published_at: Option<DateTime<Utc>>,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchOpportunity {
    #[serde(flatten)]
    pub opportunity: Opportunity,
    pub effective_class: RecommendationClass,
    pub news: Option<NewsEvidence>,
}

impl ResearchOpportunity {
    pub fn new(opportunity: Opportunity, news: Option<NewsEvidence>) -> Self {
        let effective_class = effective_recommendation(opportunity.class, news.as_ref());
        Self {
            opportunity,
            effective_class,
            news,
        }
    }
}

fn effective_recommendation(
    recommendation: RecommendationClass,
    news: Option<&NewsEvidence>,
) -> RecommendationClass {
    let fresh_news = news.filter(|evidence| {
        evidence.generated_at >= Utc::now() - chrono::Duration::hours(2)
            && evidence.generated_at <= Utc::now() + chrono::Duration::minutes(1)
    });
    match (recommendation, fresh_news) {
        (RecommendationClass::Rejected, _) => RecommendationClass::Rejected,
        (_, Some(evidence)) if evidence.confidence_effect == "reject" => {
            RecommendationClass::Rejected
        }
        (_, Some(evidence))
            if evidence.manual_review
                || matches!(evidence.confidence_effect.as_str(), "lower" | "review") =>
        {
            RecommendationClass::Watchlist
        }
        (_, Some(_)) => recommendation,
        _ => RecommendationClass::Watchlist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(effect: &str, manual_review: bool) -> NewsEvidence {
        NewsEvidence {
            opportunity_id: Uuid::new_v4(),
            generated_at: Utc::now(),
            summary: "test".into(),
            confidence_effect: effect.into(),
            manual_review,
            citations: Vec::new(),
        }
    }

    #[test]
    fn actionable_requires_completed_unchanged_news() {
        assert_eq!(
            effective_recommendation(RecommendationClass::Actionable, None),
            RecommendationClass::Watchlist
        );
        assert_eq!(
            effective_recommendation(
                RecommendationClass::Actionable,
                Some(&evidence("unchanged", false))
            ),
            RecommendationClass::Actionable
        );
        assert_eq!(
            effective_recommendation(
                RecommendationClass::Actionable,
                Some(&evidence("lower", false))
            ),
            RecommendationClass::Watchlist
        );
        assert_eq!(
            effective_recommendation(
                RecommendationClass::Actionable,
                Some(&evidence("reject", false))
            ),
            RecommendationClass::Rejected
        );
        let mut stale = evidence("unchanged", false);
        stale.generated_at = Utc::now() - chrono::Duration::hours(3);
        assert_eq!(
            effective_recommendation(RecommendationClass::Actionable, Some(&stale)),
            RecommendationClass::Watchlist
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSnapshot {
    pub scan_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub market_count: usize,
    pub quote_count: usize,
    pub opportunities: Vec<Opportunity>,
    pub source_health: Vec<SourceHealth>,
}

/// Everything about a scan except its opportunity rows; small enough for a
/// single DynamoDB item and cheap for a UI to poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSummary {
    pub scan_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub market_count: usize,
    pub quote_count: usize,
    pub evaluated_count: usize,
    pub candidate_count: usize,
    pub source_health: Vec<SourceHealth>,
}

impl ScanSnapshot {
    pub fn summary(&self) -> ScanSummary {
        ScanSummary {
            scan_id: self.scan_id,
            started_at: self.started_at,
            completed_at: self.completed_at,
            market_count: self.market_count,
            quote_count: self.quote_count,
            evaluated_count: self.opportunities.len(),
            candidate_count: self
                .opportunities
                .iter()
                .filter(|item| item.class != RecommendationClass::Rejected)
                .count(),
            source_health: self.source_health.clone(),
        }
    }
}
