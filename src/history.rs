//! Postgres history archive (`postgres` feature, `DATABASE_URL`). The file or
//! AWS store stays the operational truth; this records every scan's inputs
//! and decisions durably, grades every market the scanner ever saw against
//! the venue's closing line and settlement, and serves point-in-time frames
//! to the `backtest` binary. Optional: without it the scanner runs unchanged.

use std::{collections::HashMap, str::FromStr, time::Duration};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::{
    Row,
    postgres::{PgConnectOptions, PgPool, PgPoolOptions},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    Error, Result,
    config::Settings,
    domain::{
        BookLevel, MarketBook, NewsEvidence, Opportunity, OutcomeSide, PaperPortfolio,
        RecommendationClass, ScanCapture, SourceQuote, Sport, UsMoneylineMarket,
    },
    polymarket::{ClosingPrice, PolymarketUsClient},
    replay::{Frame, MarketOutcome},
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Markets past start are graded until this long after start; a venue that
/// has not settled a game by then is not going to.
const GRADING_WINDOW_DAYS: i64 = 14;
/// Markets graded per pass; each costs up to two paced gateway requests.
pub const GRADING_BATCH: i64 = 60;

#[derive(Clone)]
pub struct History {
    pool: PgPool,
}

/// Where a stored scan came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Recorded by the scanner as it ran; carries books.
    Live,
    /// Loaded from files written before the archive existed; no books.
    Import,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Origin::Live => "live",
            Origin::Import => "import",
        }
    }
}

/// A market whose start has passed and whose settlement is not yet recorded.
#[derive(Debug, Clone)]
pub struct PendingOutcome {
    pub market_id: String,
    pub market_slug: String,
    pub start_time: DateTime<Utc>,
    pub has_closing: bool,
    pub attempts: i32,
}

/// What one grading pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct GradingReport {
    pub attempted: usize,
    pub closing_lines_recorded: usize,
    pub settled: usize,
    pub errors: usize,
}

/// Row counts for `history status`.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryStatus {
    pub scans: i64,
    pub live_scans: i64,
    pub markets: i64,
    pub books: i64,
    pub quotes: i64,
    pub opportunities: i64,
    pub outcomes_settled: i64,
    pub outcomes_pending: i64,
    pub first_scan_at: Option<DateTime<Utc>>,
    pub last_scan_at: Option<DateTime<Utc>>,
}

