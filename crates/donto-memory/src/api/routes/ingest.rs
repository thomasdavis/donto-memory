use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use donto_memory_core::module::register_default_modules;
use serde_json::{json, Value};

use crate::api::job_log;
use crate::api::AppState;

pub async fn ingest(
    State(s): State<Arc<AppState>>,
    Path(module_iri): Path<String>,
    Json(input): Json<Value>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    let holder = input.get("holder").and_then(|v| v.as_str()).map(|s| s.to_string());
    let session_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let reg = register_default_modules();
    // Normalise short module names to full IRIs.
    let canonical = match module_iri.as_str() {
        "episodic" => "mem:module/episodic".to_string(),
        "semantic-claim" => "mem:module/semantic-claim".to_string(),
        "preference" => "mem:module/preference".to_string(),
        other => other.to_string(),
    };
    // Audit-log under the canonical name so `/ingest/episodic` and
    // `/ingest/mem:module/episodic` group cleanly in /jobs filters.
    // Unknown modules still log under the raw module_iri so probes
    // and typos remain visible.
    let endpoint = if reg.get(&canonical).is_some() {
        format!("POST /ingest/{}", canonical)
    } else {
        format!("POST /ingest/{}", module_iri)
    };

    let (status_code, resp_json): (u16, Value) = match reg.get(&canonical) {
        None => (
            404,
            json!({"error": format!("module {canonical:?} not registered")}),
        ),
        Some(module) => {
            let parsed: Result<donto_memory_core::module::IngestInput, _> =
                serde_json::from_value(input.clone());
            match parsed {
                Err(e) => (400, json!({"error": format!("bad ingest input: {e}")})),
                Ok(parsed_input) => {
                    match module
                        .ingest(
                            &s.substrate,
                            &s.pool,
                            &s.settings.consumer_iri,
                            &parsed_input,
                        )
                        .await
                    {
                        Ok(record) => (
                            200,
                            json!({
                                "record_id": record.record_id,
                                "record_iri": record.record_iri,
                                "module_iri": record.module_iri,
                                "anchored_to": {
                                    "statement_id": record.r#ref.statement_id,
                                    "frame_id": record.r#ref.frame_id,
                                    "context_iri": record.r#ref.context_iri,
                                },
                            }),
                        ),
                        Err(e) => {
                            let msg = e.to_string();
                            let status = if msg.contains("invalid input") { 400 } else { 500 };
                            (status, json!({"error": msg}))
                        }
                    }
                }
            }
        }
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let metrics = job_log::JobMetrics {
        facts_ingested: if status_code == 200 { Some(1) } else { Some(0) },
        error: if status_code >= 400 {
            resp_json
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        },
        ..Default::default()
    };
    job_log::record_job(
        &s.pool,
        &s.settings.consumer_iri,
        &endpoint,
        holder.as_deref(),
        session_id.as_deref(),
        status_code,
        elapsed_ms,
        &input,
        &resp_json,
        metrics,
    )
    .await;

    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    (status, Json(resp_json)).into_response()
}
