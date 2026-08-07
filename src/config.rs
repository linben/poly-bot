use std::{env, str::FromStr, time::Duration};

use rust_decimal::Decimal;

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct Settings {
    pub polymarket_base_url: String,
    pub scan_interval: Duration,
    pub scan_lease_ttl: Duration,
    pub request_timeout: Duration,
    pub source_concurrency: usize,
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
    pub maximum_position_fraction: Decimal,
    pub maximum_total_exposure: Decimal,
    pub kelly_fraction: Decimal,
    pub source_config_path: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            polymarket_base_url: "https://gateway.polymarket.us".into(),
            scan_interval: Duration::from_secs(300),
            scan_lease_ttl: Duration::from_secs(900),
            request_timeout: Duration::from_secs(20),
            source_concurrency: 4,
            minimum_configured_sources: 10,
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
            maximum_position_fraction: Decimal::new(5, 2),
            maximum_total_exposure: Decimal::new(5, 0),
            kelly_fraction: Decimal::new(25, 2),
            source_config_path: "config/sources.json".into(),
        }
    }
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let mut settings = Self::default();
        set_string("POLYMARKET_BASE_URL", &mut settings.polymarket_base_url);
        set_string("SOURCE_CONFIG_PATH", &mut settings.source_config_path);
        settings.source_concurrency = parse_env("SOURCE_CONCURRENCY", settings.source_concurrency)?;
        settings.minimum_configured_sources = parse_env(
            "MINIMUM_CONFIGURED_SOURCES",
            settings.minimum_configured_sources,
        )?;
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
        settings.bankroll = parse_env("PAPER_BANKROLL", settings.bankroll)?;
        settings.maximum_position_fraction = parse_env(
            "MAXIMUM_POSITION_FRACTION",
            settings.maximum_position_fraction,
        )?;
        settings.maximum_total_exposure =
            parse_env("MAXIMUM_TOTAL_EXPOSURE", settings.maximum_total_exposure)?;
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        if self.scan_interval.is_zero()
            || self.scan_lease_ttl <= self.scan_interval
            || self.request_timeout.is_zero()
        {
            return Err(Error::Config(
                "scan timing and request timeout are invalid".into(),
            ));
        }
        if self.watchlist_source_families < 3
            || self.minimum_source_families < 5
            || self.minimum_source_families < self.watchlist_source_families
            || self.minimum_configured_sources < self.minimum_source_families
        {
            return Err(Error::Config(
                "source-family quorum must be at least 3 watchlist and 5 actionable".into(),
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
            || self.maximum_total_exposure <= Decimal::ZERO
            || self.maximum_total_exposure > Decimal::new(5, 0)
            || self.maximum_total_exposure > self.bankroll
        {
            return Err(Error::Config("paper risk limits are invalid".into()));
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
