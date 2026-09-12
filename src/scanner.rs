use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use chrono::Utc;
use futures::{StreamExt, stream};
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    Result,
    config::Settings,
    consensus::build_consensus,
    domain::{
        Opportunity, PaperPortfolio, RecommendationClass, ScanSnapshot, SourceHealth, SourceQuote,
        Sport, UsMoneylineMarket,
    },
    matching::{match_quote, orient_quote},
    opportunity::OpportunityEngine,
    polymarket::PolymarketUsClient,
    sources::{OddsSource, SharedSource, SourceCatalog},
    storage::Store,
};

pub struct Scanner {
    settings: Settings,
    polymarket: PolymarketUsClient,
    sources: Vec<SharedSource>,
    store: Arc<dyn Store>,
    discovery: Mutex<Option<(Instant, Arc<Vec<UsMoneylineMarket>>)>>,
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
            discovery: Mutex::new(None),
        }
    }

    /// Load the source catalog, enforce the startup quorum, and build the
    /// Polymarket client from `settings`.
    pub fn from_settings(settings: Settings, store: Arc<dyn Store>) -> Result<Self> {
        let catalog = SourceCatalog::load(&settings.source_config_path)?;
        let sources = catalog.configured_sources(settings.request_timeout)?;
        let continuous = sources
            .iter()
            .filter(|source| !source.validation_only() && !source.confirmation_only())
            .collect::<Vec<_>>();
        let families = continuous
            .iter()
            .map(|source| source.family())
            .collect::<HashSet<_>>();
        if families.len() < settings.minimum_configured_sources {
            return Err(crate::Error::Config(format!(
                "scanner requires {} independent continuous source families; found {} sources across {} families",
                settings.minimum_configured_sources,
                continuous.len(),
                families.len()
            )));
        }
        let has_confirmation = sources.iter().any(|source| source.confirmation_only());
        if !has_confirmation && !families.iter().any(|family| family.is_reference()) {
            warn!(
                "no reference book and no confirmation-tier source configured; results cannot exceed watchlist (set ENABLE_THE_ODDS_API=true and THE_ODDS_API_KEY)"
            );
        }
        info!(
            sources = ?sources.iter().map(|source| source.id()).collect::<Vec<_>>(),
            families = families.len(),
            "sources configured"
        );
        let polymarket = PolymarketUsClient::with_concurrency(
            settings.polymarket_base_url.clone(),
            settings.request_timeout,
            settings.book_concurrency,
        )?;
        Ok(Self::new(settings, polymarket, sources, store))
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
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
        let markets = self.markets().await?;
        let continuous = self
            .sources
            .iter()
            .filter(|source| !source.confirmation_only())
            .cloned()
            .collect::<Vec<_>>();
        let (quotes, source_health) = self.collect_sources(&continuous, &Sport::ALL).await;
        let portfolio = self.store.load_portfolio(self.settings.bankroll).await?;
        let preliminary = self
            .evaluate_markets(&markets, &quotes, &portfolio, false)
            .await;
        let candidate_sports = preliminary
            .iter()
            .filter(|item| item.class != RecommendationClass::Rejected)
            .map(|item| item.sport)
            .collect::<HashSet<_>>();

        let (final_quotes, mut opportunities, mut all_health) = if candidate_sports.is_empty() {
            (quotes, preliminary, source_health)
        } else {
            let sports = candidate_sports.into_iter().collect::<Vec<_>>();
            let confirmation_sources = self.confirmation_sources(&preliminary);
            let (confirmation_quotes, confirmation_health) =
                self.collect_sources(&confirmation_sources, &sports).await;
            let mut merged_quotes = quotes;
            merged_quotes.retain(|quote| {
                !confirmation_sources
                    .iter()
                    .any(|source| quote_belongs_to(quote, source.as_ref()))
            });
            merged_quotes.extend(confirmation_quotes);
            let confirmed = self
                .evaluate_markets(&markets, &merged_quotes, &portfolio, true)
                .await;
            let mut health = source_health;
            health.extend(confirmation_health);
            (merged_quotes, confirmed, health)
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

    /// Every quota-limited confirmation-tier source, plus up to three of the
    /// continuous sources that contributed to a candidate (reference books
    /// first, then by how many candidates they touched). Their initial quotes
    /// are replaced wholesale, so a candidate must survive fresh prices.
    fn confirmation_sources(&self, opportunities: &[Opportunity]) -> Vec<SharedSource> {
        let mut frequency = HashMap::<&str, usize>::new();
        for source_id in opportunities
            .iter()
            .filter(|item| item.class != RecommendationClass::Rejected)
            .flat_map(|item| item.source_ids.iter())
        {
            for source in &self.sources {
                if quote_id_belongs_to(source_id, source.as_ref()) {
                    *frequency.entry(source.id()).or_default() += 1;
                }
            }
        }
        let mut refetch = self
            .sources
            .iter()
            .filter(|source| !source.confirmation_only() && frequency.contains_key(source.id()))
            .cloned()
            .collect::<Vec<_>>();
        refetch.sort_by(|left, right| {
            right
                .family()
                .is_reference()
                .cmp(&left.family().is_reference())
                .then(frequency.get(right.id()).cmp(&frequency.get(left.id())))
                .then(left.id().cmp(right.id()))
        });
        refetch.truncate(3);
        refetch.extend(
            self.sources
                .iter()
                .filter(|source| source.confirmation_only())
                .cloned(),
        );
        refetch
    }

    async fn collect_sources(
        &self,
        selected: &[SharedSource],
        sports: &[Sport],
    ) -> (Vec<SourceQuote>, Vec<SourceHealth>) {
        let requests = selected
            .iter()
            .cloned()
            .map(|source| async move {
                let started = Instant::now();
                let result = source.collect(sports).await;
                (source, result, started.elapsed())
            })
            .collect::<Vec<_>>();
        let results = stream::iter(requests)
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

    /// Markets are reused for `discovery_refresh`; a live or started game is
    /// dropped at evaluation time so a stale cache cannot leak an in-play book.
    async fn markets(&self) -> Result<Arc<Vec<UsMoneylineMarket>>> {
        let mut cache = self.discovery.lock().await;
        if let Some((fetched, markets)) = cache.as_ref()
            && fetched.elapsed() < self.settings.discovery_refresh
        {
            return Ok(Arc::clone(markets));
        }
        let started = Instant::now();
        let markets = Arc::new(self.polymarket.discover_moneylines().await?);
        info!(
            markets = markets.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "market discovery refreshed"
        );
        *cache = Some((Instant::now(), Arc::clone(&markets)));
        Ok(markets)
    }

    async fn evaluate_markets(
        &self,
        markets: &[UsMoneylineMarket],
        quotes: &[SourceQuote],
        portfolio: &PaperPortfolio,
        confirmation: bool,
    ) -> Vec<Opportunity> {
        let engine = OpportunityEngine::new(self.settings.clone());
        let max_age = if confirmation {
            self.settings.confirmation_max_age
        } else {
            self.settings.max_quote_age
        };
        let now = Utc::now();
        let evaluable = markets
            .iter()
            .filter(|market| market.start_time > now)
            .filter_map(|market| {
                let matching = quotes
                    .iter()
                    .filter_map(|quote| {
                        match_quote(market, quote)
                            .map(|orientation| orient_quote(quote, orientation))
                    })
                    .collect::<Vec<_>>();
                build_consensus(&matching, max_age.as_secs() as i64)
                    .ok()
                    .map(|consensus| (market, consensus))
            })
            .collect::<Vec<_>>();
        let slugs = evaluable
            .iter()
            .map(|(market, _)| market.market_slug.as_str())
            .collect::<Vec<_>>();
        let books = self.polymarket.fetch_books(&slugs).await;

        let mut opportunities = Vec::new();
        for ((market, consensus), book) in evaluable.iter().zip(books) {
            match book {
                Ok(book) => {
                    opportunities.extend(engine.evaluate(market, &book, consensus, portfolio))
                }
                Err(error) => warn!(market = market.market_slug, %error, "book fetch failed"),
            }
        }
        opportunities
    }
}

/// Adapters that fan one collector out into several books emit quote ids of
/// the form `<collector>:<book>`; both shapes belong to the collector.
fn quote_id_belongs_to(quote_source_id: &str, source: &dyn OddsSource) -> bool {
    quote_source_id == source.id()
        || quote_source_id
            .strip_prefix(source.id())
            .is_some_and(|rest| rest.starts_with(':'))
}

fn quote_belongs_to(quote: &SourceQuote, source: &dyn OddsSource) -> bool {
    quote_id_belongs_to(&quote.source_id, source)
}

fn deduplicate_health(health: &mut Vec<SourceHealth>) {
    health.sort_by(|left, right| {
        left.source_id
            .cmp(&right.source_id)
            .then(right.checked_at.cmp(&left.checked_at))
    });
    health.dedup_by(|left, right| left.source_id == right.source_id);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use rust_decimal::Decimal;

    use super::*;
    use crate::{
        domain::{OutcomeSide, SourceFamily},
        storage::LocalStore,
    };

    struct FakeSource {
        id: &'static str,
        family: SourceFamily,
        confirmation: bool,
    }

    #[async_trait]
    impl OddsSource for FakeSource {
        fn id(&self) -> &str {
            self.id
        }
        fn family(&self) -> SourceFamily {
            self.family.clone()
        }
        fn confirmation_only(&self) -> bool {
            self.confirmation
        }
        async fn collect(&self, _sports: &[Sport]) -> Result<Vec<SourceQuote>> {
            Ok(Vec::new())
        }
        async fn probe(&self) -> SourceHealth {
            unreachable!()
        }
    }

    fn source(id: &'static str, family: SourceFamily, confirmation: bool) -> SharedSource {
        Arc::new(FakeSource {
            id,
            family,
            confirmation,
        })
    }

    fn opportunity(class: RecommendationClass, source_ids: &[&str]) -> Opportunity {
        Opportunity {
            id: Uuid::new_v4(),
            generated_at: Utc::now(),
            class,
            sport: Sport::Mlb,
            event_id: "event".into(),
            market_id: "market".into(),
            market_slug: "market".into(),
            participant: "A".into(),
            side: OutcomeSide::Long,
            fair_probability: Decimal::ZERO,
            conservative_probability: Decimal::ZERO,
            executable_price: Decimal::ZERO,
            maker_price: None,
            raw_edge: Decimal::ZERO,
            net_edge: Decimal::ZERO,
            quantity: Decimal::ZERO,
            maximum_loss: Decimal::ZERO,
            estimated_fee: Decimal::ZERO,
            source_count: source_ids.len(),
            family_count: source_ids.len(),
            source_ids: source_ids.iter().map(|id| id.to_string()).collect(),
            book_time: Utc::now(),
            reasons: Vec::new(),
        }
    }

    #[test]
    fn confirmation_refetches_contributors_and_every_confirmation_tier_source() {
        let directory = std::env::temp_dir().join(format!("polybot-scanner-{}", Uuid::new_v4()));
        let sources = vec![
            source("espn", SourceFamily::DraftKings, false),
            source("kalshi", SourceFamily::Kalshi, false),
            source("polymarket_global", SourceFamily::PolymarketGlobal, false),
            source("idle", SourceFamily::Bovada, false),
            source("pinnacle_direct", SourceFamily::Pinnacle, false),
            source(
                "the_odds_api",
                SourceFamily::Other("the_odds_api".into()),
                true,
            ),
        ];
        let scanner = Scanner::new(
            Settings::default(),
            PolymarketUsClient::new("http://127.0.0.1:9", Duration::from_secs(1)).unwrap(),
            sources,
            Arc::new(LocalStore::new(&directory).unwrap()),
        );
        let opportunities = vec![
            opportunity(
                RecommendationClass::Watchlist,
                &["espn:draftkings", "kalshi", "polymarket_global"],
            ),
            opportunity(
                RecommendationClass::Watchlist,
                &["kalshi", "polymarket_global"],
            ),
            // Rejected candidates never drive a refetch.
            opportunity(RecommendationClass::Rejected, &["idle", "pinnacle_direct"]),
        ];
        let selected = scanner
            .confirmation_sources(&opportunities)
            .iter()
            .map(|source| source.id().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            selected,
            vec!["kalshi", "polymarket_global", "espn", "the_odds_api"]
        );
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn fan_out_quote_ids_belong_to_their_collector() {
        let espn = source("espn", SourceFamily::DraftKings, false);
        assert!(quote_id_belongs_to("espn", espn.as_ref()));
        assert!(quote_id_belongs_to("espn:draftkings", espn.as_ref()));
        assert!(!quote_id_belongs_to("espnbet", espn.as_ref()));
        assert!(!quote_id_belongs_to("kalshi", espn.as_ref()));
    }
}
