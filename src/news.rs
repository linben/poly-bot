use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use serde::Deserialize;
use tracing::{info, warn};

use crate::{
    Error, Result,
    domain::{NewsCitation, NewsEvidence, Opportunity, RecommendationClass},
    storage::Store,
};

/// Turns a candidate into cited news evidence. Reviewers may preserve or
/// downgrade a candidate; they never create edge.
#[async_trait]
pub trait NewsReviewer: Send + Sync {
    fn name(&self) -> &'static str;
    async fn review(&self, opportunity: &Opportunity) -> Result<NewsEvidence>;
}

pub type SharedReviewer = Arc<dyn NewsReviewer>;

/// Select the reviewer from `NEWS_REVIEWER`:
/// - `keyword` (default when `BRAVE_SEARCH_API_KEY` is set): Brave Search plus
///   a deterministic risk-term classifier; no model, runs anywhere.
/// - `bedrock`: Brave Search summarized by Bedrock (needs the `aws` feature).
/// - `none` (default without a Brave key): no evidence is produced, so
///   candidates stay on the watchlist.
/// - `off`: records `unchanged` without searching. Removes the news veto;
///   only for operators who review candidates by hand.
pub async fn reviewer_from_env(timeout: Duration) -> Result<Option<SharedReviewer>> {
    let brave_key = std::env::var("BRAVE_SEARCH_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let requested = std::env::var("NEWS_REVIEWER")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            if brave_key.is_some() {
                "keyword".into()
            } else {
                "none".into()
            }
        });
    match requested.as_str() {
        "none" => Ok(None),
        "off" => Ok(Some(Arc::new(DisabledReviewer))),
        "keyword" => {
            let key = brave_key.ok_or_else(|| {
                Error::Config("NEWS_REVIEWER=keyword requires BRAVE_SEARCH_API_KEY".into())
            })?;
            Ok(Some(Arc::new(KeywordReviewer::new(
                BraveSearchClient::new(key, timeout)?,
            ))))
        }
        #[cfg(feature = "aws")]
        "bedrock" => Ok(Some(Arc::new(BedrockNewsEnricher::from_env().await?))),
        #[cfg(not(feature = "aws"))]
        "bedrock" => Err(Error::Config(
            "NEWS_REVIEWER=bedrock requires a build with the aws feature".into(),
        )),
        other => Err(Error::Config(format!(
            "NEWS_REVIEWER: unknown reviewer {other}"
        ))),
    }
}

/// Drains the store's news queue, skipping candidates with evidence newer than
/// `refresh`, and writes one evidence record per candidate. Returns the number
/// of candidates reviewed.
pub async fn process_news_queue(
    store: &dyn Store,
    reviewer: &dyn NewsReviewer,
    refresh: Duration,
) -> Result<usize> {
    let queued = store.take_news_queue().await?;
    if queued.is_empty() {
        return Ok(0);
    }
    let refresh = chrono::Duration::from_std(refresh)
        .map_err(|error| Error::Config(format!("news refresh: {error}")))?;
    let mut seen = std::collections::HashSet::new();
    let mut reviewed = 0;
    for opportunity in queued {
        if opportunity.class == RecommendationClass::Rejected || !seen.insert(opportunity.id) {
            continue;
        }
        let cached = store
            .news_for(opportunity.id)
            .await?
            .is_some_and(|evidence| evidence.generated_at >= Utc::now() - refresh);
        if cached {
            continue;
        }
        match reviewer.review(&opportunity).await {
            Ok(evidence) => {
                info!(
                    opportunity = %opportunity.id,
                    participant = %opportunity.participant,
                    effect = %evidence.confidence_effect,
                    reviewer = reviewer.name(),
                    "news evidence recorded"
                );
                store.save_news(&evidence).await?;
                reviewed += 1;
            }
            Err(error) => {
                warn!(opportunity = %opportunity.id, %error, "news review failed");
            }
        }
    }
    Ok(reviewed)
}

/// Records `unchanged` without evidence. Explicit opt-out of the news veto.
pub struct DisabledReviewer;

#[async_trait]
impl NewsReviewer for DisabledReviewer {
    fn name(&self) -> &'static str {
        "off"
    }

    async fn review(&self, opportunity: &Opportunity) -> Result<NewsEvidence> {
        Ok(NewsEvidence {
            opportunity_id: opportunity.id,
            generated_at: Utc::now(),
            summary: "News review disabled by configuration (NEWS_REVIEWER=off).".into(),
            confidence_effect: "unchanged".into(),
            manual_review: false,
            citations: Vec::new(),
        })
    }
}

