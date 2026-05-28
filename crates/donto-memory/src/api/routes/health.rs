use std::sync::Arc;

use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;

use crate::api::AppState;

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

pub async fn version(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "service": "donto-memory",
        "version": env!("CARGO_PKG_VERSION"),
        "substrate_url": s.settings.dontosrv_url,
        "substrate_contract_floor": s.settings.substrate_contract_floor,
    }))
}

pub async fn substrate(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let contract = s.substrate.contract_version().await;
    let health = s.substrate.substrate_health().await;
    Json(json!({
        "contract": contract.ok(),
        "health": health.ok().or_else(|| Some(json!({"error": "unavailable"}))),
    }))
}
