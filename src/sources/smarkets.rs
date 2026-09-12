//! Smarkets betting exchange, public (unauthenticated) market data.
//!
//! Independent order flow from a UK exchange, so it counts as its own family
//! but is never a reference book. Per sport the adapter lists upcoming events
//! of the matching type, resolves each event's competition (parent event) so
//! only NFL / NBA / WNBA / MLB games survive, then batch-fetches the winner
//! market, its two contracts and the order book, pricing each side at the
//! best executable back offer (`10000 / offer_price`).
//!
//! Prices are basis points of implied probability (8772 = 87.72%). Quantities
//! are Smarkets liquidity units (payout in 1/10000 of the market currency, so
//! 10000 = one pound of payout); a side needs at least that much resting at
//! the best offer to be quoted.
//!
//! Limitations: competitions are matched by exact name ("NFL", "MLB", "NBA",
//! "WNBA"), so college, European and pre-season exhibition games are dropped;
//! tennis keeps every tour. Team names come from the event name ("Away at
//! Home") because contract names are nicknames ("Cowboys", "Astros (P.
//! Lambert)"). Tennis participants use the contract names as written.
//!
//! Environment:
//! - `SMARKETS_BASE_URL` (default `https://api.smarkets.com/v3`)

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::future::try_join_all;
use rust_decimal::Decimal;
use serde::{Deserialize, de::DeserializeOwned};
use tokio::sync::Semaphore;
use tracing::warn;

use crate::{
    Error, Result,
    domain::{
        DEFAULT_START_TIME_TOLERANCE_MINUTES, SourceFamily, SourceHealth, SourceQuote, Sport,
    },
    sources::{OddsSource, http_client, probe_by_collecting},
};

const SOURCE_ID: &str = "smarkets";
const PARSER_VERSION: &str = "smarkets-v1";
const PAGE_LIMIT: usize = 100;
const MAX_PAGES: usize = 3;
/// Comma-joined ids per batched request. The API answers 404 to more than 50.
const BATCH_SIZE: usize = 50;
/// In-flight requests across all sports. The quotes endpoint is limited to 20
/// requests per minute (`x-ratelimit-limit`), so a full pass must stay at a
/// handful of large batches; this just keeps the burst polite.
const MAX_IN_FLIGHT: usize = 4;
/// Full probability in Smarkets price units.
const FULL_BOOK: i64 = 10_000;
/// Best offers summing above this are too illiquid to be a price.
const MAX_OVERROUND: i64 = 11_500;
/// Widest best-bid/best-offer gap (10 percentage points) still considered a price.
const MAX_SPREAD: i64 = 1_000;
/// Minimum resting quantity at the best offer on each side.
const MIN_OFFER_QUANTITY: i64 = 10_000;

pub struct SmarketsSource {
    base_url: String,
    client: reqwest::Client,
    in_flight: Semaphore,
}

