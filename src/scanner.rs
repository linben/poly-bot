use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use chrono::Utc;
use futures::{StreamExt, stream};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    Result,
    config::Settings,
    consensus::build_consensus,
    domain::{
        Opportunity, PaperPortfolio, RecommendationClass, ScanSnapshot, SourceHealth, SourceQuote,
        UsMoneylineMarket,
    },
    matching::{match_quote, orient_quote},
    opportunity::OpportunityEngine,
    polymarket::PolymarketUsClient,
    sources::SharedSource,
    storage::Store,
};

pub struct Scanner {
    settings: Settings,
    polymarket: PolymarketUsClient,
    sources: Vec<SharedSource>,
    store: Arc<dyn Store>,
}

impl Scanner {
    pub fn new(
        settings: Settings,
        polymarket: PolymarketUsClient,
        sources: Vec<SharedSource>,
        store: Arc<dyn Store>,
    ) -> Self {
        Self {
            settings,
            polymarket,
            sources,
            store,
        }
    }

    pub async fn run_once(&self) -> Result<ScanSnapshot> {
        let scan_id = Uuid::new_v4();
        if !self
            .store
            .acquire_scan_lease(scan_id, self.settings.scan_lease_ttl)
            .await?
        {
            return Err(crate::Error::Storage(
                "another scanner invocation holds the lease".into(),
            ));
        }
        let result = self.run_once_with_lease(scan_id).await;
        let release = self.store.release_scan_lease(scan_id).await;
        match (result, release) {
            (Ok(snapshot), Ok(())) => Ok(snapshot),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    async fn run_once_with_lease(&self, scan_id: Uuid) -> Result<ScanSnapshot> {
        let started_at = Utc::now();
        info!(%scan_id, "starting scan");
        let markets = self.polymarket.discover_moneylines().await?;
        let (quotes, source_health) = self.collect_sources(&self.sources).await;
        let portfolio = self.store.load_portfolio(self.settings.bankroll).await?;
        let preliminary = self
            .evaluate_markets(&markets, &quotes, &portfolio, false)
            .await;
        let needs_confirmation = preliminary
            .iter()
            .any(|item| item.class != RecommendationClass::Rejected);

        let (final_quotes, mut opportunities, mut all_health) = if needs_confirmation {
            let confirmation_sources = self.confirmation_sources(&preliminary);
            let selected_ids = confirmation_sources
                .iter()
                .map(|source| source.id().to_string())
                .collect::<HashSet<_>>();
            let (confirmation_quotes, confirmation_health) =
                self.collect_sources(&confirmation_sources).await;
            let mut merged_quotes = quotes;
            merged_quotes.retain(|quote| !selected_ids.contains(&quote.source_id));
            merged_quotes.extend(confirmation_quotes);
            let confirmed = self
                .evaluate_markets(&markets, &merged_quotes, &portfolio, true)
                .await;
            let mut health = source_health;
            health.extend(confirmation_health);
            (merged_quotes, confirmed, health)
        } else {
            (quotes, preliminary, source_health)
        };
        opportunities.sort_by_key(|opportunity| Reverse(opportunity.net_edge));
        deduplicate_health(&mut all_health);

        let snapshot = ScanSnapshot {
            scan_id,
            started_at,
            completed_at: Utc::now(),
            market_count: markets.len(),
            quote_count: final_quotes.len(),
            opportunities,
            source_health: all_health,
        };
        self.store.save_scan(&snapshot, &final_quotes).await?;
        let news_candidates = snapshot
            .opportunities
            .iter()
            .filter(|item| item.class != RecommendationClass::Rejected)
            .cloned()
            .collect::<Vec<_>>();
        self.store.enqueue_news(&news_candidates).await?;
        info!(
            %scan_id,
            markets = snapshot.market_count,
            quotes = snapshot.quote_count,
            opportunities = news_candidates.len(),
            "scan complete"
        );
        Ok(snapshot)
    }

    fn confirmation_sources(&self, opportunities: &[Opportunity]) -> Vec<SharedSource> {
        let mut frequency = HashMap::<String, usize>::new();
        for source_id in opportunities
            .iter()
            .filter(|item| item.class != RecommendationClass::Rejected)
            .flat_map(|item| item.source_ids.iter())
        {
            *frequency.entry(source_id.clone()).or_default() += 1;
        }
        let mut candidates = self
            .sources
            .iter()
            .filter(|source| frequency.contains_key(source.id()))
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .family()
                .is_reference()
                .cmp(&left.family().is_reference())
                .then(frequency.get(right.id()).cmp(&frequency.get(left.id())))
                .then(left.id().cmp(right.id()))
        });
        candidates.truncate(3);
        candidates
    }

    async fn collect_sources(
        &self,
        selected: &[SharedSource],
    ) -> (Vec<SourceQuote>, Vec<SourceHealth>) {
        let results = stream::iter(selected.iter().cloned())
            .map(|source| async move {
                let started = Instant::now();
                let result = source.collect().await;
                (source, result, started.elapsed())
            })
            .buffer_unordered(self.settings.source_concurrency)
            .collect::<Vec<_>>()
            .await;
        let mut quotes = Vec::new();
        let mut health = Vec::new();
        for (source, result, elapsed) in results {
            match result {
                Ok(mut source_quotes) => {
                    let count = source_quotes.len();
                    for quote in &mut source_quotes {
                        quote.validation_only |= source.validation_only();
                    }
                    quotes.extend(source_quotes);
                    health.push(SourceHealth {
                        source_id: source.id().into(),
                        family: source.family(),
                        checked_at: Utc::now(),
                        reachable: true,
                        odds_found: count,
                        latency_ms: elapsed.as_millis() as u64,
                        message: format!("{count} quotes"),
                    });
                }
                Err(error) => {
                    warn!(source = source.id(), %error, "source collection failed");
                    health.push(SourceHealth {
                        source_id: source.id().into(),
                        family: source.family(),
                        checked_at: Utc::now(),
                        reachable: false,
                        odds_found: 0,
                        latency_ms: elapsed.as_millis() as u64,
                        message: error.to_string(),
                    });
                }
            }
        }
        (quotes, health)
    }

    async fn evaluate_markets(
        &self,
        markets: &[UsMoneylineMarket],
        quotes: &[SourceQuote],
        portfolio: &PaperPortfolio,
        confirmation: bool,
    ) -> Vec<Opportunity> {
        let engine = OpportunityEngine::new(self.settings.clone());
        let mut opportunities = Vec::new();
        for market in markets {
            let matching = quotes
                .iter()
                .filter_map(|quote| {
                    match_quote(market, quote).map(|orientation| orient_quote(quote, orientation))
                })
                .collect::<Vec<_>>();
            let max_age = if confirmation {
                self.settings.confirmation_max_age
            } else {
                self.settings.max_quote_age
            };
            let Ok(consensus) = build_consensus(&matching, max_age.as_secs() as i64) else {
                continue;
            };
            match self.polymarket.fetch_book(&market.market_slug).await {
                Ok(book) => {
                    opportunities.extend(engine.evaluate(market, &book, &consensus, portfolio))
                }
                Err(error) => warn!(market = market.market_slug, %error, "book fetch failed"),
            }
        }
        opportunities
    }
}

fn deduplicate_health(health: &mut Vec<SourceHealth>) {
    health.sort_by(|left, right| {
        left.source_id
            .cmp(&right.source_id)
            .then(right.checked_at.cmp(&left.checked_at))
    });
    health.dedup_by(|left, right| left.source_id == right.source_id);
}
