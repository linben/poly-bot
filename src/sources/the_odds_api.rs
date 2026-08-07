use std::{collections::BTreeMap, env, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{
    Error, Result,
    domain::{SourceFamily, SourceHealth, SourceQuote, Sport},
    sources::OddsSource,
};

pub struct TheOddsApiSource {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl TheOddsApiSource {
    pub fn new(api_key: String, timeout: Duration) -> Result<Self> {
        Ok(Self {
            api_key,
            base_url: env::var("THE_ODDS_API_BASE_URL")
                .unwrap_or_else(|_| "https://api.the-odds-api.com".into()),
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent("polybot-validator/0.1")
                .build()?,
        })
    }

    async fn sport_keys(&self) -> Result<Vec<(String, Sport)>> {
        let mut keys = vec![
            ("americanfootball_nfl".into(), Sport::Nfl),
            ("basketball_nba".into(), Sport::Nba),
            ("basketball_wnba".into(), Sport::Wnba),
            ("baseball_mlb".into(), Sport::Mlb),
        ];
        if env::var("THE_ODDS_API_INCLUDE_TENNIS")
            .map(|value| value == "true")
            .unwrap_or(false)
        {
            let url = format!("{}/v4/sports?apiKey={}", self.base_url, self.api_key);
            let sports: Vec<ApiSport> = self
                .client
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            keys.extend(
                sports
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
        SourceFamily::Validation("the_odds_api".into())
    }

    fn validation_only(&self) -> bool {
        true
    }

    async fn collect(&self) -> Result<Vec<SourceQuote>> {
        let mut quotes = Vec::new();
        for (key, sport) in self.sport_keys().await? {
            let url = format!(
                "{}/v4/sports/{key}/odds?apiKey={}&regions=us,eu&markets=h2h&oddsFormat=decimal&dateFormat=iso",
                self.base_url, self.api_key
            );
            let events: Vec<ApiEvent> = self
                .client
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for mut event in events {
                let bookmakers = std::mem::take(&mut event.bookmakers);
                for bookmaker in bookmakers {
                    if let Some(quote) = normalize_event(sport, &event, bookmaker)? {
                        quotes.push(quote);
                    }
                }
            }
        }
        Ok(quotes)
    }

    async fn probe(&self) -> SourceHealth {
        let started = std::time::Instant::now();
        match self.sport_keys().await {
            Ok(keys) => SourceHealth {
                source_id: self.id().into(),
                family: self.family(),
                checked_at: Utc::now(),
                reachable: true,
                odds_found: 0,
                latency_ms: started.elapsed().as_millis() as u64,
                message: format!("{} configured sport keys", keys.len()),
            },
            Err(error) => SourceHealth {
                source_id: self.id().into(),
                family: self.family(),
                checked_at: Utc::now(),
                reachable: false,
                odds_found: 0,
                latency_ms: started.elapsed().as_millis() as u64,
                message: error.to_string(),
            },
        }
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
    let source_timestamp = bookmaker
        .last_update
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|error| Error::InvalidData(format!("bookmaker timestamp: {error}")))?
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    Ok(Some(SourceQuote {
        source_id: format!("the_odds_api:{}", bookmaker.key),
        family: SourceFamily::Validation(bookmaker.key),
        sport,
        event_id: event.id.clone(),
        participant_a: event.home_team.clone(),
        participant_b: event.away_team.clone(),
        participant_a_provider_ids: BTreeMap::new(),
        participant_b_provider_ids: BTreeMap::new(),
        start_time: DateTime::parse_from_rfc3339(&event.commence_time)
            .map_err(|error| Error::InvalidData(format!("commence time: {error}")))?
            .with_timezone(&Utc),
        decimal_odds_a: Decimal::from_f64_retain(a.price)
            .ok_or_else(|| Error::InvalidData("home odds are not finite".into()))?,
        decimal_odds_b: Decimal::from_f64_retain(b.price)
            .ok_or_else(|| Error::InvalidData("away odds are not finite".into()))?,
        decimal_odds_neutral: neutral,
        source_timestamp,
        fetched_at: Utc::now(),
        parser_version: "the-odds-api-v4".into(),
        validation_only: true,
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
    #[serde(default)]
    outcomes: Vec<ApiOutcome>,
}

#[derive(Deserialize)]
struct ApiOutcome {
    name: String,
    price: f64,
}
