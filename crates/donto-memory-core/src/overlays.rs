//! CRUD over the consumer overlay tables.
//!
//! The overlay tables are owned by donto-memory (M10 §6.1) — the
//! substrate does not write to them. Reads at runtime + writes for
//! access events / state bumps / reconsolidation queue happen here.

use chrono::{DateTime, Utc};
use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;
use thiserror::Error;
use tokio_postgres::types::ToSql;
use tokio_postgres::{NoTls, Row};
use uuid::Uuid;

use crate::types::{AccessKind, MemoryRecord, MemoryRecordRef};

#[derive(Debug, Error)]
pub enum OverlayError {
    /// Wrap tokio-postgres errors with the full DB-side detail
    /// (severity / code / message / hint). The default
    /// `tokio_postgres::Error`'s `Display` impl returns only the
    /// useless string "db error" for any error originating from the
    /// server — losing the SQLSTATE and the message.
    #[error("postgres: {0}")]
    Postgres(String),
    #[error("pool: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),
    #[error("config: {0}")]
    Config(String),
}

impl From<tokio_postgres::Error> for OverlayError {
    fn from(e: tokio_postgres::Error) -> Self {
        if let Some(db) = e.as_db_error() {
            OverlayError::Postgres(format!(
                "{severity} {code}: {message}{detail}{hint}",
                severity = db.severity(),
                code = db.code().code(),
                message = db.message(),
                detail = db.detail().map(|d| format!(" — {d}")).unwrap_or_default(),
                hint = db.hint().map(|h| format!(" (hint: {h})")).unwrap_or_default(),
            ))
        } else {
            OverlayError::Postgres(e.to_string())
        }
    }
}

/// Build a deadpool_postgres pool from a DSN string.
///
/// Tweaks vs deadpool defaults:
///   - `pool_size = 32` (default 10). Every /recall briefly holds
///     ~3 connections (substrate proxy, access event write, audit
///     log write); the default silently timed out the 6th
///     concurrent recall.
///   - `statement_timeout = 10s` (server-side). A slow substrate
///     query (e.g. a context filter that the planner mis-plans
///     against the 39M-row donto_statement table) can otherwise
///     hold a pool connection for minutes and starve every other
///     handler.
///   - `application_name = donto-memory` so pg_stat_activity shows
///     where load is coming from.
pub fn pool_from_dsn(dsn: &str) -> Result<Pool, OverlayError> {
    let pg_cfg = tokio_postgres::Config::from_str(dsn)
        .map_err(|e| OverlayError::Config(e.to_string()))?;
    let mut cfg = Config::new();
    cfg.host = pg_cfg.get_hosts().iter().find_map(|h| match h {
        tokio_postgres::config::Host::Tcp(s) => Some(s.clone()),
        #[allow(unreachable_patterns)]
        _ => None,
    });
    cfg.port = pg_cfg.get_ports().first().copied();
    cfg.user = pg_cfg.get_user().map(str::to_owned);
    cfg.password = pg_cfg
        .get_password()
        .map(|p| String::from_utf8_lossy(p).into_owned());
    cfg.dbname = pg_cfg.get_dbname().map(str::to_owned);
    cfg.application_name = Some("donto-memory".into());
    // Server-side cap. SET statement_timeout takes effect on the
    // next statement; this propagates to every checkout via the
    // `options` connect parameter so the cap is in force from the
    // very first query a connection runs.
    cfg.options = Some("-c statement_timeout=10000".into());
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    cfg.pool = Some(deadpool_postgres::PoolConfig::new(32));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| OverlayError::Config(e.to_string()))
}

pub fn hash_query(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

// -- modules --------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleRow {
    pub module_iri: String,
    pub consumer_iri: String,
    pub form: String,
    pub function: String,
    pub version: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub config: serde_json::Value,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

pub async fn list_modules(pool: &Pool) -> Result<Vec<ModuleRow>, OverlayError> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "select module_iri, consumer_iri, form, function, version, \
                    label, description, config, enabled, \
                    created_at, modified_at \
               from donto_x_memory_module order by enabled desc, module_iri",
            &[],
        )
        .await?;
    Ok(rows.into_iter().map(module_row_from).collect())
}

