use std::{env, str::FromStr, time::Duration};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Where state lives and which event-driven services run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    /// Single host: file-backed store, in-process news loop and terminal UI.
    Local,
    /// AWS: S3/DynamoDB/SQS store and the Lambda news worker; the terminal UI
    /// attaches from the operator's machine.
    Cloud,
}

impl FromStr for RunMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "cloud" | "aws" => Ok(Self::Cloud),
            other => Err(format!("expected local or cloud, got {other}")),
        }
    }
}

/// `Deserialize` reads the JSON a scan was archived with (`#[serde(default)]`
/// fills fields added since), so a replay can audit under the same gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub run_mode: RunMode,
    pub polymarket_base_url: String,
    pub scan_interval: Duration,
    pub scan_lease_ttl: Duration,
    pub request_timeout: Duration,
    pub source_concurrency: usize,
    /// Parallel Polymarket US book requests. Measured: 32 concurrent requests
    /// return in ~20 ms each with no throttling; 8 leaves headroom.
    pub book_concurrency: usize,
    /// How long a market-discovery result is reused. Events change slowly;
    /// books are refetched every scan.
    pub discovery_refresh: Duration,
    /// Local store: delete scan snapshots older than this many days.
    pub retention_days: u32,
    pub minimum_configured_sources: usize,
    pub minimum_source_families: usize,
    pub watchlist_source_families: usize,
    pub max_quote_age: Duration,
    pub confirmation_max_age: Duration,
    pub book_max_age: Duration,
    pub minimum_raw_edge: Decimal,
    pub minimum_net_edge: Decimal,
    pub minimum_price: Decimal,
    pub maximum_price: Decimal,
    pub bankroll: Decimal,
    /// A side whose quarter-Kelly size or depth-limited maximum loss is below
    /// this fraction of bankroll is rejected as too small to matter. Zero
    /// disables the floor (exploration).
    pub minimum_position_fraction: Decimal,
    pub maximum_position_fraction: Decimal,
    pub maximum_total_exposure: Decimal,
    /// Cap on paper exposure across every market of one event, so both sides
    /// or several markets of the same game cannot absorb the whole budget.
    pub maximum_event_exposure: Decimal,
    /// Actionable requires a reference sportsbook (Pinnacle-class) in the
    /// consensus. Only the confirmation tier supplies one; switching this off
    /// lets a full free-source quorum reach actionable.
    pub require_reference_book: bool,
    pub kelly_fraction: Decimal,
    /// Subtracted from the backed side's fair probability before net edge and
    /// Kelly. Bookmaker consensus over-states the probability of the outcome
    /// you back by a roughly constant intercept (Kaunitz et al. 2017 measured
    /// 0.034-0.037 on football closing odds); this is the prior until paper
    /// settlement history can fit it.
    pub consensus_bias: Decimal,
    /// A market starting sooner than this is not actionable: prices near start
    /// move on lineups faster than a five-minute scan follows.
    pub minimum_lead: Duration,
    /// Exchange families (Kalshi, Polymarket global) are excluded from the
    /// consensus for games starting further out than this; exchange prices
    /// are calibrated 30-240 min before close and drift beyond that.
    pub exchange_max_lead: Duration,
    /// Maker rebate coefficient from the venue fee schedule (Theta =
    /// -0.0125). Used only to report `maker_net_edge`.
    pub maker_rebate_coefficient: Decimal,
    /// Public gateway limit is 20 requests/second/IP; stay under it.
    pub polymarket_requests_per_second: u32,
    /// How often open paper positions are checked for a closing line and a
    /// settlement price.
    pub settlement_poll: Duration,
    pub source_config_path: String,
    /// Postgres history archive (`DATABASE_URL`). Optional: unset means the
    /// file or AWS store alone; set (with the `postgres` feature) records
    /// every scan's inputs, grades every market seen, and enables `backtest`.
    #[serde(skip)]
    pub database_url: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            run_mode: RunMode::Local,
            polymarket_base_url: "https://gateway.polymarket.us".into(),
            scan_interval: Duration::from_secs(300),
            scan_lease_ttl: Duration::from_secs(900),
            request_timeout: Duration::from_secs(20),
            source_concurrency: 4,
            book_concurrency: 8,
            discovery_refresh: Duration::from_secs(900),
            retention_days: 14,
            minimum_configured_sources: 3,
            minimum_source_families: 5,
            watchlist_source_families: 3,
            max_quote_age: Duration::from_secs(360),
            confirmation_max_age: Duration::from_secs(90),
            book_max_age: Duration::from_secs(15),
            minimum_raw_edge: Decimal::new(5, 2),
            minimum_net_edge: Decimal::new(3, 2),
            minimum_price: Decimal::new(35, 2),
            maximum_price: Decimal::new(65, 2),
            bankroll: Decimal::ONE_HUNDRED,
            minimum_position_fraction: Decimal::new(1, 2),
            maximum_position_fraction: Decimal::new(5, 2),
            require_reference_book: true,
            maximum_total_exposure: Decimal::new(5, 0),
            maximum_event_exposure: Decimal::new(25, 1),
            kelly_fraction: Decimal::new(25, 2),
            consensus_bias: Decimal::new(2, 2),
            minimum_lead: Duration::from_secs(15 * 60),
            exchange_max_lead: Duration::from_secs(4 * 3600),
            maker_rebate_coefficient: Decimal::new(125, 4),
            polymarket_requests_per_second: 18,
            settlement_poll: Duration::from_secs(600),
            source_config_path: "config/sources.json".into(),
            database_url: None,
        }
    }
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let mut settings = Self::default();
        set_string("POLYMARKET_BASE_URL", &mut settings.polymarket_base_url);
        set_string("SOURCE_CONFIG_PATH", &mut settings.source_config_path);
        settings.database_url = env::var("DATABASE_URL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        settings.run_mode = parse_env("RUN_MODE", settings.run_mode)?;
        settings.source_concurrency = parse_env("SOURCE_CONCURRENCY", settings.source_concurrency)?;
        settings.book_concurrency = parse_env("POLYMARKET_CONCURRENCY", settings.book_concurrency)?;
        settings.discovery_refresh = Duration::from_secs(parse_env(
            "DISCOVERY_REFRESH_SECONDS",
            settings.discovery_refresh.as_secs(),
        )?);
        settings.retention_days = parse_env("LOCAL_RETENTION_DAYS", settings.retention_days)?;
        settings.minimum_configured_sources = parse_env(
            "MINIMUM_CONFIGURED_SOURCES",
            settings.minimum_configured_sources,
        )?;
        settings.scan_interval = Duration::from_secs(parse_env(
            "SCAN_INTERVAL_SECONDS",
            settings.scan_interval.as_secs(),
        )?);
        settings.scan_lease_ttl = Duration::from_secs(parse_env(
            "SCAN_LEASE_TTL_SECONDS",
            settings.scan_lease_ttl.as_secs(),
        )?);
        settings.minimum_source_families =
            parse_env("MINIMUM_SOURCE_FAMILIES", settings.minimum_source_families)?;
        settings.watchlist_source_families = parse_env(
            "WATCHLIST_SOURCE_FAMILIES",
            settings.watchlist_source_families,
        )?;
        settings.minimum_raw_edge = parse_env("MINIMUM_RAW_EDGE", settings.minimum_raw_edge)?;
        settings.minimum_net_edge = parse_env("MINIMUM_NET_EDGE", settings.minimum_net_edge)?;
        settings.minimum_price = parse_env("MINIMUM_PRICE", settings.minimum_price)?;
        settings.maximum_price = parse_env("MAXIMUM_PRICE", settings.maximum_price)?;
        settings.require_reference_book =
            parse_env("REQUIRE_REFERENCE_BOOK", settings.require_reference_book)?;
        settings.bankroll = parse_env("PAPER_BANKROLL", settings.bankroll)?;
        settings.minimum_position_fraction = parse_env(
            "MINIMUM_POSITION_FRACTION",
            settings.minimum_position_fraction,
        )?;
        settings.maximum_position_fraction = parse_env(
            "MAXIMUM_POSITION_FRACTION",
            settings.maximum_position_fraction,
        )?;
        settings.maximum_total_exposure =
            parse_env("MAXIMUM_TOTAL_EXPOSURE", settings.maximum_total_exposure)?;
        settings.maximum_event_exposure =
            parse_env("MAXIMUM_EVENT_EXPOSURE", settings.maximum_event_exposure)?;
        settings.kelly_fraction = parse_env("KELLY_FRACTION", settings.kelly_fraction)?;
        settings.consensus_bias = parse_env("CONSENSUS_BIAS", settings.consensus_bias)?;
        settings.max_quote_age = Duration::from_secs(parse_env(
            "MAX_QUOTE_AGE_SECONDS",
            settings.max_quote_age.as_secs(),
        )?);
        settings.confirmation_max_age = Duration::from_secs(parse_env(
            "CONFIRMATION_MAX_AGE_SECONDS",
            settings.confirmation_max_age.as_secs(),
        )?);
        settings.book_max_age = Duration::from_secs(parse_env(
            "BOOK_MAX_AGE_SECONDS",
            settings.book_max_age.as_secs(),
        )?);
        settings.minimum_lead = Duration::from_secs(
            parse_env("MINIMUM_LEAD_MINUTES", settings.minimum_lead.as_secs() / 60)? * 60,
        );
        settings.exchange_max_lead = Duration::from_secs(
            parse_env(
                "EXCHANGE_MAX_LEAD_HOURS",
                settings.exchange_max_lead.as_secs() / 3600,
            )? * 3600,
        );
        settings.maker_rebate_coefficient = parse_env(
            "MAKER_REBATE_COEFFICIENT",
            settings.maker_rebate_coefficient,
        )?;
        settings.polymarket_requests_per_second = parse_env(
            "POLYMARKET_REQUESTS_PER_SECOND",
            settings.polymarket_requests_per_second,
        )?;
        settings.settlement_poll = Duration::from_secs(parse_env(
            "SETTLEMENT_POLL_SECONDS",
            settings.settlement_poll.as_secs(),
        )?);
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        if self.scan_interval.is_zero()
            || self.scan_lease_ttl <= self.scan_interval
            || self.request_timeout.is_zero()
            || self.book_concurrency == 0
            || self.source_concurrency == 0
        {
            return Err(Error::Config(
                "scan timing, request timeout, and concurrency must be positive".into(),
            ));
        }
        if self.watchlist_source_families < 1
            || self.minimum_source_families < self.watchlist_source_families
            || self.minimum_configured_sources < self.watchlist_source_families
        {
            return Err(Error::Config(
                "source-family quorum must be at least 1 watchlist, actionable at least watchlist, with at least that many configured continuous families".into(),
            ));
        }
        if self.minimum_price <= Decimal::ZERO
            || self.maximum_price >= Decimal::ONE
            || self.minimum_price > self.maximum_price
        {
            return Err(Error::Config("contract price range is invalid".into()));
        }
        if self.bankroll <= Decimal::ZERO
            || self.maximum_position_fraction < Decimal::new(1, 2)
            || self.maximum_position_fraction > Decimal::new(5, 2)
            || self.minimum_position_fraction < Decimal::ZERO
            || self.minimum_position_fraction > self.maximum_position_fraction
            || self.maximum_total_exposure <= Decimal::ZERO
            || self.maximum_total_exposure > Decimal::new(5, 0)
            || self.maximum_total_exposure > self.bankroll
        {
            return Err(Error::Config("paper risk limits are invalid".into()));
        }
        if self.maximum_event_exposure <= Decimal::ZERO
            || self.maximum_event_exposure > self.maximum_total_exposure
        {
            return Err(Error::Config(
                "MAXIMUM_EVENT_EXPOSURE must be positive and at most MAXIMUM_TOTAL_EXPOSURE".into(),
            ));
        }
        if self.kelly_fraction <= Decimal::ZERO || self.kelly_fraction > Decimal::ONE {
            return Err(Error::Config("KELLY_FRACTION must be in (0, 1]".into()));
        }
        if self.consensus_bias < Decimal::ZERO || self.consensus_bias >= Decimal::new(5, 1) {
            return Err(Error::Config("CONSENSUS_BIAS must be in [0, 0.5)".into()));
        }
        if self.maker_rebate_coefficient < Decimal::ZERO {
            return Err(Error::Config(
                "MAKER_REBATE_COEFFICIENT must be non-negative".into(),
            ));
        }
        if self.max_quote_age.is_zero()
            || self.confirmation_max_age.is_zero()
            || self.confirmation_max_age > self.max_quote_age
            || self.book_max_age.is_zero()
        {
            return Err(Error::Config(
                "quote and book age windows must be positive, confirmation at most the preliminary window".into(),
            ));
        }
        if self.polymarket_requests_per_second == 0 || self.settlement_poll.is_zero() {
            return Err(Error::Config(
                "POLYMARKET_REQUESTS_PER_SECOND and SETTLEMENT_POLL_SECONDS must be positive"
                    .into(),
            ));
        }
        Ok(())
    }
}

fn set_string(name: &str, target: &mut String) {
    if let Ok(value) = env::var(name)
        && !value.trim().is_empty()
    {
        *target = value;
    }
}

fn parse_env<T>(name: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| Error::Config(format!("{name}: {error}"))),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_enforce_requested_risk_limits() {
        let settings = Settings::default();
        settings.validate().unwrap();
        assert_eq!(settings.bankroll, Decimal::ONE_HUNDRED);
        assert_eq!(settings.maximum_position_fraction, Decimal::new(5, 2));
        assert_eq!(settings.maximum_total_exposure, Decimal::new(5, 0));
    }

    #[test]
    fn position_risk_above_five_percent_is_invalid() {
        let settings = Settings {
            maximum_position_fraction: Decimal::new(6, 2),
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }
}
