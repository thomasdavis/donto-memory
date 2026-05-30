//! GET /explore — interactive memory explorer.
//! GET /explore/holders.json — JSON: top-level holder list.
//! GET /explore/sessions.json?holder= — sessions under a holder.
//! GET /explore/records.json?holder=&session= — records in a session.
//! GET /explore/facts.json?record_iri=  — facts derived from a record.
//!
//! The page is a single self-contained HTML that progressively
//! reveals: pick a holder, see its sessions, drill into a session,
//! drill into a record, see the facts. Pure fetch() over the JSON
//! endpoints — no build step, no client deps.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::AppState;

// ----- HTML page ---------------------------------------------------

pub async fn page() -> Html<&'static str> {
    Html(include_str!("../../../assets/explore.html"))
}

// ----- JSON endpoints ----------------------------------------------

#[derive(Debug, Deserialize)]
pub struct HolderQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize)]
pub struct SessionsQuery {
    pub holder: String,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize)]
pub struct RecordsQuery {
    pub holder: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize)]
pub struct FactsQuery {
    /// Either `record_iri` (claims/<uuid> or episodic/<uuid>) or
    /// `session` (returns all extracted facts under the session
    /// context).
    #[serde(default)]
    pub record_iri: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    100
}

/// GET /explore/holders.json
pub async fn holders(
    State(s): State<Arc<AppState>>,
    Query(q): Query<HolderQuery>,
) -> Response {
    let pool = &s.pool;
    let conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(500, e.to_string()),
    };
    let limit = q.limit.clamp(1, 1000);
    let rows = match conn
        .query(
            "select holder_iri, count(*) as n,
                    count(distinct session_iri) as sessions,
                    min(lower(tx_time)) as first_seen,
                    max(lower(tx_time)) as last_seen
               from donto_x_memory_record
              where holder_iri is not null
                and ($1::text is null or holder_iri ilike '%' || $1 || '%')
              group by holder_iri
              order by max(lower(tx_time)) desc
              limit $2",
            &[&q.q, &limit],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return err(500, e.to_string()),
    };
    let holders: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "holder": r.get::<_, String>("holder_iri"),
                "records": r.get::<_, i64>("n"),
                "sessions": r.get::<_, i64>("sessions"),
                "first_seen": r.get::<_, chrono::DateTime<chrono::Utc>>("first_seen"),
                "last_seen": r.get::<_, chrono::DateTime<chrono::Utc>>("last_seen"),
            })
        })
        .collect();
    Json(json!({"count": holders.len(), "holders": holders})).into_response()
}

/// GET /explore/sessions.json?holder=...
pub async fn sessions(
    State(s): State<Arc<AppState>>,
    Query(q): Query<SessionsQuery>,
) -> Response {
    let conn = match s.pool.get().await {
        Ok(c) => c,
        Err(e) => return err(500, e.to_string()),
    };
    let limit = q.limit.clamp(1, 1000);
    let rows = match conn
        .query(
            "select coalesce(session_iri, '(no session)') as session,
                    module_iri,
                    count(*) as n,
                    min(lower(tx_time)) as first_seen,
                    max(lower(tx_time)) as last_seen
               from donto_x_memory_record
              where holder_iri = $1
              group by 1, 2
              order by max(lower(tx_time)) desc
              limit $2",
            &[&q.holder, &limit],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return err(500, e.to_string()),
    };
    let sessions: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "session": r.get::<_, String>("session"),
                "module_iri": r.get::<_, String>("module_iri"),
                "records": r.get::<_, i64>("n"),
                "first_seen": r.get::<_, chrono::DateTime<chrono::Utc>>("first_seen"),
                "last_seen": r.get::<_, chrono::DateTime<chrono::Utc>>("last_seen"),
            })
        })
        .collect();
    Json(json!({"holder": q.holder, "count": sessions.len(), "sessions": sessions}))
        .into_response()
}