fn module_row_from(r: Row) -> ModuleRow {
    ModuleRow {
        module_iri: r.get(0),
        consumer_iri: r.get(1),
        form: r.get(2),
        function: r.get(3),
        version: r.get(4),
        label: r.get(5),
        description: r.get(6),
        config: r.get(7),
        enabled: r.get(8),
        created_at: r.get(9),
        modified_at: r.get(10),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn upsert_module(
    pool: &Pool,
    module_iri: &str,
    form: &str,
    function: &str,
    label: Option<&str>,
    description: Option<&str>,
    config: &serde_json::Value,
    version: &str,
    enabled: bool,
) -> Result<(), OverlayError> {
    let c = pool.get().await?;
    c.execute(
        "insert into donto_x_memory_module \
             (module_iri, form, function, label, description, config, version, enabled) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict (module_iri) do update set \
             form = excluded.form, \
             function = excluded.function, \
             label = coalesce(excluded.label, donto_x_memory_module.label), \
             description = coalesce(excluded.description, donto_x_memory_module.description), \
             config = donto_x_memory_module.config || excluded.config, \
             version = excluded.version, \
             enabled = excluded.enabled, \
             modified_at = now()",
        &[
            &module_iri,
            &form,
            &function,
            &label,
            &description,
            config,
            &version,
            &enabled,
        ],
    )
    .await?;
    Ok(())
}

// -- records --------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn create_record(
    pool: &Pool,
    record_iri: &str,
    module_iri: &str,
    r#ref: &MemoryRecordRef,
    holder_iri: Option<&str>,
    session_iri: Option<&str>,
    expected_policy_iri: Option<&str>,
    metadata: &serde_json::Value,
) -> Result<Uuid, OverlayError> {
    let c = pool.get().await?;
    // The substrate dedups identical triples by content_hash, so an
    // ingest of an existing (subject, predicate, object) returns the
    // pre-existing statement_id. We then build the same record_iri,
    // which would violate the unique constraint. ON CONFLICT lets
    // both fresh inserts and repeat-ingest return the same record_id
    // — the second path picks up the row already there.
    let row = c
        .query_one(
            "insert into donto_x_memory_record \
                 (record_iri, module_iri, root_statement, root_frame, root_context, \
                  holder_iri, session_iri, expected_policy_iri, metadata) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             on conflict (record_iri) do update set \
                 metadata = donto_x_memory_record.metadata || excluded.metadata \
             returning record_id",
            &[
                &record_iri,
                &module_iri,
                &r#ref.statement_id,
                &r#ref.frame_id,
                &r#ref.context_iri,
                &holder_iri,
                &session_iri,
                &expected_policy_iri,
                metadata,
            ],
        )
        .await?;
    Ok(row.get(0))
}

pub async fn get_record(
    pool: &Pool,
    record_id: Uuid,
) -> Result<Option<MemoryRecord>, OverlayError> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            "select record_id, record_iri, module_iri, \
                    root_statement, root_frame, root_context, \
                    holder_iri, session_iri, expected_policy_iri, \
                    lower(tx_time) as tx_lo, upper(tx_time) as tx_hi, \
                    metadata \
               from donto_x_memory_record \
              where record_id = $1",
            &[&record_id],
        )
        .await?;
    Ok(row.map(record_from_row))
}

/// Enumerate `root_statement` UUIDs owned by a holder under a
/// module — used by semantic-claim retrieve to filter rows by
/// statement_id (the substrate scope alone leaks across holders
/// who share a session).
pub async fn list_root_statements_for_holder(
    pool: &Pool,
    holder_iri: &str,
    module_iri: &str,
    session_iri: Option<&str>,
) -> Result<std::collections::HashSet<Uuid>, OverlayError> {
    let c = pool.get().await?;
    let rows = match session_iri {
        Some(s) => {
            c.query(
                "select root_statement from donto_x_memory_record
                  where holder_iri = $1 and module_iri = $2 and session_iri = $3
                    and root_statement is not null",
                &[&holder_iri, &module_iri, &s],
            )
            .await?
        }
        None => {
            c.query(
                "select root_statement from donto_x_memory_record
                  where holder_iri = $1 and module_iri = $2
                    and root_statement is not null",
                &[&holder_iri, &module_iri],
            )
            .await?
        }
    };
    Ok(rows.into_iter().map(|r| r.get::<_, Uuid>(0)).collect())
}

