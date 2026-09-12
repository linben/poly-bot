//! Pinnacle's guest API: the read-only, unauthenticated feed behind
//! pinnacle.com's odds pages. Pinnacle is the reference book in this
//! system (`SourceFamily::Pinnacle`), so this adapter is what lets a
//! candidate reach `actionable` without a paid feed.
//!
//! Environment:
//! - `PINNACLE_BASE_URL` (default `https://guest.api.arcadia.pinnacle.com/0.1`)
//! - `PINNACLE_GUEST_API_KEY` (default: the public key embedded in the web
//!   app; the API rejects requests without one)
//!
//! Per sport two requests run concurrently: `matchups` (participants, start
//! time, live flag) and `markets/straight` (prices), joined on `matchupId`.
//! Team sports are league-scoped; tennis uses the sport-wide endpoints so
//! every tour (ATP, WTA, Challenger, ITF) is covered by one pair of calls.

use std::{collections::BTreeMap, env, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::warn;

use crate::{
    Result,
    domain::{
        DEFAULT_START_TIME_TOLERANCE_MINUTES, SourceFamily, SourceHealth, SourceQuote, Sport,
    },
    sources::{OddsSource, espn::american_to_decimal, http_client, probe_by_collecting},
};

const DEFAULT_BASE_URL: &str = "https://guest.api.arcadia.pinnacle.com/0.1";
/// Public key shipped in pinnacle.com's front-end bundle; identifies the
/// guest (logged-out) client, not a user.
const DEFAULT_GUEST_KEY: &str = "CmX2KcMrXuFmNg6YFbmTxE0y9CIrOi0R";
/// Period 0 (full game) moneyline.
const MONEYLINE_KEY: &str = "s;0;m";
/// Implied probabilities of a two-way market must sum inside this band; the
/// book's hold is normally 2-4%.
const MIN_OVERROUND: Decimal = Decimal::from_parts(100, 0, 0, false, 2);
const MAX_OVERROUND: Decimal = Decimal::from_parts(115, 0, 0, false, 2);

pub struct PinnacleSource {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl PinnacleSource {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            base_url: env::var("PINNACLE_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.into())
                .trim_end_matches('/')
                .to_string(),
            api_key: env::var("PINNACLE_GUEST_API_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_GUEST_KEY.into()),
            client: http_client(timeout)?,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self
            .client
            .get(format!("{}{path}", self.base_url))
            .header("X-API-Key", &self.api_key)
            .header("X-Device-UUID", "polybot-research-scanner")
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn collect_sport(&self, sport: Sport) -> Result<Vec<SourceQuote>> {
        let scope = scope(sport);
        let (matchups, markets) = futures::future::try_join(
            self.get::<Vec<Matchup>>(&format!("{scope}/matchups")),
            self.get::<Vec<Market>>(&format!("{scope}/markets/straight")),
        )
        .await?;
        Ok(normalize(sport, matchups, markets, Utc::now()))
    }
}

/// `/leagues/{id}` for team sports, `/sports/33` for every tennis tour.
fn scope(sport: Sport) -> &'static str {
    match sport {
        Sport::Nfl => "/leagues/889",
        Sport::Nba => "/leagues/487",
        Sport::Wnba => "/leagues/578",
        Sport::Mlb => "/leagues/246",
        Sport::Tennis => "/sports/33",
    }
}

#[async_trait]
impl OddsSource for PinnacleSource {
    fn id(&self) -> &str {
        "pinnacle"
    }

    fn family(&self) -> SourceFamily {
        SourceFamily::Pinnacle
    }

    async fn collect(&self, sports: &[Sport]) -> Result<Vec<SourceQuote>> {
        let per_sport =
            futures::future::try_join_all(sports.iter().map(|sport| self.collect_sport(*sport)))
                .await?;
        Ok(per_sport.into_iter().flatten().collect())
    }

    async fn probe(&self) -> SourceHealth {
        probe_by_collecting(self, &Sport::ALL).await
    }
}

/// Join open period-0 moneylines onto pregame game matchups. Props,
/// alternates, live and started matchups are dropped silently (expected);
/// malformed rows are logged.
fn normalize(
    sport: Sport,
    matchups: Vec<Matchup>,
    markets: Vec<Market>,
    fetched_at: DateTime<Utc>,
) -> Vec<SourceQuote> {
    let games = matchups
        .into_iter()
        .filter(|matchup| {
            matchup.kind == "matchup"
                && matchup.special.is_none()
                && matchup.parent_id.is_none()
                && !matchup.is_live
        })
        .map(|matchup| (matchup.id, matchup))
        .collect::<BTreeMap<_, _>>();

    markets
        .into_iter()
        .filter(|market| {
            market.key == MONEYLINE_KEY && market.status == "open" && !market.is_alternate
        })
        .filter_map(|market| {
            let matchup = games.get(&market.matchup_id)?;
            let start_time = match DateTime::parse_from_rfc3339(&matchup.start_time) {
                Ok(start) => start.with_timezone(&Utc),
                Err(error) => {
                    warn!(matchup = matchup.id, %error, "pinnacle: bad startTime");
                    return None;
                }
            };
            if start_time <= fetched_at {
                return None;
            }
            let home = matchup.participant("home")?;
            let away = matchup.participant("away")?;
            if market.prices.len() != 2 {
                return None;
            }
            let home_price = market.price("home")?;
            let away_price = market.price("away")?;
            if home_price == 0 || away_price == 0 {
                return None;
            }
            let decimal_odds_a = american_to_decimal(home_price).round_dp(4);
            let decimal_odds_b = american_to_decimal(away_price).round_dp(4);
            let overround = Decimal::ONE / decimal_odds_a + Decimal::ONE / decimal_odds_b;
            if !(MIN_OVERROUND..=MAX_OVERROUND).contains(&overround) {
                warn!(
                    matchup = matchup.id,
                    %overround,
                    "pinnacle: implausible two-way moneyline"
                );
                return None;
            }
            Some(SourceQuote {
                source_id: "pinnacle".into(),
                family: SourceFamily::Pinnacle,
                sport,
                event_id: matchup.id.to_string(),
                participant_a: home.name.clone(),
                participant_b: away.name.clone(),
                participant_a_provider_ids: BTreeMap::new(),
                participant_b_provider_ids: BTreeMap::new(),
                start_time,
                start_time_tolerance_minutes: DEFAULT_START_TIME_TOLERANCE_MINUTES,
                decimal_odds_a,
                decimal_odds_b,
                decimal_odds_neutral: None,
                source_timestamp: fetched_at,
                fetched_at,
                parser_version: "pinnacle-v1".into(),
                validation_only: false,
            })
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Matchup {
    id: u64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    start_time: String,
    #[serde(default)]
    is_live: bool,
    #[serde(default)]
    parent_id: Option<u64>,
    #[serde(default)]
    special: Option<serde_json::Value>,
    #[serde(default)]
    participants: Vec<Participant>,
}

impl Matchup {
    fn participant(&self, alignment: &str) -> Option<&Participant> {
        self.participants
            .iter()
            .find(|participant| participant.alignment == alignment)
    }
}

#[derive(Deserialize)]
struct Participant {
    name: String,
    #[serde(default)]
    alignment: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Market {
    key: String,
    matchup_id: u64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    is_alternate: bool,
    #[serde(default)]
    prices: Vec<Price>,
}

impl Market {
    fn price(&self, designation: &str) -> Option<i64> {
        self.prices
            .iter()
            .find(|price| price.designation.as_deref() == Some(designation))
            .map(|price| price.price)
    }
}

#[derive(Deserialize)]
struct Price {
    #[serde(default)]
    designation: Option<String>,
    price: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MATCHUPS: &str = r#"[
      {"id": 1630889899, "type": "matchup", "startTime": "2099-09-13T17:00:00Z", "isLive": false,
       "parentId": null, "special": null,
       "participants": [{"name": "Pittsburgh Steelers", "alignment": "home"}, {"name": "Atlanta Falcons", "alignment": "away"}]},
      {"id": 1635782187, "type": "special", "startTime": "2099-09-13T17:00:00Z", "isLive": false,
       "parentId": 1630889899, "special": {"category": "Player Props", "description": "Passing Yards"},
       "participants": [{"name": "Over", "alignment": "neutral"}, {"name": "Under", "alignment": "neutral"}]},
      {"id": 1630889900, "type": "matchup", "startTime": "2000-01-01T00:00:00Z", "isLive": true,
       "parentId": null, "special": null,
       "participants": [{"name": "Old Team", "alignment": "home"}, {"name": "Older Team", "alignment": "away"}]}
    ]"#;

    const MARKETS: &str = r#"[
      {"key": "s;0;m", "matchupId": 1630889899, "status": "open", "isAlternate": false,
       "prices": [{"designation": "home", "price": -112}, {"designation": "away", "price": -101}]},
      {"key": "s;0;s", "matchupId": 1630889899, "status": "open", "isAlternate": false,
       "prices": [{"designation": "home", "price": -105, "points": -1.5}, {"designation": "away", "price": -105, "points": 1.5}]},
      {"key": "s;1;m", "matchupId": 1630889899, "status": "open", "isAlternate": false,
       "prices": [{"designation": "home", "price": -120}, {"designation": "away", "price": 105}]},
      {"key": "s;0;m", "matchupId": 1630889899, "status": "open", "isAlternate": true,
       "prices": [{"designation": "home", "price": -300}, {"designation": "away", "price": 250}]},
      {"key": "s;0;m", "matchupId": 1635782187, "status": "open", "isAlternate": false,
       "prices": [{"designation": "home", "price": -115}, {"designation": "away", "price": -105}]},
      {"key": "s;0;m", "matchupId": 1630889900, "status": "open", "isAlternate": false,
       "prices": [{"designation": "home", "price": -115}, {"designation": "away", "price": -105}]}
    ]"#;

    fn quotes() -> Vec<SourceQuote> {
        normalize(
            Sport::Nfl,
            serde_json::from_str(MATCHUPS).unwrap(),
            serde_json::from_str(MARKETS).unwrap(),
            Utc::now(),
        )
    }

    #[test]
    fn game_moneyline_becomes_one_quote_and_everything_else_is_dropped() {
        let quotes = quotes();
        assert_eq!(quotes.len(), 1, "{quotes:?}");
        let quote = &quotes[0];
        assert_eq!(quote.participant_a, "Pittsburgh Steelers");
        assert_eq!(quote.participant_b, "Atlanta Falcons");
        assert_eq!(quote.decimal_odds_a, Decimal::new(18929, 4));
        assert_eq!(quote.decimal_odds_b, Decimal::new(19901, 4));
        assert_eq!(quote.family, SourceFamily::Pinnacle);
        assert_eq!(quote.event_id, "1630889899");
    }

    #[test]
    fn tennis_uses_the_sport_scope_and_team_leagues_are_pinned() {
        assert_eq!(scope(Sport::Tennis), "/sports/33");
        assert_eq!(scope(Sport::Nfl), "/leagues/889");
    }
}
