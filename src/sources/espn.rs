use std::{collections::BTreeMap, env, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use futures::{StreamExt, stream};
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

/// ESPN's public scoreboard and core odds feeds. No key, no quota. Today the
/// core odds endpoint carries one provider (DraftKings, id 100); every
/// provider it returns is emitted as its own quote with its own family.
///
/// Environment:
/// - `ESPN_BASE_URL` (default `https://site.api.espn.com`)
/// - `ESPN_CORE_BASE_URL` (default `https://sports.core.api.espn.com`)
pub struct EspnOddsSource {
    base_url: String,
    core_base_url: String,
    client: reqwest::Client,
}

impl EspnOddsSource {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            base_url: env::var("ESPN_BASE_URL")
                .unwrap_or_else(|_| "https://site.api.espn.com".into())
                .trim_end_matches('/')
                .to_string(),
            core_base_url: env::var("ESPN_CORE_BASE_URL")
                .unwrap_or_else(|_| "https://sports.core.api.espn.com".into())
                .trim_end_matches('/')
                .to_string(),
            client: http_client(timeout)?,
        })
    }

    async fn collect_sport(&self, sport: Sport) -> Result<Vec<SourceQuote>> {
        let Some((espn_sport, league)) = espn_path(sport) else {
            return Ok(Vec::new());
        };
        let scoreboard: Scoreboard = self
            .client
            .get(format!(
                "{}/apis/site/v2/sports/{espn_sport}/{league}/scoreboard",
                self.base_url
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let fetched_at = Utc::now();
        let pregame = scoreboard
            .events
            .into_iter()
            .filter_map(|event| {
                let competition = event.competitions.into_iter().next()?;
                (competition.status.state() == "pre").then_some((event.id, event.date, competition))
            })
            .collect::<Vec<_>>();

        let results = stream::iter(pregame)
            .map(|(event_id, date, competition)| async move {
                let odds = self
                    .client
                    .get(format!(
                        "{}/v2/sports/{espn_sport}/leagues/{league}/events/{event_id}/competitions/{}/odds",
                        self.core_base_url, competition.id
                    ))
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<OddsResponse>()
                    .await?;
                Ok::<_, Error>((event_id, date, competition, odds))
            })
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;

        let mut quotes = Vec::new();
        for result in results {
            match result {
                Ok((event_id, date, competition, odds)) => {
                    match normalize_competition(
                        sport,
                        &event_id,
                        &date,
                        &competition,
                        odds,
                        fetched_at,
                    ) {
                        Ok(mut normalized) => quotes.append(&mut normalized),
                        Err(error) => warn!(%event_id, %error, "ESPN event skipped"),
                    }
                }
                Err(error) => warn!(%error, "ESPN odds fetch failed"),
            }
        }
        Ok(quotes)
    }
}

#[async_trait]
impl OddsSource for EspnOddsSource {
    fn id(&self) -> &str {
        "espn"
    }

    fn family(&self) -> SourceFamily {
        SourceFamily::DraftKings
    }

    async fn collect(&self, sports: &[Sport]) -> Result<Vec<SourceQuote>> {
        let mut quotes = Vec::new();
        for sport in sports {
            quotes.extend(self.collect_sport(*sport).await?);
        }
        Ok(quotes)
    }

    async fn probe(&self) -> SourceHealth {
        probe_by_collecting(self, &Sport::ALL).await
    }
}

fn espn_path(sport: Sport) -> Option<(&'static str, &'static str)> {
    match sport {
        Sport::Nfl => Some(("football", "nfl")),
        Sport::Nba => Some(("basketball", "nba")),
        Sport::Wnba => Some(("basketball", "wnba")),
        Sport::Mlb => Some(("baseball", "mlb")),
        Sport::Tennis => None,
    }
}

fn normalize_competition(
    sport: Sport,
    event_id: &str,
    date: &str,
    competition: &Competition,
    odds: OddsResponse,
    fetched_at: DateTime<Utc>,
) -> Result<Vec<SourceQuote>> {
    let home = competition
        .competitors
        .iter()
        .find(|competitor| competitor.home_away == "home")
        .ok_or_else(|| Error::InvalidData("no home competitor".into()))?;
    let away = competition
        .competitors
        .iter()
        .find(|competitor| competitor.home_away == "away")
        .ok_or_else(|| Error::InvalidData("no away competitor".into()))?;
    let start_time = parse_espn_date(date)?;

    Ok(odds
        .items
        .into_iter()
        .filter_map(|item| {
            let home_line = item.home_team_odds.as_ref()?.money_line?;
            let away_line = item.away_team_odds.as_ref()?.money_line?;
            if home_line == 0 || away_line == 0 {
                return None;
            }
            let provider_key = provider_key(&item.provider.name);
            Some(SourceQuote {
                source_id: format!("espn:{provider_key}"),
                family: provider_family(&item.provider.name),
                sport,
                event_id: event_id.to_string(),
                participant_a: home.team.display_name.clone(),
                participant_b: away.team.display_name.clone(),
                participant_a_provider_ids: BTreeMap::from([(
                    "espn".to_string(),
                    home.team.id.clone(),
                )]),
                participant_b_provider_ids: BTreeMap::from([(
                    "espn".to_string(),
                    away.team.id.clone(),
                )]),
                start_time,
                start_time_tolerance_minutes: DEFAULT_START_TIME_TOLERANCE_MINUTES,
                decimal_odds_a: american_to_decimal(home_line),
                decimal_odds_b: american_to_decimal(away_line),
                decimal_odds_neutral: None,
                source_timestamp: fetched_at,
                fetched_at,
                parser_version: "espn-core-odds-v1".into(),
                validation_only: false,
            })
        })
        .collect())
}

fn provider_key(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn provider_family(name: &str) -> SourceFamily {
    match provider_key(name).as_str() {
        "draftkings" => SourceFamily::DraftKings,
        "espnbet" | "thescorebet" => SourceFamily::Penn,
        "caesars" | "caesarssportsbook" | "williamhill" => SourceFamily::Caesars,
        "fanduel" => SourceFamily::FanDuel,
        "betmgm" => SourceFamily::BetMgm,
        other => SourceFamily::Other(other.to_string()),
    }
}

pub fn american_to_decimal(odds: i64) -> Decimal {
    let magnitude = Decimal::from(odds.abs());
    if odds > 0 {
        Decimal::ONE + magnitude / Decimal::ONE_HUNDRED
    } else {
        Decimal::ONE + Decimal::ONE_HUNDRED / magnitude
    }
}

fn parse_espn_date(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(parsed) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%MZ") {
        return Ok(parsed.and_utc());
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| Error::InvalidData(format!("ESPN date {value}: {error}")))
}

#[derive(Deserialize)]
struct Scoreboard {
    #[serde(default)]
    events: Vec<Event>,
}

#[derive(Deserialize)]
struct Event {
    id: String,
    date: String,
    #[serde(default)]
    competitions: Vec<Competition>,
}

#[derive(Deserialize)]
struct Competition {
    id: String,
    status: Status,
    #[serde(default)]
    competitors: Vec<Competitor>,
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "type")]
    kind: StatusType,
}

