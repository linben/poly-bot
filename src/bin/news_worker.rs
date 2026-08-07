use aws_lambda_events::event::sqs::{BatchItemFailure, SqsBatchResponse, SqsEvent};
use chrono::Utc;
use lambda_runtime::{Error as LambdaError, LambdaEvent, service_fn};
use polybot::{
    domain::Opportunity,
    news::BedrockNewsEnricher,
    storage::{Store, aws::AwsStore},
};

async fn handler(
    event: LambdaEvent<SqsEvent>,
    enricher: &BedrockNewsEnricher,
    store: &AwsStore,
) -> Result<SqsBatchResponse, LambdaError> {
    let mut failures = Vec::new();
    let refresh_seconds = std::env::var("NEWS_REFRESH_SECONDS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(3600);
    for record in event.payload.records {
        let message_id = record.message_id.unwrap_or_default();
        let result = async {
            let body = record.body.ok_or("SQS record has no body")?;
            let opportunity: Opportunity = serde_json::from_str(&body)?;
            if store
                .news_for(opportunity.id)
                .await?
                .is_some_and(|evidence| {
                    evidence.generated_at >= Utc::now() - chrono::Duration::seconds(refresh_seconds)
                })
            {
                return Ok(());
            }
            let evidence = enricher.enrich(&opportunity).await?;
            store.save_news(&evidence).await?;
            Ok::<(), LambdaError>(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!(%message_id, %error, "news record failed");
            failures.push(BatchItemFailure {
                item_identifier: message_id,
            });
        };
    }
    Ok(SqsBatchResponse {
        batch_item_failures: failures,
    })
}

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    polybot::init_tracing();
    let enricher = BedrockNewsEnricher::from_env().await?;
    let store = AwsStore::from_env().await?;
    lambda_runtime::run(service_fn(|event| handler(event, &enricher, &store))).await
}
