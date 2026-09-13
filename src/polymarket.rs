use std::{cmp::Reverse, collections::BTreeMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use futures::{StreamExt, future, stream};
use reqwest::{Client, StatusCode};
use rust_decimal::Decimal;
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{
    sync::{Mutex, Semaphore},
    time::Instant,
};

use crate::{
    Error, Result,
    domain::{BookLevel, MarketBook, MarketParticipant, Sport, UsMoneylineMarket},
};

/// Polymarket US public gateway. Measured 2026-09: book requests answer in
/// ~20 ms and 32 concurrent requests were not throttled, but the documented
/// public limit is 20 requests/second/IP, so request starts are paced (18/s
/// by default) and a 429 is retried once after backing off for a second. An
/// NFL events page is 42 MB uncompressed (1.4 MB gzip) and takes ~2 s
/// server-side, so discovery runs all sports concurrently and callers cache
/// the result.
#[derive(Clone)]
pub struct PolymarketUsClient {
    base_url: String,
    client: Client,
    permits: Arc<Semaphore>,
    concurrency: usize,
    /// Earliest instant the next request may start; shared by clones.
    pacer: Arc<Mutex<Instant>>,
    min_interval: Duration,
}

/// Pre-game closing line for a market: side prices at the last observation
/// before the scheduled start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosingPrice {
    pub long_price: Decimal,
    pub short_price: Decimal,
    pub observed_at: DateTime<Utc>,
}

impl PolymarketUsClient {
    const DEFAULT_REQUESTS_PER_SECOND: u32 = 18;
    const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(1);

    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Result<Self> {
        Self::with_concurrency(base_url, timeout, 8)
    }

