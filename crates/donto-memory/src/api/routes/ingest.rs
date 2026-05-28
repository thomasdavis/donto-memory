use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use donto_memory_core::module::{register_default_modules, IngestInput};
use serde_json::json;

use crate::api::AppState;

pub async fn ingest(
    State(s): State<Arc<AppState>>,
    Path(module_iri): Path<String>,
    Json(input): Json<IngestInput>,
) -> impl IntoResponse {
    let reg = register_default_modules();
    // Normalise short module names (episodic / semantic-claim / preference)
    // to their full IRI for convenience.
    let canonical = match module_iri.as_str() {
        "episodic" => "mem:module/episodic".to_string(),
        "semantic-claim" => "mem:module/semantic-claim".to_string(),
        "preference" => "mem:module/preference".to_string(),
        other => other.to_string(),
    };
    let Some(module) = reg.get(&canonical) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("module {canonical:?} not registered")})),
        )
            .into_response();
    };
    match module
        .ingest(&s.substrate, &s.pool, &s.settings.consumer_iri, &input)
        .await
    {
        Ok(record) => Json(json!({
            "record_id": record.record_id,
            "record_iri": record.record_iri,
            "module_iri": record.module_iri,
            "anchored_to": {
                "statement_id": record.r#ref.statement_id,
                "frame_id": record.r#ref.frame_id,
                "context_iri": record.r#ref.context_iri,
            },
        }))
        .into_response(),
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("invalid input") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({"error": msg}))).into_response()
        }
    }
}