impl SmarketsSource {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            base_url: env::var("SMARKETS_BASE_URL")
                .unwrap_or_else(|_| "https://api.smarkets.com/v3".into())
                .trim_end_matches('/')
                .to_string(),
            client: http_client(timeout)?,
            in_flight: Semaphore::new(MAX_IN_FLIGHT),
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let _permit = self.in_flight.acquire().await.map_err(|_| Error::Source {
            source_id: SOURCE_ID.into(),
            message: "request limiter closed".into(),
        })?;
        Ok(self
            .client
            .get(format!("{}{path}", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn fetch_events(&self, event_type: &str) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut query = format!(
            "?state=upcoming&type={event_type}&limit={PAGE_LIMIT}&sort=start_datetime,id&with_new_type=true"
        );
        for _ in 0..MAX_PAGES {
            let page: EventsPage = self.get(&format!("/events/{query}")).await?;
            events.extend(page.events);
            match page.pagination.and_then(|pagination| pagination.next_page) {
                Some(next) if next.starts_with('?') && next != query => query = next,
                _ => break,
            }
        }
        Ok(events)
    }

    /// Competition name per parent id, fetched in batches.
    async fn fetch_competitions(&self, ids: &BTreeSet<String>) -> Result<HashMap<String, String>> {
        let batches = batched(ids.iter().map(String::as_str), |joined| async move {
            self.get::<EventsPage>(&format!("/events/{joined}/")).await
        });
        Ok(try_join_all(batches)
            .await?
            .into_iter()
            .flat_map(|page| page.events)
            .map(|event| (event.id, event.name))
            .collect())
    }

    async fn fetch_markets(&self, event_ids: &[&str]) -> Result<Vec<Market>> {
        let batches = batched(event_ids.iter().copied(), |joined| async move {
            self.get::<MarketsPage>(&format!("/events/{joined}/markets/"))
                .await
        });
        Ok(try_join_all(batches)
            .await?
            .into_iter()
            .flat_map(|page| page.markets)
            .collect())
    }

    async fn fetch_contracts(&self, market_ids: &[&str]) -> Result<Vec<Contract>> {
        let batches = batched(market_ids.iter().copied(), |joined| async move {
            self.get::<ContractsPage>(&format!("/markets/{joined}/contracts/"))
                .await
        });
        Ok(try_join_all(batches)
            .await?
            .into_iter()
            .flat_map(|page| page.contracts)
            .collect())
    }

    async fn fetch_quotes(&self, market_ids: &[&str]) -> Result<HashMap<String, Book>> {
        let batches = batched(market_ids.iter().copied(), |joined| async move {
            self.get::<HashMap<String, Book>>(&format!("/markets/{joined}/quotes/"))
                .await
        });
        Ok(try_join_all(batches).await?.into_iter().flatten().collect())
    }

    /// One Smarkets event type (e.g. `basketball_match`) can serve several
    /// sports; `sports` restricts which competitions are kept.
    async fn collect_type(&self, event_type: &str, sports: &[Sport]) -> Result<Vec<SourceQuote>> {
        let now = Utc::now();
        let events = self
            .fetch_events(event_type)
            .await?
            .into_iter()
            .filter(|event| event.is_pregame(now))
            .collect::<Vec<_>>();
        let competitions = if sports.contains(&Sport::Tennis) {
            HashMap::new()
        } else {
            let parents = events
                .iter()
                .filter_map(|event| event.parent_id.clone())
                .collect::<BTreeSet<_>>();
            self.fetch_competitions(&parents).await?
        };
        let events = events
            .into_iter()
            .filter_map(|event| {
                let sport = event_sport(&event, &competitions, sports)?;
                Some((event.id.clone(), (sport, event)))
            })
            .collect::<BTreeMap<_, _>>();
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let event_ids = events.keys().map(String::as_str).collect::<Vec<_>>();
        let markets = select_winner_markets(self.fetch_markets(&event_ids).await?);
        if markets.is_empty() {
            return Ok(Vec::new());
        }
        let market_ids = markets.keys().map(String::as_str).collect::<Vec<_>>();
        let (contracts, quotes) = futures::try_join!(
            self.fetch_contracts(&market_ids),
            self.fetch_quotes(&market_ids)
        )?;
        let fetched_at = Utc::now();

        let mut by_market: HashMap<&str, Vec<&Contract>> = HashMap::new();
        for contract in &contracts {
            by_market
                .entry(contract.market_id.as_str())
                .or_default()
                .push(contract);
        }

        let mut normalized = Vec::with_capacity(markets.len());
        for (market_id, market) in &markets {
            let Some((sport, event)) = events.get(&market.event_id) else {
                continue;
            };
            let sides = by_market
                .get(market_id.as_str())
                .map_or(&[][..], Vec::as_slice);
            match normalize_market(*sport, event, market, sides, &quotes, fetched_at) {
                Ok(quote) => normalized.push(quote),
                Err(error) => {
                    warn!(event = %event.name, %market_id, %error, "Smarkets market skipped")
                }
            }
        }
        Ok(normalized)
    }
}

#[async_trait]
impl OddsSource for SmarketsSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    fn family(&self) -> SourceFamily {
        SourceFamily::Other(SOURCE_ID.into())
    }

    async fn collect(&self, sports: &[Sport]) -> Result<Vec<SourceQuote>> {
        let mut by_type: BTreeMap<&'static str, Vec<Sport>> = BTreeMap::new();
        for &sport in sports {
            by_type.entry(event_type(sport)).or_default().push(sport);
        }
        let requests = by_type
            .iter()
            .map(|(event_type, sports)| self.collect_type(event_type, sports));
        let per_type = try_join_all(requests).await?;
        Ok(per_type.into_iter().flatten().collect())
    }

    async fn probe(&self) -> SourceHealth {
        probe_by_collecting(self, &Sport::ALL).await
    }
}

fn event_type(sport: Sport) -> &'static str {
    match sport {
        Sport::Nfl => "american_football_match",
        Sport::Nba | Sport::Wnba => "basketball_match",
        Sport::Mlb => "baseball_match",
        Sport::Tennis => "tennis_match",
    }
}

