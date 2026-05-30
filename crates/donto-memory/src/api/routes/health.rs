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
    // The substrate's /discovery/substrate-health query can take 20+
    // seconds on prod-sized corpora (count(*) over the 39M-row
    // donto_statement table). Run contract + health in parallel and
    // fast-fail health at 8s so monitoring polls don't block on the
    // diagnostic.
    let contract_fut = s.substrate.contract_version();
    let health_fut = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        s.substrate.substrate_health(),
    );
    let (contract, health) = tokio::join!(contract_fut, health_fut);

    let health_json = match health {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({"error": format!("substrate-health failed: {e}")}),
        Err(_) => json!({
            "error": "substrate-health timeout (>8s) — counts over the substrate's \
                      donto_statement table are slow at scale. Hit \
                      GET /discovery/substrate-health on dontosrv directly for the full \
                      response."
        }),
    };
    Json(json!({
        "contract": contract.ok(),
        "health": health_json,
    }))
}
