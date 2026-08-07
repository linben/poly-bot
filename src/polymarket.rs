use std::{cmp::Reverse, collections::BTreeMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::{
    Error, Result,
    domain::{BookLevel, MarketBook, MarketParticipant, Sport, UsMoneylineMarket},
};

#[derive(Clone)]
pub struct PolymarketUsClient {
    base_url: String,
    client: Client,
    last_request: Arc<Mutex<Option<tokio::time::Instant>>>,
}

impl PolymarketUsClient {
    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Result<Self> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent("polybot-research/0.1")
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            last_request: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn discover_moneylines(&self) -> Result<Vec<UsMoneylineMarket>> {
        let mut markets = Vec::new();
        for sport in Sport::ALL {
            markets.extend(self.discover_sport(sport).await?);
        }
        markets.sort_by_key(|market| market.start_time);
        Ok(markets)
    }

    pub async fn discover_sport(&self, sport: Sport) -> Result<Vec<UsMoneylineMarket>> {
        let mut result = Vec::new();
        let limit = 100;
        for page in 0..20 {
            self.throttle().await;
            let url = format!(
                "{}{}?limit={limit}&offset={}",
                self.base_url,
                sport.discovery_path(),
                page * limit
            );
            let response = self.client.get(url).send().await?.error_for_status()?;
            let payload: EventsResponse = response.json().await?;
            let count = payload.events.len();
            for event in payload.events {
                let event_id = event.id.clone();
                match normalize_event(sport, event) {
                    Ok(markets) => result.extend(markets),
                    Err(error) => {
                        tracing::warn!(%sport, %event_id, %error, "event normalization failed");
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
        self.throttle().await;
        let slug = url::form_urlencoded::byte_serialize(market_slug.as_bytes()).collect::<String>();
        let url = format!("{}/v1/markets/{slug}/book", self.base_url);
        let payload: BookResponse = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        normalize_book(payload.market_data)
    }

    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            let interval = Duration::from_millis(110);
            if elapsed < interval {
                tokio::time::sleep(interval - elapsed).await;
            }
        }
        *last = Some(tokio::time::Instant::now());
    }
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
    Ok(MarketParticipant {
        side_id: side.id.clone(),
        name: side
            .description
            .clone()
            .or_else(|| side.team.as_ref().map(|team| team.name.clone()))
            .ok_or_else(|| Error::InvalidData(format!("market side {} has no name", side.id)))?,
        long: side.long,
        team_id: side.team_id,
        provider_ids,
    })
}

fn settlement_rules_supported(sport: Sport, description: &str) -> bool {
    let normalized = description.to_lowercase();
    match sport {
        Sport::Nfl => normalized.contains("tie") && normalized.contains("$0.50"),
        Sport::Tennis => {
            normalized.contains("$0.50")
                && (normalized.contains("walkover") || normalized.contains("withdrawal"))
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