/// Sport a competition name belongs to. Exact names only: Smarkets labels the
/// leagues literally ("NFL", "MLB") and everything else (college, Euroleague,
/// KBO, pre-season tournaments) must not leak into US moneyline matching.
fn league_sport(competition: &str) -> Option<Sport> {
    match competition.trim() {
        "NFL" => Some(Sport::Nfl),
        "NBA" => Some(Sport::Nba),
        "WNBA" => Some(Sport::Wnba),
        "MLB" => Some(Sport::Mlb),
        _ => None,
    }
}

fn event_sport(
    event: &Event,
    competitions: &HashMap<String, String>,
    sports: &[Sport],
) -> Option<Sport> {
    let sport = if sports.contains(&Sport::Tennis) {
        Sport::Tennis
    } else {
        league_sport(competitions.get(event.parent_id.as_ref()?)?)?
    };
    sports.contains(&sport).then_some(sport)
}

/// Full-game two-way winner market per event. `WINNER_2_WAY` is the plain
/// moneyline; `WINNER_DNB` is how Smarkets labels the NFL "Winner" (overtime
/// included, tie voids). Three-way, half, alternate and prop markets are out.
fn select_winner_markets(markets: Vec<Market>) -> BTreeMap<String, Market> {
    let mut by_event: BTreeMap<String, Market> = BTreeMap::new();
    for market in markets {
        if market.state != "open" || !market.is_winner() {
            continue;
        }
        match by_event.get(&market.event_id) {
            Some(existing) if existing.rank() <= market.rank() => {}
            _ => {
                by_event.insert(market.event_id.clone(), market);
            }
        }
    }
    by_event
        .into_values()
        .map(|market| (market.id.clone(), market))
        .collect()
}

fn normalize_market(
    sport: Sport,
    event: &Event,
    market: &Market,
    contracts: &[&Contract],
    quotes: &HashMap<String, Book>,
    fetched_at: DateTime<Utc>,
) -> Result<SourceQuote> {
    let invalid =
        |message: String| Error::InvalidData(format!("Smarkets {}: {message}", market.id));
    let [first, second] = contracts else {
        return Err(invalid(format!("{} contracts", contracts.len())));
    };
    let (side_a, side_b) = match (first.side(), second.side()) {
        (Some(Side::A), Some(Side::B)) => (first, second),
        (Some(Side::B), Some(Side::A)) => (second, first),
        _ => {
            return Err(invalid(format!(
                "contract types {} / {}",
                first.contract_type.name, second.contract_type.name
            )));
        }
    };
    let (name_a, name_b) = participant_names(event, side_a, side_b);

    let price_a = best_offer(quotes, &side_a.id).map_err(&invalid)?;
    let price_b = best_offer(quotes, &side_b.id).map_err(&invalid)?;
    let overround = price_a + price_b;
    if !(FULL_BOOK..=MAX_OVERROUND).contains(&overround) {
        return Err(invalid(format!("offers imply {overround} bp")));
    }

    let provider_ids =
        |contract: &Contract| BTreeMap::from([(SOURCE_ID.to_string(), contract.id.clone())]);
    Ok(SourceQuote {
        source_id: SOURCE_ID.into(),
        family: SourceFamily::Other(SOURCE_ID.into()),
        sport,
        event_id: event.id.clone(),
        participant_a: name_a,
        participant_b: name_b,
        participant_a_provider_ids: provider_ids(side_a),
        participant_b_provider_ids: provider_ids(side_b),
        start_time: event
            .start_datetime
            .ok_or_else(|| invalid("no start time".into()))?,
        start_time_tolerance_minutes: DEFAULT_START_TIME_TOLERANCE_MINUTES,
        decimal_odds_a: price_to_decimal(price_a),
        decimal_odds_b: price_to_decimal(price_b),
        decimal_odds_neutral: None,
        source_timestamp: fetched_at,
        fetched_at,
        parser_version: PARSER_VERSION.into(),
        validation_only: false,
    })
}