/// GET /explore/records.json?holder=...&session=...&module=...
pub async fn records(
    State(s): State<Arc<AppState>>,
    Query(q): Query<RecordsQuery>,
) -> Response {
    let conn = match s.pool.get().await {
        Ok(c) => c,
        Err(e) => return err(500, e.to_string()),
    };
    let limit = q.limit.clamp(1, 1000);
    let rows = match conn
        .query(
            "select record_id, record_iri, module_iri,
                    session_iri, holder_iri,
                    root_statement, root_context,
                    lower(tx_time) as created_at,
                    metadata
               from donto_x_memory_record
              where holder_iri = $1
                and ($2::text is null or session_iri = $2)
                and ($3::text is null or module_iri = $3)
              order by lower(tx_time) desc
              limit $4",
            &[&q.holder, &q.session, &q.module, &limit],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return err(500, e.to_string()),
    };
    let records: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "record_id": r.get::<_, uuid::Uuid>("record_id"),
                "record_iri": r.get::<_, String>("record_iri"),
                "module_iri": r.get::<_, String>("module_iri"),
                "session_iri": r.get::<_, Option<String>>("session_iri"),
                "holder_iri": r.get::<_, Option<String>>("holder_iri"),
                "root_statement": r.get::<_, Option<uuid::Uuid>>("root_statement"),
                "root_context": r.get::<_, Option<String>>("root_context"),
                "created_at": r.get::<_, chrono::DateTime<chrono::Utc>>("created_at"),
                "metadata": r.get::<_, Value>("metadata"),
            })
        })
        .collect();
    Json(json!({"holder": q.holder, "count": records.len(), "records": records}))
        .into_response()
}

