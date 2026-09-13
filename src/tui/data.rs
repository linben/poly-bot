//! What the UI renders: the latest store contents plus derived statistics.
//! Loaded off the render loop on a fixed cadence.

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use tokio::sync::watch;

use crate::{
    config::{RunMode, Settings},
    domain::{PaperPortfolio, RecommendationClass, ResearchOpportunity, ScanSummary, Sport},
    storage::Store,
};

#[derive(Debug, Clone)]
pub struct StoreView {
    pub loaded_at: DateTime<Utc>,
    pub scan: Option<ScanSummary>,
    pub rows: Vec<ResearchOpportunity>,
    pub portfolio: PaperPortfolio,
    pub error: Option<String>,
    pub stats: EdgeStats,
}

/// Summary of where the market sits relative to consensus this scan.
#[derive(Debug, Clone, Default)]
pub struct EdgeStats {
    pub evaluated: usize,
    pub actionable: usize,
    pub watchlist: usize,
    pub rejected: usize,
    /// Rows by family count: index 0 = one family, 1 = two, 2 = three or more.
    pub by_families: [usize; 3],
    pub by_sport: Vec<(Sport, usize)>,
    /// Over rows with >= 3 families.
    pub quorum_rows: usize,
    pub mean_raw_edge: Option<f64>,
    pub sd_raw_edge: Option<f64>,
    pub best_raw_edge: Option<f64>,
    pub best_net_edge: Option<f64>,
}

/// What a side still needs to clear the gates in force: the three
/// independent categories the operator can act on (price band, family
/// quorum, edge). Size floors follow from edge and are not counted twice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateGap {
    /// Executable price outside the configured band.
    pub price: bool,
    /// Families short of the watchlist quorum.
    pub families: usize,
    /// Percentage points short of the binding edge gate (raw or net).
    pub edge_pp: f64,
}

impl GateGap {
    pub fn of(row: &ResearchOpportunity, settings: &Settings) -> Self {
        let o = &row.opportunity;
        let raw_short = decimal_f64(settings.minimum_raw_edge - o.raw_edge);
        let net_short = decimal_f64(settings.minimum_net_edge - o.net_edge);
        Self {
            price: o.executable_price < settings.minimum_price
                || o.executable_price > settings.maximum_price,
            families: settings
                .watchlist_source_families
                .saturating_sub(o.family_count),
            edge_pp: raw_short.max(net_short).max(0.0) * 100.0,
        }
    }

    /// Number of failed categories; 0 for a row inside every gate.
    pub fn failures(&self) -> u8 {
        u8::from(self.price) + u8::from(self.families > 0) + u8::from(self.edge_pp > 0.0)
    }

    /// `edge +4.3pp · fam +2 · price`, or `-` when nothing is missing.
    pub fn label(&self) -> String {
        let mut parts = Vec::with_capacity(3);
        if self.edge_pp > 0.0 {
            parts.push(format!("edge +{:.1}pp", self.edge_pp));
        }
        if self.families > 0 {
            parts.push(format!("fam +{}", self.families));
        }
        if self.price {
            parts.push("price".to_string());
        }
        if parts.is_empty() {
            "-".into()
        } else {
            parts.join(" · ")
        }
    }
}

impl EdgeStats {
    pub fn from_rows(rows: &[ResearchOpportunity]) -> Self {
        let mut stats = Self {
            evaluated: rows.len(),
            ..Self::default()
        };
        let mut by_sport = std::collections::BTreeMap::<String, (Sport, usize)>::new();
        let mut quorum = Vec::new();
        for row in rows {
            match row.effective_class {
                RecommendationClass::Actionable => stats.actionable += 1,
                RecommendationClass::Watchlist => stats.watchlist += 1,
                RecommendationClass::Rejected => stats.rejected += 1,
            }
            let bucket = row.opportunity.family_count.clamp(1, 3) - 1;
            stats.by_families[bucket] += 1;
            by_sport
                .entry(row.opportunity.sport.to_string())
                .or_insert((row.opportunity.sport, 0))
                .1 += 1;
            if row.opportunity.family_count >= 3 {
                quorum.push((
                    decimal_f64(row.opportunity.raw_edge),
                    decimal_f64(row.opportunity.net_edge),
                ));
            }
        }
        stats.by_sport = by_sport.into_values().collect();
        stats.quorum_rows = quorum.len();
        if !quorum.is_empty() {
            let n = quorum.len() as f64;
            let mean = quorum.iter().map(|(raw, _)| raw).sum::<f64>() / n;
            let variance = quorum
                .iter()
                .map(|(raw, _)| (raw - mean).powi(2))
                .sum::<f64>()
                / n;
            stats.mean_raw_edge = Some(mean);
            stats.sd_raw_edge = Some(variance.sqrt());
            stats.best_raw_edge = quorum.iter().map(|(raw, _)| *raw).reduce(f64::max);
            stats.best_net_edge = quorum.iter().map(|(_, net)| *net).reduce(f64::max);
        }
        stats
    }
}

pub fn decimal_f64(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

pub async fn load_store_view(store: &dyn Store, settings: &Settings) -> StoreView {
    let loaded_at = Utc::now();
    let result: crate::Result<(
        Option<ScanSummary>,
        Vec<ResearchOpportunity>,
        PaperPortfolio,
    )> = async {
        let scan = store.latest_scan().await?;
        let opportunities = store.latest_opportunities().await?;
        let mut rows = Vec::with_capacity(opportunities.len());
        for opportunity in opportunities {
            let news = if opportunity.class == RecommendationClass::Rejected {
                None
            } else {
                store.news_for(opportunity.id).await?
            };
            rows.push(ResearchOpportunity::new(opportunity, news));
        }
        let portfolio = store.load_portfolio(settings.bankroll).await?;
        Ok((scan, rows, portfolio))
    }
    .await;
    match result {
        Ok((scan, rows, portfolio)) => StoreView {
            loaded_at,
            stats: EdgeStats::from_rows(&rows),
            scan,
            rows,
            portfolio,
            error: None,
        },
        Err(error) => StoreView {
            loaded_at,
            scan: None,
            rows: Vec::new(),
            portfolio: PaperPortfolio::new(settings.bankroll),
            error: Some(error.to_string()),
            stats: EdgeStats::default(),
        },
    }
}

/// Poll the store on a cadence suited to its latency: files every 2 s,
/// DynamoDB every 10 s. `refresh` forces an immediate reload.
pub fn spawn_store_poller(
    store: Arc<dyn Store>,
    settings: Settings,
    mut refresh: watch::Receiver<u64>,
) -> watch::Receiver<Arc<StoreView>> {
    let cadence = match settings.run_mode {
        RunMode::Local => Duration::from_secs(2),
        RunMode::Cloud => Duration::from_secs(10),
    };
    let (tx, rx) = watch::channel(Arc::new(StoreView {
        loaded_at: Utc::now(),
        scan: None,
        rows: Vec::new(),
        portfolio: PaperPortfolio::new(settings.bankroll),
        error: None,
        stats: EdgeStats::default(),
    }));
    tokio::spawn(async move {
        loop {
            let view = load_store_view(store.as_ref(), &settings).await;
            if tx.send(Arc::new(view)).is_err() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(cadence) => {}
                changed = refresh.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}