/// Team contracts carry nicknames, so full names come from the event title:
/// "Away at Home" (US listing) or "Home vs Away". Player contracts are full
/// names already.
fn participant_names(event: &Event, side_a: &Contract, side_b: &Contract) -> (String, String) {
    if side_a.side() == Some(Side::A) && side_a.contract_type.name == "HOME" {
        if let Some((away, home)) = event.name.split_once(" at ") {
            return (home.trim().to_string(), away.trim().to_string());
        }
        if let Some((home, away)) = event.name.split_once(" vs ") {
            return (home.trim().to_string(), away.trim().to_string());
        }
    }
    (side_a.name.clone(), side_b.name.clone())
}

/// Best executable back price for a contract, guarded by resting size and
/// bid/offer spread. Errors carry the reason for the skip log.
fn best_offer(
    quotes: &HashMap<String, Book>,
    contract_id: &str,
) -> std::result::Result<i64, String> {
    let book = quotes
        .get(contract_id)
        .ok_or_else(|| format!("contract {contract_id}: no order book"))?;
    let offer = book
        .offers
        .iter()
        .min_by_key(|level| level.price)
        .ok_or_else(|| format!("contract {contract_id}: no offers"))?;
    if offer.quantity < MIN_OFFER_QUANTITY {
        return Err(format!(
            "contract {contract_id}: best offer quantity {}",
            offer.quantity
        ));
    }
    let bid = book
        .bids
        .iter()
        .map(|level| level.price)
        .max()
        .ok_or_else(|| format!("contract {contract_id}: no bids"))?;
    if offer.price - bid > MAX_SPREAD {
        return Err(format!(
            "contract {contract_id}: spread {} bp",
            offer.price - bid
        ));
    }
    if offer.price <= 0 || offer.price > FULL_BOOK {
        return Err(format!("contract {contract_id}: price {}", offer.price));
    }
    Ok(offer.price)
}

/// Smarkets price (basis points of probability) to decimal odds, 4 dp.
pub fn price_to_decimal(price: i64) -> Decimal {
    (Decimal::from(FULL_BOOK) / Decimal::from(price)).round_dp(4)
}

/// Splits `ids` into comma-joined batches and maps each through `request`.
fn batched<'a, F, Fut>(ids: impl Iterator<Item = &'a str>, request: F) -> impl Iterator<Item = Fut>
where
    F: Fn(String) -> Fut,
{
    let ids = ids.collect::<Vec<_>>();
    ids.chunks(BATCH_SIZE)
        .map(|chunk| chunk.join(","))
        .collect::<Vec<_>>()
        .into_iter()
        .map(request)
}

#[derive(Deserialize)]
struct EventsPage {
    events: Vec<Event>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Deserialize)]
struct Pagination {
    next_page: Option<String>,
}

#[derive(Deserialize)]
struct Event {
    id: String,
    name: String,
    /// Absent on competition (parent) events.
    #[serde(default)]
    start_datetime: Option<DateTime<Utc>>,
    #[serde(default)]
    bettable: bool,
    #[serde(default)]
    parent_id: Option<String>,
}

impl Event {
    /// Still bettable and not yet started; live and suspended events are out.
    fn is_pregame(&self, now: DateTime<Utc>) -> bool {
        self.bettable && self.start_datetime.is_some_and(|start| start > now)
    }
}

#[derive(Deserialize)]
struct MarketsPage {
    markets: Vec<Market>,
}