/// Enumerate `record_iri` values owned by a holder under a given
/// module — optionally narrowed to a session. Returns a set the
/// caller can use as a "row belongs to me" filter after a substrate
/// recall (the substrate scopes by context, which can be shared
/// across holders, so we need an overlay-side allowlist to prevent
/// cross-holder leakage).
pub async fn list_record_iris_for_holder(
    pool: &Pool,
    holder_iri: &str,
    module_iri: &str,
    session_iri: Option<&str>,
) -> Result<std::collections::HashSet<String>, OverlayError> {
    let c = pool.get().await?;
    let rows = match session_iri {
        Some(s) => {
            c.query(
                "select record_iri from donto_x_memory_record
                  where holder_iri = $1 and module_iri = $2 and session_iri = $3",
                &[&holder_iri, &module_iri, &s],
            )
            .await?
        }
        None => {
            c.query(
                "select record_iri from donto_x_memory_record
                  where holder_iri = $1 and module_iri = $2",
                &[&holder_iri, &module_iri],
            )
            .await?
        }
    };
    Ok(rows.into_iter().map(|r| r.get::<_, String>(0)).collect())
}

/// Enumerate distinct session IRIs known for a holder + module
/// combination. Used by module retrieve() to expand a holder-only
/// recall into an explicit `include: [...]` of all known session
/// contexts (the substrate's `include_descendants` flag does not
/// match prefix-shaped IRIs when the contexts are stored as flat
/// siblings — which is the case for memory contexts).
pub async fn list_sessions_for_holder(
    pool: &Pool,
    holder_iri: &str,
    module_iri: &str,
) -> Result<Vec<String>, OverlayError> {
    let c = pool.get().await?;
    let rows = c
        .query(
            "select distinct session_iri
               from donto_x_memory_record
              where holder_iri = $1
                and module_iri = $2
                and session_iri is not null
              limit 1000",
            &[&holder_iri, &module_iri],
        )
        .await?;
    Ok(rows.into_iter().map(|r| r.get::<_, String>(0)).collect())
}

/// A statement row materialised from `donto_statement`, used by the
/// full-text search path on `/recall`. Mirrors the fields the substrate
/// would normally return, plus the trgm-similarity score we order by.
/// Self-read policy IRI applied to every memory context. The policy
/// row + bulk assignment for existing contexts is seeded by SQL on
/// the production DB; this constant lets future memorize calls insert
/// fresh assignments for any new context they create.
pub const MEMORY_SELF_READ_POLICY: &str = "policy:memory-self-read";

/// Ensure the holder has read_content on the given context. Inserts a
/// `donto_access_assignment` row pointing at the
/// `policy:memory-self-read` capsule, idempotent on the unique
/// `(target_kind, target_id, policy_iri)` constraint. Safe to call
/// on every memorize — the conflict path is a no-op.
pub async fn ensure_memory_self_read_grant(
    pool: &Pool,
    context_iri: &str,
) -> Result<(), OverlayError> {
    let c = pool.get().await?;
    c.execute(
        "insert into donto_access_assignment \
             (target_kind, target_id, policy_iri, assigned_by, notes) \
         values ('context', $1, $2, 'donto-memory:auto', 'memory self-read auto-grant') \
         on conflict (target_kind, target_id, policy_iri) do nothing",
        &[&context_iri, &MEMORY_SELF_READ_POLICY],
    )
    .await?;
    Ok(())
}

