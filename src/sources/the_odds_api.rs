use std::{
    collections::BTreeMap,
    env,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::warn;

use crate::{
    Error, Result,
    domain::{
        DEFAULT_START_TIME_TOLERANCE_MINUTES, SourceFamily, SourceHealth, SourceQuote, Sport,
    },
    sources::{OddsSource, http_client, probe_by_collecting},
};

/// The Odds API (free plan: 500 credits per month, one credit per region per
/// market per request). Far too small for a five-minute feed, but one call
/// returns every major US and EU book for a sport, which is exactly what the
/// confirmation pass needs after the free continuous sources flag a candidate.
///
/// Environment:
/// - `THE_ODDS_API_KEY` (required)
/// - `THE_ODDS_API_BASE_URL` (default `https://api.the-odds-api.com`)
/// - `THE_ODDS_API_REGIONS` (default `us,eu`; each region costs one credit)
/// - `THE_ODDS_API_MIN_REMAINING` (default `40`; refuse to spend below this)
/// - `THE_ODDS_API_INCLUDE_TENNIS` (default `false`; discovers ATP/WTA keys)
pub struct TheOddsApiSource {
    api_key: String,
    base_url: String,
    regions: String,
    minimum_remaining: i64,
    include_tennis: bool,
    remaining: AtomicI64,
    client: reqwest::Client,
}

impl TheOddsApiSource {
    pub fn new(api_key: String, timeout: Duration) -> Result<Self> {
        let minimum_remaining = env::var("THE_ODDS_API_MIN_REMAINING")
            .ok()
            .map(|value| {
                value
                    .parse::<i64>()
                    .map_err(|error| Error::Config(format!("THE_ODDS_API_MIN_REMAINING: {error}")))
            })
            .transpose()?
            .unwrap_or(40);
        Ok(Self {
            api_key,
            base_url: env::var("THE_ODDS_API_BASE_URL")
                .unwrap_or_else(|_| "https://api.the-odds-api.com".into()),
            regions: env::var("THE_ODDS_API_REGIONS").unwrap_or_else(|_| "us,eu".into()),
            minimum_remaining,
            include_tennis: env::var("THE_ODDS_API_INCLUDE_TENNIS")
                .map(|value| value.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            remaining: AtomicI64::new(i64::MAX),
            client: http_client(timeout)?,
        })
    }

    pub fn credits_remaining(&self) -> Option<i64> {
        let value = self.remaining.load(Ordering::Relaxed);
        (value != i64::MAX).then_some(value)
    }

    fn budget_allows(&self) -> bool {
        self.remaining.load(Ordering::Relaxed) >= self.minimum_remaining
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: String) -> Result<T> {
        let response = self.client.get(url).send().await?;
        if let Some(remaining) = response
            .headers()
            .get("x-requests-remaining")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<f64>().ok())
        {
            self.remaining
                .store(remaining.floor() as i64, Ordering::Relaxed);
        }
        Ok(response.error_for_status()?.json().await?)
    }

    async fn sport_keys(&self, sports: &[Sport]) -> Result<Vec<(String, Sport)>> {
        let mut keys = Vec::new();
        for sport in sports {
            match sport {
                Sport::Nfl => keys.push(("americanfootball_nfl".into(), Sport::Nfl)),
                Sport::Nba => keys.push(("basketball_nba".into(), Sport::Nba)),
                Sport::Wnba => keys.push(("basketball_wnba".into(), Sport::Wnba)),
                Sport::Mlb => keys.push(("baseball_mlb".into(), Sport::Mlb)),
                Sport::Tennis => {}
            }
        }
        if self.include_tennis && sports.contains(&Sport::Tennis) {
            // The sports index is free; it does not consume credits.
            let url = format!("{}/v4/sports?apiKey={}", self.base_url, self.api_key);
            let listed: Vec<ApiSport> = self.get_json(url).await?;
            keys.extend(
                listed
                    .into_iter()
                    .filter(|sport| sport.active && sport.group.eq_ignore_ascii_case("tennis"))
                    .map(|sport| (sport.key, Sport::Tennis)),
            );
        }
        Ok(keys)
    }
}

#[async_trait]
impl OddsSource for TheOddsApiSource {
    fn id(&self) -> &str {
        "the_odds_api"
    }

    fn family(&self) -> SourceFamily {
        SourceFamily::Other("the_odds_api".into())
    }

    fn confirmation_only(&self) -> bool {
        true
    }

    async fn collect(&self, sports: &[Sport]) -> Result<Vec<SourceQuote>> {
        let mut quotes = Vec::new();
        for (key, sport) in self.sport_keys(sports).await? {
            if !self.budget_allows() {
                warn!(
                    remaining = ?self.credits_remaining(),
                    floor = self.minimum_remaining,
                    "The Odds API credit floor reached; skipping remaining sports"
                );
                break;
            }
            let url = format!(
                "{}/v4/sports/{key}/odds?apiKey={}&regions={}&markets=h2h&oddsFormat=decimal&dateFormat=iso",
                self.base_url, self.api_key, self.regions
            );
            let events: Vec<ApiEvent> = self.get_json(url).await?;
            for mut event in events {
                let bookmakers = std::mem::take(&mut event.bookmakers);
                for bookmaker in bookmakers {
                    match normalize_event(sport, &event, bookmaker) {
                        Ok(Some(quote)) => quotes.push(quote),
                        Ok(None) => {}
                        Err(error) => {
                            warn!(event = %event.id, %error, "The Odds API bookmaker skipped")
                        }
                    }
                }
            }
        }
        Ok(quotes)
    }

    async fn probe(&self) -> SourceHealth {
        // A probe spends credits; restrict it to one cheap sport.
        let mut health = probe_by_collecting(self, &[Sport::Mlb]).await;
        if let Some(remaining) = self.credits_remaining() {
            health.message = format!("{} ({remaining} credits remaining)", health.message);
        }
        health
    }
}

fn normalize_event(
    sport: Sport,
    event: &ApiEvent,
    bookmaker: ApiBookmaker,
) -> Result<Option<SourceQuote>> {
    let Some(market) = bookmaker
        .markets
        .into_iter()
        .find(|market| market.key == "h2h")
    else {
        return Ok(None);
    };
    let outcome_a = market
        .outcomes
        .iter()
        .find(|outcome| outcome.name == event.home_team);
    let outcome_b = market
        .outcomes
        .iter()
        .find(|outcome| outcome.name == event.away_team);
    let (Some(a), Some(b)) = (outcome_a, outcome_b) else {
        return Ok(None);
    };
    let neutral_outcome = market.outcomes.iter().find(|outcome| {
        outcome.name.eq_ignore_ascii_case("draw") || outcome.name.eq_ignore_ascii_case("tie")
    });
    let neutral = match neutral_outcome {
        Some(outcome) => Some(
            Decimal::from_f64_retain(outcome.price)
                .ok_or_else(|| Error::InvalidData("neutral odds are not finite".into()))?,
        ),
        None => None,
    };
    let source_timestamp = market
        .last_update
        .as_deref()
        .or(bookmaker.last_update.as_deref())
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|error| Error::InvalidData(format!("bookmaker timestamp: {error}")))?
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    Ok(Some(SourceQuote {
        source_id: format!("the_odds_api:{}", bookmaker.key),
        family: SourceFamily::from_odds_api_key(&bookmaker.key),
        sport,
        event_id: event.id.clone(),
        participant_a: event.home_team.clone(),
        participant_b: event.away_team.clone(),
        participant_a_provider_ids: BTreeMap::new(),
        participant_b_provider_ids: BTreeMap::new(),
        start_time: DateTime::parse_from_rfc3339(&event.commence_time)
            .map_err(|error| Error::InvalidData(format!("commence time: {error}")))?
            .with_timezone(&Utc),
        start_time_tolerance_minutes: DEFAULT_START_TIME_TOLERANCE_MINUTES,
        decimal_odds_a: Decimal::from_f64_retain(a.price)
            .ok_or_else(|| Error::InvalidData("home odds are not finite".into()))?,
        decimal_odds_b: Decimal::from_f64_retain(b.price)
            .ok_or_else(|| Error::InvalidData("away odds are not finite".into()))?,
        decimal_odds_neutral: neutral,
        source_timestamp,
        fetched_at: Utc::now(),
        parser_version: "the-odds-api-v4".into(),
        validation_only: false,
    }))
}

