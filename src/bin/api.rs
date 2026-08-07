use std::sync::Arc;

#[cfg(not(feature = "aws"))]
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{Html, IntoResponse},
    routing::get,
};
use polybot::storage::Store;
#[cfg(not(feature = "aws"))]
use polybot::{Result, domain::ResearchOpportunity, storage::LocalStore};
#[cfg(not(feature = "aws"))]
use std::net::SocketAddr;
#[cfg(not(feature = "aws"))]
use tower_http::cors::CorsLayer;

#[cfg(not(feature = "aws"))]
#[derive(Clone)]
struct AppState {
    store: Arc<dyn Store>,
}

#[cfg(not(feature = "aws"))]
async fn index() -> Html<&'static str> {
    Html(include_str!("../../web/index.html"))
}

#[cfg(not(feature = "aws"))]
async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

#[cfg(not(feature = "aws"))]
async fn config() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/javascript")],
        "window.POLYBOT_CONFIG = {};",
    )
}

#[cfg(not(feature = "aws"))]
async fn opportunities(
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<ResearchOpportunity>>, (StatusCode, String)> {
    latest_research(state.store.as_ref())
        .await
        .map(Json)
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("storage failure: {error}"),
            )
        })
}

async fn latest_research(
    store: &dyn Store,
) -> polybot::Result<Vec<polybot::domain::ResearchOpportunity>> {
    let opportunities = store.latest_opportunities().await?;
    let mut result = Vec::with_capacity(opportunities.len());
    for opportunity in opportunities {
        let news = if opportunity.class == polybot::domain::RecommendationClass::Rejected {
            None
        } else {
            store.news_for(opportunity.id).await?
        };
        result.push(polybot::domain::ResearchOpportunity::new(opportunity, news));
    }
    Ok(result)
}

#[cfg(not(feature = "aws"))]
fn app(store: Arc<dyn Store>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/config.js", get(config))
        .route("/health", get(health))
        .route("/api/opportunities", get(opportunities))
        .layer(CorsLayer::permissive())
        .with_state(AppState { store })
}

#[cfg(not(feature = "aws"))]
#[tokio::main]
async fn main() -> Result<()> {
    polybot::init_tracing();
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let store: Arc<dyn Store> = Arc::new(LocalStore::new(data_dir)?);
    let address: SocketAddr = std::env::var("LISTEN_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()
        .map_err(|error| polybot::Error::Config(format!("LISTEN_ADDRESS: {error}")))?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| polybot::Error::Config(format!("listen: {error}")))?;
    tracing::info!(%address, "dashboard API listening");
    axum::serve(listener, app(store))
        .await
        .map_err(|error| polybot::Error::Config(format!("server: {error}")))
}

#[cfg(feature = "aws")]
#[tokio::main]
async fn main() -> std::result::Result<(), lambda_http::Error> {
    use lambda_http::{Body, Request, Response, service_fn};
    use polybot::storage::aws::AwsStore;

    polybot::init_tracing();
    let store: Arc<dyn Store> = Arc::new(AwsStore::from_env().await?);
    lambda_http::run(service_fn(move |request: Request| {
        let store = store.clone();
        async move {
            let path = request.uri().path();
            let (status, content_type, body) = match path {
                "/" => (
                    200,
                    "text/html; charset=utf-8",
                    include_str!("../../web/index.html").into(),
                ),
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
