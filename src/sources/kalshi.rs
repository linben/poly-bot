//! Kalshi regulated exchange, public (unauthenticated) market data.
//!
//! Reads the `KX{LEAGUE}GAME` series through `GET /markets` and turns each
//! two-market event into one moneyline quote priced at `1 / yes_ask` per side.
//! The base URL can be overridden with the `KALSHI_BASE_URL` environment
//! variable (default `https://api.elections.kalshi.com/trade-api/v2`).

use std::{collections::BTreeMap, env, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{
    Error, Result,
    domain::{
        DEFAULT_START_TIME_TOLERANCE_MINUTES, SourceFamily, SourceHealth, SourceQuote, Sport,
    },
    sources::{OddsSource, http_client, probe_by_collecting},
};

const SOURCE_ID: &str = "kalshi";
const PARSER_VERSION: &str = "kalshi-v2-markets";
const PAGE_LIMIT: usize = 200;
/// Hard stop on pagination so a misbehaving cursor cannot loop forever.
const MAX_PAGES: usize = 100;
/// Widest ask-minus-bid spread (in dollars) still considered a real price.
const MAX_SPREAD: Decimal = Decimal::from_parts(6, 0, 0, false, 2);
/// Date-only rules: accept any start on that calendar day (noon ET +/- 14h).
const DATE_ONLY_TOLERANCE_MINUTES: i64 = 14 * 60;

pub struct KalshiSource {
    base_url: String,
    client: reqwest::Client,
}

impl KalshiSource {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            base_url: env::var("KALSHI_BASE_URL")
                .unwrap_or_else(|_| "https://api.elections.kalshi.com/trade-api/v2".into()),
            client: http_client(timeout)?,
        })
    }

    async fn fetch_series(&self, series_ticker: &str) -> Result<Vec<ApiMarket>> {
        let mut markets = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let mut url = format!(
                "{}/markets?limit={PAGE_LIMIT}&status=open&series_ticker={series_ticker}",
                self.base_url
            );
            if !cursor.is_empty() {
                url.push_str("&cursor=");
                url.push_str(&cursor);
            }
            let page: ApiPage = self
                .client
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            markets.extend(page.markets);
            match page.cursor {
                Some(next) if !next.is_empty() && next != cursor => cursor = next,
                _ => return Ok(markets),
            }
        }
        Err(Error::Source {
            source_id: SOURCE_ID.into(),
            message: format!("{series_ticker}: pagination exceeded {MAX_PAGES} pages"),
        })
    }
}

#[async_trait]
impl OddsSource for KalshiSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    fn family(&self) -> SourceFamily {
        SourceFamily::Kalshi
    }

    async fn collect(&self, sports: &[Sport]) -> Result<Vec<SourceQuote>> {
        let requests = sports.iter().filter_map(|&sport| {
            let series = series_ticker(sport)?;
            Some(async move {
                let markets = self.fetch_series(series).await?;
                Ok::<_, crate::Error>(normalize_markets(sport, markets, Utc::now()))
            })
        });
        let per_sport = futures::future::try_join_all(requests).await?;
        Ok(per_sport.into_iter().flatten().collect())
    }

    async fn probe(&self) -> SourceHealth {
        probe_by_collecting(self, &Sport::ALL).await
    }
}

/// Tennis is deliberately absent: Kalshi tennis markets carry no start time
/// and settle under a different walkover rule.
fn series_ticker(sport: Sport) -> Option<&'static str> {
    match sport {
        Sport::Nfl => Some("KXNFLGAME"),
        Sport::Nba => Some("KXNBAGAME"),
        Sport::Wnba => Some("KXWNBAGAME"),
        Sport::Mlb => Some("KXMLBGAME"),
        Sport::Tennis => None,
    }
}

/// Group markets by event and emit one quote per two-sided, liquid event.
fn normalize_markets(
    sport: Sport,
    markets: Vec<ApiMarket>,
    fetched_at: DateTime<Utc>,
) -> Vec<SourceQuote> {
    let mut events: BTreeMap<String, Vec<ApiMarket>> = BTreeMap::new();
    for market in markets {
        if market.status != "active" {
            tracing::debug!(ticker = %market.ticker, status = %market.status, "kalshi: skipping inactive market");
            continue;
        }
        events
            .entry(market.event_ticker.clone())
            .or_default()
            .push(market);
    }
    events
        .into_iter()
        .filter_map(|(event_ticker, markets)| {
            normalize_event(sport, &event_ticker, &markets, fetched_at)
        })
        .collect()
}

