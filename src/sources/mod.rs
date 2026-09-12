mod canonical;
mod catalog;
mod espn;
mod kalshi;
mod polymarket_global;
mod the_odds_api;

use std::{sync::Arc, time::Duration, time::Instant};

use async_trait::async_trait;
use chrono::Utc;

use crate::{
    Result,
    domain::{SourceFamily, SourceHealth, SourceQuote, Sport},
};

pub use canonical::CanonicalJsonSource;
pub use catalog::{SourceCatalog, SourceSpec, SourceTier};
pub use espn::EspnOddsSource;
pub use kalshi::KalshiSource;
pub use polymarket_global::PolymarketGlobalSource;
pub use the_odds_api::TheOddsApiSource;

/// Identifying user agent with a contact URL. Some public sports APIs reject
/// bare `name/version` tokens; this format is accepted and honest.
pub const USER_AGENT: &str = "polybot/0.1 (+https://github.com/polybot; research scanner)";

pub fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(USER_AGENT)
        .gzip(true)
        .build()?)
}

#[async_trait]
pub trait OddsSource: Send + Sync {
    fn id(&self) -> &str;
    fn family(&self) -> SourceFamily;
    /// Quotes never enter consensus; kept for side-by-side comparison only.
    fn validation_only(&self) -> bool {
        false
    }
    /// Quota-limited source that is only queried to confirm candidates the
    /// continuous sources already produced. Never part of the preliminary pass.
    fn confirmation_only(&self) -> bool {
        false
    }
    /// Collect quotes restricted to `sports`. Adapters that cannot filter
    /// upstream must filter their output.
    async fn collect(&self, sports: &[Sport]) -> Result<Vec<SourceQuote>>;
    async fn probe(&self) -> SourceHealth;
}

pub type SharedSource = Arc<dyn OddsSource>;

pub fn health_ok(source: &dyn OddsSource, started: Instant, message: String) -> SourceHealth {
    SourceHealth {
        source_id: source.id().into(),
        family: source.family(),
        checked_at: Utc::now(),
        reachable: true,
        odds_found: 0,
        latency_ms: started.elapsed().as_millis() as u64,
        message,
    }
}

pub fn health_err(source: &dyn OddsSource, started: Instant, error: &crate::Error) -> SourceHealth {
    SourceHealth {
        source_id: source.id().into(),
        family: source.family(),
        checked_at: Utc::now(),
        reachable: false,
        odds_found: 0,
        latency_ms: started.elapsed().as_millis() as u64,
        message: error.to_string(),
    }
}

/// Probe by running a real collection so the health record proves parsing,
/// not just reachability.
pub async fn probe_by_collecting(source: &dyn OddsSource, sports: &[Sport]) -> SourceHealth {
    let started = Instant::now();
    match source.collect(sports).await {
        Ok(quotes) => {
            let mut health = health_ok(
                source,
                started,
                format!("{} normalized quotes", quotes.len()),
            );
            health.odds_found = quotes.len();
            health
        }
        Err(error) => health_err(source, started, &error),
    }
}

pub async fn probe_url(
    client: &reqwest::Client,
    source_id: &str,
    family: SourceFamily,
    url: &str,
) -> SourceHealth {
    let started = Instant::now();
    match client.get(url).send().await {
        Ok(response) => SourceHealth {
            source_id: source_id.into(),
            family,
            checked_at: Utc::now(),
            reachable: response.status().is_success() || response.status().is_redirection(),
            odds_found: 0,
            latency_ms: started.elapsed().as_millis() as u64,
            message: format!("HTTP {}", response.status()),
        },
        Err(error) => SourceHealth {
            source_id: source_id.into(),
            family,
            checked_at: Utc::now(),
            reachable: false,
            odds_found: 0,
            latency_ms: started.elapsed().as_millis() as u64,
            message: error.to_string(),
        },
    }
}
