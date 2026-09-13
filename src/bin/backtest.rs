//! Replay archived scans through the classifier as of each scan's own clock,
//! simulate the paper portfolio, and grade every observed market side against
//! the venue's settlement (`postgres` feature). Gates come from the
//! environment exactly as for the scanner, so a policy change is a `.env`
//! change. `--audit` instead recomputes each live frame under the gates it
//! was archived with and diffs against the stored rows: proof that replay and
//! live have not diverged.

use std::{collections::HashMap, path::PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use clap::{Parser, ValueEnum};
use polybot::{
    Error, Result,
    config::Settings,
    domain::{Opportunity, RecommendationClass},
    history::History,
    replay::{FillModel, Frame, Replay, ReplayOptions},
    scanner::{consensus_markets, evaluate_books},
};
use rust_decimal::Decimal;
use serde::Serialize;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Debug, Parser)]
struct Args {
    /// Inclusive start (RFC 3339 or YYYY-MM-DD). Default: 30 days before `through`.
    #[arg(long)]
    from: Option<String>,
    /// Exclusive end (RFC 3339 or YYYY-MM-DD). Default and maximum: now.
    #[arg(long)]
    through: Option<String>,
    /// Least class that opens a paper position.
    #[arg(long, value_enum, default_value_t = OpenClass::Actionable)]
    open_class: OpenClass,
    #[arg(long, value_enum, default_value_t = Fill::SameBook)]
    fill: Fill,
    /// Hours after scheduled start at which a settled market releases its
    /// simulated exposure.
    #[arg(long, default_value_t = 4)]
    settlement_lag_hours: i64,
    /// Write the JSON report here; refuses to overwrite.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Parity audit: recompute every live frame under its archived gates and
    /// diff against the stored rows instead of simulating.
    #[arg(long)]
    audit: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OpenClass {
    Watchlist,
    Actionable,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Fill {
    SameBook,
    NextBook,
}

#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let args = Args::parse();
    let settings = Settings::from_env()?;
    let history = History::from_settings(&settings)
        .await?
        .ok_or_else(|| Error::Config("DATABASE_URL is required".into()))?;

    let now = Utc::now();
    let through = match &args.through {
        Some(text) => parse_time(text)?,
        None => now,
    };
    if through > now {
        return Err(Error::Config(format!(
            "--through {through} is in the future; a replay cannot see past now"
        )));
    }
    let from = match &args.from {
        Some(text) => parse_time(text)?,
        None => through - chrono::Duration::days(30),
    };
    if from >= through {
        return Err(Error::Config(format!(
            "--from {from} must precede --through {through}"
        )));
    }
    if let Some(path) = &args.report
        && path.exists()
    {
        return Err(Error::Config(format!(
            "{} exists; reports are never overwritten, use a fresh path",
            path.display()
        )));
    }

    let mut reader = history.reader().await?;
    let scan_ids = reader.scan_ids(from, through).await?;
    if scan_ids.is_empty() {
        return Err(Error::InvalidData(format!(
            "no archived scans between {from} and {through}"
        )));
    }
    info!(frames = scan_ids.len(), %from, %through, "replay window");

    let report = if args.audit {
        let mut audit = Audit::default();
        for scan_id in scan_ids {
            let frame = reader.frame(scan_id).await?;
            audit.frame(&frame);
        }
        serde_json::to_value(&audit)?
    } else {
        let outcomes = reader.outcomes().await?;
        let options = ReplayOptions {
            settings: settings.clone(),
            open_class: match args.open_class {
                OpenClass::Watchlist => RecommendationClass::Watchlist,
                OpenClass::Actionable => RecommendationClass::Actionable,
            },
            fill: match args.fill {
                Fill::SameBook => FillModel::SameBook,
                Fill::NextBook => FillModel::NextBook,
            },
            settlement_lag_hours: args.settlement_lag_hours,
        };
        let mut replay = Replay::new(options);
        for scan_id in scan_ids {
            let frame = reader.frame(scan_id).await?;
            replay.step(&frame, &outcomes);
        }
        serde_json::to_value(replay.finish(through, &outcomes))?
    };
    reader.finish().await?;

    let run_id = Uuid::new_v4();
    let mut envelope = serde_json::json!({
        "run_id": run_id,
        "from": from,
        "through": through,
        "mode": if args.audit { "audit" } else { "replay" },
    });
    envelope
        .as_object_mut()
        .expect("object")
        .extend(report.as_object().cloned().unwrap_or_default());
    history
        .record_backtest(
            run_id,
            from,
            through,
            &serde_json::to_value(&settings)?,
            &envelope,
        )
        .await?;
    let text = serde_json::to_string_pretty(&envelope)?;
    if let Some(path) = &args.report {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Error::Storage(format!("create {}: {error}", parent.display())))?;
        }
        std::fs::write(path, &text)
            .map_err(|error| Error::Storage(format!("write {}: {error}", path.display())))?;
        info!(path = %path.display(), %run_id, "report written");
    }
    println!("{text}");
    Ok(())
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    if let Ok(value) = DateTime::parse_from_rfc3339(text) {
        return Ok(value.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map(|date| date.and_hms_opt(0, 0, 0).expect("midnight").and_utc())
        .map_err(|error| Error::Config(format!("time {text:?}: {error}")))
}

/// Recompute each live frame under the gates and portfolio it was archived
/// with and compare against the stored rows. Any mismatch means the archive,
/// the replay, or the classifier drifted; the count is the finding.
#[derive(Debug, Default, Serialize)]
struct Audit {
    frames: usize,
    frames_audited: usize,
    /// Imported frames (no books) or frames whose archived settings do not
    /// parse; both are excluded, not assumed.
    frames_skipped: usize,
    rows_stored: usize,
    rows_recomputed: usize,
    rows_matched: usize,
    rows_only_stored: usize,
    rows_only_recomputed: usize,
    mismatches: usize,
    /// First twenty mismatches, for diagnosis.
    samples: Vec<Mismatch>,
}

#[derive(Debug, Serialize)]
struct Mismatch {
    scan_id: Uuid,
    opportunity_id: Uuid,
    market_slug: String,
    field: &'static str,
    stored: String,
    recomputed: String,
}

impl Audit {
    fn frame(&mut self, frame: &Frame) {
        self.frames += 1;
        if frame.origin != "live" || !frame.has_books() {
            self.frames_skipped += 1;
            return;
        }
        let settings: Settings = match serde_json::from_value(frame.settings.clone()) {
            Ok(settings) => settings,
            Err(error) => {
                warn!(scan = %frame.scan_id, %error, "archived settings unreadable; frame skipped");
                self.frames_skipped += 1;
                return;
            }
        };
        self.frames_audited += 1;
        let quotes = frame.quotes.iter().collect::<Vec<_>>();
        let evaluable = consensus_markets(&frame.markets, &quotes, &settings, frame.evaluated_at);
        let recomputed = evaluate_books(
            &settings,
            &evaluable,
            &frame.books,
            &frame.portfolio,
            frame.evaluated_at,
        );
        self.rows_stored += frame.opportunities.len();
        self.rows_recomputed += recomputed.len();
        let mut stored = frame
            .opportunities
            .iter()
            .map(|row| (row.id, row))
            .collect::<HashMap<_, _>>();
        for row in &recomputed {
            let Some(original) = stored.remove(&row.id) else {
                self.rows_only_recomputed += 1;
                continue;
            };
            let mut matched = true;
            for (field, before, after) in differences(original, row) {
                matched = false;
                self.mismatches += 1;
                if self.samples.len() < 20 {
                    self.samples.push(Mismatch {
                        scan_id: frame.scan_id,
                        opportunity_id: row.id,
                        market_slug: row.market_slug.clone(),
                        field,
                        stored: before,
                        recomputed: after,
                    });
                }
            }
            if matched {
                self.rows_matched += 1;
            }
        }
        self.rows_only_stored += stored.len();
    }
}

fn differences(
    stored: &Opportunity,
    recomputed: &Opportunity,
) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    if stored.class != recomputed.class {
        out.push((
            "class",
            format!("{:?}", stored.class),
            format!("{:?}", recomputed.class),
        ));
    }
    let decimals: [(&'static str, Decimal, Decimal); 5] = [
        (
            "fair_probability",
            stored.fair_probability,
            recomputed.fair_probability,
        ),
        (
            "executable_price",
            stored.executable_price,
            recomputed.executable_price,
        ),
        ("net_edge", stored.net_edge, recomputed.net_edge),
        ("quantity", stored.quantity, recomputed.quantity),
        ("maximum_loss", stored.maximum_loss, recomputed.maximum_loss),
    ];
    for (field, before, after) in decimals {
        if before != after {
            out.push((field, before.to_string(), after.to_string()));
        }
    }
    if stored.reasons != recomputed.reasons {
        out.push((
            "reasons",
            stored.reasons.join(" | "),
            recomputed.reasons.join(" | "),
        ));
    }
    out
}
