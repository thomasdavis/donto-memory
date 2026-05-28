//! axum HTTP server.

use std::sync::Arc;

use axum::{response::Html, routing::{get, post}, Router};
use deadpool_postgres::Pool;
use donto_memory_core::{substrate::SubstrateClient, Settings};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

mod docs;
mod openapi;
mod routes;

/// State shared across all routes.
#[derive(Clone)]
pub struct AppState {
    pub settings: Settings,
    pub substrate: SubstrateClient,
    pub pool: Pool,
}

pub fn router(state: AppState) -> Router {
    let state = Arc::new(state);
    Router::new()
        .route("/", get(homepage))
        .route("/api", get(api_summary))
        .route("/health", get(routes::health::health))
        .route("/version", get(routes::health::version))
        .route("/substrate", get(routes::health::substrate))
        .route("/modules", get(routes::modules::list))
        .route("/ingest/:module_iri", post(routes::ingest::ingest))
        .route("/memorize", post(routes::memorize::memorize))
        .route("/memorize/batch", post(routes::memorize::memorize_batch))
        .route("/recall", post(routes::recall::recall))
        .route(
            "/reconsolidate/enqueue",
            post(routes::reconsolidate::enqueue),
        )
        .route("/reconsolidate/queue", get(routes::reconsolidate::queue))
        .route("/openapi.json", get(openapi_doc))
        .route("/docs", get(swagger_ui))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods(Any)
                .allow_headers(Any)
                .allow_origin(Any),
        )
}

/// Static homepage. Served as text/html.
async fn homepage() -> Html<&'static str> {
    Html(docs::HOMEPAGE)
}

async fn swagger_ui() -> Html<&'static str> {
    Html(docs::SWAGGER_HTML)
}

async fn openapi_doc() -> axum::Json<serde_json::Value> {
    axum::Json(openapi::document())
}

/// JSON summary at `/api` (the old `/` payload, kept for programmatic
/// callers that hit the root expecting JSON).
async fn api_summary() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "service": "donto-memory",
        "version": env!("CARGO_PKG_VERSION"),
        "substrate_contract_floor": donto_memory_core::SUBSTRATE_CONTRACT_FLOOR,
        "endpoints": [
            "GET /health", "GET /version", "GET /substrate",
            "GET /modules",
            "POST /ingest/:module_iri",
            "POST /recall",
            "POST /reconsolidate/enqueue",
            "GET /reconsolidate/queue",
            "GET /openapi.json", "GET /docs"
        ],
    }))
}