/// Brave Search plus a deterministic classifier. A citation counts against the
/// candidate only when it names the participant and carries a risk term, so
/// generic injury coverage of other teams does not downgrade every candidate.
pub struct KeywordReviewer {
    search: BraveSearchClient,
}

impl KeywordReviewer {
    pub fn new(search: BraveSearchClient) -> Self {
        Self { search }
    }
}

#[async_trait]
impl NewsReviewer for KeywordReviewer {
    fn name(&self) -> &'static str {
        "keyword"
    }

    async fn review(&self, opportunity: &Opportunity) -> Result<NewsEvidence> {
        let citations = self.search.search(opportunity).await?;
        Ok(classify_citations(opportunity, citations))
    }
}

const HARD_RISK_TERMS: &[&str] = &[
    "ruled out",
    "scratched",
    "suspended",
    "suspension",
    "withdraw",
    "walkover",
    "postponed",
    "cancelled",
    "canceled",
    "will not play",
    "out for the season",
    "placed on the il",
    "injured list",
];
const SOFT_RISK_TERMS: &[&str] = &[
    "questionable",
    "doubtful",
    "game-time decision",
    "injur",
    "illness",
    "rain delay",
    "weather delay",
    "lineup change",
];

pub fn classify_citations(opportunity: &Opportunity, citations: Vec<NewsCitation>) -> NewsEvidence {
    if citations.is_empty() {
        return NewsEvidence {
            opportunity_id: opportunity.id,
            generated_at: Utc::now(),
            summary: "No recent corroborating news was found.".into(),
            confidence_effect: "review".into(),
            manual_review: true,
            citations,
        };
    }
    let participant_terms = participant_terms(&opportunity.participant);
    let mut hard = Vec::new();
    let mut soft = Vec::new();
    for (index, citation) in citations.iter().enumerate() {
        let text = format!("{} {}", citation.title, citation.snippet).to_lowercase();
        if !participant_terms.iter().any(|term| text.contains(term)) {
            continue;
        }
        if let Some(term) = HARD_RISK_TERMS.iter().find(|term| text.contains(*term)) {
            hard.push(format!("[{}] {term}", index + 1));
        } else if let Some(term) = SOFT_RISK_TERMS.iter().find(|term| text.contains(*term)) {
            soft.push(format!("[{}] {term}", index + 1));
        }
    }
    let (effect, manual_review, summary) = if !hard.is_empty() {
        (
            "review",
            true,
            format!(
                "Coverage naming {} carries hard risk terms: {}.",
                opportunity.participant,
                hard.join(", ")
            ),
        )
    } else if !soft.is_empty() {
        (
            "lower",
            false,
            format!(
                "Coverage naming {} carries soft risk terms: {}.",
                opportunity.participant,
                soft.join(", ")
            ),
        )
    } else {
        (
            "unchanged",
            false,
            format!(
                "{} recent results; none naming {} carry injury, suspension, withdrawal, weather, or schedule risk terms.",
                citations.len(),
                opportunity.participant
            ),
        )
    };
    NewsEvidence {
        opportunity_id: opportunity.id,
        generated_at: Utc::now(),
        summary,
        confidence_effect: effect.into(),
        manual_review,
        citations,
    }
}

/// Lower-cased full name plus each token of at least four characters, so
/// "Ravens" or "Sabalenka" alone is enough to attribute a headline.
fn participant_terms(participant: &str) -> Vec<String> {
    let full = participant.to_lowercase();
    let mut terms = vec![full.clone()];
    terms.extend(
        full.split(|character: char| !character.is_alphanumeric())
            .filter(|token| token.len() >= 4)
            .map(str::to_string),
    );
    terms
}

pub struct BraveSearchClient {
    api_key: String,
    client: reqwest::Client,
}

