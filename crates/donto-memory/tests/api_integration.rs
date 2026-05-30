//! Full HTTP integration tests for donto-memory.
//!
//! Spins up the axum app in-process and exercises every public
//! route via tower::ServiceExt::oneshot. Tests self-skip if the
//! live substrate or Postgres is unreachable.
//!
//! Run via:
//!   cargo test -p donto-memory --test api_integration -- --test-threads=1

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use donto_memory_core::{
    overlays,
    substrate::SubstrateClient,
    Settings,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn dontosrv_url() -> String {
    std::env::var("DONTOSRV_URL").unwrap_or_else(|_| "http://localhost:7879".into())
}
fn donto_dsn() -> Option<String> {
    std::env::var("DONTO_DSN")
        .or_else(|_| std::env::var("DONTO_MEMORY_DONTO_DSN"))
        .ok()
}

fn unique_id(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        ^ (std::process::id() as u64);
    format!("{prefix}-{n:x}")
}

async fn build_state() -> Option<(donto_memory::api::AppState, Settings)> {
    let dsn = donto_dsn()?;
    let mut s = Settings::default();
    s.dontosrv_url = dontosrv_url();
    s.donto_dsn = Some(dsn.clone());
    s.reconsolidation_coalesce_window_seconds = 0;
    let substrate = SubstrateClient::new(&s.dontosrv_url).ok()?;
    substrate.contract_version().await.ok()?;
    let pool = overlays::pool_from_dsn(&dsn).ok()?;
    donto_memory_core::module::register_default_modules();
    let st = donto_memory::api::AppState {
        settings: s.clone(),
        substrate,
        pool,
        async_memorize_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    };
    Some((st, s))
}

async fn body_json(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn body_text(resp: axum::response::Response) -> (StatusCode, String) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn post(path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

// ---------------------------------------------------------------------
// Health / version / discovery
// ---------------------------------------------------------------------

#[tokio::test]
async fn health_returns_ok() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, body) = body_json(app.oneshot(get("/health")).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn version_exposes_contract_floor() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, body) = body_json(app.oneshot(get("/version")).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["service"], "donto-memory");
    assert_eq!(body["substrate_contract_floor"], "0.1.0-m10");
}

#[tokio::test]
async fn root_serves_html_homepage() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let resp = app.oneshot(get("/")).await.unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (_, text) = body_text(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/html"), "got content-type {ct}");
    assert!(text.contains("donto-memory"));
    assert!(text.contains("memories.apexpots.com"));
}

#[tokio::test]
async fn docs_serves_swagger_html() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, text) = body_text(app.oneshot(get("/docs")).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("swagger-ui"));
    assert!(text.contains("/openapi.json"));
}

#[tokio::test]
async fn openapi_lists_every_documented_path() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, body) = body_json(app.oneshot(get("/openapi.json")).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["openapi"], "3.1.0");
    let paths = body["paths"].as_object().unwrap();
    for expected in [
        "/", "/health", "/version", "/substrate", "/modules",
        "/ingest/{module_iri}",
        "/memorize", "/memorize/batch",
        "/recall",
        "/reconsolidate/enqueue", "/reconsolidate/queue",
        "/openapi.json", "/docs",
    ] {
        assert!(paths.contains_key(expected), "missing path {expected}");
    }
}

#[tokio::test]
async fn modules_endpoint_lists_three_defaults() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, body) = body_json(app.oneshot(get("/modules")).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let modules = body["modules"].as_array().unwrap();
    let iris: Vec<&str> = modules
        .iter()
        .filter_map(|m| m["module_iri"].as_str())
        .collect();
    for expected in [
        "mem:module/episodic",
        "mem:module/semantic-claim",
        "mem:module/preference",
    ] {
        assert!(iris.contains(&expected), "module {expected} missing");
    }
}

#[tokio::test]
async fn substrate_endpoint_returns_contract_and_health() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, body) = body_json(app.oneshot(get("/substrate")).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["contract"]["actions"].is_array());
}

// ---------------------------------------------------------------------
// Ingest paths (per-module)
// ---------------------------------------------------------------------

