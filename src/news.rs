use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::{
    Error, Result,
    domain::{NewsCitation, Opportunity},
};

#[cfg(feature = "aws")]
use crate::domain::NewsEvidence;

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
#[derive(Deserialize)]
struct ModelNews {
    summary: String,
    confidence_effect: String,
    manual_review: bool,
}