    pub fn with_concurrency(
        base_url: impl Into<String>,
        timeout: Duration,
        concurrency: usize,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent(crate::sources::USER_AGENT)
            .gzip(true)
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            concurrency: concurrency.max(1),
            pacer: Arc::new(Mutex::new(Instant::now())),
            min_interval: Self::interval_for(Self::DEFAULT_REQUESTS_PER_SECOND),
        })
    }

    /// Space request starts so no more than `requests_per_second` begin in
    /// any one-second window.
    pub fn with_rate_limit(mut self, requests_per_second: u32) -> Self {
        self.min_interval = Self::interval_for(requests_per_second);
        self
    }

    fn interval_for(requests_per_second: u32) -> Duration {
        Duration::from_secs(1) / requests_per_second.max(1)
    }

    pub async fn discover_moneylines(&self) -> Result<Vec<UsMoneylineMarket>> {
        let per_sport =
            future::try_join_all(Sport::ALL.iter().map(|sport| self.discover_sport(*sport)))
                .await?;
        let mut markets = per_sport.into_iter().flatten().collect::<Vec<_>>();
        markets.sort_by_key(|market| market.start_time);
        Ok(markets)
    }

    pub async fn discover_sport(&self, sport: Sport) -> Result<Vec<UsMoneylineMarket>> {
        let mut result = Vec::new();
        let limit = 100;
        for page in 0..20 {
            let url = format!(
                "{}{}?limit={limit}&offset={}",
                self.base_url,
                sport.discovery_path(),
                page * limit
            );
            let payload: EventsResponse = self.get_json(&url).await?;
            let count = payload.events.len();
            for event in payload.events {
                let event_id = event.id.clone();
                match normalize_event(sport, event) {
                    Ok(markets) => result.extend(markets),
                    Err(error) => {
                        tracing::debug!(%sport, %event_id, %error, "event normalization failed");
                    }
                }
            }
            if count < limit {
                break;
            }
        }
        Ok(result)
    }

    pub async fn fetch_book(&self, market_slug: &str) -> Result<MarketBook> {
        let url = format!(
            "{}/v1/markets/{}/book",
            self.base_url,
            encode_slug(market_slug)
        );
        let payload: BookResponse = self.get_json(&url).await?;
        normalize_book(payload.market_data)
    }

    /// Fetch many books concurrently, bounded by the client's permit count.
    /// Results keep input order; individual failures do not fail the batch.
    pub async fn fetch_books(&self, slugs: &[&str]) -> Vec<Result<MarketBook>> {
        let requests = slugs
            .iter()
            .map(|slug| self.fetch_book(slug))
            .collect::<Vec<_>>();
        stream::iter(requests)
            .buffered(self.concurrency * 2)
            .collect()
            .await
    }

    /// YES payout per contract (0..=1) once the market has settled; `None`
    /// while the venue has not published a settlement (404).
    pub async fn fetch_settlement(&self, market_slug: &str) -> Result<Option<Decimal>> {
        let url = format!(
            "{}/v1/markets/{}/settlement",
            self.base_url,
            encode_slug(market_slug)
        );
        let Some(payload) = self.get_json_opt::<SettlementResponse>(&url).await? else {
            return Ok(None);
        };
        decimal_from_value(&payload.settlement, "settlement").map(Some)
    }

    /// Closing line: the last price-history point at or before `start_time`,
    /// or the earliest point when the series starts after it (the live series
    /// begins 15 minutes before start). `None` when no history exists.
    pub async fn fetch_closing_price(
        &self,
        market_slug: &str,
        start_time: DateTime<Utc>,
    ) -> Result<Option<ClosingPrice>> {
        let url = format!(
            "{}/v1/price-history?symbol={}&fixedInterval=INTERVAL_LIVE&fidelity=1",
            self.base_url,
            encode_slug(market_slug)
        );
        let Some(payload) = self.get_json_opt::<PriceHistoryResponse>(&url).await? else {
            return Ok(None);
        };
        select_closing_point(&payload.history, start_time)
            .map(|point| {
                Ok(ClosingPrice {
                    long_price: decimal_from_f64(point.long_price, "longPrice")?,
                    short_price: decimal_from_f64(point.short_price, "shortPrice")?,
                    observed_at: DateTime::from_timestamp(point.timestamp, 0).ok_or_else(|| {
                        Error::InvalidData(format!(
                            "price history timestamp out of range: {}",
                            point.timestamp
                        ))
                    })?,
                })
            })
            .transpose()
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.get_json_opt(url)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("gateway returned 404 for {url}")))
    }

    /// Every gateway GET: bounded by the permit count, paced by the shared
    /// rate limiter, retried once after a 429. A 404 is `Ok(None)`.
    async fn get_json_opt<T: DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        let _permit = self.permits.acquire().await.expect("semaphore open");
        let mut retried = false;
        loop {
            self.pace().await;
            let response = self.client.get(url).send().await?;
            match response.status() {
                StatusCode::NOT_FOUND => return Ok(None),
                StatusCode::TOO_MANY_REQUESTS if !retried => {
                    retried = true;
                    tracing::warn!(%url, "gateway rate limited the request; backing off");
                    self.back_off(Self::RATE_LIMIT_BACKOFF).await;
                }
                _ => return Ok(Some(response.error_for_status()?.json().await?)),
            }
        }
    }

    /// Reserve the next permitted start slot and wait for it.
    async fn pace(&self) {
        let start = {
            let mut next = self.pacer.lock().await;
            let start = (*next).max(Instant::now());
            *next = start + self.min_interval;
            start
        };
        tokio::time::sleep_until(start).await;
    }

    /// Push every pending request start out by at least `delay`.
    async fn back_off(&self, delay: Duration) {
        let mut next = self.pacer.lock().await;
        *next = (*next).max(Instant::now() + delay);
    }
}

fn encode_slug(market_slug: &str) -> String {
    url::form_urlencoded::byte_serialize(market_slug.as_bytes()).collect()
}

/// Last point at or before `start_time`; otherwise the earliest point.
fn select_closing_point(
    points: &[RawPricePoint],
    start_time: DateTime<Utc>,
) -> Option<&RawPricePoint> {
    let cutoff = start_time.timestamp();
    points
        .iter()
        .filter(|point| point.timestamp <= cutoff)
        .max_by_key(|point| point.timestamp)
        .or_else(|| points.iter().min_by_key(|point| point.timestamp))
}

fn decimal_from_f64(value: f64, field: &str) -> Result<Decimal> {
    Decimal::try_from(value)
        .map(|value| value.round_dp(4))
        .map_err(|error| Error::InvalidData(format!("{field}: {error}")))
}

fn normalize_book(data: RawMarketData) -> Result<MarketBook> {
    let mut bids = data
        .bids
        .into_iter()
        .map(normalize_level)
        .collect::<Result<Vec<_>>>()?;
    let mut offers = data
        .offers
        .into_iter()
        .map(normalize_level)
        .collect::<Result<Vec<_>>>()?;
    bids.sort_by_key(|level| Reverse(level.yes_price));
    offers.sort_by_key(|level| level.yes_price);
    Ok(MarketBook {
        market_slug: data.market_slug,
        bids,
        offers,
        state: data.state,
        transact_time: parse_datetime(&data.transact_time, "book transactTime")?,
        fetched_at: Utc::now(),
    })
}

