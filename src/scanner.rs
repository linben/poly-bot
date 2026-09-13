use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use futures::{StreamExt, stream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    Result,
    config::Settings,
    consensus::build_consensus,
    domain::{
        ConsensusPrice, MarketBook, Opportunity, PaperPortfolio, RecommendationClass, ScanCapture,
        ScanSnapshot, SourceFamily, SourceHealth, SourceQuote, Sport, UsMoneylineMarket,
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
            .flat_map(|source| source.families())
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
                "no reference book configured; results cannot exceed watchlist (enable Pinnacle, or set ENABLE_THE_ODDS_API=true and THE_ODDS_API_KEY)"
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
        )?
        .with_rate_limit(settings.polymarket_requests_per_second);
        Ok(Self::new(settings, polymarket, sources, store))
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn polymarket(&self) -> &PolymarketUsClient {
        &self.polymarket
    }

    pub fn source_ids(&self) -> Vec<String> {
        self.sources
            .iter()
            .map(|source| source.id().to_string())
            .collect()
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
            .evaluate_markets(&markets, &quotes, &portfolio, None, Utc::now())
            .await;
        let candidate_sports = preliminary
            .opportunities
            .iter()
            .filter(|item| item.class != RecommendationClass::Rejected)
            .map(|item| item.sport)
            .collect::<HashSet<_>>();

        let (final_quotes, evaluation, mut all_health) = if candidate_sports.is_empty() {
            (quotes, preliminary, source_health)
        } else {
            let sports = candidate_sports.into_iter().collect::<Vec<_>>();
            let confirmation_sources = self.confirmation_sources(&preliminary.opportunities);
            let (confirmation_quotes, confirmation_health) =
                self.collect_sources(&confirmation_sources, &sports).await;
            let merged_quotes =
                merge_confirmation(quotes, confirmation_quotes, &confirmation_sources, &sports);
            let confirmed = self
                .evaluate_markets(
                    &markets,
                    &merged_quotes,
                    &portfolio,
                    Some(confirmation_sources.as_slice()),
                    Utc::now(),
                )
                .await;
            let mut health = source_health;
            health.extend(confirmation_health);
            (merged_quotes, confirmed, health)
        };
        let Evaluation {
            evaluated_at,
            mut opportunities,
            books,
        } = evaluation;
        opportunities.sort_by_key(|opportunity| Reverse(opportunity.net_edge));
        deduplicate_health(&mut all_health);

        let snapshot = ScanSnapshot {
            scan_id,
            started_at,
            evaluated_at,
            completed_at: Utc::now(),
            market_count: markets.len(),
            quote_count: final_quotes.len(),
            opportunities,
            source_health: all_health,
        };
        let capture = ScanCapture {
            snapshot,
            markets: Arc::unwrap_or_clone(markets),
            books,
            quotes: final_quotes,
            portfolio,
        };
        self.store.save_scan(&capture).await?;
        let snapshot = capture.snapshot;
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

    /// `refetched` is `Some` on the confirmation pass: quotes from those
    /// sources were just collected again and must be within
    /// `confirmation_max_age`. Every other family keeps its preliminary
    /// `fetched_at` and is judged against the preliminary window; applying the
    /// tight window to them dropped whole families whenever the first pass
    /// took longer than the window.
    async fn evaluate_markets(
        &self,
        markets: &[UsMoneylineMarket],
        quotes: &[SourceQuote],
        portfolio: &PaperPortfolio,
        refetched: Option<&[SharedSource]>,
        now: DateTime<Utc>,
    ) -> Evaluation {
        let quotes = match refetched {
            Some(refetched) => apply_confirmation_window(
                quotes,
                refetched,
                self.settings.confirmation_max_age,
                now,
            ),
            None => quotes.iter().collect(),
        };
        let evaluable = consensus_markets(markets, &quotes, &self.settings, now);
        let slugs = evaluable
            .iter()
            .map(|(market, _)| market.market_slug.as_str())
            .collect::<Vec<_>>();
        let fetched = self.polymarket.fetch_books(&slugs).await;

        let mut books = Vec::with_capacity(fetched.len());
        for ((market, _), book) in evaluable.iter().zip(fetched) {
            match book {
                Ok(book) => books.push(book),
                Err(error) => warn!(market = market.market_slug, %error, "book fetch failed"),
            }
        }
        let opportunities = evaluate_books(&self.settings, &evaluable, &books, portfolio, now);
        Evaluation {
            evaluated_at: now,
            opportunities,
            books,
        }
    }
}

/// One evaluation pass: the clock it used, its rows, and the books it
/// fetched (only for markets that had a consensus).
struct Evaluation {
    evaluated_at: DateTime<Utc>,
    opportunities: Vec<Opportunity>,
    books: Vec<MarketBook>,
}

/// Every market not yet started that has a consensus from `quotes` as of
/// `now`. Shared by the live scan and the replay so both match, orient, and
/// filter quotes identically.
pub fn consensus_markets<'a>(
    markets: &'a [UsMoneylineMarket],
    quotes: &[&SourceQuote],
    settings: &Settings,
    now: DateTime<Utc>,
) -> Vec<(&'a UsMoneylineMarket, ConsensusPrice)> {
    let max_age_seconds = settings.max_quote_age.as_secs() as i64;
    let exchange_max_lead =
        chrono::Duration::from_std(settings.exchange_max_lead).unwrap_or_default();
    markets
        .iter()
        .filter(|market| market.start_time > now)
        .filter_map(|market| {
            let exclude_exchanges = market.start_time - now > exchange_max_lead;
            let matching = quotes
                .iter()
                .filter(|quote| !(exclude_exchanges && is_exchange_family(&quote.family)))
                .filter_map(|quote| {
                    match_quote(market, quote).map(|orientation| orient_quote(quote, orientation))
                })
                .collect::<Vec<_>>();
            match build_consensus(&matching, max_age_seconds, now) {
                Ok(consensus) => Some((market, consensus)),
                Err(error) if matching.is_empty() => {
                    debug!(market = %market.market_slug, %error, "consensus unavailable");
                    None
                }
                Err(error) => {
                    warn!(market = %market.market_slug, %error, "consensus unavailable");
                    None
                }
            }
        })
        .collect()
}