#[derive(Debug)]
pub struct StatementHit {
    pub statement_id: Uuid,
    pub subject: String,
    pub predicate: String,
    pub object_iri: Option<String>,
    pub object_lit: Option<serde_json::Value>,
    pub context: String,
    pub tx_lo: chrono::DateTime<chrono::Utc>,
    pub tx_hi: Option<chrono::DateTime<chrono::Utc>>,
    pub flags: i16,
    pub score: f64,
}

/// Tokenize a free-text query into ILIKE patterns. Splits on
/// whitespace, drops empties, lowercases, and wraps each token in
/// `%…%`. Multi-word queries become AND-of-ILIKE per token so
/// "dogs cats" finds rows matching both "dogs" somewhere and "cats"
/// somewhere — rather than the literal substring "dogs cats".
fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("%{}%", t.to_lowercase()))
        .collect()
}

/// Full-text search over `donto_statement`, restricted to statement_ids
/// the caller already vetted as belonging to the holder. Multi-word
/// queries are AND-of-ILIKE per token (each token must match at least
/// one of subject / object_lit / object_iri / predicate). The
/// statement_id whitelist keeps the result set small enough that a
/// sequential ILIKE scan is fine without the gin_trgm indexes.
///
/// Skipped automatically when the owned set is empty.
pub async fn fulltext_search_owned_statements(
    pool: &Pool,
    owned: &std::collections::HashSet<Uuid>,
    query: &str,
    limit: i32,
) -> Result<Vec<StatementHit>, OverlayError> {
    let tokens = tokenize_query(query);
    if owned.is_empty() || tokens.is_empty() {
        return Ok(Vec::new());
    }
    let c = pool.get().await?;
    let ids: Vec<Uuid> = owned.iter().copied().collect();
    // Each token must match somewhere (NOT EXISTS unmatched token in
    // the array). Score is a sum-of-tiers across tokens — every token
    // hit contributes; subject hits weigh more than predicate hits.
    let rows = c
        .query(
            "select statement_id, subject, predicate, object_iri, object_lit, \
                    context, lower(tx_time) as tx_lo, upper(tx_time) as tx_hi, \
                    flags, \
                    coalesce(( \
                      select sum( \
                        case when subject ilike t then 1.0::float8 \
                             when coalesce(object_lit->>'v','') ilike t then 0.8::float8 \
                             when coalesce(object_iri,'') ilike t then 0.6::float8 \
                             when predicate ilike t then 0.4::float8 \
                             else 0.0::float8 end) \
                      from unnest($1::text[]) as t \
                    ), 0.0::float8) as score \
               from donto_statement \
              where statement_id = any($2) \
                and upper(tx_time) is null \
                and not exists ( \
                  select 1 from unnest($1::text[]) as t \
                   where not ( \
                        subject ilike t \
                     or coalesce(object_lit->>'v','') ilike t \
                     or coalesce(object_iri,'') ilike t \
                     or predicate ilike t ) \
                ) \
              order by score desc, lower(tx_time) desc \
              limit $3",
            &[&tokens, &ids, &(limit as i64)],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| StatementHit {
            statement_id: r.get(0),
            subject: r.get(1),
            predicate: r.get(2),
            object_iri: r.get(3),
            object_lit: r.get(4),
            context: r.get(5),
            tx_lo: r.get(6),
            tx_hi: r.get(7),
            flags: r.get(8),
            score: r.get(9),
        })
        .collect())
}

