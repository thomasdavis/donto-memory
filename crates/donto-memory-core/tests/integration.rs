//! Integration tests for donto-memory-core against a live donto substrate
//! and the consumer overlay tables.
//!
//! These tests are *live tests* — they expect:
//!   * dontosrv reachable at `DONTOSRV_URL` (default http://localhost:7879)
//!   * Postgres reachable at `DONTO_DSN`
//!   * The substrate has been migrated through `0001_memory_overlays.sql`
//!     and the overlays registered (i.e. `donto-memory migrate` has run).
//!
//! Tests self-skip if any of the above is unavailable.

use std::collections::BTreeMap;

use donto_memory_core::{
    extract::{ExtractedFact, MemoryExtractor},
    fusion::rrf_fuse,
    module::{register_default_modules, IngestInput},
    overlays,
    substrate::SubstrateClient,
    types::{AccessKind, RecallQuery, RecallRow},
    Settings,
};
use uuid::Uuid;

fn dontosrv_url() -> String {
    std::env::var("DONTOSRV_URL").unwrap_or_else(|_| "http://localhost:7879".into())
}

fn donto_dsn() -> Option<String> {
    std::env::var("DONTO_DSN")
        .or_else(|_| std::env::var("DONTO_MEMORY_DONTO_DSN"))
        .ok()
}

fn unique_id(prefix: &str) -> String {
    format!(
        "{prefix}-{:x}",
        rand_u64()
    )
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        ^ (std::process::id() as u64)
}

async fn live_substrate() -> Option<SubstrateClient> {
    let c = SubstrateClient::new(dontosrv_url()).ok()?;
    c.contract_version().await.ok()?;
    Some(c)
}

async fn live_settings() -> Option<Settings> {
    let dsn = donto_dsn()?;
    let mut s = Settings::default();
    s.dontosrv_url = dontosrv_url();
    s.donto_dsn = Some(dsn);
    s.consumer_iri = "ctx:memory".into();
    s.reconsolidation_coalesce_window_seconds = 0; // never coalesce in tests
    Some(s)
}

// -- substrate client ---------------------------------------------------

#[tokio::test]
async fn substrate_health_returns_known_columns() {
    let Some(c) = live_substrate().await else {
        eprintln!("skip — no substrate");
        return;
    };
    let h = c.substrate_health().await.unwrap();
    assert!(h.get("currently_believed_statements").is_some());
    assert!(h.get("distinct_predicates").is_some());
    assert!(h.get("registered_overlays").is_some());
}

#[tokio::test]
async fn substrate_overlays_endpoint_lists_memory_overlays() {
    let Some(c) = live_substrate().await else {
        eprintln!("skip");
        return;
    };
    let res = c.overlays().await.unwrap();
    let overlays = res
        .get("overlays")
        .and_then(|v| v.as_array())
        .expect("overlays array");
    let iris: Vec<&str> = overlays
        .iter()
        .filter_map(|o| o.get("overlay_iri")?.as_str())
        .collect();
    // donto-memory's overlays should appear here (registered at migrate time).
    for expected in [
        "ctx:memory/overlay/module",
        "ctx:memory/overlay/record",
        "ctx:memory/overlay/access",
        "ctx:memory/overlay/state",
        "ctx:memory/overlay/reconsolidation_queue",
    ] {
        assert!(
            iris.contains(&expected),
            "expected overlay {expected:?} in {iris:?}"
        );
    }
}

#[tokio::test]
async fn contract_floor_handshake_succeeds() {
    let Some(c) = live_substrate().await else {
        eprintln!("skip");
        return;
    };
    c.assert_contract_floor("0.1.0-m10").await.unwrap();
}

#[tokio::test]
async fn contract_floor_rejects_future_required_version() {
    let Some(c) = live_substrate().await else {
        eprintln!("skip");
        return;
    };
    let err = c.assert_contract_floor("9.99.99-future").await.unwrap_err();
    assert!(matches!(
        err,
        donto_memory_core::substrate::SubstrateError::ContractFloor { .. }
    ));
}

// -- module ingest + retrieve (whole pipeline) -------------------------