/// Classify every market in `evaluable` whose book is present in `books`
/// (matched by slug). Markets without a book are skipped: the live scan
/// failed to fetch it, or the replay never stored one.
pub fn evaluate_books(
    settings: &Settings,
    evaluable: &[(&UsMoneylineMarket, ConsensusPrice)],
    books: &[MarketBook],
    portfolio: &PaperPortfolio,
    now: DateTime<Utc>,
) -> Vec<Opportunity> {
    let engine = OpportunityEngine::new(settings.clone());
    let by_slug = books
        .iter()
        .map(|book| (book.market_slug.as_str(), book))
        .collect::<HashMap<_, _>>();
    evaluable
        .iter()
        .filter_map(|(market, consensus)| {
            by_slug
                .get(market.market_slug.as_str())
                .map(|book| engine.evaluate(market, book, consensus, portfolio, now))
        })
        .flatten()
        .collect()
}

/// Exchange prices are calibrated close to the start; further out they are
/// excluded from the consensus (`exchange_max_lead`). Sportsbooks are not.
fn is_exchange_family(family: &SourceFamily) -> bool {
    matches!(
        family,
        SourceFamily::Kalshi | SourceFamily::PolymarketGlobal
    )
}

/// Keep every quote except those from a refetched source whose `fetched_at`
/// is older than `max_age`: a refetch that failed or fell back to a stale
/// cache must not confirm a candidate on the preliminary price.
fn apply_confirmation_window<'a>(
    quotes: &'a [SourceQuote],
    refetched: &[SharedSource],
    max_age: Duration,
    now: DateTime<Utc>,
) -> Vec<&'a SourceQuote> {
    let cutoff = now - chrono::Duration::from_std(max_age).unwrap_or_default();
    quotes
        .iter()
        .filter(|quote| {
            quote.fetched_at >= cutoff
                || !refetched
                    .iter()
                    .any(|source| quote_belongs_to(quote, source.as_ref()))
        })
        .collect()
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

