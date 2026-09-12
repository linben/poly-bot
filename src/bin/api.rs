//! Dashboard API. Local mode serves axum on `LISTEN_ADDRESS`; the `aws` build
//! is the Lambda handler behind API Gateway.

#[cfg(not(feature = "aws"))]
#[tokio::main]
async fn main() -> polybot::Result<()> {
    use polybot::{config::Settings, dashboard, storage::store_for};

    polybot::init_tracing();
    let settings = Settings::from_env()?;
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let store = store_for(settings.run_mode, &data_dir).await?;
    dashboard::serve(store, dashboard::listen_address()?).await
}

#[cfg(feature = "aws")]
#[tokio::main]
async fn main() -> std::result::Result<(), lambda_http::Error> {
    use std::sync::Arc;

    use lambda_http::{Body, Request, Response, service_fn};
    use polybot::{
        dashboard::{INDEX_HTML, latest_research},
        storage::{Store, aws::AwsStore},
    };

    polybot::init_tracing();
    let store: Arc<dyn Store> = Arc::new(AwsStore::from_env().await?);
    lambda_http::run(service_fn(move |request: Request| {
        let store = store.clone();
        async move {
            let path = request.uri().path();
            let (status, content_type, body) = match path {
                "/" => (200, "text/html; charset=utf-8", INDEX_HTML.into()),
                "/health" => (200, "application/json", r#"{"status":"ok"}"#.into()),
                "/api/opportunities" => match latest_research(store.as_ref()).await {
                    Ok(items) => (200, "application/json", serde_json::to_string(&items)?),
                    Err(error) => (
                        500,
                        "application/json",
                        serde_json::json!({"error": error.to_string()}).to_string(),
                    ),
                },
                _ => (404, "application/json", r#"{"error":"not found"}"#.into()),
            };
            let response = Response::builder()
                .status(status)
                .header("content-type", content_type)
                .header("cache-control", "no-store")
                .body(Body::Text(body))?;
            Ok::<Response<Body>, lambda_http::Error>(response)
        }
    }))
    .await
}