#[derive(Deserialize)]
struct ApiSport {
    key: String,
    group: String,
    active: bool,
}

#[derive(Deserialize)]
struct ApiEvent {
    id: String,
    commence_time: String,
    home_team: String,
    away_team: String,
    #[serde(default)]
    bookmakers: Vec<ApiBookmaker>,
}

#[derive(Deserialize)]
struct ApiBookmaker {
    key: String,
    last_update: Option<String>,
    #[serde(default)]
    markets: Vec<ApiMarket>,
}

#[derive(Deserialize)]
struct ApiMarket {
    key: String,
    last_update: Option<String>,
    #[serde(default)]
    outcomes: Vec<ApiOutcome>,
}

#[derive(Deserialize)]
struct ApiOutcome {
    name: String,
    price: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmakers_map_to_families_and_enter_consensus() {
        let event: ApiEvent = serde_json::from_str(
            r#"{
              "id": "abc",
              "commence_time": "2026-09-13T17:00:00Z",
              "home_team": "Cincinnati Bengals",
              "away_team": "Tampa Bay Buccaneers",
              "bookmakers": [{
                "key": "pinnacle",
                "last_update": "2026-09-12T17:00:00Z",
                "markets": [{"key": "h2h", "last_update": "2026-09-12T17:05:00Z", "outcomes": [
                  {"name": "Cincinnati Bengals", "price": 1.48},
                  {"name": "Tampa Bay Buccaneers", "price": 2.72}
                ]}]
              }]
            }"#,
        )
        .unwrap();
        let mut event = event;
        let bookmaker = event.bookmakers.remove(0);
        let quote = normalize_event(Sport::Nfl, &event, bookmaker)
            .unwrap()
            .unwrap();
        assert_eq!(quote.family, SourceFamily::Pinnacle);
        assert!(quote.family.is_reference());
        assert!(!quote.validation_only);
        assert_eq!(quote.source_id, "the_odds_api:pinnacle");
        assert_eq!(
            quote.source_timestamp,
            DateTime::parse_from_rfc3339("2026-09-12T17:05:00Z").unwrap()
        );
        assert_eq!(
            quote.decimal_odds_a,
            Decimal::from_f64_retain(1.48).unwrap()
        );
    }

    #[test]
    fn skins_collapse_onto_one_family() {
        assert_eq!(
            SourceFamily::from_odds_api_key("betrivers"),
            SourceFamily::from_odds_api_key("unibet_us")
        );
        assert_eq!(
            SourceFamily::from_odds_api_key("williamhill_us"),
            SourceFamily::Caesars
        );
        assert_eq!(
            SourceFamily::from_odds_api_key("novig"),
            SourceFamily::Other("novig".into())
        );
        assert!(!SourceFamily::from_odds_api_key("novig").is_reference());
    }
}
