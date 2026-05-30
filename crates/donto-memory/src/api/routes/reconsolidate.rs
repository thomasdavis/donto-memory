use std::sync::Arc;

use axum::extract::Query;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use chrono::Utc;
use donto_memory_core::overlays;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::api::extract::JsonReq;
use crate::api::AppState;

#[derive(Debug, Deserialize)]
pub struct EnqueueReq {
    pub record_id: Uuid,
    #[serde(default = "default_reason")]
    pub reason: String,
    #[serde(default)]
    pub priority: f64,
}

#[derive(Debug, Deserialize)]
pub struct QueueQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_reason() -> String {
    "explicit".to_string()
}
fn default_limit() -> i64 {
    100
}

pub async fn enqueue(
    State(s): State<Arc<AppState>>,
    JsonReq(req): JsonReq<EnqueueReq>,
) -> impl IntoResponse {
    let requested_at = Utc::now();
    match overlays::get_record(&s.pool, req.record_id).await {
        Ok(Some(_)) => match overlays::enqueue_reconsolidation(
            &s.pool,
            req.record_id,
            &req.reason,
            req.priority,
            None,
            &serde_json::Value::Null,
            s.settings.reconsolidation_coalesce_window_seconds,
        )
        .await
        {
            Ok(qid) => Json(json!({
                "queue_id": qid,
                "record_id": req.record_id,
                "reason": req.reason,
                "priority": req.priority,
                "available_at": requested_at,
            }))
            .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response(),
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("record {} not found", req.record_id)})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn queue(
    State(s): State<Arc<AppState>>,
    Query(q): Query<QueueQuery>,
) -> impl IntoResponse {
    let conn = match s.pool.get().await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let limit = q.limit.clamp(1, 1000);
    let rows = conn
        .query(
            "select queue_id, record_id, reason, priority, available_at, \
                    claimed_at, claimed_by, completed_at \
               from donto_x_memory_reconsolidation_queue \
              where completed_at is null \
              order by priority desc, available_at asc limit $1",
            &[&limit],
        )
        .await;
    match rows {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "queue_id": r.get::<_, Uuid>(0),
                        "record_id": r.get::<_, Uuid>(1),
                        "reason": r.get::<_, String>(2),
                        "priority": r.get::<_, f64>(3),
                        "available_at": r.get::<_, chrono::DateTime<chrono::Utc>>(4),
                        "claimed_at": r.get::<_, Option<chrono::DateTime<chrono::Utc>>>(5),
                        "claimed_by": r.get::<_, Option<String>>(6),
                        "completed_at": r.get::<_, Option<chrono::DateTime<chrono::Utc>>>(7),
                    })
                })
                .collect();
            Json(json!({"count": items.len(), "items": items})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