fn normalize_event(
    sport: Sport,
    event_ticker: &str,
    markets: &[ApiMarket],
    fetched_at: DateTime<Utc>,
) -> Option<SourceQuote> {
    let [a, b] = markets else {
        tracing::warn!(
            event_ticker,
            count = markets.len(),
            "kalshi: event is not two-sided"
        );
        return None;
    };
    let ask_a = liquid_ask(event_ticker, a)?;
    let ask_b = liquid_ask(event_ticker, b)?;
    let Some((start_time, start_time_tolerance_minutes)) = parse_start_time(&a.rules_primary)
    else {
        tracing::warn!(event_ticker, rules = %a.rules_primary, "kalshi: no start date in rules");
        return None;
    };
    let source_timestamp = match (
        parse_rfc3339(event_ticker, &a.updated_time),
        parse_rfc3339(event_ticker, &b.updated_time),
    ) {
        (Some(x), Some(y)) => x.max(y),
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => return None,
    };
    Some(SourceQuote {
        source_id: SOURCE_ID.into(),
        family: SourceFamily::Kalshi,
        sport,
        event_id: event_ticker.into(),
        participant_a: a.yes_sub_title.trim().into(),
        participant_b: b.yes_sub_title.trim().into(),
        participant_a_provider_ids: BTreeMap::from([(SOURCE_ID.into(), a.ticker.clone())]),
        participant_b_provider_ids: BTreeMap::from([(SOURCE_ID.into(), b.ticker.clone())]),
        start_time,
        start_time_tolerance_minutes,
        decimal_odds_a: Decimal::ONE / ask_a,
        decimal_odds_b: Decimal::ONE / ask_b,
        decimal_odds_neutral: None,
        source_timestamp,
        fetched_at,
        parser_version: PARSER_VERSION.into(),
        validation_only: false,
    })
}

/// The YES ask of a market whose book is real: both sides quoted, ask strictly
/// inside (0, 1), and spread no wider than [`MAX_SPREAD`].
fn liquid_ask(event_ticker: &str, market: &ApiMarket) -> Option<Decimal> {
    let ticker = market.ticker.as_str();
    let (Some(bid), Some(ask)) = (
        parse_dollars(market.yes_bid_dollars.as_deref()),
        parse_dollars(market.yes_ask_dollars.as_deref()),
    ) else {
        tracing::warn!(
            event_ticker,
            ticker,
            "kalshi: missing or malformed yes bid/ask"
        );
        return None;
    };
    if ask <= Decimal::ZERO || ask >= Decimal::ONE || bid <= Decimal::ZERO {
        tracing::warn!(event_ticker, ticker, %bid, %ask, "kalshi: one-sided or empty book");
        return None;
    }
    if ask - bid > MAX_SPREAD {
        tracing::warn!(event_ticker, ticker, %bid, %ask, "kalshi: spread too wide");
        return None;
    }
    Some(ask)
}

fn parse_dollars(value: Option<&str>) -> Option<Decimal> {
    Decimal::from_str_exact(value?.trim()).ok()
}

fn parse_rfc3339(event_ticker: &str, value: &str) -> Option<DateTime<Utc>> {
    match DateTime::parse_from_rfc3339(value) {
        Ok(time) => Some(time.with_timezone(&Utc)),
        Err(error) => {
            tracing::warn!(event_ticker, value, %error, "kalshi: bad updated_time");
            None
        }
    }
}

/// Extract the scheduled start from the rules text, e.g.
/// `... originally scheduled for Sep 14, 2026 at 9:40 PM EDT, then ...`.
/// Returns the UTC instant plus the matcher tolerance: 15 minutes when a
/// clock time is present, otherwise the whole calendar day around noon ET.
fn parse_start_time(rules: &str) -> Option<(DateTime<Utc>, i64)> {
    let rest = rules
        .split_once("originally scheduled for ")
        .or_else(|| rules.split_once("scheduled for "))?
        .1;
    let (month_day, rest) = rest.split_once(',')?;
    let mut month_day = month_day.split_whitespace();
    let month = month_from_abbrev(month_day.next()?)?;
    let day: u32 = month_day.next()?.parse().ok()?;
    let rest = rest.trim_start();
    let year_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    let year: i32 = rest[..year_len].parse().ok()?;
    let rest = &rest[year_len..];
    let date = NaiveDate::from_ymd_opt(year, month, day)?;

    let Some(clock) = rest.strip_prefix(" at ") else {
        let noon = date.and_time(NaiveTime::from_hms_opt(12, 0, 0)?);
        return Some((eastern_to_utc(noon, None), DATE_ONLY_TOLERANCE_MINUTES));
    };
    let clock = clock.split(',').next()?;
    let mut parts = clock.split_whitespace();
    let (hour, minute) = parts.next()?.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    let hour = match (parts.next()?, hour) {
        ("AM", 12) => 0,
        ("AM", h) if h < 12 => h,
        ("PM", 12) => 12,
        ("PM", h) if h < 12 => h + 12,
        _ => return None,
    };
    let zone = parts.next().filter(|zone| matches!(*zone, "EDT" | "EST"));
    let local = date.and_time(NaiveTime::from_hms_opt(hour, minute, 0)?);
    Some((
        eastern_to_utc(local, zone),
        DEFAULT_START_TIME_TOLERANCE_MINUTES,
    ))
}

