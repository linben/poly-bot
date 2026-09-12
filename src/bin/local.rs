//! Local run mode: one process hosting the scan loop, the news-review loop,
//! retention pruning, and the terminal UI. No cloud resources are touched.
//! With a TTY the UI owns the screen and logs are kept in memory; with
//! `--headless` (systemd) logs go to stderr and there is no UI.

use std::{io::IsTerminal, sync::Arc, time::Duration};

use chrono::Utc;
use clap::Parser;
use polybot::{
    Result,
    config::{RunMode, Settings},
    news::{SharedReviewer, process_news_queue, reviewer_from_env},
    scanner::Scanner,
    storage::{Store, store_for},
    tui::{
        self, TuiOptions,
        feed::{EngineCommand, EnginePort, EngineStatus, Phase, engine_channel},
        log::LogBuffer,
    },
};
use tokio::{
    sync::watch,
    time::{Instant, MissedTickBehavior, interval, sleep_until},
};
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "data", env = "DATA_DIR")]
    data_dir: String,
    /// Run one scan plus one news pass, then exit. Useful for smoke tests.
    #[arg(long)]
    once: bool,
    /// No terminal UI; log to stderr. Implied when stdout is not a TTY.
    #[arg(long)]
    headless: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let interactive = !args.headless
        && !args.once
        && std::io::stdout().is_terminal()
        && std::io::stdin().is_terminal();
    let logs = if interactive {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            "info,hyper_util=warn,hyper=warn,reqwest=warn,rustls=warn,h2=warn".into()
        });
        LogBuffer::install(filter, 500)
    } else {
        polybot::init_tracing();
        None
    };

    let mut settings = Settings::from_env()?;
    if settings.run_mode != RunMode::Local {
        warn!("local binary forces RUN_MODE=local");
        settings.run_mode = RunMode::Local;
    }
    let store = store_for(settings.run_mode, &args.data_dir).await?;
    let scanner = Arc::new(Scanner::from_settings(
        settings.clone(),
        Arc::clone(&store),
    )?);
    let reviewer = reviewer_from_env(settings.request_timeout).await?;
    match &reviewer {
        Some(reviewer) => info!(reviewer = reviewer.name(), "news reviewer active"),
        None => warn!(
            "no news reviewer configured; candidates stay on the watchlist (set BRAVE_SEARCH_API_KEY or NEWS_REVIEWER)"
        ),
    }
    let news_refresh = Duration::from_secs(
        std::env::var("NEWS_REFRESH_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(3600),
    );

    if args.once {
        let snapshot = scanner.run_once().await?;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        if let Some(reviewer) = &reviewer {
            let reviewed =
                process_news_queue(store.as_ref(), reviewer.as_ref(), news_refresh).await?;
            info!(reviewed, "news pass complete");
        }
        return Ok(());
    }

    let source_ids = scanner.source_ids();
    let (port, handle) = engine_channel(EngineStatus::new(
        source_ids,
        reviewer.as_ref().map(|reviewer| reviewer.name()),
    ));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let engine = tokio::spawn(engine_loop(
        Arc::clone(&scanner),
        Arc::clone(&store),
        reviewer,
        news_refresh,
        settings.clone(),
        port,
        shutdown_tx.clone(),
        shutdown_rx.clone(),
    ));

    if interactive {
        let ui = tui::run(TuiOptions {
            settings: settings.clone(),
            store: Arc::clone(&store),
            engine: Some(handle),
            logs,
        })
        .await;
        let _ = shutdown_tx.send(true);
        let _ = engine.await;
        ui
    } else {
        let mut engine = engine;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown requested");
                let _ = shutdown_tx.send(true);
                let _ = engine.await;
            }
            result = &mut engine => error!(?result, "engine exited"),
        }
        Ok(())
    }
}

/// Scan on a fixed cadence (a slow scan delays the next tick), drain the news
/// queue between scans, prune old snapshots, and publish status for the UI.
#[allow(clippy::too_many_arguments)]
async fn engine_loop(
    scanner: Arc<Scanner>,
    store: Arc<dyn Store>,
    reviewer: Option<SharedReviewer>,
    news_refresh: Duration,
    settings: Settings,
    mut port: EnginePort,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let retention = Duration::from_secs(u64::from(settings.retention_days) * 86_400);
    let mut news_ticker = interval(Duration::from_secs(20));
    news_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut next_scan = Instant::now();
    loop {
        if *shutdown_rx.borrow() {
            port.status
                .send_modify(|status| status.phase = Phase::ShuttingDown);
            return;
        }
        tokio::select! {
            _ = sleep_until(next_scan) => {
                next_scan = Instant::now() + settings.scan_interval;
                run_scan(&scanner, &store, &settings, &port, retention).await;
            }
            _ = news_ticker.tick() => {
                if let Some(reviewer) = &reviewer {
                    port.status.send_modify(|status| status.phase = Phase::Reviewing);
                    match process_news_queue(store.as_ref(), reviewer.as_ref(), news_refresh).await {
                        Ok(0) => {}
                        Ok(reviewed) => {
                            info!(reviewed, "news pass complete");
                            port.status.send_modify(|status| {
                                status.last_news_pass = Some((Utc::now(), reviewed));
                            });
                        }
                        Err(error) => error!(%error, "news pass failed"),
                    }
                    port.status.send_modify(|status| status.phase = Phase::Idle);
                } else if let Err(error) = store.take_news_queue().await {
                    warn!(%error, "draining unused news queue failed");
                }
            }
            command = port.commands.recv() => match command {
                Some(EngineCommand::ScanNow) => next_scan = Instant::now(),
                Some(EngineCommand::Shutdown) | None => {
                    let _ = shutdown_tx.send(true);
                }
            },
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    port.status.send_modify(|status| status.phase = Phase::ShuttingDown);
                    return;
                }
            }
        }
    }
}

async fn run_scan(
    scanner: &Scanner,
    store: &Arc<dyn Store>,
    settings: &Settings,
    port: &EnginePort,
    retention: Duration,
) {
    let started = Instant::now();
    let next_scan_at =
        Utc::now() + chrono::Duration::from_std(settings.scan_interval).unwrap_or_default();
    port.status.send_modify(|status| {
        status.phase = Phase::Scanning { since: Utc::now() };
        status.next_scan_at = Some(next_scan_at);
    });
    match scanner.run_once().await {
        Ok(snapshot) => {
            let summary = snapshot.summary();
            info!(
                scan = %summary.scan_id,
                markets = summary.market_count,
                quotes = summary.quote_count,
                evaluated = summary.evaluated_count,
                candidates = summary.candidate_count,
                elapsed_ms = started.elapsed().as_millis(),
                "scan finished"
            );
            port.status.send_modify(|status| {
                status.scans_completed += 1;
                status.last_scan = Some(summary);
                status.last_scan_duration_ms = Some(started.elapsed().as_millis() as u64);
                status.last_error = None;
                status.phase = Phase::Idle;
            });
        }
        Err(error) => {
            error!(%error, "scan failed; retrying next interval");
            let message = error.to_string();
            port.status.send_modify(|status| {
                status.scans_failed += 1;
                status.last_error = Some(message);
                status.phase = Phase::Idle;
            });
        }
    }
    match store.prune(retention).await {
        Ok(0) => {}
        Ok(removed) => info!(removed, "pruned old scan files"),
        Err(error) => warn!(%error, "retention pruning failed"),
    }
}
