use std::{env, fs, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    domain::SourceFamily,
    sources::{CanonicalJsonSource, SharedSource, TheOddsApiSource},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceTier {
    Primary,
    Fallback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpec {
    pub id: String,
    pub display_name: String,
    pub family: String,
    pub tier: SourceTier,
    pub reference: bool,
    pub homepage: String,
    pub endpoint_env: String,
}

impl SourceSpec {
    pub fn source_family(&self) -> Result<SourceFamily> {
        parse_family(&self.family)
    }
}

#[derive(Debug, Deserialize)]
struct CatalogDocument {
    sources: Vec<SourceSpec>,
}

#[derive(Debug, Clone)]
pub struct SourceCatalog {
    pub specs: Vec<SourceSpec>,
}

impl SourceCatalog {
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)
            .map_err(|error| Error::Config(format!("cannot read {path}: {error}")))?;
        let document: CatalogDocument = serde_json::from_str(&content)?;
        Ok(Self {
            specs: document.sources,
        })
    }

    pub fn configured_sources(&self, timeout: Duration) -> Result<Vec<SharedSource>> {
        let mut sources: Vec<SharedSource> = Vec::new();
        for spec in &self.specs {
            let Ok(endpoint) = env::var(&spec.endpoint_env) else {
                continue;
            };
            if endpoint.trim().is_empty() {
                continue;
            }
            sources.push(Arc::new(CanonicalJsonSource::new(
                spec.clone(),
                endpoint,
                timeout,
            )?));
        }

        let validator_enabled = env::var("ENABLE_THE_ODDS_API_VALIDATOR")
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if validator_enabled
            && let Ok(api_key) = env::var("THE_ODDS_API_KEY")
            && !api_key.trim().is_empty()
        {
            sources.push(Arc::new(TheOddsApiSource::new(api_key, timeout)?));
        }
        Ok(sources)
    }

    pub fn primary_specs(&self) -> impl Iterator<Item = &SourceSpec> {
        self.specs
            .iter()
            .filter(|spec| spec.tier == SourceTier::Primary)
    }
}

pub fn parse_family(value: &str) -> Result<SourceFamily> {
    match value {
        "pinnacle" => Ok(SourceFamily::Pinnacle),
        "circa" => Ok(SourceFamily::Circa),
        "bookmaker" => Ok(SourceFamily::Bookmaker),
        "bet_online" => Ok(SourceFamily::BetOnline),
        "bet365" => Ok(SourceFamily::Bet365),
        "draft_kings" => Ok(SourceFamily::DraftKings),
        "fan_duel" => Ok(SourceFamily::FanDuel),
        "caesars" => Ok(SourceFamily::Caesars),
        "bet_mgm" => Ok(SourceFamily::BetMgm),
        "fanatics" => Ok(SourceFamily::Fanatics),
        "kambi" => Ok(SourceFamily::Kambi),
        "hard_rock" => Ok(SourceFamily::HardRock),
        "penn" => Ok(SourceFamily::Penn),
        other => Err(Error::Config(format!("unknown source family {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_ten_primary_books_and_distinct_families() {
        let document: CatalogDocument =
            serde_json::from_str(include_str!("../../config/sources.json")).unwrap();
        let primary = document
            .sources
            .iter()
            .filter(|source| source.tier == SourceTier::Primary)
            .collect::<Vec<_>>();
        let families = primary
            .iter()
            .map(|source| source.family.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(primary.len(), 10);
        assert_eq!(families.len(), 10);
        assert_eq!(primary.iter().filter(|source| source.reference).count(), 4);
    }
}
