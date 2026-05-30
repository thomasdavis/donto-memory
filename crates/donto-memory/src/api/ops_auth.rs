//! Optional bearer-token gate for the operator surfaces (/jobs/*,
//! /explore/*). When the runtime has `DONTO_MEMORY_OPS_TOKEN` set,
//! every request to these routes must carry the matching token via
//! either `Authorization: Bearer <token>` or `?token=<token>`.
//! Without the token configured, all requests pass through — keeps
//! existing local-dev workflows unbroken.

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::Json;
use serde::Deserialize;

use crate::api::AppState;

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    token: Option<String>,
}

/// Returns 401 unless the request carries the configured ops token.
/// No-op when `ops_token` is unset.
pub async fn require_ops_token(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TokenQuery>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let Some(expected) = state.settings.ops_token.as_deref() else {
        // No token configured → open. Backwards-compatible with the
        // pre-gate behavior; an operator deploys with the env var set
        // to lock the ops surfaces.
        return Ok(next.run(req).await);
    };

    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let presented = header_token.or(q.token);

    match presented {
        Some(t) if constant_time_eq(t.as_bytes(), expected.as_bytes()) => {
            Ok(next.run(req).await)
        }
        _ => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "operator surface — supply token via Authorization: Bearer <token> or ?token=<token>"
            })),
        )),
    }
}

/// Compare two byte slices in constant time. Prevents byte-by-byte
/// timing oracles on token comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}
