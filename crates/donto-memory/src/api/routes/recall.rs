use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use donto_memory_core::hot_path;
use donto_memory_core::module::register_default_modules;
use donto_memory_core::types::RecallQuery;
use serde_json::{json, Value};

use crate::api::job_log;
use crate::api::AppState;

pub async fn recall(
    State(s): State<Arc<AppState>>,
    Json(mut query): Json<RecallQuery>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    let request_json = serde_json::to_value(&query).unwrap_or_else(|_| json!({}));
    let holder = query.holder.clone();
    let session_id = query.session_id.clone();

    if query.limit > s.settings.recall_max_limit {
        query.limit = s.settings.recall_max_limit;
    }
    let reg = register_default_modules();

    let (status_code, resp_json): (u16, Value) = match hot_path::compose_bundle(
        &s.substrate,
        &s.pool,
        &s.settings.consumer_iri,
        reg,
        &query,
        s.settings.enable_reconsolidation_enqueue,
        s.settings.reconsolidation_coalesce_window_seconds,
    )
    .await
    {
        Ok(bundle) => (200, serde_json::to_value(&bundle).unwrap_or_else(|_| json!({}))),
        Err(e) => (500, json!({"error": e.to_string()})),
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let metrics = if status_code == 200 {
        job_log::metrics_from_recall(&resp_json)
    } else {
        job_log::JobMetrics {
            error: resp_json
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ..Default::default()
        }
    };
    job_log::record_job(
        &s.pool,
        &s.settings.consumer_iri,
        "POST /recall",
        Some(&holder),
        session_id.as_deref(),
        status_code,
        elapsed_ms,
        &request_json,
        &resp_json,
        metrics,
    )
    .await;

    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    (status, Json(resp_json)).into_response()
}