fn month_from_abbrev(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| name.eq_ignore_ascii_case(month))
        .map(|index| index as u32 + 1)
}

/// Convert an America/New_York wall-clock time to UTC. An explicit `EDT`/`EST`
/// wins; otherwise US daylight-saving rules decide (second Sunday of March
/// through first Sunday of November).
fn eastern_to_utc(local: NaiveDateTime, zone: Option<&str>) -> DateTime<Utc> {
    let offset_hours = match zone {
        Some("EDT") => 4,
        Some("EST") => 5,
        _ if is_us_dst(local.date()) => 4,
        _ => 5,
    };
    local.and_utc() + chrono::Duration::hours(offset_hours)
}

fn is_us_dst(date: NaiveDate) -> bool {
    let year = date.year();
    match (nth_sunday(year, 3, 2), nth_sunday(year, 11, 1)) {
        (Some(start), Some(end)) => date >= start && date < end,
        _ => false,
    }
}

fn nth_sunday(year: i32, month: u32, n: u64) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let to_sunday = (7 - first.weekday().num_days_from_sunday()) % 7;
    first.checked_add_days(Days::new(u64::from(to_sunday) + 7 * (n - 1)))
}

#[derive(Deserialize)]
struct ApiPage {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    markets: Vec<ApiMarket>,
}

#[derive(Deserialize)]
struct ApiMarket {
    ticker: String,
    event_ticker: String,
    yes_sub_title: String,
    #[serde(default)]
    yes_bid_dollars: Option<String>,
    #[serde(default)]
    yes_ask_dollars: Option<String>,
    status: String,
    #[serde(default)]
    rules_primary: String,
    #[serde(default)]
    updated_time: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn rules_with_clock_time_give_exact_instant() {
        let rules = "If Miami wins the Miami vs Arizona professional baseball game originally scheduled for Sep 14, 2026 at 9:40 PM EDT, then the market resolves to Yes.";
        assert_eq!(
            parse_start_time(rules),
            Some((
                utc("2026-09-15T01:40:00Z"),
                DEFAULT_START_TIME_TOLERANCE_MINUTES
            ))
        );
        // Explicit zone beats the DST rule; missing zone falls back to it.
        let winter = "game scheduled for Jan 5, 2026 at 1:00 PM EST, then";
        assert_eq!(
            parse_start_time(winter).unwrap().0,
            utc("2026-01-05T18:00:00Z")
        );
        let unzoned = "game scheduled for Jan 5, 2026 at 12:30 AM, then";
        assert_eq!(
            parse_start_time(unzoned).unwrap().0,
            utc("2026-01-05T05:30:00Z")
        );
        assert!(parse_start_time("no schedule here").is_none());
    }

    #[test]
    fn date_only_rules_cover_the_whole_day() {
        let rules = "If Baltimore wins the Baltimore vs Cleveland professional football game originally scheduled for Sep 21, 2026, then the market resolves to Yes.";
        assert_eq!(
            parse_start_time(rules),
            Some((utc("2026-09-21T16:00:00Z"), DATE_ONLY_TOLERANCE_MINUTES))
        );
    }

    #[test]
    fn dst_boundaries_follow_us_rules() {
        assert!(!is_us_dst(NaiveDate::from_ymd_opt(2026, 3, 7).unwrap()));
        assert!(is_us_dst(NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()));
        assert!(is_us_dst(NaiveDate::from_ymd_opt(2026, 10, 31).unwrap()));
        assert!(!is_us_dst(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()));
    }