impl BraveSearchClient {
    pub fn new(api_key: String, timeout: Duration) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            "X-Subscription-Token",
            HeaderValue::from_str(&api_key)
                .map_err(|error| Error::Config(format!("BRAVE_SEARCH_API_KEY: {error}")))?,
        );
        Ok(Self {
            api_key,
            client: reqwest::Client::builder()
                .timeout(timeout)
                .default_headers(headers)
                .user_agent("polybot-news-research/0.1")
                .build()?,
        })
    }

    pub async fn search(&self, opportunity: &Opportunity) -> Result<Vec<NewsCitation>> {
        if self.api_key.is_empty() {
            return Ok(Vec::new());
        }
        let query = format!(
            "{} {} {} injury lineup suspension withdrawal weather schedule latest",
            opportunity.participant, opportunity.market_slug, opportunity.sport
        );
        let payload: BraveResponse = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&[
                ("q", query.as_str()),
                ("count", "8"),
                ("freshness", "pd"),
                ("safesearch", "moderate"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(payload
            .web
            .map(|web| web.results)
            .unwrap_or_default()
            .into_iter()
            .map(|result| NewsCitation {
                title: result.title,
                url: result.url,
                published_at: result
                    .page_age
                    .as_deref()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.with_timezone(&Utc)),
                snippet: result.description.unwrap_or_default(),
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    description: Option<String>,
    page_age: Option<String>,
}

#[cfg(feature = "aws")]
pub struct BedrockNewsEnricher {
    model_id: String,
    client: aws_sdk_bedrockruntime::Client,
    search: BraveSearchClient,
}

#[cfg(feature = "aws")]
impl BedrockNewsEnricher {
    pub async fn from_env() -> Result<Self> {
        let model_id = std::env::var("BEDROCK_MODEL_ID")
            .map_err(|_| Error::Config("BEDROCK_MODEL_ID is required".into()))?;
        let config = aws_config::load_from_env().await;
        let api_key = match std::env::var("BRAVE_SEARCH_API_KEY") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                let secret_id = std::env::var("APP_SECRET_ID").map_err(|_| {
                    Error::Config("BRAVE_SEARCH_API_KEY or APP_SECRET_ID is required".into())
                })?;
                let response = aws_sdk_secretsmanager::Client::new(&config)
                    .get_secret_value()
                    .secret_id(secret_id)
                    .send()
                    .await
                    .map_err(|error| Error::Config(format!("read application secret: {error}")))?;
                let secret: serde_json::Value =
                    serde_json::from_str(response.secret_string().ok_or_else(|| {
                        Error::Config("application secret has no string value".into())
                    })?)?;
                secret
                    .get("brave_search_api_key")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        Error::Config("application secret has no brave_search_api_key".into())
                    })?
            }
        };
        Ok(Self {
            model_id,
            client: aws_sdk_bedrockruntime::Client::new(&config),
            search: BraveSearchClient::new(api_key, Duration::from_secs(15))?,
        })
    }

    pub async fn enrich(&self, opportunity: &Opportunity) -> Result<NewsEvidence> {
        use aws_sdk_bedrockruntime::primitives::Blob;

        let citations = self.search.search(opportunity).await?;
        if citations.is_empty() {
            return Ok(NewsEvidence {
                opportunity_id: opportunity.id,
                generated_at: Utc::now(),
                summary: "No recent corroborating news was found.".into(),
                confidence_effect: "review".into(),
                manual_review: true,
                citations,
            });
        }

        let evidence = citations
            .iter()
            .enumerate()
            .map(|(index, citation)| {
                format!(
                    "[{}] {} | {} | {}",
                    index + 1,
                    citation.title,
                    citation.url,
                    citation.snippet
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "You review news for a prediction-market research tool. Only use the supplied \
             evidence. Do not estimate probabilities or recommend a trade. Identify injuries, \
             lineups, suspensions, withdrawals, weather, or schedule changes affecting {}. \
             Return strict JSON with keys summary (string), confidence_effect \
             (one of unchanged, lower, reject, review), manual_review (boolean). Include [n] \
             citation markers in the summary.\n\nEvidence:\n{}",
            opportunity.participant, evidence
        );
        let request = serde_json::json!({
            "anthropic_version": "bedrock-2023-05-31",
            "max_tokens": 500,
            "temperature": 0,
            "messages": [{"role": "user", "content": prompt}]
        });
        let response = self
            .client
            .invoke_model()
            .model_id(&self.model_id)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(serde_json::to_vec(&request)?))
            .send()
            .await
            .map_err(|error| Error::InvalidData(format!("Bedrock: {error}")))?;
        let payload: serde_json::Value = serde_json::from_slice(response.body().as_ref())?;
        let text = payload
            .pointer("/content/0/text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::InvalidData("Bedrock response has no content text".into()))?;
        let normalized = text
            .trim()
            .trim_start_matches("```json")
            .trim_end_matches("```")
            .trim();
        let result: ModelNews = serde_json::from_str(normalized)?;
        if !["unchanged", "lower", "reject", "review"].contains(&result.confidence_effect.as_str())
        {
            return Err(Error::InvalidData(
                "Bedrock returned an invalid confidence effect".into(),
            ));
        }
        Ok(NewsEvidence {
            opportunity_id: opportunity.id,
            generated_at: Utc::now(),
            summary: result.summary,
            confidence_effect: result.confidence_effect,
            manual_review: result.manual_review,
            citations,
        })
    }
}

#[cfg(feature = "aws")]
#[async_trait]
impl NewsReviewer for BedrockNewsEnricher {
    fn name(&self) -> &'static str {
        "bedrock"
    }

    async fn review(&self, opportunity: &Opportunity) -> Result<NewsEvidence> {
        self.enrich(opportunity).await
    }
}

#[cfg(feature = "aws")]
#[derive(Deserialize)]
struct ModelNews {
    summary: String,
    confidence_effect: String,
    manual_review: bool,
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use uuid::Uuid;

    use super::*;
    use crate::domain::{OutcomeSide, Sport};

    fn opportunity(participant: &str) -> Opportunity {
        Opportunity {
            id: Uuid::new_v4(),
            generated_at: Utc::now(),
            class: RecommendationClass::Watchlist,
            sport: Sport::Nfl,
            event_id: "event".into(),
            market_id: "market".into(),
            market_slug: "market".into(),
            participant: participant.into(),
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
            source_count: 0,
            family_count: 0,
            source_ids: Vec::new(),
            book_time: Utc::now(),
            reasons: Vec::new(),
        }
    }

    fn citation(title: &str, snippet: &str) -> NewsCitation {
        NewsCitation {
            title: title.into(),
            url: "https://example.com".into(),
            published_at: None,
            snippet: snippet.into(),
        }
    }

    #[test]
    fn no_citations_forces_manual_review() {
        let evidence = classify_citations(&opportunity("Baltimore Ravens"), Vec::new());
        assert_eq!(evidence.confidence_effect, "review");
        assert!(evidence.manual_review);
    }

    #[test]
    fn risk_terms_only_count_when_the_participant_is_named() {
        let evidence = classify_citations(
            &opportunity("Baltimore Ravens"),
            vec![citation(
                "Colts QB ruled out for Sunday",
                "Indianapolis will start its backup.",
            )],
        );
        assert_eq!(evidence.confidence_effect, "unchanged");

        let evidence = classify_citations(
            &opportunity("Baltimore Ravens"),
            vec![citation(
                "Ravens star ruled out",
                "Baltimore loses its left tackle for the opener.",
            )],
        );
        assert_eq!(evidence.confidence_effect, "review");
        assert!(evidence.manual_review);

        let evidence = classify_citations(
            &opportunity("Baltimore Ravens"),
            vec![citation(
                "Ravens receiver questionable",
                "Listed as questionable after limited practice.",
            )],
        );
        assert_eq!(evidence.confidence_effect, "lower");
        assert!(!evidence.manual_review);
    }

    #[tokio::test]
    async fn queue_drain_reviews_once_and_honors_refresh_window() {
        use crate::storage::{LocalStore, Store};
        let directory = std::env::temp_dir().join(format!("polybot-news-{}", Uuid::new_v4()));
        let store = LocalStore::new(&directory).unwrap();
        let candidate = opportunity("Baltimore Ravens");
        let mut rejected = opportunity("Carolina Panthers");
        rejected.class = RecommendationClass::Rejected;
        store
            .enqueue_news(&[candidate.clone(), candidate.clone(), rejected])
            .await
            .unwrap();

        let reviewed = process_news_queue(&store, &DisabledReviewer, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(reviewed, 1);
        let evidence = store.news_for(candidate.id).await.unwrap().unwrap();
        assert_eq!(evidence.confidence_effect, "unchanged");
        assert!(store.take_news_queue().await.unwrap().is_empty());

        // Re-queued within the refresh window: cached evidence is reused.
        store
            .enqueue_news(std::slice::from_ref(&candidate))
            .await
            .unwrap();
        let reviewed = process_news_queue(&store, &DisabledReviewer, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(reviewed, 0);
        std::fs::remove_dir_all(directory).ok();
    }
}