#[tokio::test]
async fn ingest_episodic_via_short_name() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let holder = unique_id("agent:apitest");
    let session = unique_id("apitest-ep");
    let (status, body) = body_json(
        app.oneshot(post(
            "/ingest/episodic",
            &json!({
                "holder": holder,
                "session_id": session,
                "text": "API ingest test (episodic)."
            }),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["module_iri"], "mem:module/episodic");
    assert!(body["record_id"].as_str().is_some());
}

#[tokio::test]
async fn ingest_episodic_via_full_iri() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let holder = unique_id("agent:apitest-full");
    let (status, body) = body_json(
        app.oneshot(post(
            "/ingest/mem:module%2Fepisodic",
            &json!({
                "holder": holder,
                "text": "Full-IRI route ingest."
            }),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["module_iri"], "mem:module/episodic");
}

#[tokio::test]
async fn ingest_unknown_module_returns_404() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, body) = body_json(
        app.oneshot(post(
            "/ingest/mem:module%2Fdoes-not-exist",
            &json!({"holder": "agent:x", "text": "x"}),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap_or("").contains("not registered"));
}

#[tokio::test]
async fn ingest_episodic_empty_text_returns_400() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, _) = body_json(
        app.oneshot(post(
            "/ingest/episodic",
            &json!({"holder": "agent:x", "text": ""}),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ingest_semantic_requires_subject_predicate() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let holder = unique_id("agent:semitest");
    let (status, _) = body_json(
        app.oneshot(post(
            "/ingest/semantic-claim",
            &json!({"holder": holder, "text": "no subject"}),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ingest_preference_requires_key_and_value() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let holder = unique_id("agent:preftest");
    let (status, _) = body_json(
        app.oneshot(post(
            "/ingest/preference",
            &json!({"holder": holder, "value": "en"}),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------
// Memorize (LLM-optional)
// ---------------------------------------------------------------------

#[tokio::test]
async fn memorize_without_llm_stores_episodic_with_warning() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let holder = unique_id("agent:memtest");
    let (status, body) = body_json(
        app.oneshot(post(
            "/memorize",
            &json!({
                "holder": holder,
                "session_id": unique_id("mem"),
                "text": "Memorize test without LLM configured."
            }),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["episodic_record_iri"].as_str().is_some());
    assert_eq!(body["extracted"], true);
    assert_eq!(body["facts_extracted"], 0);
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("LLM not configured")),
        "should warn about missing LLM: {body}"
    );
}

#[tokio::test]
async fn memorize_with_extract_false_yields_no_warnings() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let holder = unique_id("agent:memtest-noex");
    let (status, body) = body_json(
        app.oneshot(post(
            "/memorize",
            &json!({
                "holder": holder,
                "text": "Plain chunk, no extraction please.",
                "extract": false
            }),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["extracted"], false);
    assert_eq!(body["facts_extracted"], 0);
    let warnings = body["warnings"].as_array().unwrap();
    assert!(warnings.is_empty(), "got {warnings:?}");
}

#[tokio::test]
async fn memorize_empty_text_returns_400() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, body) = body_json(
        app.oneshot(post(
            "/memorize",
            &json!({"holder": "agent:x", "text": ""}),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap_or("").contains("text"));
}

#[tokio::test]
async fn memorize_batch_processes_every_item() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let holder = unique_id("agent:batchtest");
    let (status, body) = body_json(
        app.oneshot(post(
            "/memorize/batch",
            &json!({
                "items": [
                    {"holder": holder, "text": "Batch item 1", "extract": false},
                    {"holder": holder, "text": "Batch item 2", "extract": false},
                    {"holder": holder, "text": "Batch item 3", "extract": false}
                ]
            }),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r["episodic_record_iri"].as_str().is_some()));
}

#[tokio::test]
async fn memorize_batch_empty_returns_400() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, _) = body_json(
        app.oneshot(post("/memorize/batch", &json!({"items": []})))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------
// Recall
// ---------------------------------------------------------------------

#[tokio::test]
async fn recall_returns_just_ingested_row() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let holder = unique_id("agent:recalltest");
    let session = unique_id("rt-sess");
    let marker = unique_id("rt-marker");
    let _ = body_json(
        app.clone()
            .oneshot(post(
                "/ingest/episodic",
                &json!({
                    "holder": holder,
                    "session_id": session,
                    "text": format!("recall test {marker}")
                }),
            ))
            .await
            .unwrap(),
    )
    .await;

    let (status, body) = body_json(
        app.oneshot(post(
            "/recall",
            &json!({
                "holder": holder,
                "action": "read_metadata",
                "query": marker,
                "session_id": session,
                "module_iris": ["mem:module/episodic"],
                "limit": 5,
                "permitted_only": false
            }),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["row_count"].as_i64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn recall_with_unknown_module_returns_empty() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let (status, body) = body_json(
        app.oneshot(post(
            "/recall",
            &json!({
                "holder": unique_id("agent:none"),
                "action": "read_metadata",
                "module_iris": ["mem:module/does-not-exist"],
                "limit": 5
            }),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["row_count"], 0);
    let modules: Vec<&str> = body["modules_used"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(modules.is_empty());
}

// ---------------------------------------------------------------------
// Reconsolidation
// ---------------------------------------------------------------------

#[tokio::test]
async fn reconsolidate_enqueue_unknown_record_returns_404() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);
    let bogus = uuid::Uuid::new_v4();
    let (status, _) = body_json(
        app.oneshot(post(
            "/reconsolidate/enqueue",
            &json!({"record_id": bogus}),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reconsolidate_enqueue_round_trips() {
    let Some((st, _)) = build_state().await else {
        eprintln!("skip");
        return;
    };
    let app = donto_memory::api::router(st);

    // Create a record we can enqueue.
    let holder = unique_id("agent:reconsoltest");
    let (_, ingest_body) = body_json(
        app.clone()
            .oneshot(post(
                "/ingest/episodic",
                &json!({"holder": holder, "text": "Reconsol test."}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let record_id = ingest_body["record_id"].as_str().unwrap();

    let (status, body) = body_json(
        app.clone()
            .oneshot(post(
                "/reconsolidate/enqueue",
                &json!({
                    "record_id": record_id,
                    "reason": "explicit",
                    "priority": 0.5
                }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["queue_id"].as_str().is_some());

    // List the queue and confirm it shows up.
    let (qstatus, qbody) = body_json(app.oneshot(get("/reconsolidate/queue")).await.unwrap()).await;
    assert_eq!(qstatus, StatusCode::OK);
    let items = qbody["items"].as_array().unwrap();
    assert!(items
        .iter()
        .any(|i| i["record_id"].as_str() == Some(record_id)));
}
