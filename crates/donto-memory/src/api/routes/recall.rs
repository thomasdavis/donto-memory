use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use donto_memory_core::module::register_default_modules;
use donto_memory_core::types::RecallQuery;
use donto_memory_core::hot_path;
use serde_json::json;

use crate::api::AppState;

pub async fn recall(
    State(s): State<Arc<AppState>>,
    Json(mut query): Json<RecallQuery>,
) -> impl IntoResponse {
    if query.limit > s.settings.recall_max_limit {
        query.limit = s.settings.recall_max_limit;
    }
    let reg = register_default_modules();
    match hot_path::compose_bundle(
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
        Ok(bundle) => Json(bundle).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