fn normalize_event(sport: Sport, event: RawEvent) -> Result<Vec<UsMoneylineMarket>> {
    if event.live.unwrap_or(false)
        || event.ended.unwrap_or(false)
        || event.closed.unwrap_or(false)
        || !event.active.unwrap_or(true)
    {
        return Ok(Vec::new());
    }
    let start_time = parse_datetime(
        event
            .start_time
            .as_deref()
            .or(event.start_date.as_deref())
            .ok_or_else(|| Error::InvalidData(format!("event {} has no start time", event.id)))?,
        "event startTime",
    )?;
    if start_time <= Utc::now() {
        return Ok(Vec::new());
    }

    let event_id = event.id.clone();
    let game_id = event.game_id;
    let sportradar_game_id = event.sportradar_game_id.clone();
    event
        .markets
        .into_iter()
        .filter(|market| {
            market.sports_market_type.as_deref() == Some(sport.moneyline_type())
                && market.active.unwrap_or(true)
                && !market.closed.unwrap_or(false)
                && !market.hidden.unwrap_or(false)
        })
        .map(|market| {
            normalize_market(
                sport,
                &event_id,
                game_id,
                sportradar_game_id.as_deref(),
                start_time,
                market,
            )
        })
        .collect()
}

fn normalize_market(
    sport: Sport,
    event_id: &str,
    game_id: Option<i64>,
    sportradar_game_id: Option<&str>,
    start_time: DateTime<Utc>,
    market: RawMarket,
) -> Result<UsMoneylineMarket> {
    let long_side = market
        .market_sides
        .iter()
        .find(|side| side.long)
        .ok_or_else(|| Error::InvalidData(format!("market {} has no long side", market.id)))?;
    let short_side = market
        .market_sides
        .iter()
        .find(|side| !side.long)
        .ok_or_else(|| Error::InvalidData(format!("market {} has no short side", market.id)))?;
    if !settlement_rules_supported(sport, market.description.as_deref().unwrap_or_default()) {
        return Err(Error::InvalidData(format!(
            "market {} has an unrecognized settlement profile",
            market.id
        )));
    }

    Ok(UsMoneylineMarket {
        event_id: event_id.to_string(),
        game_id,
        sportradar_game_id: sportradar_game_id.map(str::to_string),
        sport,
        start_time,
        market_id: market.id,
        market_slug: market.slug,
        description: market.description.unwrap_or_default(),
        sports_market_type: market.sports_market_type.unwrap_or_default(),
        long_participant: normalize_participant(long_side)?,
        short_participant: normalize_participant(short_side)?,
        tick_size: decimal_from_value(&market.order_price_min_tick_size, "tick size")?,
        minimum_quantity: decimal_from_value(&market.minimum_trade_qty, "minimum quantity")?,
        fee_coefficient: decimal_from_value(&market.fee_coefficient, "fee coefficient")?,
        ep3_status: market.ep3_status.unwrap_or_default(),
        ep3_synced_at: market
            .ep3_synced_at
            .as_deref()
            .map(|value| parse_datetime(value, "ep3SyncedAt"))
            .transpose()?,
    })
}

fn normalize_participant(side: &RawMarketSide) -> Result<MarketParticipant> {
    let mut provider_ids = BTreeMap::new();
    if let Some(team) = &side.team {
        for provider in &team.provider_ids {
            provider_ids.insert(provider.provider.clone(), provider.provider_id.clone());
        }
    }
    // `description` is a display nickname ("Ravens", "+17.50"); the team record
    // carries the canonical full name every external source uses.
    let name = side
        .team
        .as_ref()
        .map(|team| team.name.clone())
        .filter(|name| !name.trim().is_empty())
        .or_else(|| side.description.clone())
        .ok_or_else(|| Error::InvalidData(format!("market side {} has no name", side.id)))?;
    Ok(MarketParticipant {
        side_id: side.id.clone(),
        name,
        long: side.long,
        team_id: side.team_id,
        provider_ids,
    })
}

