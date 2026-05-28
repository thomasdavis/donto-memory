//! axum HTTP server.

use std::sync::Arc;

use axum::{
    http::header,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use deadpool_postgres::Pool;
use donto_memory_core::{substrate::SubstrateClient, Settings};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

mod docs;
pub mod job_log;
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
        .route("/agent.md", get(agent_md))
        .route("/llms.txt", get(llms_txt))
        .route("/jobs", get(routes::jobs::list_html))
        .route("/jobs/list.json", get(routes::jobs::list_json))
        .route("/jobs/:id", get(routes::jobs::detail_html))
        .route("/jobs/:id/raw", get(routes::jobs::detail_json))
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

/// Markdown guide aimed at AI agents. Served as text/markdown so an
/// agent fetching the URL can ingest it directly.
async fn agent_md() -> Response {
    (
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        docs::AGENT_MD,
    )
        .into_response()
}

/// llms.txt convention — same content as /agent.md but at the
/// canonical "I am an AI; tell me how to use this site" path. Served
/// as text/plain.
async fn llms_txt() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        docs::AGENT_MD,
    )
        .into_response()
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
