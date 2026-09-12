//! Polymarket global (non-US) moneyline quotes from the public Gamma API.
//!
//! Environment:
//! - `POLYMARKET_GAMMA_BASE_URL` overrides the Gamma host
//!   (default `https://gamma-api.polymarket.com`).
//! - `POLYMARKET_GLOBAL_MIN_LIQUIDITY` is the minimum `liquidityNum` in USD a
//!   market needs before its price is trusted (Decimal, default `2000`).
//! - `POLYMARKET_GLOBAL_MIN_VOLUME` is the minimum traded `volumeNum` in USD
//!   (Decimal, default `250`). A freshly listed market carries market-maker
//!   liquidity seeded near 50/50 before anyone has traded it; observed live on
//!   ITF tennis at 0.45/0.46 while the US book sat at 0.93. Volume is the
//!   signal that the price has been tested.

use std::{collections::BTreeMap, env, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{
    Error, Result,
    domain::{
        DEFAULT_START_TIME_TOLERANCE_MINUTES, SourceFamily, SourceHealth, SourceQuote, Sport,
    },
    sources::{OddsSource, http_client, probe_by_collecting},
};

const SOURCE_ID: &str = "polymarket_global";
const PARSER_VERSION: &str = "gamma-moneyline-v1";
const DEFAULT_BASE_URL: &str = "https://gamma-api.polymarket.com";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 5;
/// Widest bid/ask spread (in price units) still considered a real two-sided market.
const MAX_SPREAD: Decimal = Decimal::from_parts(4, 0, 0, false, 2);
const DEFAULT_MIN_LIQUIDITY: Decimal = Decimal::from_parts(2000, 0, 0, false, 0);
const DEFAULT_MIN_VOLUME: Decimal = Decimal::from_parts(250, 0, 0, false, 0);

/// Floors a Gamma market must clear before its price enters consensus.
#[derive(Debug, Clone, Copy)]
pub struct MarketFloors {
    pub min_liquidity: Decimal,
    pub min_volume: Decimal,
}

impl Default for MarketFloors {
    fn default() -> Self {
        Self {
            min_liquidity: DEFAULT_MIN_LIQUIDITY,
            min_volume: DEFAULT_MIN_VOLUME,
        }
    }
}

pub struct PolymarketGlobalSource {
    base_url: String,
    floors: MarketFloors,
    client: reqwest::Client,
}

impl PolymarketGlobalSource {
    pub fn new(timeout: Duration) -> Result<Self> {
        let floors = MarketFloors {
            min_liquidity: decimal_env("POLYMARKET_GLOBAL_MIN_LIQUIDITY", DEFAULT_MIN_LIQUIDITY)?,
            min_volume: decimal_env("POLYMARKET_GLOBAL_MIN_VOLUME", DEFAULT_MIN_VOLUME)?,
        };
        Ok(Self {
            base_url: env::var("POLYMARKET_GAMMA_BASE_URL")
                .map(|url| url.trim_end_matches('/').to_owned())
                .unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            floors,
            client: http_client(timeout)?,
        })
    }

    async fn fetch_page(&self, tag: &str, offset: usize) -> Result<Vec<GammaEvent>> {
        let url = format!("{}/events", self.base_url);
        let offset = offset.to_string();
        let limit = PAGE_SIZE.to_string();
        Ok(self
            .client
            .get(url)
            .query(&[
                ("tag_slug", tag),
                ("active", "true"),
                ("closed", "false"),
                ("limit", limit.as_str()),
                ("offset", offset.as_str()),
                ("order", "startDate"),
                ("ascending", "false"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

fn decimal_env(name: &str, default: Decimal) -> Result<Decimal> {
    match env::var(name) {
        Ok(raw) => Decimal::from_str_exact(raw.trim())
            .map_err(|error| Error::Config(format!("{name} {raw:?}: {error}"))),
        Err(_) => Ok(default),
    }
}

fn tag_slug(sport: Sport) -> &'static str {
    match sport {
        Sport::Nfl => "nfl",
        Sport::Nba => "nba",
        Sport::Wnba => "wnba",
        Sport::Mlb => "mlb",
        Sport::Tennis => "tennis",
    }
}

impl PolymarketGlobalSource {
    async fn collect_sport(&self, sport: Sport) -> Result<Vec<SourceQuote>> {
        let tag = tag_slug(sport);
        let mut quotes = Vec::new();
        for page in 0..MAX_PAGES {
            let events = self.fetch_page(tag, page * PAGE_SIZE).await?;
            let full_page = events.len() >= PAGE_SIZE;
            let fetched_at = Utc::now();
            for event in events {
                for market in &event.markets {
                    if let Some(quote) =
                        normalize_market(sport, &event.teams, market, self.floors, fetched_at)
                    {
                        quotes.push(quote);
                    }
                }
            }
            if !full_page {
                break;
            }
        }
        Ok(quotes)
    }
}
#[async_trait]
impl OddsSource for PolymarketGlobalSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    fn family(&self) -> SourceFamily {
        SourceFamily::PolymarketGlobal
    }

    async fn collect(&self, sports: &[Sport]) -> Result<Vec<SourceQuote>> {
        let per_sport =
            futures::future::try_join_all(sports.iter().map(|&sport| self.collect_sport(sport)))
                .await?;
        Ok(per_sport.into_iter().flatten().collect())
    }

    async fn probe(&self) -> SourceHealth {
        probe_by_collecting(self, &Sport::ALL).await
    }
}

/// Turn one Gamma market into a quote. Returns `None` for markets that are
/// not tradable two-sided moneylines (expected, silent) and for markets whose
/// payload cannot be parsed (logged, so a schema drift is visible).
fn normalize_market(
    sport: Sport,
    teams: &[GammaTeam],
    market: &GammaMarket,
    floors: MarketFloors,
    fetched_at: DateTime<Utc>,
) -> Option<SourceQuote> {
    if market.sports_market_type.as_deref() != Some("moneyline")
        || !market.accepting_orders
        || !market.active
        || market.closed
    {
        return None;
    }
    let (Some(bid), Some(ask)) = (market.best_bid, market.best_ask) else {
        return None;
    };
    let liquidity = Decimal::from_f64_retain(market.liquidity_num.unwrap_or(0.0))?;
    let volume = Decimal::from_f64_retain(market.volume_num.unwrap_or(0.0))?;
    if liquidity < floors.min_liquidity || volume < floors.min_volume {
        return None;
    }
    let bid = Decimal::from_f64_retain(bid)?.round_dp(4);
    let ask = Decimal::from_f64_retain(ask)?.round_dp(4);
    if !(bid > Decimal::ZERO && bid < ask && ask < Decimal::ONE) || ask - bid > MAX_SPREAD {
        return None;
    }

    let outcomes: Vec<String> = match serde_json::from_str(&market.outcomes) {
        Ok(outcomes) => outcomes,
        Err(error) => {
            tracing::warn!(market = %market.id, %error, "polymarket_global: bad outcomes payload");
            return None;
        }
    };
    let [participant_a, participant_b] = <[String; 2]>::try_from(outcomes).ok()?;
    if participant_a.eq_ignore_ascii_case("yes") || participant_b.eq_ignore_ascii_case("no") {
        return None;
    }
    let participant_a = full_name(participant_a, teams);
    let participant_b = full_name(participant_b, teams);
    let Some(start_time) = market
        .game_start_time
        .as_deref()
        .and_then(parse_game_start_time)
    else {
        tracing::warn!(
            market = %market.id,
            value = ?market.game_start_time,
            "polymarket_global: unparseable gameStartTime"
        );
        return None;
    };
    let source_timestamp = match &market.updated_at {
        Some(raw) => match DateTime::parse_from_rfc3339(raw) {
            Ok(updated) => updated.with_timezone(&Utc),
            Err(error) => {
                tracing::warn!(market = %market.id, %error, "polymarket_global: bad updatedAt");
                return None;
            }
        },
        None => fetched_at,
    };

    let (ids_a, ids_b) = provider_ids(market);
    Some(SourceQuote {
        source_id: SOURCE_ID.into(),
        family: SourceFamily::PolymarketGlobal,
        sport,
        event_id: market.id.clone(),
        participant_a,
        participant_b,
        participant_a_provider_ids: ids_a,
        participant_b_provider_ids: ids_b,
        start_time,
        start_time_tolerance_minutes: DEFAULT_START_TIME_TOLERANCE_MINUTES,
        decimal_odds_a: (Decimal::ONE / ask).round_dp(4),
        decimal_odds_b: (Decimal::ONE / (Decimal::ONE - bid)).round_dp(4),
        decimal_odds_neutral: None,
        source_timestamp,
        fetched_at,
        parser_version: PARSER_VERSION.into(),
        validation_only: false,
    })
}

/// Per-side provider ids. The CLOB token id is the only identifier Gamma
/// exposes that differs between the two outcomes; when it is absent the
/// matcher falls back to names rather than seeing both sides share one id.
fn provider_ids(market: &GammaMarket) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let tokens = market
        .clob_token_ids
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .and_then(|tokens| <[String; 2]>::try_from(tokens).ok());
    match tokens {
        Some([token_a, token_b]) => (
            BTreeMap::from([(SOURCE_ID.to_owned(), token_a)]),
            BTreeMap::from([(SOURCE_ID.to_owned(), token_b)]),
        ),
        None => (BTreeMap::new(), BTreeMap::new()),
    }
}

/// NFL outcomes are short aliases ("Colts"); the event's `teams` list carries
/// the full name the matcher expects ("Indianapolis Colts"). Other leagues
/// already use full names and pass through unchanged.
fn full_name(outcome: String, teams: &[GammaTeam]) -> String {
    teams
        .iter()
        .find(|team| {
            team.name.eq_ignore_ascii_case(&outcome)
                || team
                    .alias
                    .as_deref()
                    .is_some_and(|alias| alias.eq_ignore_ascii_case(&outcome))
                || team
                    .abbreviation
                    .as_deref()
                    .is_some_and(|abbreviation| abbreviation.eq_ignore_ascii_case(&outcome))
        })
        .map_or(outcome, |team| team.name.clone())
}

/// Gamma writes `gameStartTime` as Postgres-style `2026-09-22 17:05:00+00`,
/// occasionally with fractional seconds or a `+HH:MM` offset; some markets
/// use RFC 3339. A missing or bare offset is UTC.
fn parse_game_start_time(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(raw.get(..19)?, "%Y-%m-%d %H:%M:%S").ok()?;
    let mut rest = raw.get(19..)?;
    if let Some(stripped) = rest.strip_prefix('.') {
        let digits = stripped.trim_start_matches(|c: char| c.is_ascii_digit());
        rest = digits;
    }
    let offset = match rest.trim() {
        "" | "Z" | "z" => FixedOffset::east_opt(0)?,
        offset => parse_offset(offset)?,
    };
    offset
        .from_local_datetime(&naive)
        .single()
        .map(|local| local.with_timezone(&Utc))
}

/// `+00`, `-05`, `+02:00`, `+0200`.
fn parse_offset(raw: &str) -> Option<FixedOffset> {
    let (sign, digits) = match raw.as_bytes().first()? {
        b'+' => (1, &raw[1..]),
        b'-' => (-1, &raw[1..]),
        _ => return None,
    };
    let hours: i32 = digits.get(..2)?.parse().ok()?;
    let minutes: i32 = match digits.get(2..).unwrap_or("") {
        "" => 0,
        tail => tail.trim_start_matches(':').parse().ok()?,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

#[derive(Deserialize)]
struct GammaEvent {
    #[serde(default)]
    markets: Vec<GammaMarket>,
    #[serde(default)]
    teams: Vec<GammaTeam>,
}

#[derive(Deserialize)]
struct GammaTeam {
    name: String,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    abbreviation: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    id: String,
    #[serde(default)]
    sports_market_type: Option<String>,
    #[serde(default)]
    game_start_time: Option<String>,
    /// JSON-encoded array of outcome names, e.g. `"[\"Rays\", \"Yankees\"]"`.
    #[serde(default)]
    outcomes: String,
    #[serde(default)]
    clob_token_ids: Option<String>,
    #[serde(default)]
    best_bid: Option<f64>,
    #[serde(default)]
    best_ask: Option<f64>,
    #[serde(default)]
    liquidity_num: Option<f64>,
    #[serde(default)]
    volume_num: Option<f64>,
    #[serde(default)]
    accepting_orders: bool,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    updated_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT_FIXTURE: &str = r#"[{
        "id": "12345",
        "title": "Tampa Bay Rays vs. New York Yankees",
        "teams": [
            {"id": 1, "name": "Tampa Bay Rays", "alias": "Rays", "abbreviation": "tb"},
            {"id": 2, "name": "New York Yankees", "alias": "Yankees", "abbreviation": "nyy"}
        ],
        "markets": [
            {
                "id": "608540",
                "question": "Tampa Bay Rays vs. New York Yankees",
                "sportsMarketType": "moneyline",
                "gameStartTime": "2026-09-22 17:05:00+00",
                "outcomes": "[\"Rays\", \"New York Yankees\"]",
                "outcomePrices": "[\"0.425\", \"0.575\"]",
                "bestBid": 0.42,
                "bestAsk": 0.43,
                "liquidityNum": 118054.37,
                "volumeNum": 41000.5,
                "acceptingOrders": true,
                "active": true,
                "closed": false,
                "updatedAt": "2026-09-12T17:36:51.772231Z",
                "clobTokenIds": "[\"9564\", \"4368\"]"
            },
            {
                "id": "608541",
                "question": "Rays vs. Yankees: run in the first inning?",
                "sportsMarketType": "nrfi",
                "gameStartTime": "2026-09-22 17:05:00+00",
                "outcomes": "[\"Yes\", \"No\"]",
                "bestBid": 0.5,
                "bestAsk": 0.52,
                "liquidityNum": 50000,
                "acceptingOrders": true,
                "active": true,
                "closed": false,
                "updatedAt": "2026-09-12T17:36:51.772231Z"
            },
            {
                "id": "608542",
                "question": "Thin market",
                "sportsMarketType": "moneyline",
                "gameStartTime": "2026-09-22 17:05:00+00",
                "outcomes": "[\"Tampa Bay Rays\", \"New York Yankees\"]",
                "bestBid": 0.42,
                "bestAsk": 0.43,
                "liquidityNum": 1999.99,
                "volumeNum": 900,
                "acceptingOrders": true,
                "active": true,
                "closed": false,
                "updatedAt": "2026-09-12T17:36:51.772231Z"
            },
            {
                "id": "608543",
                "question": "Seeded, never traded",
                "sportsMarketType": "moneyline",
                "gameStartTime": "2026-09-22 17:05:00+00",
                "outcomes": "[\"Tampa Bay Rays\", \"New York Yankees\"]",
                "bestBid": 0.45,
                "bestAsk": 0.46,
                "liquidityNum": 2538.47,
                "volumeNum": 85,
                "acceptingOrders": true,
                "active": true,
                "closed": false,
                "updatedAt": "2026-09-12T17:36:51.772231Z"
            }
        ]
    }]"#;

    fn quotes(fixture: &str) -> Vec<SourceQuote> {
        let events: Vec<GammaEvent> = serde_json::from_str(fixture).unwrap();
        let fetched_at = Utc::now();
        events
            .iter()
            .flat_map(|event| event.markets.iter().map(move |market| (event, market)))
            .filter_map(|(event, market)| {
                normalize_market(
                    Sport::Mlb,
                    &event.teams,
                    market,
                    MarketFloors::default(),
                    fetched_at,
                )
            })
            .collect()
    }

    #[test]
    fn parses_postgres_style_game_start_time() {
        let expected = Utc.with_ymd_and_hms(2026, 9, 22, 17, 5, 0).unwrap();
        assert_eq!(
            parse_game_start_time("2026-09-22 17:05:00+00"),
            Some(expected)
        );
        assert_eq!(
            parse_game_start_time("2026-09-22 19:05:00+02"),
            Some(expected)
        );
        assert_eq!(
            parse_game_start_time("2026-09-22 12:05:00.123-05:00"),
            Some(expected + chrono::Duration::milliseconds(123))
        );
        assert_eq!(
            parse_game_start_time("2026-09-22T17:05:00Z"),
            Some(expected)
        );
        assert_eq!(parse_game_start_time("tomorrow"), None);
    }

    #[test]
    fn normalizes_moneyline_and_drops_non_moneyline_and_thin_markets() {
        let quotes = quotes(EVENT_FIXTURE);
        assert_eq!(
            quotes.len(),
            1,
            "nrfi and sub-floor markets must be dropped"
        );
        let quote = &quotes[0];
        assert_eq!(quote.source_id, "polymarket_global");
        assert_eq!(quote.family, SourceFamily::PolymarketGlobal);
        assert_eq!(quote.event_id, "608540");
        assert_eq!(quote.participant_a, "Tampa Bay Rays");
        assert_eq!(quote.participant_b, "New York Yankees");
        assert_eq!(
            quote.decimal_odds_a,
            Decimal::from_str_exact("2.3256").unwrap()
        );
        assert_eq!(
            quote.decimal_odds_b,
            Decimal::from_str_exact("1.7241").unwrap()
        );
        assert_eq!(
            quote.start_time,
            Utc.with_ymd_and_hms(2026, 9, 22, 17, 5, 0).unwrap()
        );
        assert_eq!(
            quote.source_timestamp,
            DateTime::parse_from_rfc3339("2026-09-12T17:36:51.772231Z").unwrap()
        );
        assert_eq!(
            quote.start_time_tolerance_minutes,
            DEFAULT_START_TIME_TOLERANCE_MINUTES
        );
        assert_eq!(quote.parser_version, "gamma-moneyline-v1");
        assert_eq!(
            quote.participant_a_provider_ids["polymarket_global"],
            "9564"
        );
        assert_eq!(
            quote.participant_b_provider_ids["polymarket_global"],
            "4368"
        );
        assert!(!quote.validation_only);
    }

    #[test]
    fn drops_wide_spreads_and_markets_not_accepting_orders() {
        let wide = EVENT_FIXTURE.replacen("\"bestAsk\": 0.43", "\"bestAsk\": 0.47", 1);
        assert!(quotes(&wide).is_empty());
        let halted =
            EVENT_FIXTURE.replacen("\"acceptingOrders\": true", "\"acceptingOrders\": false", 1);
        assert!(quotes(&halted).is_empty());
    }
}
