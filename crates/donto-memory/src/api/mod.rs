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
    /// Serializes background memorize-extraction tasks so only one
    /// runs at a time. Synchronous /memorize calls and other endpoints
    /// are not affected.
    pub async_memorize_lock: Arc<tokio::sync::Mutex<()>>,
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
    // Live queue snapshot — operators use this to drain-before-restart
    // without writing the multi-CTE SQL by hand. Best-effort: on DB
    // error we surface a null instead of failing the whole summary.
    let async_queue = fetch_async_queue_snapshot(&state.pool).await;

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
        "async_memorize_queue": async_queue,
    }))
}

/// Snapshot of the async-memorize queue health for the /api summary.
/// Lets an operator drain-before-restart with a single
/// `curl /api | jq .async_memorize_queue.pending` instead of running
/// the multi-CTE SQL by hand.
async fn fetch_async_queue_snapshot(pool: &deadpool_postgres::Pool) -> serde_json::Value {
    let Ok(c) = pool.get().await else {
        return serde_json::Value::Null;
    };
    let row = c
        .query_one(
            "with queued as (
               select response->>'queue_id' as q, created_at, holder
               from donto_x_memory_job_log
               where endpoint = 'POST /memorize (queued)'
             ),
             completed as (
               select response->>'queue_id' as q
               from donto_x_memory_job_log
               where endpoint like 'POST /memorize (async%'
             ),
             lost as (
               select response->>'orphaned_queue_id' as q
               from donto_x_memory_job_log
               where endpoint = 'POST /memorize (lost)'
             ),
             pending as (
               select q.q, q.created_at, q.holder
               from queued q
               left join completed c on q.q = c.q
               left join lost    l on q.q = l.q
               where c.q is null and l.q is null
             )
             select
               (select count(*) from pending)::bigint as pending,
               (select extract(epoch from (now() - min(created_at)))
                  from pending)::bigint as oldest_pending_age_seconds,
               (select count(*) from donto_x_memory_job_log
                  where endpoint = 'POST /memorize (async)'
                    and created_at > now() - interval '24 hours')::bigint as completed_24h,
               (select count(*) from donto_x_memory_job_log
                  where endpoint = 'POST /memorize (async-failed)'
                    and created_at > now() - interval '24 hours')::bigint as failed_24h,
               (select count(*) from donto_x_memory_job_log
                  where endpoint = 'POST /memorize (lost)'
                    and created_at > now() - interval '24 hours')::bigint as lost_24h",
            &[],
        )
        .await;
    match row {
        Ok(r) => {
            let pending: i64 = r.get("pending");
            let age: Option<i64> = r.try_get("oldest_pending_age_seconds").ok();
            serde_json::json!({
                "pending":                    pending,
                "oldest_pending_age_seconds": age,
                "completed_24h":              r.get::<_, i64>("completed_24h"),
                "failed_24h":                 r.get::<_, i64>("failed_24h"),
                "lost_24h":                   r.get::<_, i64>("lost_24h"),
                "drain_safe":                 pending == 0,
            })
        }
        Err(_) => serde_json::Value::Null,
    }
}
