use std::time::Duration;

use crate::{
    Error, Result,
    domain::{SourceFamily, SourceHealth, SourceQuote},
    sources::{OddsSource, SourceSpec, probe_url},
};
use async_trait::async_trait;
use chrono::Utc;

pub struct CanonicalJsonSource {
    spec: SourceSpec,
    endpoint: String,
    client: reqwest::Client,
}

impl CanonicalJsonSource {
    pub fn new(spec: SourceSpec, endpoint: String, timeout: Duration) -> Result<Self> {
        Ok(Self {
            spec,
            endpoint,
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent("polybot-source-collector/0.1")
                .build()?,
        })
    }
}

fn parse_canonical(payload: serde_json::Value) -> Result<Vec<SourceQuote>> {
    let quotes = match payload {
        serde_json::Value::Array(quotes) => quotes,
        serde_json::Value::Object(mut object) => object
            .remove("quotes")
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| {
                Error::InvalidData(
                    "canonical source must be an array or an object with a quotes array".into(),
                )
            })?,
        _ => {
            return Err(Error::InvalidData(
                "canonical source must be an array or an object".into(),
            ));
        }
    };
    quotes
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[async_trait]
impl OddsSource for CanonicalJsonSource {
    fn id(&self) -> &str {
        &self.spec.id
    }

    fn family(&self) -> SourceFamily {
        self.spec
            .source_family()
            .expect("catalog family was validated")
    }

    async fn collect(&self) -> Result<Vec<SourceQuote>> {
        let payload: serde_json::Value = self
            .client
            .get(&self.endpoint)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut quotes = parse_canonical(payload)?;
        for quote in &mut quotes {
            quote.source_id = self.spec.id.clone();
            quote.family = self.family();
            quote.validation_only = false;
            quote.fetched_at = Utc::now();
        }
        Ok(quotes)
    }

    async fn probe(&self) -> SourceHealth {
        let mut health =
            probe_url(&self.client, &self.spec.id, self.family(), &self.endpoint).await;
        if health.reachable {
            match self.collect().await {
                Ok(quotes) => {
                    health.odds_found = quotes.len();
                    health.message = format!("{} normalized quotes", quotes.len());
                }
                Err(error) => {
                    health.reachable = false;
                    health.message = error.to_string();
                }
            }
        }
        health
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_fixture_uses_decimal_strings() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/canonical_quotes.json"))
                .unwrap();
        let quotes = parse_canonical(document).unwrap();
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes[0].event_id, "provider-event-1");
        assert_eq!(quotes[0].decimal_odds_a, rust_decimal::Decimal::new(19, 1));
        assert_eq!(quotes[0].sport, crate::domain::Sport::Nba);
    }
}
