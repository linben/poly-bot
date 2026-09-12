//! Local run mode: one process hosting the scan loop, the news-review loop,
//! retention pruning, and the dashboard. No cloud resources are touched.

use std::{sync::Arc, time::Duration};

use clap::Parser;
use polybot::{
    Result,
    config::{RunMode, Settings},
    dashboard,
    news::{SharedReviewer, process_news_queue, reviewer_from_env},
    scanner::Scanner,
    storage::{Store, store_for},
};
use tokio::time::{Instant, MissedTickBehavior, interval};
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "data")]
    data_dir: String,
    /// Run one scan plus one news pass, then exit. Useful for smoke tests.
    #[arg(long)]
    once: bool,
    /// Skip the dashboard server.
    #[arg(long)]
    no_dashboard: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let args = Args::parse();
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

    let scan_loop = tokio::spawn(scan_loop(
        Arc::clone(&scanner),
        Arc::clone(&store),
        settings.scan_interval,
        Duration::from_secs(u64::from(settings.retention_days) * 86_400),
    ));
    let news_loop = tokio::spawn(news_loop(
        Arc::clone(&store),
        reviewer,
        news_refresh,
        Duration::from_secs(20),
    ));
    let dashboard = if args.no_dashboard {
        None
    } else {
        Some(tokio::spawn(dashboard::serve(
            Arc::clone(&store),
            dashboard::listen_address()?,
        )))
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("shutdown requested"),
        result = scan_loop => error!(?result, "scan loop exited"),
        result = news_loop => error!(?result, "news loop exited"),
        result = async {
            match dashboard {
                Some(handle) => handle.await,
                None => std::future::pending().await,
            }
        } => error!(?result, "dashboard exited"),
    }
    Ok(())
}

/// Fixed cadence: a slow scan delays the next tick instead of bunching them.
async fn scan_loop(
    scanner: Arc<Scanner>,
    store: Arc<dyn Store>,
    every: Duration,
    retention: Duration,
) {
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let started = Instant::now();
        match scanner.run_once().await {
            Ok(snapshot) => {
                let candidates = snapshot
                    .opportunities
                    .iter()
                    .filter(|item| item.class != polybot::domain::RecommendationClass::Rejected)
                    .count();
                info!(
                    scan = %snapshot.scan_id,
                    markets = snapshot.market_count,
                    quotes = snapshot.quote_count,
                    evaluated = snapshot.opportunities.len(),
                    candidates,
                    elapsed_ms = started.elapsed().as_millis(),
                    "scan finished"
                );
            }
            Err(error) => error!(%error, "scan failed; retrying next interval"),
        }
        match store.prune(retention).await {
            Ok(0) => {}
            Ok(removed) => info!(removed, "pruned old scan files"),
            Err(error) => warn!(%error, "retention pruning failed"),
        }
    }
}

async fn news_loop(
    store: Arc<dyn Store>,
    reviewer: Option<SharedReviewer>,
    refresh: Duration,
    poll: Duration,
) {
    let Some(reviewer) = reviewer else {
        // Nothing to review; keep the queue from growing without bound.
        let mut ticker = interval(Duration::from_secs(600));
        loop {
            ticker.tick().await;
            if let Err(error) = store.take_news_queue().await {
                warn!(%error, "draining unused news queue failed");
            }
        }
    };
    let mut ticker = interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match process_news_queue(store.as_ref(), reviewer.as_ref(), refresh).await {
            Ok(0) => {}
            Ok(reviewed) => info!(reviewed, "news pass complete"),
            Err(error) => error!(%error, "news pass failed"),
        }
    }
}
