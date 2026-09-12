use std::{collections::BTreeMap, env, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    Error, Result,
    domain::{
        DEFAULT_START_TIME_TOLERANCE_MINUTES, SourceFamily, SourceHealth, SourceQuote, Sport,
    },
    sources::{OddsSource, espn::american_to_decimal, http_client, probe_by_collecting},
};

/// Action Network's public web scoreboard. No key, no quota. One request per
/// league returns every game with a row of moneylines per sportsbook; each
/// mapped book is emitted as its own quote under `action_network:{book}` with
/// that book's family. Unmapped books (Consensus, Open, state skins) and rows
/// whose last update is older than [`MAX_LINE_AGE`] are dropped.
///
/// Environment:
/// - `ACTION_NETWORK_BASE_URL` (default `https://api.actionnetwork.com/web/v1`)
pub struct ActionNetworkSource {
    base_url: String,
    client: reqwest::Client,
}

/// `inserted` is when the book's line last *changed*, not when it was last
/// confirmed. Observed live: NFL lines set 17 h before kickoff and WNBA heavy
/// favourites untouched for days are still the live price, while true stale
/// openers (Westgate, months old) are what this guards against. 48 h keeps
/// the former and drops the latter.
const MAX_LINE_AGE: chrono::Duration = chrono::Duration::hours(48);

/// Books requested from the API and accepted client-side. The API frequently
/// ignores the `bookIds` filter and returns its default set, so membership
/// here is the real gate.
const BOOK_IDS: [u64; 12] = [68, 69, 75, 123, 79, 71, 2988, 78, 34, 1, 21, 16];