/// Same as `fulltext_search_owned_statements` but for episodic chunks.
/// Episodic records are anchored by `record_iri` as the statement's
/// subject (the chunk text lives in `object_lit->>'v'`), so the
/// whitelist is a set of record IRIs (strings), not statement UUIDs.
pub async fn fulltext_search_owned_episodic(
    pool: &Pool,
    owned_record_iris: &std::collections::HashSet<String>,
    predicate_filter: &str,
    query: &str,
    limit: i32,
) -> Result<Vec<StatementHit>, OverlayError> {
    let tokens = tokenize_query(query);
    if owned_record_iris.is_empty() || tokens.is_empty() {
        return Ok(Vec::new());
    }
    let c = pool.get().await?;
    let iris: Vec<String> = owned_record_iris.iter().cloned().collect();
    // Each token must appear somewhere in the chunk text. AND across
    // tokens via NOT EXISTS unmatched-token. Score sums hits so
    // chunks containing all tokens rank above partial matches when
    // we later relax the AND to OR.
    let rows = c
        .query(
            "select statement_id, subject, predicate, object_iri, object_lit, \
                    context, lower(tx_time) as tx_lo, upper(tx_time) as tx_hi, \
                    flags \
               from donto_statement \
              where predicate = $1 \
                and subject = any($2) \
                and upper(tx_time) is null \
                and not exists ( \
                  select 1 from unnest($3::text[]) as t \
                   where not coalesce(object_lit->>'v','') ilike t \
                ) \
              order by lower(tx_time) desc \
              limit $4",
            &[&predicate_filter, &iris, &tokens, &(limit as i64)],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| StatementHit {
            statement_id: r.get(0),
            subject: r.get(1),
            predicate: r.get(2),
            object_iri: r.get(3),
            object_lit: r.get(4),
            context: r.get(5),
            tx_lo: r.get(6),
            tx_hi: r.get(7),
            flags: r.get(8),
            score: 1.0,
        })
        .collect())
}

pub async fn find_record_by_statement(
    pool: &Pool,
    statement_id: Uuid,
) -> Result<Option<MemoryRecord>, OverlayError> {
    let c = pool.get().await?;
    let row = c
        .query_opt(
            "select record_id, record_iri, module_iri, \
                    root_statement, root_frame, root_context, \
                    holder_iri, session_iri, expected_policy_iri, \
                    lower(tx_time) as tx_lo, upper(tx_time) as tx_hi, \
                    metadata \
               from donto_x_memory_record \
              where root_statement = $1 \
              order by lower(tx_time) desc limit 1",
            &[&statement_id],
        )
        .await?;
    Ok(row.map(record_from_row))
}

fn record_from_row(r: Row) -> MemoryRecord {
    MemoryRecord {
        record_id: r.get(0),
        record_iri: r.get(1),
        module_iri: r.get(2),
        r#ref: MemoryRecordRef {
            statement_id: r.get(3),
            frame_id: r.get(4),
            context_iri: r.get(5),
        },
        holder_iri: r.get(6),
        session_iri: r.get(7),
        expected_policy_iri: r.get(8),
        tx_lo: r.get(9),
        tx_hi: r.get(10),
        metadata: r.get(11),
    }
}

// -- access events --------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn record_access(
    pool: &Pool,
    record_id: Uuid,
    actor_iri: &str,
    access_kind: AccessKind,
    query_hash: Option<&str>,
    rank: Option<i32>,
    score: Option<f64>,
) -> Result<Uuid, OverlayError> {
    let c = pool.get().await?;
    let kind = access_kind.as_str();
    let row = c
        .query_one(
            "insert into donto_x_memory_access \
                 (record_id, actor_iri, query_hash, access_kind, rank, score) \
             values ($1, $2, $3, $4, $5, $6) \
             returning access_id",
            &[&record_id, &actor_iri, &query_hash, &kind, &rank, &score],
        )
        .await?;
    Ok(row.get(0))
}

// -- state (bitemporal append) -------------------------------------------

