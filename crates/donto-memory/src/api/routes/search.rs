//! POST /search — substrate-wide ranked full-text search.
//!
//! Unlike POST /recall (a holder-scoped Memory Evidence Bundle over the
//! `ctx:memory/*` modules only), this searches the **entire substrate**:
//! every `ctx:*` context in `donto_statement` — genealogy/research-db, all
//! the `ctx:genes/*` worlds, the memory contexts, everything. It is the
//! "search all of donto" primitive that lets a consumer (the Omega recall
//! tool) reach the whole 39M-statement graph, not just one holder's memory.
//!
//! ACCESS: this is intentionally a PUBLIC route (unlike /jobs and /explore,
//! which are ops-token-gated). It returns triples from every context,
//! INCLUDING `ctx:memory/*` (memorized chat). That public exposure is a
//! deliberate, accepted decision (owner sign-off 2026-05-31): the substrate
//! is treated as openly searchable. If that posture ever changes, move this
//! route behind `ops_auth::require_ops_token` and/or add
//! `AND context NOT LIKE 'ctx:memory/%'` to the candidate CTE below.
//!
//! ## Index
//!
//! Backed by the GIN expression index `donto_statement_fts_name`. The exact
//! indexed expression is the `FTS_EXPR` const below — `ops/fts-name-index.sql`
//! is the single source of truth for the DDL. Conceptually it is:
//!
//!   to_tsvector('simple', <humanized subject> ' ' <humanized object_iri>
//!                         ' ' left(<literal value>, 120))
//!     WHERE upper(tx_time) IS NULL
//!
//! where "humanized" = `replace`-ing `/ - :` with spaces so IRI path segments
//! (`.../person/caroline-rose`) tokenise into words. Names live in three
//! places across the corpus — readable IRI segments (`ctx:genes/*`),
//! `rdfs:label` literals over opaque md5 IRIs (`ctx:genealogy/research-db`),
//! and short labels — and this single index covers all of them. The
//! `left(...,120)` cap keeps long episodic text out of the index (size +
//! relevance) while still indexing names/labels.
//!
//! DO NOT paraphrase the expression in code: the query MUST use the EXACT
//! same string as the index (see `FTS_EXPR`) and the `upper(tx_time) IS NULL`
//! predicate, or the planner falls back to a 39M-row seq scan. In particular
//! each `coalesce(..., '')` is LOAD-BEARING: in Postgres `to_tsvector(x || NULL
//! || y)` is NULL, so a missing coalesce on the (usually-NULL) object_iri or
//! object_lit arm both breaks rows and de-qualifies the index.
//!
//! `plainto_tsquery` ANDs the query's lexemes; results are ranked by `ts_rank`
//! over a bounded candidate set (see CANDIDATE_CAP). Sub-second across the
//! full table for selective terms; bounded (not exhaustive) for hot tokens.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Response;
use axum::{http::StatusCode, response::IntoResponse, Json};
use deadpool_postgres::tokio_postgres::types::ToSql;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::extract::JsonReq;
use crate::api::job_log;
use crate::api::AppState;

/// POST /search request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchQuery {
    /// Free-text query. Lexemes are ANDed (plainto_tsquery). Matches names
    /// in IRI path segments, `rdfs:label` literals, and short labels.
    pub query: String,
    /// Optional context-prefix filter, e.g. `ctx:genes` or
    /// `ctx:genealogy/research-db`. Matches `context LIKE '<prefix>%'`.
    #[serde(default)]
    pub context_prefix: Option<String>,
    /// Max rows. Clamped to [1, 500].
    #[serde(default = "default_search_limit")]
    pub limit: i64,
}

fn default_search_limit() -> i64 {
    50
}

/// MUST match the indexed expression in `donto_statement_fts_name` exactly,
/// or Postgres will not use the index (→ 39M-row seq scan). Keep in sync
/// with the migration that creates the index.
const FTS_EXPR: &str = "to_tsvector('simple', \
    coalesce(replace(replace(replace({c}.subject,'/',' '),'-',' '),':',' '),'') || ' ' || \
    coalesce(replace(replace(replace({c}.object_iri,'/',' '),'-',' '),':',' '),'') || ' ' || \
    left(coalesce({c}.object_lit->>'v',''),120))";

const SEARCH_TIMEOUT_MS: u64 = 9000;
/// Cap on the candidate set that gets ts_rank-scored. Bounds worst-case
/// latency independent of how many rows match (a hot term stops the index
/// scan here instead of ranking millions).
const CANDIDATE_CAP: i64 = 2000;