impl ActionNetworkSource {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            base_url: env::var("ACTION_NETWORK_BASE_URL")
                .unwrap_or_else(|_| "https://api.actionnetwork.com/web/v1".into())
                .trim_end_matches('/')
                .to_string(),
            client: http_client(timeout)?,
        })
    }

    async fn collect_sport(&self, sport: Sport) -> Result<Vec<SourceQuote>> {
        let Some(league) = league_slug(sport) else {
            return Ok(Vec::new());
        };
        let book_ids = BOOK_IDS
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let scoreboard: Scoreboard = self
            .client
            .get(format!("{}/scoreboard/{league}", self.base_url))
            .query(&[("period", "game"), ("bookIds", book_ids.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(normalize_games(sport, scoreboard.games, Utc::now()))
    }
}

#[async_trait]
impl OddsSource for ActionNetworkSource {
    fn id(&self) -> &str {
        "action_network"
    }

    fn family(&self) -> SourceFamily {
        SourceFamily::Other("action_network".into())
    }

    fn families(&self) -> Vec<SourceFamily> {
        BOOK_IDS
            .iter()
            .filter_map(|&book_id| book(book_id))
            .map(|(_, family)| family)
            .collect()
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

fn league_slug(sport: Sport) -> Option<&'static str> {
    match sport {
        Sport::Nfl => Some("nfl"),
        Sport::Nba => Some("nba"),
        Sport::Wnba => Some("wnba"),
        Sport::Mlb => Some("mlb"),
        Sport::Tennis => None,
    }
}

fn book(book_id: u64) -> Option<(&'static str, SourceFamily)> {
    Some(match book_id {
        68 => ("draftkings", SourceFamily::DraftKings),
        69 => ("fanduel", SourceFamily::FanDuel),
        75 => ("betmgm", SourceFamily::BetMgm),
        123 => ("caesars", SourceFamily::Caesars),
        79 => ("bet365", SourceFamily::Bet365),
        71 => ("betrivers", SourceFamily::Kambi),
        2988 => ("fanatics", SourceFamily::Fanatics),
        78 => ("circa", SourceFamily::Circa),
        34 => ("bookmaker", SourceFamily::Bookmaker),
        1 => ("betonline", SourceFamily::BetOnline),
        21 => ("bovada", SourceFamily::Bovada),
        16 => ("heritage", SourceFamily::Other("heritage".into())),
        _ => return None,
    })
}

/// Pregame full-game moneylines from every mapped book, home as participant A.
/// `fetched_at` is also the reference for line staleness.
fn normalize_games(sport: Sport, games: Vec<Game>, fetched_at: DateTime<Utc>) -> Vec<SourceQuote> {
    let mut quotes = Vec::new();
    for game in games {
        if game.status != "scheduled" {
            debug!(game_id = game.id, status = %game.status, "Action Network game not pregame");
            continue;
        }
        match normalize_game(sport, &game, fetched_at) {
            Ok(mut normalized) => quotes.append(&mut normalized),
            Err(error) => warn!(game_id = game.id, %error, "Action Network game skipped"),
        }
    }
    quotes
}

fn normalize_game(
    sport: Sport,
    game: &Game,
    fetched_at: DateTime<Utc>,
) -> Result<Vec<SourceQuote>> {
    let team = |team_id: u64, role: &str| {
        game.teams
            .iter()
            .find(|team| team.id == team_id)
            .ok_or_else(|| Error::InvalidData(format!("no {role} team {team_id}")))
    };
    let home = team(game.home_team_id, "home")?;
    let away = team(game.away_team_id, "away")?;
    let start_time = parse_timestamp(&game.start_time)?;

    let mut quotes = Vec::new();
    for row in &game.odds {
        if row.kind != "game" {
            continue;
        }
        let Some((slug, family)) = book(row.book_id) else {
            continue;
        };
        let (Some(home_line), Some(away_line)) = (row.ml_home, row.ml_away) else {
            debug!(
                game_id = game.id,
                book_id = row.book_id,
                "Action Network one-sided row"
            );
            continue;
        };
        if home_line == 0 || away_line == 0 {
            debug!(
                game_id = game.id,
                book_id = row.book_id,
                "Action Network zero moneyline"
            );
            continue;
        }
        let source_timestamp = match row.inserted.as_deref().map(parse_timestamp) {
            Some(Ok(inserted)) => inserted,
            Some(Err(error)) => {
                warn!(game_id = game.id, book_id = row.book_id, %error, "Action Network row skipped");
                continue;
            }
            None => fetched_at,
        };
        if fetched_at - source_timestamp > MAX_LINE_AGE {
            debug!(game_id = game.id, book_id = row.book_id, %source_timestamp, "Action Network stale line");
            continue;
        }
        let decimal_odds_a = american_to_decimal(home_line).round_dp(4);
        let decimal_odds_b = american_to_decimal(away_line).round_dp(4);
        let overround = Decimal::ONE / decimal_odds_a + Decimal::ONE / decimal_odds_b;
        if !(Decimal::ONE..=Decimal::new(115, 2)).contains(&overround) {
            warn!(game_id = game.id, book_id = row.book_id, home_line, away_line, %overround, "Action Network implausible prices");
            continue;
        }
        quotes.push(SourceQuote {
            source_id: format!("action_network:{slug}"),
            family,
            sport,
            event_id: game.id.to_string(),
            participant_a: home.full_name.clone(),
            participant_b: away.full_name.clone(),
            participant_a_provider_ids: BTreeMap::from([(
                "action_network".to_string(),
                home.id.to_string(),
            )]),
            participant_b_provider_ids: BTreeMap::from([(
                "action_network".to_string(),
                away.id.to_string(),
            )]),
            start_time,
            start_time_tolerance_minutes: DEFAULT_START_TIME_TOLERANCE_MINUTES,
            decimal_odds_a,
            decimal_odds_b,
            decimal_odds_neutral: None,
            source_timestamp,
            fetched_at,
            parser_version: "action_network-v1".into(),
            validation_only: false,
        });
    }
    Ok(quotes)
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| Error::InvalidData(format!("Action Network timestamp {value}: {error}")))
}

#[derive(Deserialize)]
struct Scoreboard {
    #[serde(default)]
    games: Vec<Game>,
}

#[derive(Deserialize)]
struct Game {
    id: u64,
    start_time: String,
    status: String,
    home_team_id: u64,
    away_team_id: u64,
    #[serde(default)]
    teams: Vec<Team>,
    #[serde(default)]
    odds: Vec<OddsRow>,
}

#[derive(Deserialize)]
struct Team {
    id: u64,
    full_name: String,
}

#[derive(Deserialize)]
struct OddsRow {
    book_id: u64,
    #[serde(rename = "type")]
    kind: String,
    ml_home: Option<i64>,
    ml_away: Option<i64>,
    inserted: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a live `/scoreboard/nfl?period=game` response. Books:
    /// 68 DraftKings, 69 FanDuel, 75 BetMGM (fresh), 30 Open (unmapped),
    /// 1 BetOnline (stale, three weeks old), 71 BetRivers (one-sided).
    const FIXTURE: &str = r#"{"league":{"name":"nfl"},"games":[
      {"id":290844,"start_time":"2026-09-13T17:00:00.000Z","status":"scheduled",
       "home_team_id":135,"away_team_id":127,
       "teams":[{"id":127,"full_name":"New York Jets","abbr":"NYJ"},{"id":135,"full_name":"Tennessee Titans","abbr":"TEN"}],
       "odds":[
         {"book_id":68,"type":"game","ml_home":-130,"ml_away":110,"inserted":"2026-09-12T18:50:02.294126+00:00"},
         {"book_id":69,"type":"game","ml_home":-118,"ml_away":-100,"inserted":"2026-09-12T15:57:07.241901+00:00"},
         {"book_id":75,"type":"game","ml_home":-120,"ml_away":100,"inserted":"2026-09-12T18:39:50.215594+00:00"},
         {"book_id":30,"type":"game","ml_home":-145,"ml_away":120,"inserted":"2026-09-12T18:42:12.525190+00:00"},
         {"book_id":1,"type":"game","ml_home":-130,"ml_away":110,"inserted":"2026-08-24T13:42:12.525190+00:00"},
         {"book_id":71,"type":"game","ml_home":-125,"ml_away":null,"inserted":"2026-09-12T19:06:14.350421+00:00"}
       ]},
      {"id":290801,"start_time":"2026-09-12T00:15:00.000Z","status":"complete",
       "home_team_id":112,"away_team_id":120,
       "teams":[{"id":112,"full_name":"Green Bay Packers","abbr":"GB"},{"id":120,"full_name":"Washington Commanders","abbr":"WAS"}],
       "odds":[{"book_id":68,"type":"game","ml_home":-160,"ml_away":135,"inserted":"2026-09-12T18:50:02.294126+00:00"}]}
    ]}"#;

    fn fetched_at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-12T21:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn scheduled_game_emits_one_quote_per_mapped_fresh_book() {
        let scoreboard: Scoreboard = serde_json::from_str(FIXTURE).unwrap();
        let quotes = normalize_games(Sport::Nfl, scoreboard.games, fetched_at());

        let mut source_ids = quotes
            .iter()
            .map(|quote| quote.source_id.as_str())
            .collect::<Vec<_>>();
        source_ids.sort_unstable();
        assert_eq!(
            source_ids,
            [
                "action_network:betmgm",
                "action_network:draftkings",
                "action_network:fanduel"
            ]
        );
        let families = quotes
            .iter()
            .map(|quote| quote.family.clone())
            .collect::<Vec<_>>();
        assert!(families.contains(&SourceFamily::DraftKings));
        assert!(families.contains(&SourceFamily::FanDuel));
        assert!(families.contains(&SourceFamily::BetMgm));
        assert!(quotes.iter().all(|quote| quote.event_id == "290844"));

        let draftkings = quotes
            .iter()
            .find(|quote| quote.source_id == "action_network:draftkings")
            .unwrap();
        assert_eq!(draftkings.sport, Sport::Nfl);
        assert_eq!(draftkings.participant_a, "Tennessee Titans");
        assert_eq!(draftkings.participant_b, "New York Jets");
        assert_eq!(
            draftkings.participant_a_provider_ids["action_network"],
            "135"
        );
        assert_eq!(
            draftkings.participant_b_provider_ids["action_network"],
            "127"
        );
        assert_eq!(draftkings.decimal_odds_a, Decimal::new(17692, 4));
        assert_eq!(draftkings.decimal_odds_b, Decimal::new(21, 1));
        assert_eq!(
            draftkings.start_time,
            DateTime::parse_from_rfc3339("2026-09-13T17:00:00Z").unwrap()
        );
        assert_eq!(
            draftkings.source_timestamp,
            DateTime::parse_from_rfc3339("2026-09-12T18:50:02.294126Z").unwrap()
        );
        assert_eq!(draftkings.fetched_at, fetched_at());
        assert_eq!(draftkings.parser_version, "action_network-v1");
    }

    #[test]
    fn stale_row_is_fresh_again_when_fetched_within_the_window() {
        let scoreboard: Scoreboard = serde_json::from_str(FIXTURE).unwrap();
        let fetched_at = DateTime::parse_from_rfc3339("2026-08-24T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let quotes = normalize_games(Sport::Nfl, scoreboard.games, fetched_at);
        assert!(
            quotes
                .iter()
                .any(|quote| quote.source_id == "action_network:betonline")
        );
    }

    #[test]
    fn families_cover_every_mapped_book() {
        let source = ActionNetworkSource::new(Duration::from_secs(1)).unwrap();
        let families = source.families();
        assert_eq!(families.len(), BOOK_IDS.len());
        assert!(families.contains(&SourceFamily::DraftKings));
        assert!(families.contains(&SourceFamily::FanDuel));
        assert_eq!(
            source.family(),
            SourceFamily::Other("action_network".into())
        );
    }

    #[tokio::test]
    async fn tennis_is_not_offered_and_needs_no_request() {
        let source = ActionNetworkSource::new(Duration::from_secs(1)).unwrap();
        assert!(source.collect(&[Sport::Tennis]).await.unwrap().is_empty());
    }
}