#[tokio::test]
async fn episodic_round_trip() {
    let Some(s) = live_settings().await else {
        eprintln!("skip — no DSN");
        return;
    };
    let Some(sub) = live_substrate().await else {
        eprintln!("skip — no substrate");
        return;
    };
    let pool = overlays::pool_from_dsn(s.donto_dsn.as_ref().unwrap()).unwrap();
    let reg = register_default_modules();
    let episodic = reg.get("mem:module/episodic").unwrap();

    let holder = unique_id("agent:itest");
    let session = unique_id("itest-episodic");
    let unique_marker = unique_id("itest-marker");
    let text = format!("Integration test memory chunk {unique_marker} for episodic recall.");

    let record = episodic
        .ingest(
            &sub,
            &pool,
            &s.consumer_iri,
            &IngestInput {
                holder: holder.clone(),
                session_id: Some(session.clone()),
                text: text.clone(),
                modality: "model_output".into(),
                subject: None,
                predicate: None,
                object_iri: None,
                object_lit: None,
                source_record_iri: None,
                key: None,
                value: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(record.module_iri, "mem:module/episodic");
    assert!(record.r#ref.statement_id.is_some());

    // The substrate now has the chunk; recall surfaces it.
    let rows = episodic
        .retrieve(
            &sub,
            &s.consumer_iri,
            &RecallQuery {
                holder: holder.clone(),
                action: "read_metadata".into(),
                query: Some(unique_marker.clone()),
                session_id: Some(session.clone()),
                subject: None,
                predicate: None,
                object_iri: None,
                module_iris: None,
                lens_name: None,
                as_of_tx: None,
                polarity: "asserted".into(),
                min_maturity: 0,
                limit: 5,
                permitted_only: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let lit_v = rows[0]
        .object_lit
        .as_ref()
        .and_then(|v| v.get("v"))
        .and_then(|s| s.as_str())
        .unwrap();
    assert!(lit_v.contains(&unique_marker));
}

#[tokio::test]
async fn semantic_claim_links_to_source_record() {
    let Some(s) = live_settings().await else {
        eprintln!("skip");
        return;
    };
    let Some(sub) = live_substrate().await else {
        eprintln!("skip");
        return;
    };
    let pool = overlays::pool_from_dsn(s.donto_dsn.as_ref().unwrap()).unwrap();
    let reg = register_default_modules();
    let episodic = reg.get("mem:module/episodic").unwrap();
    let semantic = reg.get("mem:module/semantic-claim").unwrap();

    let holder = unique_id("agent:itest-sem");
    let session = unique_id("itest-sem");

    // 1. Ingest an episodic chunk so we have a source_record_iri.
    let ep = episodic
        .ingest(
            &sub,
            &pool,
            &s.consumer_iri,
            &IngestInput {
                holder: holder.clone(),
                session_id: Some(session.clone()),
                text: "Background chunk for semantic test.".into(),
                modality: "model_output".into(),
                subject: None,
                predicate: None,
                object_iri: None,
                object_lit: None,
                source_record_iri: None,
                key: None,
                value: None,
            },
        )
        .await
        .unwrap();

    // 2. Ingest a derived semantic claim citing the episodic.
    let sub_iri = unique_id("ex:itest-subject");
    let obj_iri = unique_id("ex:itest-object");
    let sc = semantic
        .ingest(
            &sub,
            &pool,
            &s.consumer_iri,
            &IngestInput {
                holder: holder.clone(),
                session_id: Some(session.clone()),
                text: String::new(),
                modality: "inferred".into(),
                subject: Some(sub_iri.clone()),
                predicate: Some("ex:itest_predicate".into()),
                object_iri: Some(obj_iri.clone()),
                object_lit: None,
                source_record_iri: Some(ep.record_iri.clone()),
                key: None,
                value: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(sc.module_iri, "mem:module/semantic-claim");

    // 3. The substrate now holds a `mem:claim/derived_from` triple.
    let derived = sub
        .recall(
            &holder,
            "read_metadata",
            Some(&sc.r#ref.statement_id.unwrap().to_string()),
            Some("mem:claim/derived_from"),
            None,
            None,
            "asserted",
            0,
            None,
            None,
            None,
            5,
            false,
        )
        .await
        .unwrap();
    assert!(!derived.rows.is_empty(), "derived_from triple missing");
}

#[tokio::test]
async fn preference_supersedes_prior_value() {
    let Some(s) = live_settings().await else {
        eprintln!("skip");
        return;
    };
    let Some(sub) = live_substrate().await else {
        eprintln!("skip");
        return;
    };
    let pool = overlays::pool_from_dsn(s.donto_dsn.as_ref().unwrap()).unwrap();
    let reg = register_default_modules();
    let preference = reg.get("mem:module/preference").unwrap();

    let holder = unique_id("agent:itest-pref");
    let key = "preferred_tone";

    let _r1 = preference
        .ingest(
            &sub,
            &pool,
            &s.consumer_iri,
            &IngestInput {
                holder: holder.clone(),
                session_id: None,
                text: String::new(),
                modality: "model_output".into(),
                subject: None,
                predicate: None,
                object_iri: None,
                object_lit: None,
                source_record_iri: None,
                key: Some(key.into()),
                value: Some("formal".into()),
            },
        )
        .await
        .unwrap();

    let r2 = preference
        .ingest(
            &sub,
            &pool,
            &s.consumer_iri,
            &IngestInput {
                holder: holder.clone(),
                session_id: None,
                text: String::new(),
                modality: "model_output".into(),
                subject: None,
                predicate: None,
                object_iri: None,
                object_lit: None,
                source_record_iri: None,
                key: Some(key.into()),
                value: Some("casual".into()),
            },
        )
        .await
        .unwrap();

    // Both statements live forever — recall returns ≥2 for this subject.
    let rows = preference
        .retrieve(
            &sub,
            &s.consumer_iri,
            &RecallQuery {
                holder: holder.clone(),
                action: "read_metadata".into(),
                query: None,
                session_id: None,
                subject: None,
                predicate: None,
                object_iri: None,
                module_iris: None,
                lens_name: None,
                as_of_tx: None,
                polarity: "asserted".into(),
                min_maturity: 0,
                limit: 10,
                permitted_only: false,
            },
        )
        .await
        .unwrap();
    assert!(
        rows.len() >= 2,
        "both preferences must live; got {} rows for holder {}",
        rows.len(),
        holder
    );

    // The new preference has a supersedes argument edge to the old.
    let new_stmt = r2.r#ref.statement_id.unwrap();
    let conn = pool.get().await.unwrap();
    let argr = conn
        .query(
            "select relation, target_statement_id from donto_argument \
              where source_statement_id = $1 and upper(tx_time) is null",
            &[&new_stmt],
        )
        .await
        .unwrap();
    let relations: Vec<String> = argr.iter().map(|r| r.get(0)).collect();
    assert!(
        relations.contains(&"supersedes".to_string()),
        "supersedes edge missing; got {relations:?}"
    );
}

// -- hot path composer --------------------------------------------------

#[tokio::test]
async fn hot_path_compose_bundle_returns_fused_rows() {
    let Some(s) = live_settings().await else {
        eprintln!("skip");
        return;
    };
    let Some(sub) = live_substrate().await else {
        eprintln!("skip");
        return;
    };
    let pool = overlays::pool_from_dsn(s.donto_dsn.as_ref().unwrap()).unwrap();
    let reg = register_default_modules();
    let episodic = reg.get("mem:module/episodic").unwrap();

    let holder = unique_id("agent:itest-bundle");
    let session = unique_id("itest-bundle");
    let marker = unique_id("itest-bundle-marker");
    let _r = episodic
        .ingest(
            &sub,
            &pool,
            &s.consumer_iri,
            &IngestInput {
                holder: holder.clone(),
                session_id: Some(session.clone()),
                text: format!("Bundle test {marker}"),
                modality: "model_output".into(),
                subject: None,
                predicate: None,
                object_iri: None,
                object_lit: None,
                source_record_iri: None,
                key: None,
                value: None,
            },
        )
        .await
        .unwrap();

    let bundle = donto_memory_core::hot_path::compose_bundle(
        &sub,
        &pool,
        &s.consumer_iri,
        reg,
        &RecallQuery {
            holder: holder.clone(),
            action: "read_metadata".into(),
            query: Some(marker.clone()),
            session_id: Some(session.clone()),
            subject: None,
            predicate: None,
            object_iri: None,
            module_iris: Some(vec!["mem:module/episodic".into()]),
            lens_name: None,
            as_of_tx: None,
            polarity: "asserted".into(),
            min_maturity: 0,
            limit: 5,
            permitted_only: false,
        },
        false, // don't enqueue, keeps the test fast
        s.reconsolidation_coalesce_window_seconds,
    )
    .await
    .unwrap();

    assert!(bundle.row_count >= 1);
    assert_eq!(bundle.holder, holder);
    let any_match = bundle.rows.iter().any(|r| {
        r.object_lit
            .as_ref()
            .and_then(|v| v.get("v"))
            .and_then(|s| s.as_str())
            .map_or(false, |s| s.contains(&marker))
    });
    assert!(any_match, "bundle should contain the marker row");
}

// -- access events + state bump + reconsolidation queue ----------------

#[tokio::test]
async fn recall_records_access_and_state() {
    let Some(s) = live_settings().await else {
        eprintln!("skip");
        return;
    };
    let Some(sub) = live_substrate().await else {
        eprintln!("skip");
        return;
    };
    let pool = overlays::pool_from_dsn(s.donto_dsn.as_ref().unwrap()).unwrap();
    let reg = register_default_modules();
    let episodic = reg.get("mem:module/episodic").unwrap();

    let holder = unique_id("agent:itest-access");
    let session = unique_id("itest-access");
    let marker = unique_id("itest-access-marker");
    let _r = episodic
        .ingest(
            &sub,
            &pool,
            &s.consumer_iri,
            &IngestInput {
                holder: holder.clone(),
                session_id: Some(session.clone()),
                text: format!("Access test {marker}"),
                modality: "model_output".into(),
                subject: None,
                predicate: None,
                object_iri: None,
                object_lit: None,
                source_record_iri: None,
                key: None,
                value: None,
            },
        )
        .await
        .unwrap();

    let bundle = donto_memory_core::hot_path::compose_bundle(
        &sub,
        &pool,
        &s.consumer_iri,
        reg,
        &RecallQuery {
            holder: holder.clone(),
            action: "read_metadata".into(),
            query: Some(marker.clone()),
            session_id: Some(session.clone()),
            subject: None,
            predicate: None,
            object_iri: None,
            module_iris: Some(vec!["mem:module/episodic".into()]),
            lens_name: None,
            as_of_tx: None,
            polarity: "asserted".into(),
            min_maturity: 0,
            limit: 5,
            permitted_only: false,
        },
        true, // enqueue
        s.reconsolidation_coalesce_window_seconds,
    )
    .await
    .unwrap();
    assert!(bundle.row_count >= 1);

    let conn = pool.get().await.unwrap();
    let access_rows = conn
        .query(
            "select count(*)::bigint from donto_x_memory_access \
               where actor_iri = $1",
            &[&holder],
        )
        .await
        .unwrap();
    let access_count: i64 = access_rows[0].get(0);
    assert!(access_count >= 1, "expected an access event; got {access_count}");

    // State row exists for the record.
    let state_rows = conn
        .query(
            "select count(*)::bigint from donto_x_memory_state s \
               join donto_x_memory_record r on r.record_id = s.record_id \
              where r.holder_iri = $1 and upper(s.tx_time) is null",
            &[&holder],
        )
        .await
        .unwrap();
    let state_count: i64 = state_rows[0].get(0);
    assert!(state_count >= 1);

    // Reconsolidation queue row exists.
    let q_rows = conn
        .query(
            "select count(*)::bigint from donto_x_memory_reconsolidation_queue q \
               join donto_x_memory_record r on r.record_id = q.record_id \
              where r.holder_iri = $1 and q.reason = 'recall'",
            &[&holder],
        )
        .await
        .unwrap();
    let q_count: i64 = q_rows[0].get(0);
    assert!(q_count >= 1, "expected a recall enqueue; got {q_count}");
}

// -- sleep-path worker (one pass) --------------------------------------

#[tokio::test]
async fn sleep_path_worker_drains_queue_in_one_pass() {
    let Some(s) = live_settings().await else {
        eprintln!("skip");
        return;
    };
    let Some(sub) = live_substrate().await else {
        eprintln!("skip");
        return;
    };
    let pool = overlays::pool_from_dsn(s.donto_dsn.as_ref().unwrap()).unwrap();
    let reg = register_default_modules();
    let episodic = reg.get("mem:module/episodic").unwrap();

    let holder = unique_id("agent:itest-sleep");
    let session = unique_id("itest-sleep");
    let rec = episodic
        .ingest(
            &sub,
            &pool,
            &s.consumer_iri,
            &IngestInput {
                holder: holder.clone(),
                session_id: Some(session.clone()),
                text: "Sleep-path test chunk.".into(),
                modality: "model_output".into(),
                subject: None,
                predicate: None,
                object_iri: None,
                object_lit: None,
                source_record_iri: None,
                key: None,
                value: None,
            },
        )
        .await
        .unwrap();

    // Explicit enqueue with priority 1.0 so we know the worker picks it.
    overlays::enqueue_reconsolidation(
        &pool,
        rec.record_id,
        "explicit",
        1.0,
        None,
        &serde_json::Value::Null,
        0,
    )
    .await
    .unwrap();

    let unfinished_before: i64 = {
        let c = pool.get().await.unwrap();
        let r = c
            .query_one(
                "select count(*) from donto_x_memory_reconsolidation_queue \
                  where record_id = $1 and completed_at is null",
                &[&rec.record_id],
            )
            .await
            .unwrap();
        r.get(0)
    };
    assert!(unfinished_before >= 1);

    donto_memory_core::sleep_path::run_worker(&s, &sub, &pool, true)
        .await
        .unwrap();

    let unfinished_after: i64 = {
        let c = pool.get().await.unwrap();
        let r = c
            .query_one(
                "select count(*) from donto_x_memory_reconsolidation_queue \
                  where record_id = $1 and completed_at is null",
                &[&rec.record_id],
            )
            .await
            .unwrap();
        r.get(0)
    };
    assert_eq!(
        unfinished_after, 0,
        "worker should drain the queue for this record; {unfinished_after} unfinished"
    );
}

// -- LLM extractor (no live LLM expected) ------------------------------

#[tokio::test]
async fn extractor_returns_none_when_unconfigured() {
    let s = Settings::default(); // no LLM env set
    assert!(MemoryExtractor::from_settings(&s).is_none());
}

// -- fusion edge cases -------------------------------------------------

#[tokio::test]
async fn rrf_empty_inputs_yield_empty_output() {
    let out = rrf_fuse(BTreeMap::new(), 60);
    assert!(out.is_empty());
}

#[tokio::test]
async fn rrf_three_modules_same_row() {
    use chrono::Utc;
    let sid: Uuid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".parse().unwrap();
    let row = RecallRow {
        statement_id: sid,
        subject: "ex:s".into(),
        predicate: "ex:p".into(),
        object_iri: None,
        object_lit: None,
        context: "ctx:t".into(),
        polarity: "asserted".into(),
        maturity: 1,
        valid_lo: None,
        valid_hi: None,
        tx_lo: Utc::now(),
        tx_hi: None,
        resolved_subject: None,
        resolved_object: None,
        effective_actions: BTreeMap::new(),
        action_allowed: true,
        record_iri: None,
        module_iri: Some("mem:test".into()),
        score: None,
        rank: Some(1),
    };
    let mut input = BTreeMap::new();
    input.insert("m1".into(), vec![row.clone()]);
    input.insert("m2".into(), vec![row.clone()]);
    input.insert("m3".into(), vec![row.clone()]);
    let out = rrf_fuse(input, 60);
    assert_eq!(out.len(), 1, "all three modules surface the same row");
    let expected_score = 3.0 / 61.0; // three modules, all rank 1
    let s = out[0].score.unwrap();
    assert!(
        (s - expected_score).abs() < 1e-9,
        "score = {s}, expected ~{expected_score}"
    );
}

// -- extract: ExtractedFact JSON roundtrip -----------------------------

#[tokio::test]
async fn extracted_fact_round_trips_object_iri() {
    let f = ExtractedFact {
        subject: "ex:s".into(),
        predicate: "ex:p".into(),
        object_iri: Some("ex:o".into()),
        object_lit: None,
        confidence: Some(0.91),
        modality: Some("inferred".into()),
        notes: None,
    };
    let s = serde_json::to_string(&f).unwrap();
    let back: ExtractedFact = serde_json::from_str(&s).unwrap();
    assert_eq!(back.subject, "ex:s");
    assert_eq!(back.object_iri.as_deref(), Some("ex:o"));
    assert!(back.object_lit.is_none());
}

#[tokio::test]
async fn extracted_fact_round_trips_object_lit() {
    let f = ExtractedFact {
        subject: "ex:s".into(),
        predicate: "ex:bornInYear".into(),
        object_iri: None,
        object_lit: Some(serde_json::json!({"v": 1979, "dt": "xsd:integer"})),
        confidence: Some(0.7),
        modality: None,
        notes: None,
    };
    let s = serde_json::to_string(&f).unwrap();
    let back: ExtractedFact = serde_json::from_str(&s).unwrap();
    assert_eq!(back.object_lit.unwrap()["v"], 1979);
}

// -- overlays: query hash determinism ---------------------------------

#[test]
fn hash_query_is_deterministic_and_unique() {
    let a = overlays::hash_query("hello world");
    let b = overlays::hash_query("hello world");
    let c = overlays::hash_query("hello world!");
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.len(), 64); // sha256 hex
}

// -- access_kind / module spec wire form -------------------------------

#[test]
fn access_kind_str_round_trip() {
    for k in [
        AccessKind::Retrieved,
        AccessKind::Surfaced,
        AccessKind::Cited,
        AccessKind::Ignored,
        AccessKind::Corrected,
    ] {
        let s: String = serde_json::to_string(&k).unwrap();
        let back: AccessKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, k);
    }
}