#[derive(Deserialize)]
struct Market {
    id: String,
    event_id: String,
    name: String,
    state: String,
    #[serde(default)]
    market_type: Option<MarketType>,
}

#[derive(Deserialize, Default)]
struct MarketType {
    #[serde(default)]
    name: String,
}

impl Market {
    fn type_name(&self) -> &str {
        self.market_type
            .as_ref()
            .map_or("", |kind| kind.name.as_str())
    }

    fn is_winner(&self) -> bool {
        match self.type_name() {
            "WINNER_2_WAY" | "WINNER_DNB" => true,
            "" => matches!(self.name.as_str(), "Match winner" | "Winner" | "Match Odds"),
            _ => false,
        }
    }

    /// Lower is preferred when an event lists both winner variants.
    fn rank(&self) -> u8 {
        match self.type_name() {
            "WINNER_2_WAY" => 0,
            "WINNER_DNB" => 1,
            _ => 2,
        }
    }
}

#[derive(Deserialize)]
struct ContractsPage {
    contracts: Vec<Contract>,
}

#[derive(Deserialize)]
struct Contract {
    id: String,
    market_id: String,
    name: String,
    #[serde(default)]
    contract_type: MarketType,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    A,
    B,
}

impl Contract {
    fn side(&self) -> Option<Side> {
        match self.contract_type.name.as_str() {
            "PLAYER_A" | "HOME" => Some(Side::A),
            "PLAYER_B" | "AWAY" => Some(Side::B),
            _ => None,
        }
    }
}

#[derive(Deserialize, Default)]
struct Book {
    #[serde(default)]
    bids: Vec<Level>,
    #[serde(default)]
    offers: Vec<Level>,
}

