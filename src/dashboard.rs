//! Dashboard HTTP surface shared by the local `api` binary and the `local`
//! supervisor. The Lambda API reuses `latest_research` and the same HTML.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{Html, IntoResponse},
    routing::get,
};
use tower_http::cors::CorsLayer;

use crate::{
    Error, Result,
    domain::{RecommendationClass, ResearchOpportunity},
    storage::Store,
};

pub const INDEX_HTML: &str = include_str!("../web/index.html");

#[derive(Clone)]
struct AppState {
    store: Arc<dyn Store>,
}

/// Latest opportunities joined with their news evidence. Rejected rows skip
/// the evidence lookup; nothing can revive them.
pub async fn latest_research(store: &dyn Store) -> Result<Vec<ResearchOpportunity>> {
    let opportunities = store.latest_opportunities().await?;
    let mut result = Vec::with_capacity(opportunities.len());
    for opportunity in opportunities {
        let news = if opportunity.class == RecommendationClass::Rejected {
            None
        } else {
            store.news_for(opportunity.id).await?
        };
        result.push(ResearchOpportunity::new(opportunity, news));
    }
    Ok(result)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

async fn config() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/javascript")],
        "window.POLYBOT_CONFIG = {};",
    )
}

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

pub fn router(store: Arc<dyn Store>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/config.js", get(config))
        .route("/health", get(health))
        .route("/api/opportunities", get(opportunities))
        .layer(CorsLayer::permissive())
        .with_state(AppState { store })
}

/// Serve the dashboard until the task is dropped or the listener fails.
pub async fn serve(store: Arc<dyn Store>, address: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| Error::Config(format!("listen on {address}: {error}")))?;
    tracing::info!(%address, "dashboard listening");
    axum::serve(listener, router(store))
        .await
        .map_err(|error| Error::Config(format!("dashboard server: {error}")))
}

pub fn listen_address() -> Result<SocketAddr> {
    std::env::var("LISTEN_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()
        .map_err(|error| Error::Config(format!("LISTEN_ADDRESS: {error}")))
}