pub async fn bump_state(
    pool: &Pool,
    record_id: Uuid,
    salience_delta: f64,
) -> Result<(), OverlayError> {
    let mut conn = pool.get().await?;
    let tx = conn.build_transaction().start().await?;
    let prior = tx
        .query_opt(
            "select state_id, salience, recall_count \
               from donto_x_memory_state \
              where record_id = $1 and upper(tx_time) is null",
            &[&record_id],
        )
        .await?;
    let (mut salience, mut recall_count) = (0.0_f64, 0_i64);
    if let Some(p) = prior {
        let state_id: Uuid = p.get(0);
        salience = p.get(1);
        recall_count = p.get(2);
        tx.execute(
            "update donto_x_memory_state \
                set tx_time = tstzrange(lower(tx_time), now(), '[)') \
              where state_id = $1",
            &[&state_id],
        )
        .await?;
    }
    salience += salience_delta;
    recall_count += 1;
    tx.execute(
        "insert into donto_x_memory_state \
             (record_id, salience, recall_count, last_accessed_at, last_modified_at) \
         values ($1, $2, $3, now(), now())",
        &[&record_id, &salience, &recall_count],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

// -- reconsolidation queue ------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn enqueue_reconsolidation(
    pool: &Pool,
    record_id: Uuid,
    reason: &str,
    priority: f64,
    available_at: Option<DateTime<Utc>>,
    payload: &serde_json::Value,
    coalesce_window_seconds: i64,
) -> Result<Option<Uuid>, OverlayError> {
    let c = pool.get().await?;
    let avail = available_at.unwrap_or_else(Utc::now);
    let existing = c
        .query_opt(
            "select queue_id from donto_x_memory_reconsolidation_queue \
              where record_id = $1 and reason = $2 \
                and completed_at is null and claimed_at is null \
                and available_at >= $3::timestamptz - make_interval(secs => $4) \
              order by available_at desc limit 1",
            &[&record_id, &reason, &avail, &(coalesce_window_seconds as f64)],
        )
        .await?;
    if let Some(row) = existing {
        return Ok(Some(row.get(0)));
    }
    let row = c
        .query_one(
            "insert into donto_x_memory_reconsolidation_queue \
                 (record_id, reason, priority, available_at, payload) \
             values ($1, $2, $3, $4, $5) returning queue_id",
            &[&record_id, &reason, &priority, &avail, payload],
        )
        .await?;
    Ok(Some(row.get(0)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimedItem {
    pub queue_id: Uuid,
    pub record_id: Uuid,
    pub reason: String,
    pub priority: f64,
    pub payload: serde_json::Value,
}

pub async fn claim_next_batch(
    pool: &Pool,
    worker_id: &str,
    batch_size: i32,
    claim_ttl_seconds: i64,
) -> Result<Vec<ClaimedItem>, OverlayError> {
    let mut conn = pool.get().await?;
    let tx = conn.build_transaction().start().await?;
    let ttl = claim_ttl_seconds as f64;
    let rows = tx
        .query(
            "with avail as ( \
                 select queue_id from donto_x_memory_reconsolidation_queue \
                  where completed_at is null \
                    and available_at <= now() \
                    and (claimed_at is null or claimed_at < now() - make_interval(secs => $1)) \
                  order by priority desc, available_at asc \
                  limit $2 for update skip locked \
             ) \
             update donto_x_memory_reconsolidation_queue q \
                set claimed_at = now(), claimed_by = $3 \
               from avail \
              where q.queue_id = avail.queue_id \
              returning q.queue_id, q.record_id, q.reason, q.priority, q.payload",
            &[&ttl, &(batch_size as i64), &worker_id],
        )
        .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|r| ClaimedItem {
            queue_id: r.get(0),
            record_id: r.get(1),
            reason: r.get(2),
            priority: r.get(3),
            payload: r.get(4),
        })
        .collect())
}

pub async fn complete_queue_item(pool: &Pool, queue_id: Uuid) -> Result<(), OverlayError> {
    let c = pool.get().await?;
    c.execute(
        "update donto_x_memory_reconsolidation_queue \
            set completed_at = now() where queue_id = $1",
        &[&queue_id],
    )
    .await?;
    Ok(())
}

// -- raw migration runner ------------------------------------------------

/// Apply every SQL file in `dir` (lex order) against `dsn`. Returns
/// the count applied. Designed for `donto-memory migrate`.
pub async fn run_migrations(dsn: &str, dir: &std::path::Path) -> Result<i32, OverlayError> {
    let pool = pool_from_dsn(dsn)?;
    let c = pool.get().await?;
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| OverlayError::Config(format!("read_dir {dir:?}: {e}")))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map_or(false, |x| x.to_ascii_lowercase() == "sql")
        })
        .collect();
    entries.sort_by_key(|e| e.path());
    let mut applied = 0;
    for entry in entries {
        let sql = std::fs::read_to_string(entry.path())
            .map_err(|e| OverlayError::Config(format!("read {:?}: {e}", entry.path())))?;
        tracing::info!(file = ?entry.path().file_name(), "applying migration");
        let _ = c.batch_execute(&sql).await?;
        applied += 1;
    }
    // Make sure the planner has stats on every memory overlay table.
    // Autovacuum's threshold for ANALYZE is `50 + 0.1 * n_rows`, so on
    // a small table (e.g. donto_x_memory_record with a few thousand
    // records) it can be hours/days before autoanalyze fires for the
    // first time. Until then the planner runs on no stats and may
    // pick seq scans over a perfectly good btree index. Running
    // ANALYZE here gives every fresh install good plans from row 1
    // and costs nothing on a healthy DB (analyzes tiny tables).
    if let Err(e) = c
        .batch_execute(
            "analyze donto_x_memory_module;
             analyze donto_x_memory_state;
             analyze donto_x_memory_access;
             analyze donto_x_memory_record;
             analyze donto_x_memory_job_log;
             analyze donto_x_memory_reconsolidation_queue;",
        )
        .await
    {
        // Don't fail the migrate just because ANALYZE hit a missing
        // table — older deploys may not have every table yet.
        tracing::warn!(error = %e, "post-migrate ANALYZE warning (non-fatal)");
    }
    Ok(applied)
}