impl History {
    /// Connect and apply pending migrations. Fails when the database is
    /// unreachable: an operator who configured the archive wants to know.
    pub async fn connect(url: &str) -> Result<Self> {
        let options = PgConnectOptions::from_str(url)
            .map_err(|error| Error::Config(format!("DATABASE_URL: {error}")))?
            .application_name("polybot");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(options)
            .await
            .map_err(storage_error)?;
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|error| Error::Storage(format!("migrate: {error}")))?;
        info!("history archive connected");
        Ok(Self { pool })
    }

    /// `Some` when `DATABASE_URL` is configured.
    pub async fn from_settings(settings: &Settings) -> Result<Option<Self>> {
        match &settings.database_url {
            Some(url) => Self::connect(url).await.map(Some),
            None => Ok(None),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Record a scan with its inputs. Idempotent on `scan_id`: a scan already
    /// stored is left untouched, so imports can be rerun.
    pub async fn record_scan(
        &self,
        capture: &ScanCapture,
        settings: &serde_json::Value,
        origin: Origin,
    ) -> Result<bool> {
        let snapshot = &capture.snapshot;
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let inserted = sqlx::query(
            "INSERT INTO scans (scan_id, started_at, evaluated_at, completed_at, market_count, quote_count, settings, portfolio, source_health, origin)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (scan_id) DO NOTHING",
        )
        .bind(snapshot.scan_id)
        .bind(snapshot.started_at)
        .bind(snapshot.evaluated_at_or_completed())
        .bind(snapshot.completed_at)
        .bind(count(snapshot.market_count)?)
        .bind(count(snapshot.quote_count)?)
        .bind(settings)
        .bind(serde_json::to_value(&capture.portfolio)?)
        .bind(serde_json::to_value(&snapshot.source_health)?)
        .bind(origin.as_str())
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?
        .rows_affected();
        if inserted == 0 {
            tx.rollback().await.map_err(storage_error)?;
            return Ok(false);
        }

        let seen_at = snapshot.evaluated_at_or_completed();
        let markets = market_rows(capture)?;
        upsert_markets(&mut tx, &markets, seen_at).await?;
        insert_books(&mut tx, capture).await?;
        insert_quotes(&mut tx, snapshot.scan_id, &capture.quotes).await?;
        insert_opportunities(&mut tx, snapshot.scan_id, &snapshot.opportunities).await?;
        tx.commit().await.map_err(storage_error)?;
        debug!(scan = %snapshot.scan_id, "scan archived");
        Ok(true)
    }

    pub async fn record_news(&self, evidence: &NewsEvidence) -> Result<()> {
        sqlx::query(
            "INSERT INTO news_evidence (opportunity_id, generated_at, confidence_effect, manual_review, summary, citations)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (opportunity_id, generated_at) DO NOTHING",
        )
        .bind(evidence.opportunity_id)
        .bind(evidence.generated_at)
        .bind(&evidence.confidence_effect)
        .bind(evidence.manual_review)
        .bind(&evidence.summary)
        .bind(serde_json::to_value(&evidence.citations)?)
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Markets that have started, are inside the grading window, have no
    /// settlement, and whose backoff has elapsed; earliest start first.
    pub async fn pending_outcomes(
        &self,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<PendingOutcome>> {
        let rows = sqlx::query(
            "SELECT m.market_id, m.market_slug, m.start_time,
                    o.closing_long IS NOT NULL AS has_closing,
                    COALESCE(o.attempts, 0) AS attempts
             FROM markets m
             LEFT JOIN market_outcomes o USING (market_id)
             WHERE m.start_time <= $1
               AND m.start_time > $1 - make_interval(days => $3::int)
               AND o.settlement IS NULL
               AND (o.next_attempt_at IS NULL OR o.next_attempt_at <= $1)
             ORDER BY m.start_time
             LIMIT $2",
        )
        .bind(now)
        .bind(limit)
        .bind(GRADING_WINDOW_DAYS as i32)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;
        rows.iter()
            .map(|row| {
                Ok(PendingOutcome {
                    market_id: row.try_get("market_id").map_err(storage_error)?,
                    market_slug: row.try_get("market_slug").map_err(storage_error)?,
                    start_time: row.try_get("start_time").map_err(storage_error)?,
                    has_closing: row.try_get("has_closing").map_err(storage_error)?,
                    attempts: row.try_get("attempts").map_err(storage_error)?,
                })
            })
            .collect()
    }

    /// Store what one grading attempt learned. Existing closing values are
    /// kept (`COALESCE`), settlement is written once, and the next attempt is
    /// scheduled with a backoff that grows with the attempt count.
    pub async fn record_outcome(
        &self,
        market_id: &str,
        closing: Option<&ClosingPrice>,
        settlement: Option<Decimal>,
        error: Option<&str>,
        attempts: i32,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let next_attempt_at = now + grading_backoff(attempts);
        sqlx::query(
            "INSERT INTO market_outcomes
                 (market_id, closing_long, closing_short, closing_observed_at, closing_recorded_at,
                  settlement, settled_recorded_at, attempts, next_attempt_at, last_error)
             VALUES ($1, $2, $3, $4, CASE WHEN $2 IS NULL THEN NULL ELSE $5 END,
                     $6, CASE WHEN $6 IS NULL THEN NULL ELSE $5 END, 1, $7, $8)
             ON CONFLICT (market_id) DO UPDATE SET
                 closing_long = COALESCE(market_outcomes.closing_long, EXCLUDED.closing_long),
                 closing_short = COALESCE(market_outcomes.closing_short, EXCLUDED.closing_short),
                 closing_observed_at = COALESCE(market_outcomes.closing_observed_at, EXCLUDED.closing_observed_at),
                 closing_recorded_at = COALESCE(market_outcomes.closing_recorded_at, EXCLUDED.closing_recorded_at),
                 settlement = COALESCE(market_outcomes.settlement, EXCLUDED.settlement),
                 settled_recorded_at = COALESCE(market_outcomes.settled_recorded_at, EXCLUDED.settled_recorded_at),
                 attempts = market_outcomes.attempts + 1,
                 next_attempt_at = EXCLUDED.next_attempt_at,
                 last_error = EXCLUDED.last_error",
        )
        .bind(market_id)
        .bind(closing.map(|price| price.long_price))
        .bind(closing.map(|price| price.short_price))
        .bind(closing.map(|price| price.observed_at))
        .bind(now)
        .bind(settlement)
        .bind(next_attempt_at)
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Grade pending markets against the venue: closing line once, then
    /// settlement until published. Gateway errors are recorded per market
    /// and never abort the pass.
    pub async fn grade_outcomes(
        &self,
        client: &PolymarketUsClient,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<GradingReport> {
        let pending = self.pending_outcomes(now, limit).await?;
        let mut report = GradingReport::default();
        for market in pending {
            report.attempted += 1;
            let mut error = None;
            let closing = if market.has_closing {
                None
            } else {
                match client
                    .fetch_closing_price(&market.market_slug, market.start_time)
                    .await
                {
                    Ok(Some(closing)) => {
                        report.closing_lines_recorded += 1;
                        Some(closing)
                    }
                    Ok(None) => None,
                    Err(fetch_error) => {
                        warn!(market = %market.market_slug, error = %fetch_error, "closing line fetch failed");
                        error = Some(fetch_error.to_string());
                        None
                    }
                }
            };
            let settlement = match client.fetch_settlement(&market.market_slug).await {
                Ok(settlement) => {
                    if settlement.is_some() {
                        report.settled += 1;
                    }
                    settlement
                }
                Err(fetch_error) => {
                    warn!(market = %market.market_slug, error = %fetch_error, "settlement fetch failed");
                    error = Some(fetch_error.to_string());
                    None
                }
            };
            if error.is_some() {
                report.errors += 1;
            }
            self.record_outcome(
                &market.market_id,
                closing.as_ref(),
                settlement,
                error.as_deref(),
                market.attempts,
                now,
            )
            .await?;
        }
        Ok(report)
    }

    /// A read-only `REPEATABLE READ` view for a replay: every frame and the
    /// outcome table come from one snapshot, so a scan or grading pass that
    /// lands mid-replay cannot make the window inconsistent.
    pub async fn reader(&self) -> Result<Reader> {
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        Ok(Reader { tx })
    }
}

pub struct Reader {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl Reader {
    /// Every recorded outcome, keyed by market id.
    pub async fn outcomes(&mut self) -> Result<HashMap<String, MarketOutcome>> {
        let rows = sqlx::query(
            "SELECT o.market_id, m.start_time, o.closing_long, o.closing_short, o.closing_observed_at,
                    o.settlement, o.settled_recorded_at
             FROM market_outcomes o JOIN markets m USING (market_id)",
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(storage_error)?;
        rows.iter()
            .map(|row| {
                let outcome = MarketOutcome {
                    market_id: row.try_get("market_id").map_err(storage_error)?,
                    start_time: row.try_get("start_time").map_err(storage_error)?,
                    closing_long: row.try_get("closing_long").map_err(storage_error)?,
                    closing_short: row.try_get("closing_short").map_err(storage_error)?,
                    closing_observed_at: row
                        .try_get("closing_observed_at")
                        .map_err(storage_error)?,
                    settlement: row.try_get("settlement").map_err(storage_error)?,
                    settled_recorded_at: row
                        .try_get("settled_recorded_at")
                        .map_err(storage_error)?,
                };
                Ok((outcome.market_id.clone(), outcome))
            })
            .collect()
    }

    /// Scan ids evaluated in `[from, through)`, oldest first.
    pub async fn scan_ids(
        &mut self,
        from: DateTime<Utc>,
        through: DateTime<Utc>,
    ) -> Result<Vec<Uuid>> {
        let rows = sqlx::query(
            "SELECT scan_id FROM scans WHERE evaluated_at >= $1 AND evaluated_at < $2 ORDER BY evaluated_at, scan_id",
        )
        .bind(from)
        .bind(through)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(storage_error)?;
        rows.iter()
            .map(|row| row.try_get("scan_id").map_err(storage_error))
            .collect()
    }

    /// Load one scan's frame. Books whose market definition is unknown
    /// (imported) are dropped with a warning rather than guessed.
    pub async fn frame(&mut self, scan_id: Uuid) -> Result<Frame> {
        let scan = sqlx::query(
            "SELECT evaluated_at, origin, settings, portfolio FROM scans WHERE scan_id = $1",
        )
        .bind(scan_id)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(storage_error)?;
        let evaluated_at = scan.try_get("evaluated_at").map_err(storage_error)?;
        let origin = scan.try_get("origin").map_err(storage_error)?;
        let settings = scan.try_get("settings").map_err(storage_error)?;
        let portfolio: PaperPortfolio =
            serde_json::from_value(scan.try_get("portfolio").map_err(storage_error)?)?;

        let book_rows = sqlx::query(
            "SELECT b.market_id, b.start_time, b.ep3_status, b.state, b.transact_time, b.fetched_at,
                    b.bids, b.offers, m.market
             FROM scan_books b JOIN markets m USING (market_id)
             WHERE b.scan_id = $1 ORDER BY b.market_id",
        )
        .bind(scan_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(storage_error)?;
        let mut markets = Vec::with_capacity(book_rows.len());
        let mut books = Vec::with_capacity(book_rows.len());
        for row in &book_rows {
            let market_id: String = row.try_get("market_id").map_err(storage_error)?;
            let Some(definition) = row
                .try_get::<Option<serde_json::Value>, _>("market")
                .map_err(storage_error)?
            else {
                warn!(%scan_id, market_id, "book without a market definition; skipped");
                continue;
            };
            let mut market: UsMoneylineMarket = serde_json::from_value(definition)?;
            market.start_time = row.try_get("start_time").map_err(storage_error)?;
            market.ep3_status = row.try_get("ep3_status").map_err(storage_error)?;
            let bids: Vec<BookLevel> =
                serde_json::from_value(row.try_get("bids").map_err(storage_error)?)?;
            let offers: Vec<BookLevel> =
                serde_json::from_value(row.try_get("offers").map_err(storage_error)?)?;
            books.push(MarketBook {
                market_slug: market.market_slug.clone(),
                bids,
                offers,
                state: row.try_get("state").map_err(storage_error)?,
                transact_time: row.try_get("transact_time").map_err(storage_error)?,
                fetched_at: row.try_get("fetched_at").map_err(storage_error)?,
            });
            markets.push(market);
        }

        let quote_rows =
            sqlx::query("SELECT quote FROM scan_quotes WHERE scan_id = $1 ORDER BY id")
                .bind(scan_id)
                .fetch_all(&mut *self.tx)
                .await
                .map_err(storage_error)?;
        let quotes = quote_rows
            .iter()
            .map(|row| {
                let quote: serde_json::Value = row.try_get("quote").map_err(storage_error)?;
                serde_json::from_value::<SourceQuote>(quote).map_err(Error::from)
            })
            .collect::<Result<Vec<_>>>()?;

        let opportunity_rows = sqlx::query(
            "SELECT o.*, m.market_slug FROM opportunities o JOIN markets m USING (market_id)
             WHERE o.scan_id = $1 ORDER BY o.net_edge DESC, o.opportunity_id",
        )
        .bind(scan_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(storage_error)?;
        let opportunities = opportunity_rows
            .iter()
            .map(opportunity_from_row)
            .collect::<Result<Vec<_>>>()?;

        Ok(Frame {
            scan_id,
            evaluated_at,
            origin,
            settings,
            portfolio,
            markets,
            books,
            quotes,
            opportunities,
        })
    }

    /// Release the snapshot.
    pub async fn finish(self) -> Result<()> {
        self.tx.rollback().await.map_err(storage_error)
    }
}

impl History {
    pub async fn record_backtest(
        &self,
        run_id: Uuid,
        from: DateTime<Utc>,
        through: DateTime<Utc>,
        settings: &serde_json::Value,
        report: &serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO backtest_runs (run_id, from_at, through_at, settings, report) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(run_id)
        .bind(from)
        .bind(through)
        .bind(settings)
        .bind(report)
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn status(&self, now: DateTime<Utc>) -> Result<HistoryStatus> {
        let row = sqlx::query(
            "SELECT
                (SELECT count(*) FROM scans) AS scans,
                (SELECT count(*) FROM scans WHERE origin = 'live') AS live_scans,
                (SELECT count(*) FROM markets) AS markets,
                (SELECT count(*) FROM scan_books) AS books,
                (SELECT count(*) FROM scan_quotes) AS quotes,
                (SELECT count(*) FROM opportunities) AS opportunities,
                (SELECT count(*) FROM market_outcomes WHERE settlement IS NOT NULL) AS outcomes_settled,
                (SELECT count(*) FROM markets m LEFT JOIN market_outcomes o USING (market_id)
                  WHERE m.start_time <= $1 AND m.start_time > $1 - make_interval(days => $2::int)
                    AND o.settlement IS NULL) AS outcomes_pending,
                (SELECT min(evaluated_at) FROM scans) AS first_scan_at,
                (SELECT max(evaluated_at) FROM scans) AS last_scan_at",
        )
        .bind(now)
        .bind(GRADING_WINDOW_DAYS as i32)
        .fetch_one(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(HistoryStatus {
            scans: row.try_get("scans").map_err(storage_error)?,
            live_scans: row.try_get("live_scans").map_err(storage_error)?,
            markets: row.try_get("markets").map_err(storage_error)?,
            books: row.try_get("books").map_err(storage_error)?,
            quotes: row.try_get("quotes").map_err(storage_error)?,
            opportunities: row.try_get("opportunities").map_err(storage_error)?,
            outcomes_settled: row.try_get("outcomes_settled").map_err(storage_error)?,
            outcomes_pending: row.try_get("outcomes_pending").map_err(storage_error)?,
            first_scan_at: row.try_get("first_scan_at").map_err(storage_error)?,
            last_scan_at: row.try_get("last_scan_at").map_err(storage_error)?,
        })
    }
}

/// Ten minutes for the first attempts (a settlement usually lands within an
/// hour of the final whistle), then hourly, capped at six hours.
fn grading_backoff(attempts: i32) -> chrono::Duration {
    match attempts {
        0..=2 => chrono::Duration::minutes(10),
        3..=8 => chrono::Duration::hours(1),
        _ => chrono::Duration::hours(6),
    }
}

struct MarketRow {
    market_id: String,
    market_slug: String,
    event_id: String,
    sport: String,
    start_time: DateTime<Utc>,
    long_participant: String,
    short_participant: String,
    definition: Option<serde_json::Value>,
}

/// Full definitions when the capture has them; otherwise the identifying
/// fields every opportunity row carries (imports), with the participant on
/// the side the row backed.
fn market_rows(capture: &ScanCapture) -> Result<Vec<MarketRow>> {
    if !capture.markets.is_empty() {
        return capture
            .markets
            .iter()
            .map(|market| {
                Ok(MarketRow {
                    market_id: market.market_id.clone(),
                    market_slug: market.market_slug.clone(),
                    event_id: market.event_id.clone(),
                    sport: market.sport.to_string(),
                    start_time: market.start_time,
                    long_participant: market.long_participant.name.clone(),
                    short_participant: market.short_participant.name.clone(),
                    definition: Some(serde_json::to_value(market)?),
                })
            })
            .collect();
    }
    let mut by_market: HashMap<&str, MarketRow> = HashMap::new();
    for opportunity in &capture.snapshot.opportunities {
        let row = by_market
            .entry(opportunity.market_id.as_str())
            .or_insert_with(|| MarketRow {
                market_id: opportunity.market_id.clone(),
                market_slug: opportunity.market_slug.clone(),
                event_id: opportunity.event_id.clone(),
                sport: opportunity.sport.to_string(),
                start_time: opportunity.start_time,
                long_participant: String::new(),
                short_participant: String::new(),
                definition: None,
            });
        match opportunity.side {
            OutcomeSide::Long => row.long_participant = opportunity.participant.clone(),
            OutcomeSide::Short => row.short_participant = opportunity.participant.clone(),
        }
    }
    Ok(by_market.into_values().collect())
}

async fn upsert_markets(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    markets: &[MarketRow],
    seen_at: DateTime<Utc>,
) -> Result<()> {
    if markets.is_empty() {
        return Ok(());
    }
    let ids = markets
        .iter()
        .map(|m| m.market_id.clone())
        .collect::<Vec<_>>();
    let slugs = markets
        .iter()
        .map(|m| m.market_slug.clone())
        .collect::<Vec<_>>();
    let events = markets
        .iter()
        .map(|m| m.event_id.clone())
        .collect::<Vec<_>>();
    let sports = markets.iter().map(|m| m.sport.clone()).collect::<Vec<_>>();
    let starts = markets.iter().map(|m| m.start_time).collect::<Vec<_>>();
    let longs = markets
        .iter()
        .map(|m| m.long_participant.clone())
        .collect::<Vec<_>>();
    let shorts = markets
        .iter()
        .map(|m| m.short_participant.clone())
        .collect::<Vec<_>>();
    let definitions = markets
        .iter()
        .map(|m| m.definition.clone())
        .collect::<Vec<_>>();
    // A newer scan wins for everything except the full definition, which an
    // import (NULL) must never erase, and the participant names, which an
    // import knows only for the side it backed.
    sqlx::query(
        "INSERT INTO markets (market_id, market_slug, event_id, sport, start_time, long_participant, short_participant, market, first_seen_at, last_seen_at)
         SELECT u.*, $9::timestamptz, $9::timestamptz
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::timestamptz[], $6::text[], $7::text[], $8::jsonb[]) AS u
         ON CONFLICT (market_id) DO UPDATE SET
             market_slug = EXCLUDED.market_slug,
             event_id = EXCLUDED.event_id,
             sport = EXCLUDED.sport,
             start_time = CASE WHEN EXCLUDED.last_seen_at >= markets.last_seen_at THEN EXCLUDED.start_time ELSE markets.start_time END,
             long_participant = CASE WHEN EXCLUDED.long_participant = '' THEN markets.long_participant ELSE EXCLUDED.long_participant END,
             short_participant = CASE WHEN EXCLUDED.short_participant = '' THEN markets.short_participant ELSE EXCLUDED.short_participant END,
             market = CASE WHEN EXCLUDED.market IS NULL THEN markets.market
                           WHEN EXCLUDED.last_seen_at >= markets.last_seen_at OR markets.market IS NULL THEN EXCLUDED.market
                           ELSE markets.market END,
             first_seen_at = LEAST(markets.first_seen_at, EXCLUDED.first_seen_at),
             last_seen_at = GREATEST(markets.last_seen_at, EXCLUDED.last_seen_at)",
    )
    .bind(&ids)
    .bind(&slugs)
    .bind(&events)
    .bind(&sports)
    .bind(&starts)
    .bind(&longs)
    .bind(&shorts)
    .bind(&definitions)
    .bind(seen_at)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

async fn insert_books(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    capture: &ScanCapture,
) -> Result<()> {
    if capture.books.is_empty() {
        return Ok(());
    }
    let by_slug = capture
        .markets
        .iter()
        .map(|market| (market.market_slug.as_str(), market))
        .collect::<HashMap<_, _>>();
    let mut market_ids = Vec::with_capacity(capture.books.len());
    let mut starts = Vec::with_capacity(capture.books.len());
    let mut statuses = Vec::with_capacity(capture.books.len());
    let mut states = Vec::with_capacity(capture.books.len());
    let mut transacts = Vec::with_capacity(capture.books.len());
    let mut fetched = Vec::with_capacity(capture.books.len());
    let mut bids = Vec::with_capacity(capture.books.len());
    let mut offers = Vec::with_capacity(capture.books.len());
    for book in &capture.books {
        let Some(market) = by_slug.get(book.market_slug.as_str()) else {
            warn!(market = %book.market_slug, "book without a market in the capture; skipped");
            continue;
        };
        market_ids.push(market.market_id.clone());
        starts.push(market.start_time);
        statuses.push(market.ep3_status.clone());
        states.push(book.state.clone());
        transacts.push(book.transact_time);
        fetched.push(book.fetched_at);
        bids.push(serde_json::to_value(&book.bids)?);
        offers.push(serde_json::to_value(&book.offers)?);
    }
    sqlx::query(
        "INSERT INTO scan_books (scan_id, market_id, start_time, ep3_status, state, transact_time, fetched_at, bids, offers)
         SELECT $1, * FROM UNNEST($2::text[], $3::timestamptz[], $4::text[], $5::text[], $6::timestamptz[], $7::timestamptz[], $8::jsonb[], $9::jsonb[])
         ON CONFLICT (scan_id, market_id) DO NOTHING",
    )
    .bind(capture.snapshot.scan_id)
    .bind(&market_ids)
    .bind(&starts)
    .bind(&statuses)
    .bind(&states)
    .bind(&transacts)
    .bind(&fetched)
    .bind(&bids)
    .bind(&offers)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

async fn insert_quotes(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scan_id: Uuid,
    quotes: &[SourceQuote],
) -> Result<()> {
    if quotes.is_empty() {
        return Ok(());
    }
    let sources = quotes
        .iter()
        .map(|q| q.source_id.clone())
        .collect::<Vec<_>>();
    let sports = quotes
        .iter()
        .map(|q| q.sport.to_string())
        .collect::<Vec<_>>();
    let events = quotes
        .iter()
        .map(|q| q.event_id.clone())
        .collect::<Vec<_>>();
    let starts = quotes.iter().map(|q| q.start_time).collect::<Vec<_>>();
    let source_timestamps = quotes
        .iter()
        .map(|q| q.source_timestamp)
        .collect::<Vec<_>>();
    let fetched = quotes.iter().map(|q| q.fetched_at).collect::<Vec<_>>();
    let payloads = quotes
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    sqlx::query(
        "INSERT INTO scan_quotes (scan_id, source_id, sport, event_id, start_time, source_timestamp, fetched_at, quote)
         SELECT $1, * FROM UNNEST($2::text[], $3::text[], $4::text[], $5::timestamptz[], $6::timestamptz[], $7::timestamptz[], $8::jsonb[])",
    )
    .bind(scan_id)
    .bind(&sources)
    .bind(&sports)
    .bind(&events)
    .bind(&starts)
    .bind(&source_timestamps)
    .bind(&fetched)
    .bind(&payloads)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

async fn insert_opportunities(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scan_id: Uuid,
    opportunities: &[Opportunity],
) -> Result<()> {
    if opportunities.is_empty() {
        return Ok(());
    }
    let n = opportunities.len();
    let mut ids = Vec::with_capacity(n);
    let mut market_ids = Vec::with_capacity(n);
    let mut event_ids = Vec::with_capacity(n);
    let mut sides = Vec::with_capacity(n);
    let mut classes = Vec::with_capacity(n);
    let mut sports = Vec::with_capacity(n);
    let mut participants = Vec::with_capacity(n);
    let mut fair = Vec::with_capacity(n);
    let mut conservative = Vec::with_capacity(n);
    let mut executable = Vec::with_capacity(n);
    let mut maker_price = Vec::with_capacity(n);
    let mut maker_net_edge = Vec::with_capacity(n);
    let mut raw_edge = Vec::with_capacity(n);
    let mut net_edge = Vec::with_capacity(n);
    let mut quantity = Vec::with_capacity(n);
    let mut maximum_loss = Vec::with_capacity(n);
    let mut estimated_fee = Vec::with_capacity(n);
    let mut family_count = Vec::with_capacity(n);
    let mut source_ids = Vec::with_capacity(n);
    let mut reasons = Vec::with_capacity(n);
    let mut start_time = Vec::with_capacity(n);
    let mut book_time = Vec::with_capacity(n);
    let mut generated_at = Vec::with_capacity(n);
    for item in opportunities {
        ids.push(item.id);
        market_ids.push(item.market_id.clone());
        event_ids.push(item.event_id.clone());
        sides.push(enum_text(&item.side)?);
        classes.push(enum_text(&item.class)?);
        sports.push(item.sport.to_string());
        participants.push(item.participant.clone());
        fair.push(item.fair_probability);
        conservative.push(item.conservative_probability);
        executable.push(item.executable_price);
        maker_price.push(item.maker_price);
        maker_net_edge.push(item.maker_net_edge);
        raw_edge.push(item.raw_edge);
        net_edge.push(item.net_edge);
        quantity.push(item.quantity);
        maximum_loss.push(item.maximum_loss);
        estimated_fee.push(item.estimated_fee);
        family_count.push(count(item.family_count)?);
        source_ids.push(serde_json::to_value(&item.source_ids)?);
        reasons.push(serde_json::to_value(&item.reasons)?);
        start_time.push(item.start_time);
        book_time.push(item.book_time);
        generated_at.push(item.generated_at);
    }
    sqlx::query(
        "INSERT INTO opportunities
             (scan_id, opportunity_id, market_id, event_id, side, class, sport, participant,
              fair_probability, conservative_probability, executable_price, maker_price, maker_net_edge,
              raw_edge, net_edge, quantity, maximum_loss, estimated_fee, family_count,
              source_ids, reasons, start_time, book_time, generated_at)
         SELECT $1, * FROM UNNEST(
             $2::uuid[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::text[],
             $9::numeric[], $10::numeric[], $11::numeric[], $12::numeric[], $13::numeric[],
             $14::numeric[], $15::numeric[], $16::numeric[], $17::numeric[], $18::numeric[], $19::int[],
             $20::jsonb[], $21::jsonb[], $22::timestamptz[], $23::timestamptz[], $24::timestamptz[])
         ON CONFLICT (scan_id, opportunity_id) DO NOTHING",
    )
    .bind(scan_id)
    .bind(&ids)
    .bind(&market_ids)
    .bind(&event_ids)
    .bind(&sides)
    .bind(&classes)
    .bind(&sports)
    .bind(&participants)
    .bind(&fair)
    .bind(&conservative)
    .bind(&executable)
    .bind(&maker_price)
    .bind(&maker_net_edge)
    .bind(&raw_edge)
    .bind(&net_edge)
    .bind(&quantity)
    .bind(&maximum_loss)
    .bind(&estimated_fee)
    .bind(&family_count)
    .bind(&source_ids)
    .bind(&reasons)
    .bind(&start_time)
    .bind(&book_time)
    .bind(&generated_at)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

fn opportunity_from_row(row: &sqlx::postgres::PgRow) -> Result<Opportunity> {
    fn get<'r, T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(
        row: &'r sqlx::postgres::PgRow,
        column: &str,
    ) -> Result<T> {
        row.try_get(column).map_err(storage_error)
    }
    let side: String = get(row, "side")?;
    let class: String = get(row, "class")?;
    let sport: String = get(row, "sport")?;
    Ok(Opportunity {
        id: get(row, "opportunity_id")?,
        generated_at: get(row, "generated_at")?,
        class: enum_from_text::<RecommendationClass>(&class)?,
        sport: enum_from_text::<Sport>(&sport)?,
        event_id: get(row, "event_id")?,
        market_id: get(row, "market_id")?,
        market_slug: get(row, "market_slug")?,
        participant: get(row, "participant")?,
        side: enum_from_text::<OutcomeSide>(&side)?,
        fair_probability: get(row, "fair_probability")?,
        conservative_probability: get(row, "conservative_probability")?,
        executable_price: get(row, "executable_price")?,
        maker_price: get(row, "maker_price")?,
        maker_net_edge: get(row, "maker_net_edge")?,
        raw_edge: get(row, "raw_edge")?,
        net_edge: get(row, "net_edge")?,
        quantity: get(row, "quantity")?,
        maximum_loss: get(row, "maximum_loss")?,
        estimated_fee: get(row, "estimated_fee")?,
        family_count: usize::try_from(get::<i32>(row, "family_count")?)
            .map_err(|error| Error::Storage(format!("family_count: {error}")))?,
        source_ids: serde_json::from_value(get(row, "source_ids")?)?,
        start_time: get(row, "start_time")?,
        book_time: get(row, "book_time")?,
        reasons: serde_json::from_value(get(row, "reasons")?)?,
    })
}

/// The serde string form of a unit enum (`snake_case`/`lowercase` renames
/// included), so the database column reads like the JSON records.
fn enum_text<T: Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(text) => Ok(text),
        other => Err(Error::Storage(format!("enum is not a string: {other}"))),
    }
}

fn enum_from_text<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(text.to_string())).map_err(Error::from)
}

fn count(value: usize) -> Result<i32> {
    i32::try_from(value).map_err(|error| Error::Storage(format!("count overflow: {error}")))
}

fn storage_error(error: sqlx::Error) -> Error {
    Error::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_text_round_trips_serde_renames() {
        assert_eq!(enum_text(&OutcomeSide::Long).unwrap(), "long");
        assert_eq!(
            enum_text(&RecommendationClass::Watchlist).unwrap(),
            "watchlist"
        );
        assert_eq!(enum_text(&Sport::Nfl).unwrap(), "nfl");
        assert_eq!(
            enum_from_text::<RecommendationClass>("actionable").unwrap(),
            RecommendationClass::Actionable
        );
    }

    #[test]
    fn grading_backoff_grows_with_attempts() {
        assert_eq!(grading_backoff(0), chrono::Duration::minutes(10));
        assert_eq!(grading_backoff(3), chrono::Duration::hours(1));
        assert_eq!(grading_backoff(20), chrono::Duration::hours(6));
    }
}