/// Recognized settlement profiles. Anything else is excluded because the
/// contract would not match the sportsbook moneyline it is compared with.
fn settlement_rules_supported(sport: Sport, description: &str) -> bool {
    let normalized = description.to_lowercase();
    match sport {
        Sport::Nfl => normalized.contains("tie") && normalized.contains("$0.50"),
        // Polymarket US voids (fair market price) a match that never starts and
        // awards a retirement to the opponent; books grade the same way.
        Sport::Tennis => {
            (normalized.contains("walkover") || normalized.contains("withdrawal"))
                && (normalized.contains("$0.50") || normalized.contains("fair market price"))
        }
        Sport::Nba | Sport::Wnba | Sport::Mlb => !description.trim().is_empty(),
    }
}

fn normalize_level(level: RawBookLevel) -> Result<BookLevel> {
    Ok(BookLevel {
        yes_price: decimal_from_value(&level.px.value, "book price")?,
        quantity: Decimal::from_str_exact(&level.qty)
            .map_err(|error| Error::InvalidData(format!("book quantity: {error}")))?,
    })
}

fn decimal_from_value(value: &serde_json::Value, field: &str) -> Result<Decimal> {
    let text = match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        _ => return Err(Error::InvalidData(format!("{field} is not numeric"))),
    };
    Decimal::from_str_exact(&text).map_err(|error| Error::InvalidData(format!("{field}: {error}")))
}

