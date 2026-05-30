//! axum HTTP server.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    response::Response,
    routing::{get, post},
    Router,
};
use std::sync::OnceLock;
use deadpool_postgres::Pool;
use donto_memory_core::{substrate::SubstrateClient, Settings};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

mod cache;
mod docs;
pub mod extract;
pub mod job_log;
mod openapi;
mod ops_auth;
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

    // /jobs/* and /explore/* expose memorized text + every recall
    // query body — content that's never meant for anonymous public
    // access. Gate them behind DONTO_MEMORY_OPS_TOKEN when set; if
    // the env var is unset, the middleware is a pass-through
    // (preserves the local-dev workflow).
    let ops_routes = Router::new()
        .route("/jobs", get(routes::jobs::list_html))
        .route("/jobs/list.json", get(routes::jobs::list_json))
        .route("/jobs/:id", get(routes::jobs::detail_html))
        .route("/jobs/:id/raw", get(routes::jobs::detail_json))
        .route("/explore", get(routes::explore::page))
        .route("/explore/stats.json", get(routes::explore::stats))
        .route("/explore/holders.json", get(routes::explore::holders))
        .route("/explore/sessions.json", get(routes::explore::sessions))
        .route("/explore/records.json", get(routes::explore::records))
        .route("/explore/facts.json", get(routes::explore::facts))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            ops_auth::require_ops_token,
        ));

    let public_routes = Router::new()
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
        .route("/integration-patterns.md", get(integration_patterns_md))
        .route("/llms.txt", get(llms_txt));

    public_routes
        .merge(ops_routes)
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_methods(Any)
                .allow_headers(Any)
                .allow_origin(Any),
        )
}

/// Static homepage. Served as text/html with ETag + Cache-Control.
async fn homepage(req: Request) -> Response {
    static ETAG: OnceLock<String> = OnceLock::new();
    cache::cacheable(&ETAG, "text/html; charset=utf-8", docs::HOMEPAGE, req.headers())
}

async fn swagger_ui(req: Request) -> Response {
    static ETAG: OnceLock<String> = OnceLock::new();
    cache::cacheable(
        &ETAG,
        "text/html; charset=utf-8",
        docs::SWAGGER_HTML,
        req.headers(),
    )
}

/// OpenAPI doc. Same etag/cache pattern, but the body is generated
/// at runtime via serde_json so we serialize once into a static
/// `OnceLock<String>` and reuse.
async fn openapi_doc(req: Request) -> Response {
    static BODY: OnceLock<String> = OnceLock::new();
    static ETAG: OnceLock<String> = OnceLock::new();
    let body = BODY.get_or_init(|| serde_json::to_string(&openapi::document()).unwrap());
    // Trick: cacheable expects a `&'static str`, and OnceLock<String>::get
    // returns &String which derefs to &str. Safe because the OnceLock
    // contents are never freed.
    cache::cacheable(&ETAG, "application/json", body.as_str(), req.headers())
}

/// Markdown guide aimed at AI agents. Served as text/markdown so an
/// agent fetching the URL can ingest it directly.
async fn agent_md(req: Request) -> Response {
    static ETAG: OnceLock<String> = OnceLock::new();
    cache::cacheable(
        &ETAG,
        "text/markdown; charset=utf-8",
        docs::AGENT_MD,
        req.headers(),
    )
}

/// llms.txt convention — same content as /agent.md but at the
/// canonical "I am an AI; tell me how to use this site" path. Served
/// as text/plain.
async fn llms_txt(req: Request) -> Response {
    static ETAG: OnceLock<String> = OnceLock::new();
    cache::cacheable(
        &ETAG,
        "text/plain; charset=utf-8",
        docs::AGENT_MD,
        req.headers(),
    )
}

/// Concrete integration-patterns spec aimed at the dev (or AI agent)
/// wiring an existing conversational backend into donto-memory.
async fn integration_patterns_md(req: Request) -> Response {
    static ETAG: OnceLock<String> = OnceLock::new();
    cache::cacheable(
        &ETAG,
        "text/markdown; charset=utf-8",
        docs::INTEGRATION_PATTERNS_MD,
        req.headers(),
    )
}


/// JSON summary at `/api` (the old `/` payload, kept for programmatic
/// callers that hit the root expecting JSON).
async fn api_summary(
    State(state): State<Arc<AppState>>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "service": "donto-memory",
        "version": env!("CARGO_PKG_VERSION"),
        "substrate_contract_floor": donto_memory_core::SUBSTRATE_CONTRACT_FLOOR,
        "endpoints": {
            "agent_contract": [
                "GET /health",
                "GET /version",
                "GET /substrate",
                "GET /modules",
                "POST /ingest/:module_iri",
                "POST /memorize",
                "POST /memorize/batch",
                "POST /recall",
                "POST /reconsolidate/enqueue",
                "GET /reconsolidate/queue"
            ],
            "documentation": [
                "GET /",
                "GET /openapi.json",
                "GET /docs",
                "GET /agent.md",
                "GET /llms.txt",
                "GET /integration-patterns.md"
            ],
            "operator": [
                "GET /jobs",
                "GET /jobs/list.json",
                "GET /jobs/:id",
                "GET /jobs/:id/raw",
                "GET /explore",
                "GET /explore/stats.json",
                "GET /explore/holders.json",
                "GET /explore/sessions.json",
                "GET /explore/records.json",
                "GET /explore/facts.json"
            ]
        },
        "ops_token_required": state.settings.ops_token.is_some(),
    }))
}
