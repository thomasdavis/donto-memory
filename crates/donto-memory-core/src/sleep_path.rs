//! Sleep-path worker — drains the reconsolidation queue.

use std::time::Duration;

use deadpool_postgres::Pool;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::delta::{ArgumentRelation, DontoDelta, DontoDeltaOp};
use crate::module::ModuleError;
use crate::overlays;
use crate::overlays::ClaimedItem;
use crate::substrate::SubstrateClient;
use crate::Settings;

/// Run the worker loop. Set `stop_after_one_pass=true` in tests.
pub async fn run_worker(
    settings: &Settings,
    substrate: &SubstrateClient,
    pool: &Pool,
    stop_after_one_pass: bool,
) -> Result<(), ModuleError> {
    let worker_id = format!(
        "donto-memory-worker:{}",
        hostname::get().unwrap_or_default().to_string_lossy()
    );
    info!(worker_id = %worker_id, "sleep worker starting");
    let mut last_prune = std::time::Instant::now() - Duration::from_secs(86_400);
    loop {
        let items = overlays::claim_next_batch(
            pool,
            &worker_id,
            settings.worker_batch_size,
            settings.worker_claim_ttl_seconds,
        )
        .await?;
        if items.is_empty() {
            // Idle tick is a good moment to run the once-a-day prune.
            if settings.job_log_retention_days > 0
                && last_prune.elapsed() >= Duration::from_secs(3600)
            {
                if let Err(e) = prune_job_log(pool, settings.job_log_retention_days).await {
                    warn!(error = %e, "job-log prune failed");
                }
                last_prune = std::time::Instant::now();
            }
            if stop_after_one_pass {
                return Ok(());
            }
            sleep(Duration::from_secs_f64(settings.worker_poll_interval_seconds)).await;
            continue;
        }
        info!(count = items.len(), "claimed batch");
        for item in items {
            if let Err(e) = process_one(substrate, pool, &item).await {
                warn!(queue_id = %item.queue_id, error = %e, "process_one failed");
            }
        }
        if stop_after_one_pass {
            return Ok(());
        }
    }
}

/// Drop audit rows older than `retention_days`. Returns the count
/// pruned. Idempotent; safe to call at any cadence.
async fn prune_job_log(pool: &Pool, retention_days: i64) -> Result<u64, overlays::OverlayError> {
    let c = pool.get().await?;
    let n = c
        .execute(
            "delete from donto_x_memory_job_log
             where created_at < now() - ($1 || ' days')::interval",
            &[&retention_days.to_string()],
        )
        .await?;
    if n > 0 {
        info!(pruned = n, retention_days, "job_log retention pruned");
    }
    Ok(n)
}

async fn process_one(
    substrate: &SubstrateClient,
    pool: &Pool,
    item: &ClaimedItem,
) -> Result<(), ModuleError> {
    let Some(record) = overlays::get_record(pool, item.record_id).await? else {
        warn!(record_id = %item.record_id, "record missing — completing queue item");
        overlays::complete_queue_item(pool, item.queue_id).await?;
        return Ok(());
    };

    // Build a small cluster: rows under the record's anchor context.
    let scope = match &record.r#ref.context_iri {
        Some(c) => Some(serde_json::json!({"include": [c]})),
        None => record
            .holder_iri
            .as_ref()
            .map(|h| serde_json::json!({"include": [format!("ctx:memory/holder/{}", h)]})),
    };

    let cluster = substrate
        .recall(
            record.holder_iri.as_deref().unwrap_or("agent:anonymous"),
            "read_metadata",
            None,
            None,
            None,
            scope.as_ref(),
            "asserted",
            0,
            None,
            None,
            None,
            50,
            false,
        )
        .await?;

    let context = record
        .r#ref
        .context_iri
        .clone()
        .unwrap_or_else(|| format!("ctx:memory/sleep/{}", record.record_id));

    let delta = default_reflect(&record.record_id, &item.reason, &cluster.rows, &context);
    apply_delta(substrate, &delta).await?;

    overlays::complete_queue_item(pool, item.queue_id).await?;
    info!(
        queue_id = %item.queue_id,
        record_id = %record.record_id,
        ops = delta.ops.len(),
        "processed"
    );
    Ok(())
}