/// GET /explore/facts.json?record_iri=...   OR   ?session=...
///
/// Returns the underlying donto_statement rows.
///
/// Schema reality (`donto_statement` has no `source_record_iri`
/// column — the link from a derived semantic claim back to its
/// episodic source is itself a statement with predicate
/// `mem:claim/derived_from`):
///
///   - record_iri `ctx:memory/claim/<uuid>` → the `<uuid>` is the
///     statement_id; the record IS the fact.
///   - record_iri `ctx:memory/episodic/<uuid>` → the record is a
///     context. Find every statement whose `subject` (the
///     statement_id of a derived claim) has a derived_from edge
///     pointing at this record_iri, then return those derived
///     claims.
///   - session=<id> → walk the two session contexts directly.
pub async fn facts(
    State(s): State<Arc<AppState>>,
    Query(q): Query<FactsQuery>,
) -> Response {
    let conn = match s.pool.get().await {
        Ok(c) => c,
        Err(e) => return err(500, e.to_string()),
    };
    let limit = q.limit.clamp(1, 2000);

    let rows_result = if let Some(iri) = q.record_iri.as_ref() {
        if let Some(stmt_uuid) = iri.strip_prefix("ctx:memory/claim/") {
            // Path 1: this record IS a single claim. Return it.
            let uuid = match uuid::Uuid::parse_str(stmt_uuid) {
                Ok(u) => u,
                Err(_) => return err(400, "invalid statement uuid in record_iri".to_string()),
            };
            conn.query(
                "select s.statement_id::text as statement_id,
                        s.subject, s.predicate,
                        s.object_iri, s.object_lit,
                        s.context, s.flags,
                        lower(s.tx_time) as tx_lo
                   from donto_statement s
                  where s.statement_id = $1
                    and upper(s.tx_time) is null
                  limit $2",
                &[&uuid, &limit],
            )
            .await
        } else if iri.starts_with("ctx:memory/episodic/") {
            // Path 2: derived-from lookup.
            //
            // Edge stored as a statement (subject=derived stmt_id as
            // text, predicate=mem:claim/derived_from,
            // object_iri=this episodic record_iri).
            //
            // The naive join `src.text = s.statement_id::text` made
            // the planner seq-scan 39M rows because the PK index on
            // donto_statement.statement_id is on the uuid column.
            // We cast the subjects to uuid in the subquery so the
            // PK index handles the lookup.
            conn.query(
                "select s.statement_id::text as statement_id,
                        s.subject, s.predicate,
                        s.object_iri, s.object_lit,
                        s.context, s.flags,
                        lower(s.tx_time) as tx_lo
                   from donto_statement s
                  where s.statement_id = any(
                          select subject::uuid
                            from donto_statement
                           where predicate = 'mem:claim/derived_from'
                             and object_iri = $1
                             and upper(tx_time) is null
                        )
                    and upper(s.tx_time) is null
                  limit $2",
                &[iri, &limit],
            )
            .await
        } else {
            return err(
                400,
                "record_iri must start with ctx:memory/claim/ or ctx:memory/episodic/"
                    .to_string(),
            );
        }
    } else if let Some(sess) = q.session.as_ref() {
        // Path 3: walk both session-scoped contexts.
        let claims_ctx = format!("ctx:memory/claims/session/{sess}");
        let episodic_ctx = format!("ctx:memory/episodic/session/{sess}");
        conn.query(
            "select s.statement_id::text as statement_id,
                    s.subject, s.predicate,
                    s.object_iri, s.object_lit,
                    s.context, s.flags,
                    lower(s.tx_time) as tx_lo
               from donto_statement s
              where s.context = any($1::text[])
                and upper(s.tx_time) is null
              order by s.tx_time desc
              limit $2",
            &[&vec![claims_ctx, episodic_ctx], &limit],
        )
        .await
    } else {
        return err(400, "either record_iri or session is required".to_string());
    };

    let rows = match rows_result {
        Ok(r) => r,
        Err(e) => return err(500, format!("db: {e}")),
    };
    let facts: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            // `flags` low bits encode polarity per substrate convention:
            //   0 asserted, 1 negated, 2 absent, 3 unknown.
            let flags: i16 = r.get("flags");
            let polarity = match flags & 0b11 {
                0 => "asserted",
                1 => "negated",
                2 => "absent",
                _ => "unknown",
            };
            json!({
                "statement_id": r.get::<_, String>("statement_id"),
                "subject": r.get::<_, String>("subject"),
                "predicate": r.get::<_, String>("predicate"),
                "object_iri": r.get::<_, Option<String>>("object_iri"),
                "object_lit": r.get::<_, Option<Value>>("object_lit"),
                "context": r.get::<_, String>("context"),
                "polarity": polarity,
                "tx_lo": r.get::<_, chrono::DateTime<chrono::Utc>>("tx_lo"),
            })
        })
        .collect();
    Json(json!({"count": facts.len(), "facts": facts})).into_response()
}

/// GET /explore/stats.json — aggregate overview shown above the
/// holder picker.
///
/// All counts hit overlay tables only (tiny). Counting facts under
/// `ctx:memory%` on the substrate's 39M-row donto_statement is too
/// slow for a UI; if that number is wanted later we can derive it
/// from a substrate matview.
pub async fn stats(State(s): State<Arc<AppState>>) -> Response {
    let conn = match s.pool.get().await {
        Ok(c) => c,
        Err(e) => return err(500, e.to_string()),
    };
    let row = match conn
        .query_one(
            "select
                (select count(*) from donto_x_memory_record) as records,
                (select count(distinct holder_iri)
                   from donto_x_memory_record
                  where holder_iri is not null) as holders,
                (select count(distinct session_iri)
                   from donto_x_memory_record
                  where session_iri is not null) as sessions,
                (select count(*) from donto_x_memory_module where enabled) as modules,
                (select count(*) from donto_x_memory_access) as recall_events",
            &[],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return err(500, e.to_string()),
    };
    Json(json!({
        "records":       row.get::<_, i64>("records"),
        "holders":       row.get::<_, i64>("holders"),
        "sessions":      row.get::<_, i64>("sessions"),
        "modules":       row.get::<_, i64>("modules"),
        "recall_events": row.get::<_, i64>("recall_events"),
    }))
    .into_response()
}

fn err(code: u16, msg: String) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({"error": msg})),
    )
        .into_response()
}
