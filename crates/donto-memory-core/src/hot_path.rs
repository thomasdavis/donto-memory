//! Hot path — deterministic recall composer.

use std::collections::BTreeMap;

use deadpool_postgres::Pool;
use futures::future::join_all;
use serde_json::json;
use tracing::warn;

use crate::fusion::rrf_fuse;
use crate::module::ModuleError;
use crate::overlays;
use crate::substrate::SubstrateClient;
use crate::types::{AccessKind, MemoryEvidenceBundle, RecallQuery, RecallRow};
use crate::ModuleRegistry;

/// Compose a Memory Evidence Bundle in one call.
pub async fn compose_bundle(
    substrate: &SubstrateClient,
    pool: &Pool,
    consumer_iri: &str,
    registry: &ModuleRegistry,
    query: &RecallQuery,
    enqueue_reconsolidation: bool,
    coalesce_window_seconds: i64,
) -> Result<MemoryEvidenceBundle, ModuleError> {
    // 1. Select modules.
    let selected: Vec<_> = if let Some(iris) = &query.module_iris {
        iris.iter().filter_map(|i| registry.get(i)).collect()
    } else {
        registry.all()
    };
    if selected.is_empty() {
        return Ok(MemoryEvidenceBundle {
            holder: query.holder.clone(),
            action: query.action.clone(),
            lens: query.lens_name.clone(),
            as_of: query.as_of_tx,
            rows: Vec::new(),
            row_count: 0,
            modules_used: Vec::new(),
            policy_report: json!({}),
        });
    }
    let modules_used: Vec<String> =
        selected.iter().map(|m| m.spec().module_iri.clone()).collect();

    // 2. Fan out.
    let futures = selected.iter().map(|m| async move {
        (
            m.spec().module_iri.clone(),
            m.retrieve(substrate, pool, consumer_iri, query).await,
        )
    });
    let mut per_module: BTreeMap<String, Vec<RecallRow>> = BTreeMap::new();
    for (iri, res) in join_all(futures).await {
        match res {
            Ok(rows) => {
                per_module.insert(iri, rows);
            }
            Err(e) => {
                warn!(module = %iri, error = %e, "module retrieve failed");
                per_module.insert(iri, Vec::new());
            }
        }
    }

    // 3. Fuse.
    let mut fused = rrf_fuse(per_module, 60);
    if query.permitted_only {
        fused.retain(|r| r.action_allowed);
    }
    let limit = query.limit.max(0) as usize;
    fused.truncate(limit);

    // 4. Side effects: access + state bump + reconsolidation enqueue.
    //
    // Previously this ran 4 sequential round-trips PER row
    // (find_record → record_access → bump_state → enqueue), making
    // a 10-row recall 40 sequential queries. We now fan out across
    // rows: each row's chain stays sequential (record_access needs
    // the record_id from find_record), but rows run concurrently
    // via futures::join_all. 10 rows × pool of 32 = comfortable.
    if !fused.is_empty() {
        let q_hash = overlays::hash_query(
            query
                .query
                .as_deref()
                .or(query.subject.as_deref())
                .or(query.predicate.as_deref())
                .unwrap_or(""),
        );

        let futures = fused.iter().enumerate().map(|(idx, row)| {
            let q_hash = q_hash.clone();
            let holder = query.holder.clone();
            let stmt_id = row.statement_id;
            let rank = row.rank;
            let score = row.score;
            async move {
                let rec = match overlays::find_record_by_statement(pool, stmt_id).await {
                    Ok(Some(r)) => r,
                    Ok(None) => return (idx, None),
                    Err(e) => {
                        warn!(error = %e, "find_record_by_statement failed");
                        return (idx, None);
                    }
                };
                if let Err(e) = overlays::record_access(
                    pool,
                    rec.record_id,
                    &holder,
                    AccessKind::Retrieved,
                    Some(&q_hash),
                    rank,
                    score,
                )
                .await
                {
                    warn!(record = %rec.record_id, error = %e, "record_access failed");
                }
                if let Err(e) = overlays::bump_state(pool, rec.record_id, 0.1).await {
                    warn!(record = %rec.record_id, error = %e, "bump_state failed");
                }
                if enqueue_reconsolidation {
                    let payload = json!({});
                    if let Err(e) = overlays::enqueue_reconsolidation(
                        pool,
                        rec.record_id,
                        "recall",
                        score.unwrap_or(0.0),
                        None,
                        &payload,
                        coalesce_window_seconds,
                    )
                    .await
                    {
                        warn!(record = %rec.record_id, error = %e, "enqueue_reconsolidation failed");
                    }
                }
                (idx, Some(rec.record_iri))
            }
        });
        let results = join_all(futures).await;
        for (idx, record_iri) in results {
            if let Some(iri) = record_iri {
                fused[idx].record_iri = Some(iri);
            }
        }
    }

    let row_count = fused.len() as i32;
    Ok(MemoryEvidenceBundle {
        holder: query.holder.clone(),
        action: query.action.clone(),
        lens: query.lens_name.clone(),
        as_of: query.as_of_tx,
        rows: fused,
        row_count,
        modules_used,
        policy_report: json!({
            "permitted_only": query.permitted_only,
            "default_action": query.action,
        }),
    })
}