pub async fn search(
    State(s): State<Arc<AppState>>,
    JsonReq(mut q): JsonReq<SearchQuery>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    let request_json = serde_json::to_value(&q).unwrap_or_else(|_| json!({}));

    q.limit = q.limit.clamp(1, 500);
    if q.query.trim().is_empty() {
        let body = json!({"error": "query must be non-empty"});
        return (StatusCode::BAD_REQUEST, Json(body)).into_response();
    }

    let raw_query = q.query.clone();
    // Escape LIKE metacharacters in the caller-supplied prefix so a literal
    // `%` or `_` in a context IRI is matched literally, not as a wildcard.
    // The trailing `%` we append is the intended prefix wildcard. Paired
    // with `ESCAPE '\'` on the LIKE below.
    let prefix_pat = q.context_prefix.as_ref().map(|p| {
        let esc = p.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        format!("{}%", esc)
    });

    // $1 = query text (used in plainto_tsquery for both the @@ filter and
    // ts_rank). Postgres lets us reference $1 multiple times.
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&raw_query];
    let mut idx = 2;

    let prefix_clause = if let Some(pat) = &prefix_pat {
        let c = format!(" AND context LIKE ${idx} ESCAPE '\\'");
        params.push(pat);
        idx += 1;
        c
    } else {
        String::new()
    };

    params.push(&q.limit);
    let limit_idx = idx;

    // Two-stage, bounded query:
    //   1. `cand` applies the index-served @@ filter and caps at
    //      CANDIDATE_CAP rows. The LIMIT lets Postgres STOP the bitmap scan
    //      early, so even a pathological term ("the" → millions of matches)
    //      collects only CANDIDATE_CAP rows in ~0.5s instead of scanning all.
    //   2. ts_rank runs over the bounded candidate set only, then ORDER BY
    //      score + final LIMIT.
    // This makes worst-case latency a function of CANDIDATE_CAP, not corpus
    // match count — correctness no longer depends on statement_timeout. For
    // real (selective) queries the candidate set is small, so ranking is
    // exact; for a hot common term we rank an arbitrary capped subset, which
    // is an acceptable degradation for that degenerate input.
    // FTS_EXPR is rendered without a table alias (single base table).
    let fts_expr = FTS_EXPR.replace("{c}.", "");
    let sql = format!(
        "WITH q AS (SELECT plainto_tsquery('simple', $1) AS tsq),
         cand AS (
           SELECT statement_id, subject, predicate, object_iri, object_lit, context
             FROM donto_statement, q
            WHERE upper(tx_time) IS NULL
              AND {fts_expr} @@ q.tsq{prefix_clause}
            LIMIT {CANDIDATE_CAP}
         )
         SELECT statement_id, subject, predicate, object_iri, object_lit, context,
                ts_rank({fts_expr}, (SELECT tsq FROM q))::real AS score
           FROM cand
          ORDER BY score DESC
          LIMIT ${limit_idx}"
    );

    let mut conn = match s.pool.get().await {
        Ok(c) => c,
        Err(e) => return internal_err(&s, &request_json, started, e.to_string()),
    };

    // Read-only tx so SET LOCAL GUCs don't leak back onto the pooled conn.
    let tx = match conn.transaction().await {
        Ok(t) => t,
        Err(e) => return internal_err(&s, &request_json, started, e.to_string()),
    };
    let setup = format!(
        "SET LOCAL statement_timeout = {to}; SET LOCAL plan_cache_mode = force_custom_plan",
        to = SEARCH_TIMEOUT_MS,
    );
    if let Err(e) = tx.batch_execute(&setup).await {
        return internal_err(&s, &request_json, started, e.to_string());
    }

    let rows = match tx.query(sql.as_str(), &params[..]).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(db) = e.as_db_error() {
                // 57014 = statement_timeout. Degrade to a clean partial 200.
                if db.code().code() == "57014" {
                    let _ = tx.rollback().await;
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let resp = json!({
                        "query": q.query,
                        "context_prefix": q.context_prefix,
                        "row_count": 0,
                        "rows": [],
                        "partial": true,
                        "note": "search exceeded the time budget; narrow the query",
                        "elapsed_ms": elapsed_ms,
                    });
                    log_job(&s, &request_json, 200, elapsed_ms, &resp, None);
                    return (StatusCode::OK, Json(resp)).into_response();
                }
                return internal_err(
                    &s, &request_json, started,
                    format!("db {}: {}", db.code().code(), db.message()),
                );
            }
            return internal_err(&s, &request_json, started, format!("db: {e}"));
        }
    };
    let _ = tx.rollback().await;

    let results: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "statement_id": r.get::<_, uuid::Uuid>("statement_id").to_string(),
                "subject": r.get::<_, String>("subject"),
                "predicate": r.get::<_, String>("predicate"),
                "object_iri": r.get::<_, Option<String>>("object_iri"),
                "object_lit": r.get::<_, Option<Value>>("object_lit"),
                "context": r.get::<_, String>("context"),
                "score": r.get::<_, Option<f32>>("score"),
            })
        })
        .collect();

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let row_count = results.len() as i32;
    let resp = json!({
        "query": q.query,
        "context_prefix": q.context_prefix,
        "row_count": results.len(),
        "rows": results,
        "partial": false,
        "elapsed_ms": elapsed_ms,
    });
    log_job(&s, &request_json, 200, elapsed_ms, &resp, Some(row_count));
    (StatusCode::OK, Json(resp)).into_response()
}

/// Fire-and-forget audit row so every /search shows in /jobs.
fn log_job(
    s: &Arc<AppState>,
    request_json: &Value,
    status: u16,
    elapsed_ms: u64,
    resp: &Value,
    rows_returned: Option<i32>,
) {
    let pool = s.pool.clone();
    let consumer = s.settings.consumer_iri.clone();
    let req = request_json.clone();
    let resp = resp.clone();
    tokio::spawn(async move {
        // Best-effort audit; surface failures so a lost /jobs row is at least
        // observable in the logs (record_job returns None on write failure).
        let logged = job_log::record_job(
            &pool, &consumer, "POST /search", None, None, status, elapsed_ms, &req, &resp,
            job_log::JobMetrics {
                rows_returned,
                error: resp.get("error").and_then(|v| v.as_str()).map(String::from),
                ..Default::default()
            },
        )
        .await;
        if logged.is_none() {
            tracing::warn!("POST /search audit row not written (job_log unavailable)");
        }
    });
}

fn internal_err(
    s: &Arc<AppState>,
    request_json: &Value,
    started: std::time::Instant,
    msg: String,
) -> Response {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let resp = json!({ "error": msg });
    log_job(s, request_json, 500, elapsed_ms, &resp, None);
    (StatusCode::INTERNAL_SERVER_ERROR, Json(resp)).into_response()
}