#[derive(Deserialize)]
struct StatusType {
    state: String,
}

impl Status {
    fn state(&self) -> &str {
        &self.kind.state
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Competitor {
    home_away: String,
    team: Team,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Team {
    id: String,
    display_name: String,
}

#[derive(Deserialize)]
struct OddsResponse {
    #[serde(default)]
    items: Vec<OddsItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OddsItem {
    provider: Provider,
    home_team_odds: Option<TeamOdds>,
    away_team_odds: Option<TeamOdds>,
}

#[derive(Deserialize)]
struct Provider {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TeamOdds {
    money_line: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn american_odds_convert_to_decimal() {
        assert_eq!(american_to_decimal(170), Decimal::new(27, 1));
        assert_eq!(
            american_to_decimal(-205),
            Decimal::ONE + Decimal::ONE_HUNDRED / Decimal::new(205, 0)
        );
    }

    #[test]
    fn competition_normalizes_with_home_as_participant_a() {
        let competition: Competition = serde_json::from_str(
            r#"{"id":"401","status":{"type":{"state":"pre"}},"competitors":[
              {"homeAway":"away","team":{"id":"27","displayName":"Tampa Bay Buccaneers"}},
              {"homeAway":"home","team":{"id":"4","displayName":"Cincinnati Bengals"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(competition.status.state(), "pre");
        let odds: OddsResponse = serde_json::from_str(
            r#"{"items":[{"provider":{"id":"100","name":"DraftKings"},
              "homeTeamOdds":{"moneyLine":-205},"awayTeamOdds":{"moneyLine":170}}]}"#,
        )
        .unwrap();
        let quotes = normalize_competition(
            Sport::Nfl,
            "401",
            "2026-09-13T17:00Z",
            &competition,
            odds,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(quotes.len(), 1);
        let quote = &quotes[0];
        assert_eq!(quote.participant_a, "Cincinnati Bengals");
        assert_eq!(quote.participant_b, "Tampa Bay Buccaneers");
        assert_eq!(quote.family, SourceFamily::DraftKings);
        assert_eq!(quote.source_id, "espn:draftkings");
        assert_eq!(quote.decimal_odds_b, Decimal::new(27, 1));
        assert_eq!(
            quote.start_time,
            DateTime::parse_from_rfc3339("2026-09-13T17:00:00Z").unwrap()
        );
        assert_eq!(quote.participant_a_provider_ids["espn"], "4");
    }
}
