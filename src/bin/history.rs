//! History archive operations (`postgres` feature): row counts, one-shot
//! outcome grading, and importing scan files written before the archive
//! existed. `local` performs archiving and grading on its own; this is the
//! manual and backfill surface.

use std::path::{Path, PathBuf};

use chrono::Utc;
use clap::{Parser, Subcommand};
use polybot::{
    Error, Result,
    config::Settings,
    domain::{
        DEFAULT_START_TIME_TOLERANCE_MINUTES, MarketBook, PaperPortfolio, ScanCapture,
        ScanSnapshot, SourceQuote, UsMoneylineMarket,
    },
    history::{GRADING_BATCH, History, Origin},
    polymarket::PolymarketUsClient,
};
use rust_decimal::Decimal;
use tracing::{info, warn};

#[derive(Debug, Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Row counts and the archived window.
    Status,
    /// Grade started markets against the venue's closing line and settlement.
    Grade {
        /// Stop after this many markets; the default is one engine pass.
        #[arg(long, default_value_t = GRADING_BATCH)]
        limit: i64,
    },
    /// Load every scan under `<data-dir>/scans` that is not yet archived.
    /// Files written before books were captured import without books.
    Import {
        #[arg(long, default_value = "data", env = "DATA_DIR")]
        data_dir: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let args = Args::parse();
    let settings = Settings::from_env()?;
    let history = History::from_settings(&settings)
        .await?
        .ok_or_else(|| Error::Config("DATABASE_URL is required".into()))?;
    match args.command {
        Command::Status => {
            let status = history.status(Utc::now()).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Command::Grade { limit } => {
            let client = PolymarketUsClient::with_concurrency(
                settings.polymarket_base_url.clone(),
                settings.request_timeout,
                settings.book_concurrency,
            )?
            .with_rate_limit(settings.polymarket_requests_per_second);
            let report = history.grade_outcomes(&client, Utc::now(), limit).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Import { data_dir } => {
            let report = import(&history, Path::new(&data_dir), &settings).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

#[derive(Debug, Default, serde::Serialize)]
struct ImportReport {
    files: usize,
    imported: usize,
    already_archived: usize,
    with_books: usize,
    failed: usize,
}

async fn import(history: &History, data_dir: &Path, settings: &Settings) -> Result<ImportReport> {
    let scans = data_dir.join("scans");
    let mut files = Vec::new();
    collect_snapshots(&scans, &mut files)?;
    files.sort();
    let mut report = ImportReport {
        files: files.len(),
        ..ImportReport::default()
    };
    // Gates for imported rows are unknown unless the current environment is
    // the one that wrote them; record the current settings and say so.
    let settings_json = serde_json::json!({
        "imported_from": data_dir.display().to_string(),
        "current_settings": serde_json::to_value(settings)?,
    });
    for path in files {
        match load_capture(&path, settings.bankroll) {
            Ok(capture) => {
                let origin = if capture.books.is_empty() {
                    Origin::Import
                } else {
                    Origin::Live
                };
                if origin == Origin::Live {
                    report.with_books += 1;
                }
                match history.record_scan(&capture, &settings_json, origin).await {
                    Ok(true) => report.imported += 1,
                    Ok(false) => report.already_archived += 1,
                    Err(error) => {
                        warn!(path = %path.display(), %error, "archive write failed");
                        report.failed += 1;
                    }
                }
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "scan file skipped");
                report.failed += 1;
            }
        }
    }
    info!(?report, "import complete");
    Ok(report)
}

/// Every `<uuid>.json` under `root`; the sibling `-quotes`, `-markets` and
/// `-books` files are read alongside.
fn collect_snapshots(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)
        .map_err(|error| Error::Storage(format!("read {}: {error}", root.display())))?
    {
        let path = entry
            .map_err(|error| Error::Storage(error.to_string()))?
            .path();
        if path.is_dir() {
            collect_snapshots(&path, out)?;
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().is_some_and(|ext| ext == "json") && uuid::Uuid::parse_str(stem).is_ok()
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The paper portfolio at scan time was not written to files; imported
/// captures carry an empty one, which the archive records as such.
fn load_capture(path: &Path, bankroll: Decimal) -> Result<ScanCapture> {
    let stem = path.with_extension("");
    let stem = stem.to_string_lossy();
    let quotes: Vec<SourceQuote> =
        read_json_or_default(&PathBuf::from(format!("{stem}-quotes.json")))?;
    let markets: Vec<UsMoneylineMarket> =
        read_json_or_default(&PathBuf::from(format!("{stem}-markets.json")))?;
    let books: Vec<MarketBook> =
        read_json_or_default(&PathBuf::from(format!("{stem}-books.json")))?;
    let mut raw: serde_json::Value = read_json(path)?;
    repair_start_times(&mut raw, &quotes, path);
    let snapshot: ScanSnapshot = serde_json::from_value(raw)
        .map_err(|error| Error::Storage(format!("decode {}: {error}", path.display())))?;
    Ok(ScanCapture {
        snapshot,
        markets,
        books,
        quotes,
        portfolio: PaperPortfolio::new(bankroll),
    })
}

/// Snapshots written before `Opportunity::start_time` existed carry no game
/// start. The scan's own quotes do: a source that publishes exact start
/// times (default tolerance) naming the row's participant in the same sport
/// supplies it. Rows with no such quote are dropped rather than given a
/// guessed time, since grading and lead checks depend on it.
fn repair_start_times(snapshot: &mut serde_json::Value, quotes: &[SourceQuote], path: &Path) {
    let Some(rows) = snapshot
        .get_mut("opportunities")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let exact = quotes
        .iter()
        .filter(|quote| quote.start_time_tolerance_minutes <= DEFAULT_START_TIME_TOLERANCE_MINUTES)
        .collect::<Vec<_>>();
    let before = rows.len();
    rows.retain_mut(|row| {
        if row.get("start_time").is_some() {
            return true;
        }
        let (Some(participant), Some(sport)) = (
            row.get("participant").and_then(serde_json::Value::as_str),
            row.get("sport").and_then(serde_json::Value::as_str),
        ) else {
            return false;
        };
        let start = exact.iter().find_map(|quote| {
            (quote.sport.to_string() == sport
                && (quote.participant_a.eq_ignore_ascii_case(participant)
                    || quote.participant_b.eq_ignore_ascii_case(participant)))
            .then_some(quote.start_time)
        });
        match start {
            Some(start) => {
                row["start_time"] = serde_json::to_value(start).expect("timestamp");
                true
            }
            None => false,
        }
    });
    if rows.len() < before {
        warn!(
            path = %path.display(),
            dropped = before - rows.len(),
            kept = rows.len(),
            "legacy rows without a recoverable start time dropped"
        );
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let content = std::fs::read(path)
        .map_err(|error| Error::Storage(format!("read {}: {error}", path.display())))?;
    serde_json::from_slice(&content)
        .map_err(|error| Error::Storage(format!("decode {}: {error}", path.display())))
}

fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if path.exists() {
        read_json(path)
    } else {
        Ok(T::default())
    }
}