fn parse_datetime(value: &str, field: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| Error::InvalidData(format!("{field}: {error}")))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsResponse {
    #[serde(default)]
    events: Vec<RawEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEvent {
    #[serde(deserialize_with = "string_from_any")]
    id: String,
    start_time: Option<String>,
    start_date: Option<String>,
    active: Option<bool>,
    closed: Option<bool>,
    live: Option<bool>,
    ended: Option<bool>,
    game_id: Option<i64>,
    sportradar_game_id: Option<String>,
    #[serde(default)]
    markets: Vec<RawMarket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarket {
    #[serde(deserialize_with = "string_from_any")]
    id: String,
    slug: String,
    description: Option<String>,
    sports_market_type: Option<String>,
    active: Option<bool>,
    closed: Option<bool>,
    hidden: Option<bool>,
    ep3_status: Option<String>,
    ep3_synced_at: Option<String>,
    #[serde(default)]
    market_sides: Vec<RawMarketSide>,
    order_price_min_tick_size: serde_json::Value,
    minimum_trade_qty: serde_json::Value,
    fee_coefficient: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarketSide {
    #[serde(deserialize_with = "string_from_any")]
    id: String,
    description: Option<String>,
    long: bool,
    team_id: Option<i64>,
    team: Option<RawTeam>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTeam {
    name: String,
    #[serde(default)]
    provider_ids: Vec<RawProviderId>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProviderId {
    provider: String,
    provider_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BookResponse {
    market_data: RawMarketData,
}

#[derive(Debug, Deserialize)]
struct SettlementResponse {
    settlement: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct PriceHistoryResponse {
    #[serde(default)]
    history: Vec<RawPricePoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPricePoint {
    timestamp: i64,
    long_price: f64,
    short_price: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarketData {
    market_slug: String,
    #[serde(default)]
    bids: Vec<RawBookLevel>,
    #[serde(default)]
    offers: Vec<RawBookLevel>,
    state: String,
    transact_time: String,
}

#[derive(Debug, Deserialize)]
struct RawBookLevel {
    px: RawAmount,
    qty: String,
}

#[derive(Debug, Deserialize)]
struct RawAmount {
    value: serde_json::Value,
}

fn string_from_any<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        _ => Err(serde::de::Error::custom("expected string or number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfl_requires_half_dollar_tie_rule() {
        assert!(settlement_rules_supported(
            Sport::Nfl,
            "If the game ends in a tie, the market will settle to $0.50."
        ));
        assert!(!settlement_rules_supported(
            Sport::Nfl,
            "The winner resolves to one."
        ));
    }

    #[test]
    fn tennis_accepts_last_fair_market_price_wording() {
        assert!(settlement_rules_supported(
            Sport::Tennis,
            "A walkover before the first serve settles at the last fair market price."
        ));
    }

    fn point(timestamp: i64, long_price: f64) -> RawPricePoint {
        RawPricePoint {
            timestamp,
            long_price,
            short_price: 1.0 - long_price,
        }
    }

    #[test]
    fn closing_point_is_last_at_or_before_start() {
        let start = DateTime::from_timestamp(1_000, 0).unwrap();
        let points = [point(100, 0.40), point(1_000, 0.45), point(1_060, 0.60)];
        let chosen = select_closing_point(&points, start).unwrap();
        assert_eq!(chosen.timestamp, 1_000);
    }

    #[test]
    fn closing_point_falls_back_to_earliest_when_series_starts_late() {
        let start = DateTime::from_timestamp(1_000, 0).unwrap();
        let points = [point(1_120, 0.55), point(1_060, 0.50), point(1_180, 0.70)];
        let chosen = select_closing_point(&points, start).unwrap();
        assert_eq!(chosen.timestamp, 1_060);
    }

    #[test]
    fn closing_point_is_none_for_empty_history() {
        let start = DateTime::from_timestamp(1_000, 0).unwrap();
        assert!(select_closing_point(&[], start).is_none());
    }

    #[test]
    fn price_history_converts_floats_to_four_places() {
        let payload: PriceHistoryResponse = serde_json::from_str(
            r#"{"history":[{"timestamp":1,"longPrice":0.43210012,"shortPrice":0.56789988}]}"#,
        )
        .unwrap();
        let point = &payload.history[0];
        assert_eq!(
            decimal_from_f64(point.long_price, "longPrice").unwrap(),
            Decimal::new(4321, 4)
        );
        assert_eq!(
            decimal_from_f64(point.short_price, "shortPrice").unwrap(),
            Decimal::new(5679, 4)
        );
    }

    #[tokio::test]
    async fn pacer_spaces_request_starts() {
        let client = PolymarketUsClient::new("http://localhost", Duration::from_secs(1))
            .unwrap()
            .with_rate_limit(50);
        let started = Instant::now();
        for _ in 0..4 {
            client.pace().await;
        }
        // First start is immediate; three more slots of 20 ms follow.
        assert!(started.elapsed() >= Duration::from_millis(60));
    }

    #[tokio::test]
    async fn settlement_maps_404_to_none_and_parses_payout() {
        use httpmock::prelude::*;

        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/v1/markets/settled/settlement");
                then.status(200)
                    .json_body(serde_json::json!({ "slug": "settled", "settlement": "1" }));
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/v1/markets/open/settlement");
                then.status(404);
            })
            .await;
        let client = PolymarketUsClient::new(server.base_url(), Duration::from_secs(5)).unwrap();

        assert_eq!(
            client.fetch_settlement("settled").await.unwrap(),
            Some(Decimal::ONE)
        );
        assert_eq!(client.fetch_settlement("open").await.unwrap(), None);
    }

    #[tokio::test]
    async fn closing_price_uses_last_pre_start_point() {
        use httpmock::prelude::*;

        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/v1/price-history")
                    .query_param("symbol", "game")
                    .query_param("fixedInterval", "INTERVAL_LIVE")
                    .query_param("fidelity", "1");
                then.status(200).json_body(serde_json::json!({
                    "history": [
                        { "timestamp": 900, "longPrice": 0.40, "shortPrice": 0.60 },
                        { "timestamp": 960, "longPrice": 0.42, "shortPrice": 0.58 },
                        { "timestamp": 1020, "longPrice": 0.70, "shortPrice": 0.30 }
                    ]
                }));
            })
            .await;
        let client = PolymarketUsClient::new(server.base_url(), Duration::from_secs(5)).unwrap();

        let closing = client
            .fetch_closing_price("game", DateTime::from_timestamp(1_000, 0).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            closing,
            ClosingPrice {
                long_price: Decimal::new(42, 2),
                short_price: Decimal::new(58, 2),
                observed_at: DateTime::from_timestamp(960, 0).unwrap(),
            }
        );
    }

    #[test]
    fn book_levels_sort_in_executable_order() {
        let payload = include_str!("../tests/fixtures/polymarket_book.json");
        let parsed: BookResponse = serde_json::from_str(payload).unwrap();
        let book = normalize_book(parsed.market_data).unwrap();
        assert_eq!(book.bids[0].yes_price, Decimal::new(44, 2));
        assert_eq!(book.offers[0].yes_price, Decimal::new(45, 2));
    }

    #[test]
    fn event_fixture_filters_to_exact_pregame_moneyline() {
        let payload: EventsResponse =
            serde_json::from_str(include_str!("../tests/fixtures/polymarket_events.json")).unwrap();
        let markets = payload
            .events
            .into_iter()
            .flat_map(|event| normalize_event(Sport::Nba, event).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(markets.len(), 1);
        assert_eq!(
            markets[0].sports_market_type,
            "basketball_team_full_game_winner"
        );
        assert_eq!(markets[0].long_participant.name, "Los Angeles Sparks");
    }
}