/// Register the donto-memory overlays via the substrate's
/// `donto_overlay_register` SQL function. Idempotent.
pub async fn register_overlays(
    dsn: &str,
    consumer_iri: &str,
) -> Result<i32, OverlayError> {
    let pool = pool_from_dsn(dsn)?;
    let c = pool.get().await?;
    let specs: &[(&str, &str, &str, &str)] = &[
        (
            "ctx:memory/overlay/module",
            "donto_x_memory_module",
            "module_iri",
            "Registered memory modules (donto-memory plugins).",
        ),
        (
            "ctx:memory/overlay/record",
            "donto_x_memory_record",
            "record_id",
            "Memory records anchored to substrate primary keys.",
        ),
        (
            "ctx:memory/overlay/access",
            "donto_x_memory_access",
            "record_id",
            "Append-only memory access events.",
        ),
        (
            "ctx:memory/overlay/state",
            "donto_x_memory_state",
            "record_id",
            "Bitemporal-versioned per-record recall state.",
        ),
        (
            "ctx:memory/overlay/reconsolidation_queue",
            "donto_x_memory_reconsolidation_queue",
            "record_id",
            "Sleep-path reconsolidation queue items.",
        ),
        (
            "ctx:memory/overlay/job_log",
            "donto_x_memory_job_log",
            "job_id",
            "Per-request audit log (memorize, recall, ingest).",
        ),
    ];
    let mut count = 0;
    for (iri, table, owns_key, desc) in specs {
        let bitemporal = true;
        let inheritance = "from_target";
        // donto_overlay_register signature:
        //   (overlay_iri, consumer_iri, table_name, owns_key,
        //    policy_inherits, fixed_policy, bitemporal, description, registered_by)
        let row = c
            .query(
                "select donto_overlay_register($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    iri,
                    &consumer_iri,
                    table,
                    owns_key,
                    &inheritance,
                    &Option::<&str>::None,
                    &bitemporal,
                    desc,
                    &"donto-memory:cli",
                ],
            )
            .await;
        match row {
            Ok(_) => {
                count += 1;
                tracing::info!(overlay = iri, "registered");
            }
            Err(e) => {
                let msg = format!("{e:?}");
                if msg.contains("already registered") {
                    tracing::info!(overlay = iri, "already registered");
                } else {
                    return Err(OverlayError::from(e));
                }
            }
        }
    }
    Ok(count)
}

#[allow(dead_code)]
fn _sql_arg_dummy(_v: &(dyn ToSql + Sync)) {}
