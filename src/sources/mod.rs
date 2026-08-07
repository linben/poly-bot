mod canonical;
mod catalog;
mod the_odds_api;

use std::{sync::Arc, time::Instant};

use async_trait::async_trait;
use chrono::Utc;

use crate::{
    Result,
    domain::{SourceFamily, SourceHealth, SourceQuote},
};

pub use canonical::CanonicalJsonSource;
pub use catalog::{SourceCatalog, SourceSpec, SourceTier};
pub use the_odds_api::TheOddsApiSource;

#[async_trait]
pub trait OddsSource: Send + Sync {
    fn id(&self) -> &str;
    fn family(&self) -> SourceFamily;
    fn validation_only(&self) -> bool {
        false
    }
    async fn collect(&self) -> Result<Vec<SourceQuote>>;
    async fn probe(&self) -> SourceHealth;
}

pub type SharedSource = Arc<dyn OddsSource>;

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