/// Substrate-only reflection. Emits potentially_same edges for cluster
/// rows sharing subject+predicate, plus a ScheduleReview obligation
/// so reviewers see the contradiction frontier light up.
fn default_reflect(
    _record_id: &uuid::Uuid,
    _reason: &str,
    cluster: &[crate::types::RecallRow],
    context: &str,
) -> DontoDelta {
    let mut delta = DontoDelta::new("donto-memory:default_reflect");
    if cluster.is_empty() {
        return delta;
    }
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<(String, String), Vec<&crate::types::RecallRow>> = BTreeMap::new();
    for row in cluster {
        buckets
            .entry((row.subject.clone(), row.predicate.clone()))
            .or_default()
            .push(row);
    }
    for ((subject, predicate), rows) in buckets {
        if rows.len() < 2 {
            continue;
        }
        let a = rows[0];
        let b = rows[1];
        delta.push(DontoDeltaOp::AddArgument {
            source_statement_id: a.statement_id,
            target_statement_id: b.statement_id,
            relation: ArgumentRelation::PotentiallySame,
            context: context.to_string(),
            strength: Some(0.5),
            evidence: Some(serde_json::json!({
                "reason": "cluster_co_occurrence",
                "subject": subject,
                "predicate": predicate,
            })),
        });
        delta.push(DontoDeltaOp::ScheduleReview {
            statement_id: a.statement_id,
            obligation_kind: "needs_contradiction_review".into(),
            priority: 0.5,
            rationale: Some(format!(
                "cluster has >1 statement for ({subject}, {predicate})"
            )),
        });
    }
    delta
}

/// Apply a [`DontoDelta`] via dontosrv HTTP. Best-effort per op.
pub async fn apply_delta(
    substrate: &SubstrateClient,
    delta: &DontoDelta,
) -> Result<(), ModuleError> {
    for op in &delta.ops {
        if let Err(e) = apply_op(substrate, op).await {
            warn!(error = %e, op = ?std::mem::discriminant(op), "delta op failed");
        }
    }
    Ok(())
}

async fn apply_op(substrate: &SubstrateClient, op: &DontoDeltaOp) -> Result<(), ModuleError> {
    match op {
        DontoDeltaOp::AssertClaim {
            subject,
            predicate,
            object_iri,
            object_lit,
            context,
            polarity,
            maturity,
            valid_from,
            valid_to,
            ..
        } => {
            let lit_json = object_lit.as_ref().map(|l| serde_json::to_value(l).unwrap());
            substrate
                .assert_statement(
                    subject,
                    predicate,
                    object_iri.as_deref(),
                    lit_json.as_ref(),
                    context,
                    polarity,
                    *maturity,
                    *valid_from,
                    *valid_to,
                )
                .await?;
        }
        DontoDeltaOp::AddArgument {
            source_statement_id,
            target_statement_id,
            relation,
            context,
            strength,
            evidence,
        } => {
            substrate
                .add_argument(
                    *source_statement_id,
                    *target_statement_id,
                    relation.as_str(),
                    context,
                    *strength,
                    evidence.as_ref(),
                )
                .await?;
        }
        DontoDeltaOp::CloseTxTime { statement_id, .. } => {
            substrate.retract(*statement_id).await?;
        }
        DontoDeltaOp::ScheduleReview { .. }
        | DontoDeltaOp::UpdateConfidence { .. }
        | DontoDeltaOp::AssertFrame { .. }
        | DontoDeltaOp::AddIdentityEdge { .. }
        | DontoDeltaOp::CloseValidTime { .. }
        | DontoDeltaOp::LinkDerivedArtifact { .. } => {
            // Deferred to follow-on M11 work; logged but not applied.
            tracing::info!("delta op deferred (not yet HTTP-wrapped)");
        }
    }
    Ok(())
}

// `hostname` isn't a direct dependency; emulate the bit we need.
mod hostname {
    use std::ffi::OsString;
    use std::process::Command;
    pub fn get() -> std::io::Result<OsString> {
        let out = Command::new("hostname").output().map(|o| o.stdout).unwrap_or_default();
        let s = String::from_utf8_lossy(&out).trim().to_string();
        Ok(OsString::from(if s.is_empty() { "unknown".to_string() } else { s }))
    }
}