#[derive(Deserialize)]
struct Level {
    price: i64,
    quantity: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTS: &str = r#"{"events":[
      {"id":"45315769","name":"Whitney Osuigwe vs Fernanda Labrana","start_datetime":"2026-09-12T22:30:00Z","state":"upcoming","bettable":true,"type":{"domain":"tennis","scope":"single_event"},"parent_id":"44600126"},
      {"id":"45170336","name":"Houston Astros at Tampa Bay Rays","start_datetime":"2026-09-12T22:10:00Z","state":"upcoming","bettable":true,"type":{"domain":"baseball","scope":"single_event"},"parent_id":"13240353"},
      {"id":"45310230","name":"Merrimack Warriors at Maine Black Bears","start_datetime":"2026-09-12T21:00:00Z","state":"upcoming","bettable":true,"type":{"domain":"american_football","scope":"single_event"},"parent_id":"42832523"},
      {"id":"45076647","name":"Chicago Bears at Carolina Panthers","start_datetime":"2026-09-13T17:00:00Z","state":"upcoming","bettable":false,"type":{"domain":"american_football","scope":"single_event"},"parent_id":"7763291"}
    ],"pagination":{"next_page":null}}"#;

    const MARKETS: &str = r#"{"markets":[
      {"id":"163795275","event_id":"45315769","name":"Match winner","state":"open","market_type":{"name":"WINNER_2_WAY"}},
      {"id":"163795276","event_id":"45315769","name":"Set 1 winner","state":"open","market_type":{"name":"SET_ONE_WINNER"}},
      {"id":"163740482","event_id":"45170336","name":"Match winner","state":"open","market_type":{"name":"WINNER_2_WAY"}},
      {"id":"163740618","event_id":"45170336","name":"Astros +3.5 / Rays -3.5","state":"open","market_type":{"name":"HANDICAP","param":"-3.5"}},
      {"id":"139996523","event_id":"45069402","name":"Winner","state":"open","market_type":{"name":"WINNER_DNB"}},
      {"id":"162801054","event_id":"45069402","name":"Winner (only regular time)","state":"open","market_type":{"name":"WINNER_3_WAY"}},
      {"id":"163740999","event_id":"45170337","name":"Match winner","state":"suspended","market_type":{"name":"WINNER_2_WAY"}}
    ]}"#;

    const CONTRACTS: &str = r#"{"contracts":[
      {"id":"452969441","market_id":"163795275","name":"Whitney Osuigwe","contract_type":{"name":"PLAYER_A"}},
      {"id":"452969442","market_id":"163795275","name":"Fernanda Labrana","contract_type":{"name":"PLAYER_B"}},
      {"id":"452793500","market_id":"163740482","name":"Astros (P. Lambert)","contract_type":{"name":"AWAY"}},
      {"id":"452793499","market_id":"163740482","name":"Rays (I. Seymour)","contract_type":{"name":"HOME"}}
    ]}"#;

    const QUOTES: &str = r#"{
      "452969441":{"bids":[{"price":8772,"quantity":393752},{"price":8547,"quantity":382923}],"offers":[{"price":9174,"quantity":16394467}]},
      "452969442":{"bids":[{"price":833,"quantity":539013},{"price":667,"quantity":7679300}],"offers":[{"price":1220,"quantity":8135400},{"price":1111,"quantity":3049346}]},
      "452793500":{"bids":[{"price":4237,"quantity":287938}],"offers":[]},
      "452793499":{"bids":[{"price":5587,"quantity":3865082}],"offers":[{"price":5780,"quantity":2681126}]}
    }"#;

    fn fixture() -> (
        Vec<Event>,
        Vec<Market>,
        Vec<Contract>,
        HashMap<String, Book>,
    ) {
        let events: EventsPage = serde_json::from_str(EVENTS).unwrap();
        let markets: MarketsPage = serde_json::from_str(MARKETS).unwrap();
        let contracts: ContractsPage = serde_json::from_str(CONTRACTS).unwrap();
        let quotes: HashMap<String, Book> = serde_json::from_str(QUOTES).unwrap();
        (events.events, markets.markets, contracts.contracts, quotes)
    }

    fn contracts_for<'a>(contracts: &'a [Contract], market_id: &str) -> Vec<&'a Contract> {
        contracts
            .iter()
            .filter(|contract| contract.market_id == market_id)
            .collect()
    }

    #[test]
    fn price_converts_to_decimal_odds() {
        assert_eq!(price_to_decimal(8772), Decimal::new(114, 2));
        assert_eq!(price_to_decimal(9174), Decimal::new(10900, 4));
        assert_eq!(price_to_decimal(1111), Decimal::new(90009, 4));
        assert_eq!(price_to_decimal(5000), Decimal::TWO);
    }

    #[test]
    fn tennis_market_normalizes_from_best_offers() {
        let (events, markets, contracts, quotes) = fixture();
        let winners = select_winner_markets(markets);
        let market = &winners["163795275"];
        let quote = normalize_market(
            Sport::Tennis,
            &events[0],
            market,
            &contracts_for(&contracts, "163795275"),
            &quotes,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(quote.source_id, "smarkets");
        assert_eq!(quote.family, SourceFamily::Other("smarkets".into()));
        assert_eq!(quote.event_id, "45315769");
        assert_eq!(quote.participant_a, "Whitney Osuigwe");
        assert_eq!(quote.participant_b, "Fernanda Labrana");
        assert_eq!(quote.participant_a_provider_ids["smarkets"], "452969441");
        assert_eq!(quote.participant_b_provider_ids["smarkets"], "452969442");
        assert_eq!(quote.decimal_odds_a, Decimal::new(10900, 4));
        assert_eq!(quote.decimal_odds_b, Decimal::new(90009, 4));
        assert_eq!(quote.decimal_odds_neutral, None);
        assert_eq!(
            quote.start_time,
            DateTime::parse_from_rfc3339("2026-09-12T22:30:00Z").unwrap()
        );
        assert_eq!(quote.source_timestamp, quote.fetched_at);
        assert_eq!(quote.parser_version, "smarkets-v1");
    }

    #[test]
    fn team_market_without_offers_on_one_side_is_skipped() {
        let (events, markets, contracts, quotes) = fixture();
        let winners = select_winner_markets(markets);
        let error = normalize_market(
            Sport::Mlb,
            &events[1],
            &winners["163740482"],
            &contracts_for(&contracts, "163740482"),
            &quotes,
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no offers"), "{error}");
    }

    #[test]
    fn team_names_come_from_event_title_with_home_as_participant_a() {
        let (events, markets, contracts, mut quotes) = fixture();
        let winners = select_winner_markets(markets);
        quotes.get_mut("452793500").unwrap().offers = vec![Level {
            price: 4386,
            quantity: 5918200,
        }];
        let quote = normalize_market(
            Sport::Mlb,
            &events[1],
            &winners["163740482"],
            &contracts_for(&contracts, "163740482"),
            &quotes,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(quote.participant_a, "Tampa Bay Rays");
        assert_eq!(quote.participant_b, "Houston Astros");
        assert_eq!(quote.participant_a_provider_ids["smarkets"], "452793499");
        assert_eq!(quote.decimal_odds_a, price_to_decimal(5780));
        assert_eq!(quote.decimal_odds_b, price_to_decimal(4386));
    }

    #[test]
    fn illiquid_and_thin_books_are_skipped() {
        let (events, markets, contracts, mut quotes) = fixture();
        let winners = select_winner_markets(markets);
        let market = &winners["163795275"];
        let sides = contracts_for(&contracts, "163795275");

        // Overround: 9174 + 2500 = 11674 bp > 11500 even with a tight spread.
        let book = quotes.get_mut("452969442").unwrap();
        book.bids = vec![Level {
            price: 2400,
            quantity: 539013,
        }];
        book.offers = vec![Level {
            price: 2500,
            quantity: 3049346,
        }];
        let error = normalize_market(
            Sport::Tennis,
            &events[0],
            market,
            &sides,
            &quotes,
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("11674 bp"), "{error}");

        // Spread: best bid 833 vs offer 1900 is 1067 bp > 1000.
        let book = quotes.get_mut("452969442").unwrap();
        book.bids = vec![Level {
            price: 833,
            quantity: 539013,
        }];
        book.offers = vec![Level {
            price: 1900,
            quantity: 3049346,
        }];
        let error = normalize_market(
            Sport::Tennis,
            &events[0],
            market,
            &sides,
            &quotes,
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("spread 1067 bp"), "{error}");

        // Size: best offer below the minimum quantity.
        quotes.get_mut("452969442").unwrap().offers = vec![Level {
            price: 1111,
            quantity: 9999,
        }];
        let error = normalize_market(
            Sport::Tennis,
            &events[0],
            market,
            &sides,
            &quotes,
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("quantity 9999"), "{error}");
    }

    #[test]
    fn winner_market_selection_keeps_one_open_two_way_market_per_event() {
        let (_, markets, _, _) = fixture();
        let winners = select_winner_markets(markets);
        let by_event = winners
            .values()
            .map(|market| (market.event_id.as_str(), market.type_name()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            by_event,
            BTreeMap::from([
                ("45315769", "WINNER_2_WAY"),
                ("45170336", "WINNER_2_WAY"),
                ("45069402", "WINNER_DNB"),
            ])
        );
    }

    #[test]
    fn competition_name_filters_sport_and_bettable_flag() {
        let (events, _, _, _) = fixture();
        let competitions = HashMap::from([
            ("13240353".to_string(), "MLB".to_string()),
            ("42832523".to_string(), "College Football".to_string()),
            ("7763291".to_string(), "NFL".to_string()),
        ]);
        let now = DateTime::parse_from_rfc3339("2026-09-12T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let kept = events
            .iter()
            .filter(|event| event.is_pregame(now))
            .filter_map(|event| {
                event_sport(event, &competitions, &[Sport::Mlb, Sport::Nfl])
                    .map(|sport| (event.id.as_str(), sport))
            })
            .collect::<Vec<_>>();
        // Tennis event has no US competition; college game is not NFL; the
        // NFL game is not bettable; only MLB remains (and it starts after now).
        assert_eq!(kept, vec![("45170336", Sport::Mlb)]);

        assert_eq!(
            event_sport(&events[0], &competitions, &[Sport::Tennis]),
            Some(Sport::Tennis)
        );
        assert_eq!(event_sport(&events[1], &competitions, &[Sport::Nba]), None);
        assert_eq!(league_sport("WNBA"), Some(Sport::Wnba));
        assert_eq!(league_sport("NBA Summer League"), None);
    }
}