    #[test]
    fn two_market_event_becomes_one_quote_and_wide_spread_is_rejected() {
        let page: ApiPage = serde_json::from_str(
            r#"{
              "cursor": "",
              "markets": [
                {
                  "ticker": "KXMLBGAME-26SEP142140MIAAZ-MIA",
                  "event_ticker": "KXMLBGAME-26SEP142140MIAAZ",
                  "title": "Miami wins",
                  "yes_sub_title": "Miami",
                  "yes_bid_dollars": "0.4200",
                  "yes_ask_dollars": "0.4500",
                  "no_bid_dollars": "0.5500",
                  "no_ask_dollars": "0.5800",
                  "status": "active",
                  "rules_primary": "If Miami wins the Miami vs Arizona professional baseball game originally scheduled for Sep 14, 2026 at 9:40 PM EDT, then the market resolves to Yes.",
                  "expected_expiration_time": "2026-09-15T04:40:00Z",
                  "updated_time": "2026-09-12T17:38:58.431951Z",
                  "volume_24h_fp": "154.93"
                },
                {
                  "ticker": "KXMLBGAME-26SEP142140MIAAZ-AZ",
                  "event_ticker": "KXMLBGAME-26SEP142140MIAAZ",
                  "title": "Arizona wins",
                  "yes_sub_title": "Arizona",
                  "yes_bid_dollars": "0.5500",
                  "yes_ask_dollars": "0.5800",
                  "status": "active",
                  "rules_primary": "If Arizona wins the Miami vs Arizona professional baseball game originally scheduled for Sep 14, 2026 at 9:40 PM EDT, then the market resolves to Yes.",
                  "updated_time": "2026-09-12T17:40:00Z"
                },
                {
                  "ticker": "KXMLBGAME-26SEP152140MIAAZ-MIA",
                  "event_ticker": "KXMLBGAME-26SEP152140MIAAZ",
                  "yes_sub_title": "Miami",
                  "yes_bid_dollars": "0.3000",
                  "yes_ask_dollars": "0.4500",
                  "status": "active",
                  "rules_primary": "... originally scheduled for Sep 15, 2026 at 9:40 PM EDT, then ...",
                  "updated_time": "2026-09-12T17:40:00Z"
                },
                {
                  "ticker": "KXMLBGAME-26SEP152140MIAAZ-AZ",
                  "event_ticker": "KXMLBGAME-26SEP152140MIAAZ",
                  "yes_sub_title": "Arizona",
                  "yes_bid_dollars": "0.5500",
                  "yes_ask_dollars": "0.5800",
                  "status": "active",
                  "rules_primary": "... originally scheduled for Sep 15, 2026 at 9:40 PM EDT, then ...",
                  "updated_time": "2026-09-12T17:40:00Z"
                }
              ]
            }"#,
        )
        .unwrap();
        let fetched_at = utc("2026-09-12T18:00:00Z");
        let quotes = normalize_markets(Sport::Mlb, page.markets, fetched_at);
        assert_eq!(quotes.len(), 1, "wide-spread event must be dropped");
        let quote = &quotes[0];
        assert_eq!(quote.source_id, "kalshi");
        assert_eq!(quote.family, SourceFamily::Kalshi);
        assert_eq!(quote.sport, Sport::Mlb);
        assert_eq!(quote.event_id, "KXMLBGAME-26SEP142140MIAAZ");
        assert_eq!(quote.participant_a, "Miami");
        assert_eq!(quote.participant_b, "Arizona");
        assert_eq!(
            quote
                .participant_a_provider_ids
                .get("kalshi")
                .map(String::as_str),
            Some("KXMLBGAME-26SEP142140MIAAZ-MIA")
        );
        assert_eq!(
            quote.decimal_odds_a,
            Decimal::ONE / Decimal::from_str_exact("0.4500").unwrap()
        );
        assert_eq!(
            quote.decimal_odds_b,
            Decimal::ONE / Decimal::from_str_exact("0.5800").unwrap()
        );
        assert_eq!(quote.decimal_odds_neutral, None);
        assert_eq!(quote.start_time, utc("2026-09-15T01:40:00Z"));
        assert_eq!(
            quote.start_time_tolerance_minutes,
            DEFAULT_START_TIME_TOLERANCE_MINUTES
        );
        assert_eq!(quote.source_timestamp, utc("2026-09-12T17:40:00Z"));
        assert_eq!(quote.fetched_at, fetched_at);
        assert_eq!(quote.parser_version, PARSER_VERSION);
        assert!(!quote.validation_only);
    }
}
