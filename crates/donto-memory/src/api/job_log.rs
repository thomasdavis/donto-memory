//! Per-request audit log. Persisted to `donto_x_memory_job_log`.
//!
//! Every /memorize, /memorize/batch, /recall, /ingest handler calls
//! [`record_job`] after returning to keep an append-only history that
//! the /jobs HTML page reads back.

use deadpool_postgres::Pool;
use serde_json::Value;
use uuid::Uuid;

#[derive(Default, Debug, Clone)]
pub struct JobMetrics {
    pub facts_extracted: Option<i32>,
    pub facts_ingested: Option<i32>,
    pub rows_returned: Option<i32>,
    pub model: Option<String>,
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub total_tokens: Option<i32>,
    pub error: Option<String>,
}

/// Pull a string out of a JSON request body by field name, if present.
pub fn field_str(req: &Value, name: &str) -> Option<String> {
    req.get(name)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Pull common metrics out of a /memorize-style response so the
/// indexed columns get populated.
pub fn metrics_from_memorize(resp: &Value) -> JobMetrics {
    let usage = resp.get("usage");
    JobMetrics {
        facts_extracted: resp.get("facts_extracted").and_then(|v| v.as_i64()).map(|n| n as i32),
        facts_ingested: resp.get("facts_ingested").and_then(|v| v.as_i64()).map(|n| n as i32),
        rows_returned: None,
        model: field_str(resp, "model"),
        prompt_tokens: usage.and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_i64()).map(|n| n as i32),
        completion_tokens: usage.and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_i64()).map(|n| n as i32),
        total_tokens: usage.and_then(|u| u.get("total_tokens")).and_then(|v| v.as_i64()).map(|n| n as i32),
        error: None,
    }
}

/// Metrics for a /recall response.
pub fn metrics_from_recall(resp: &Value) -> JobMetrics {
    JobMetrics {
        rows_returned: resp.get("row_count").and_then(|v| v.as_i64()).map(|n| n as i32),
        ..Default::default()
    }
}

/// Persist one row. Failures are logged and swallowed: a write to the
/// audit log must never break the user-visible API.
pub async fn record_job(
    pool: &Pool,
    consumer_iri: &str,
    endpoint: &str,
    holder: Option<&str>,
    session_id: Option<&str>,
    status_code: u16,
    elapsed_ms: u64,
    request: &Value,
    response: &Value,
    metrics: JobMetrics,
) -> Option<Uuid> {
    let client = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "job_log: pool.get failed; skipping audit row");
            return None;
        }
    };
    let row = client.query_one(
        "insert into donto_x_memory_job_log
            (consumer_iri, endpoint, holder, session_id, status_code, elapsed_ms,
             request, response,
             facts_extracted, facts_ingested, rows_returned, model,
             prompt_tokens, completion_tokens, total_tokens, error)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
         returning job_id",
        &[
            &consumer_iri,
            &endpoint,
            &holder,
            &session_id,
            &(status_code as i32),
            &(elapsed_ms as i64),
            &request,
            &response,
            &metrics.facts_extracted,
            &metrics.facts_ingested,
            &metrics.rows_returned,
            &metrics.model,
            &metrics.prompt_tokens,
            &metrics.completion_tokens,
            &metrics.total_tokens,
            &metrics.error,
        ],
    ).await;
    match row {
        Ok(r) => r.get::<_, Uuid>(0).into(),
        Err(e) => {
            tracing::warn!(error = %e, "job_log: insert failed; continuing");
            None
        }
    }
}