/// Replace a refetched source's quotes for the refetched sports with the
/// fresh ones. Its quotes for other sports were not collected again and must
/// survive; dropping them wholesale removed that family from every
/// non-candidate sport the moment any candidate appeared.
fn merge_confirmation(
    mut quotes: Vec<SourceQuote>,
    confirmation_quotes: Vec<SourceQuote>,
    refetched: &[SharedSource],
    sports: &[Sport],
) -> Vec<SourceQuote> {
    quotes.retain(|quote| {
        !sports.contains(&quote.sport)
            || !refetched
                .iter()
                .any(|source| quote_belongs_to(quote, source.as_ref()))
    });
    quotes.extend(confirmation_quotes);
    quotes
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
            maker_net_edge: None,
            raw_edge: Decimal::ZERO,
            net_edge: Decimal::ZERO,
            quantity: Decimal::ZERO,
            maximum_loss: Decimal::ZERO,
            estimated_fee: Decimal::ZERO,
            family_count: source_ids.len(),
            source_ids: source_ids.iter().map(|id| id.to_string()).collect(),
            start_time: Utc::now() + chrono::Duration::hours(2),
            book_time: Utc::now(),
            reasons: Vec::new(),
        }
    }

    fn quote(source_id: &str, sport: Sport, odds_a: i64) -> SourceQuote {
        SourceQuote {
            source_id: source_id.into(),
            family: SourceFamily::Kalshi,
            sport,
            event_id: "event".into(),
            participant_a: "A".into(),
            participant_b: "B".into(),
            participant_a_provider_ids: Default::default(),
            participant_b_provider_ids: Default::default(),
            start_time: Utc::now(),
            start_time_tolerance_minutes: 15,
            decimal_odds_a: Decimal::new(odds_a, 2),
            decimal_odds_b: Decimal::new(200, 2),
            decimal_odds_neutral: None,
            source_timestamp: Utc::now(),
            fetched_at: Utc::now(),
            parser_version: "test".into(),
            validation_only: false,
        }
    }

    #[test]
    fn confirmation_replaces_only_the_refetched_sports() {
        let kalshi = source("kalshi", SourceFamily::Kalshi, false);
        let initial = vec![
            quote("kalshi", Sport::Nfl, 180),
            quote("kalshi", Sport::Mlb, 180),
            quote("espn:draftkings", Sport::Mlb, 180),
        ];
        let fresh = vec![quote("kalshi", Sport::Mlb, 190)];
        let merged = merge_confirmation(initial, fresh, &[kalshi], &[Sport::Mlb]);
        let mut seen = merged
            .iter()
            .map(|q| (q.source_id.as_str(), q.sport, q.decimal_odds_a))
            .collect::<Vec<_>>();
        seen.sort_by_key(|(id, sport, _)| (id.to_string(), format!("{sport:?}")));
        assert_eq!(
            seen,
            vec![
                ("espn:draftkings", Sport::Mlb, Decimal::new(180, 2)),
                ("kalshi", Sport::Mlb, Decimal::new(190, 2)),
                ("kalshi", Sport::Nfl, Decimal::new(180, 2)),
            ]
        );
    }

    #[test]
    fn confirmation_window_only_binds_refetched_sources() {
        let now = Utc::now();
        let stale = now - chrono::Duration::seconds(120);
        let kalshi = source("kalshi", SourceFamily::Kalshi, false);
        let mut refetched_stale = quote("kalshi", Sport::Mlb, 180);
        refetched_stale.fetched_at = stale;
        let mut refetched_fresh = quote("kalshi", Sport::Nfl, 180);
        refetched_fresh.fetched_at = now - chrono::Duration::seconds(30);
        let mut untouched_stale = quote("espn:draftkings", Sport::Mlb, 180);
        untouched_stale.fetched_at = stale;
        let quotes = vec![refetched_stale, refetched_fresh, untouched_stale];

        let kept = apply_confirmation_window(&quotes, &[kalshi], Duration::from_secs(90), now);
        let mut seen = kept
            .iter()
            .map(|q| (q.source_id.as_str(), q.sport))
            .collect::<Vec<_>>();
        seen.sort_by_key(|(id, sport)| (id.to_string(), format!("{sport:?}")));
        assert_eq!(
            seen,
            vec![("espn:draftkings", Sport::Mlb), ("kalshi", Sport::Nfl)]
        );
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
