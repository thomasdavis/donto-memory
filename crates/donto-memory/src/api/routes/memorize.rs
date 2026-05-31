//! POST /memorize — store text as episodic, then LLM-extract semantic claims.
//! POST /memorize/batch — same flow over a list of items.
//!
//! This is the agent-facing "save memory" entrypoint. The flow:
//!
//!   1. Always store the raw text via the episodic module.
//!   2. If the LLM is configured, call it to extract ontological
//!      statements about the text.
//!   3. Each extracted fact becomes a semantic-claim record, with
//!      `source_record_iri` pointing at the episodic chunk so
//!      donto's argument graph + provenance trace can follow the
//!      chain back to the source.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use donto_memory_core::extract::{ExtractError, ExtractedFact, MemoryExtractor};
use donto_memory_core::module::{register_default_modules, IngestInput, MemoryModuleArc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;
use uuid::Uuid;

use crate::api::extract::JsonReq;
use crate::api::job_log;
use crate::api::AppState;

#[derive(Debug, Deserialize, Serialize)]
pub struct MemorizeReq {
    pub holder: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub text: String,
    #[serde(default = "default_modality")]
    pub modality: String,
    /// If false, skip the LLM extraction step and only store as
    /// episodic.
    #[serde(default = "default_true")]
    pub extract: bool,
    /// Override the runtime's default extraction mode.
    /// - `single` — one LLM call (~20-30 facts, ~5-10 s).
    /// - `exhaustive` — five fixed apertures in parallel
    ///   (~100+ facts, ~30-60 s, ~5× tokens).
    /// - `deep` — N sequential passes (default 3, max 10). Each pass
    ///   sees prior facts and is asked to find new angles without a
    ///   rigid per-pass lens. Higher quality + dedup at end. Slower
    ///   than `exhaustive` but no parallel cost spikes.
    ///
    /// Defaults to `DONTO_MEMORY_EXTRACT_MODE` (which itself defaults
    /// to `exhaustive`).
    #[serde(default)]
    pub mode: Option<String>,
    /// For `mode = "deep"`: how many sequential passes to run. Clamped
    /// to [1, 10]. Defaults to 3 when omitted.
    #[serde(default)]
    pub passes: Option<u32>,
    /// If true, the route returns 202 immediately after the episodic
    /// chunk is stored and runs LLM extraction in a background tokio
    /// task. If false, the route waits for extraction to finish (the
    /// original behaviour). If omitted, defaults to true for slow
    /// modes (`deep`, `exhaustive`) and false for `single` — so
    /// caller-side timeouts don't trip on multi-minute extractions.
    #[serde(default, rename = "async")]
    pub r#async: Option<bool>,
    /// Optional images to attach. Each entry is either an http(s)
    /// URL the LLM provider can fetch, or a `data:image/png;base64,…`
    /// data URL with the bytes inline. When non-empty, the extractor
    /// switches to OpenAI multimodal message format and (if set)
    /// uses `DONTO_MEMORY_LLM_VISION_MODEL` instead of the default.
    #[serde(default)]
    pub images: Vec<String>,

    /// Set by the Temporal `memorize_activity` when it re-submits a
    /// deferred request with `async:false`. Carries the original
    /// queue_id (= Temporal workflow id) so the synchronous completion
    /// audit row can stamp it and be labelled as an async completion,
    /// keeping /jobs correlation intact. Never set by external callers.
    #[serde(default)]
    pub queue_id: Option<Uuid>,

    /// Pre-extracted facts supplied by an upstream extractor (e.g. the
    /// OpenCode-agent memory worker). When non-empty, donto-memory
    /// SKIPS its own LLM extraction and ingests these directly — the
    /// episodic chunk is still stored and the self-read grant still
    /// applied. Lets the heavy agentic extraction happen out of process
    /// while donto-memory remains the single substrate-ingest authority.
    #[serde(default)]
    pub facts: Option<Vec<ExtractedFact>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MemorizeBatchReq {
    pub items: Vec<MemorizeReq>,
}

#[derive(Debug, Serialize)]
pub struct MemorizeResp {
    pub holder: String,
    pub session_id: Option<String>,
    pub episodic_record_id: Uuid,
    pub episodic_record_iri: String,
    pub extracted: bool,
    pub extract_mode: Option<String>,
    pub facts_extracted: usize,
    pub facts_ingested: usize,
    pub dedup_collisions: u32,
    pub semantic_record_ids: Vec<Uuid>,
    pub model: Option<String>,
    pub usage: Option<donto_memory_core::extract::ExtractionUsage>,
    pub aperture_yields: Vec<donto_memory_core::extract::ApertureYield>,
    /// The exhaustive list of ontological statements the LLM
    /// produced from this memorize call. Included verbatim so callers
    /// can review what the substrate now believes without a follow-up
    /// query. Always present; empty when extraction is disabled or
    /// the LLM is not configured.
    pub facts: Vec<ExtractedFact>,
    pub elapsed_ms: u64,
    pub warnings: Vec<String>,
}

fn default_modality() -> String {
    "model_output".to_string()
}
fn default_true() -> bool {
    true
}

/// Replace each `images[i]` entry with a short preview so audit-log
/// rows don't carry 50+ KB of base64 per call. Preserves http URLs
/// (they're tiny) and truncates `data:` URLs to mime + first 64 chars.
fn redact_images(mut req: Value) -> Value {
    if let Some(images) = req.get_mut("images").and_then(|v| v.as_array_mut()) {
        for img in images.iter_mut() {
            if let Some(s) = img.as_str() {
                if s.starts_with("data:") {
                    // Keep "data:<mime>;base64,<first 64 chars>…(<n bytes>)"
                    let prefix: String = s.chars().take(96).collect();
                    *img = Value::String(format!("{prefix}… ({} bytes total)", s.len()));
                }
                // http(s) URLs stay verbatim — they're small.
            }
        }
    }
    req
}

/// Decide whether this request should be processed asynchronously. If
/// `async` is set explicitly, that wins. Otherwise slow modes
/// (deep / exhaustive) default to async so callers don't trip over
/// HTTP timeouts (Cloudflare's 100s, axum default, etc.). Single mode
/// stays sync — it's fast enough.
fn should_defer(req: &MemorizeReq, default_mode: &str) -> bool {
    if let Some(b) = req.r#async {
        return b;
    }
    let mode = req.mode.as_deref().unwrap_or(default_mode).to_lowercase();
    matches!(
        mode.as_str(),
        "deep" | "sequential" | "iterative" | "exhaustive" | "multi" | "apertures"
            | "opencode" | "agentic"
    )
}

/// Start a durable `MemorizeWorkflow` by POSTing to the Temporal
/// enqueue gateway (the Python memory-worker's aiohttp `/enqueue`).
/// The full, un-redacted request is forwarded (the activity needs real
/// image bytes), with `queue_id` injected so the eventual synchronous
/// re-submission stamps it back onto the completion audit row.
///
/// Returns `Ok(())` if the workflow was started or already exists
/// (HTTP 2xx, or 409 = duplicate workflow id). Returns `Err` on any
/// transport error or other status — the caller then falls back to an
/// in-process task so the request is never dropped.
async fn try_temporal_enqueue(
    enqueue_url: &str,
    queue_id: Uuid,
    req: &MemorizeReq,
) -> Result<(), String> {
    let mut req_value = serde_json::to_value(req).map_err(|e| e.to_string())?;
    if let Some(obj) = req_value.as_object_mut() {
        obj.insert("queue_id".into(), json!(queue_id));
    }
    let payload = json!({ "workflow_id": queue_id, "req": req_value });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(enqueue_url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("enqueue request failed: {e}"))?;

    let status = resp.status();
    if status.is_success() || status.as_u16() == 409 {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!(
            "enqueue HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        ))
    }
}

pub async fn memorize(
    State(s): State<Arc<AppState>>,
    JsonReq(req): JsonReq<MemorizeReq>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    // Redact image bytes before audit-logging: a single base64 PNG
    // can be 50+ KB per memorize call, and at the 220 KB/day growth
    // rate this would fill the job log table fast. Keep only a
    // truncated preview + a count.
    let request_json = redact_images(serde_json::to_value(&req).unwrap_or_else(|_| json!({})));

    if should_defer(&req, &s.settings.extract_mode) {
        // Deferred path: return 202 immediately and run extraction out
        // of band. Preferred mechanism is the durable Temporal queue
        // (survives API restarts). If the Temporal enqueue gateway is
        // unreachable we fall back to an in-process tokio task so a
        // request is never dropped — just not restart-durable.
        let queue_id = Uuid::new_v4();

        // Try the durable queue first.
        let durable = match try_temporal_enqueue(
            &s.settings.memorize_enqueue_url,
            queue_id,
            &req,
        )
        .await
        {
            Ok(()) => {
                tracing::info!(
                    queue_id = %queue_id,
                    holder = %req.holder,
                    mode = req.mode.as_deref().unwrap_or(&s.settings.extract_mode),
                    "memorize enqueued to Temporal (durable)"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    queue_id = %queue_id,
                    error = %e,
                    "Temporal enqueue failed; falling back to in-process tokio task (non-durable)"
                );
                false
            }
        };

        let queued_resp = json!({
            "status": "queued",
            "queue_id": queue_id,
            "durable": durable,
            "holder": req.holder,
            "session_id": req.session_id,
            "extract_mode": req.mode.clone().unwrap_or_else(|| s.settings.extract_mode.clone()),
            "passes": req.passes,
            "note": if durable {
                "extraction queued on Temporal (durable). Poll /recall, /jobs, or the Temporal UI for results."
            } else {
                "extraction running in an in-process task (Temporal gateway unreachable). Poll /recall or /jobs for results."
            },
        });
        // The "queued" audit row lets the /jobs list show the work as
        // accepted-but-pending. The `durable` flag tells the startup
        // orphan-sweep whether this row can actually be lost to a
        // restart (only non-durable fallback tasks can).
        let resp_for_log = queued_resp.clone();
        job_log::record_job(
            &s.pool,
            &s.settings.consumer_iri,
            "POST /memorize (queued)",
            Some(&req.holder),
            req.session_id.as_deref(),
            202,
            started.elapsed().as_millis() as u64,
            &request_json,
            &resp_for_log,
            job_log::JobMetrics::default(),
        )
        .await;

        // Durable path: Temporal owns execution now. The activity will
        // re-submit to /memorize (async:false) and that synchronous call
        // writes the (async) completion row. Nothing more to do here.
        if durable {
            return (StatusCode::ACCEPTED, Json(queued_resp)).into_response();
        }

        // Fallback path: run in-process (non-durable).
        let s_async = Arc::clone(&s);
        let request_async = request_json.clone();
        tokio::spawn(async move {
            // Serialize background memorize tasks — one at a time.
            // The user explicitly asked for serial extraction so
            // multiple Discord messages don't trample on each other or
            // stack up parallel LLM calls.
            let _guard = s_async.async_memorize_lock.lock().await;
            let task_started = std::time::Instant::now();
            let (status_code, mut resp_json) = match memorize_one(&s_async, &req).await {
                Ok(resp) => (200u16, serde_json::to_value(&resp).unwrap_or_else(|_| json!({}))),
                Err(MemorizeError::BadInput(msg)) => (400u16, json!({"error": msg})),
                Err(other) => (500u16, json!({"error": other.to_string()})),
            };
            // Stamp queue_id into the (async)/(async-failed) audit row.
            // The orphan-recovery SQL in main.rs::mark_orphaned_queued_rows
            // joins (queued) to (async) via `response->>'queue_id'`. Without
            // this stamp every completed task looks unmatched on restart,
            // and the next startup would re-mark already-completed work
            // as (lost). Already shipped (lost) rows can't be retroactively
            // un-marked, but new completions land with the right link.
            if let Some(obj) = resp_json.as_object_mut() {
                obj.insert("queue_id".into(), json!(queue_id));
            }
            let elapsed_ms = task_started.elapsed().as_millis() as u64;
            let metrics = if status_code < 400 {
                job_log::metrics_from_memorize(&resp_json)
            } else {
                job_log::JobMetrics {
                    error: resp_json.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    ..Default::default()
                }
            };
            // Distinct label for failed async completions so /jobs
            // surfaces them at a glance instead of mixing 4xx/5xx
            // into the same column as the 200 successes.
            let endpoint_label = if status_code < 400 {
                "POST /memorize (async)"
            } else {
                "POST /memorize (async-failed)"
            };
            job_log::record_job(
                &s_async.pool,
                &s_async.settings.consumer_iri,
                endpoint_label,
                Some(&req.holder),
                req.session_id.as_deref(),
                status_code,
                elapsed_ms,
                &request_async,
                &resp_json,
                metrics,
            )
            .await;
        });

        return (StatusCode::ACCEPTED, Json(queued_resp)).into_response();
    }

    let (status_code, mut resp_json) = match memorize_one(&s, &req).await {
        Ok(resp) => (200u16, serde_json::to_value(&resp).unwrap_or_else(|_| json!({}))),
        Err(MemorizeError::BadInput(msg)) => (400u16, json!({"error": msg})),
        Err(other) => (500u16, json!({"error": other.to_string()})),
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let metrics = if status_code == 200 {
        let mut m = job_log::metrics_from_memorize(&resp_json);
        if status_code >= 400 {
            m.error = resp_json.get("error").and_then(|v| v.as_str()).map(|s| s.to_string());
        }
        m
    } else {
        job_log::JobMetrics {
            error: resp_json.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()),
            ..Default::default()
        }
    };
    // When this synchronous call is the Temporal activity re-submitting a
    // deferred request (queue_id present), stamp the queue_id into the
    // response and label the audit row as an async completion so /jobs
    // correlates it back to the original (queued) row.
    let endpoint_label = if req.queue_id.is_some() {
        if let Some(obj) = resp_json.as_object_mut() {
            obj.insert("queue_id".into(), json!(req.queue_id));
        }
        if status_code < 400 { "POST /memorize (async)" } else { "POST /memorize (async-failed)" }
    } else {
        "POST /memorize"
    };
    job_log::record_job(
        &s.pool,
        &s.settings.consumer_iri,
        endpoint_label,
        Some(&req.holder),
        req.session_id.as_deref(),
        status_code,
        elapsed_ms,
        &request_json,
        &resp_json,
        metrics,
    )
    .await;
    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    (status, Json(resp_json)).into_response()
}

pub async fn memorize_batch(
    State(s): State<Arc<AppState>>,
    JsonReq(req): JsonReq<MemorizeBatchReq>,
) -> impl IntoResponse {
    let started = std::time::Instant::now();
    // Walk the items array and redact each item's images field.
    let mut request_json = serde_json::to_value(&req).unwrap_or_else(|_| json!({}));
    if let Some(items) = request_json.get_mut("items").and_then(|v| v.as_array_mut()) {
        for it in items.iter_mut() {
            *it = redact_images(std::mem::replace(it, Value::Null));
        }
    }

    if req.items.is_empty() {
        let resp = json!({"error": "items[] is empty"});
        job_log::record_job(
            &s.pool, &s.settings.consumer_iri, "POST /memorize/batch",
            None, None, 400, started.elapsed().as_millis() as u64,
            &request_json, &resp,
            job_log::JobMetrics { error: Some("items[] is empty".to_string()), ..Default::default() },
        ).await;
        return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
    }

    let mut results: Vec<Value> = Vec::with_capacity(req.items.len());
    let mut total_facts = 0i32;
    let holder_hint = req.items.first().map(|i| i.holder.clone());
    let session_hint = req.items.first().and_then(|i| i.session_id.clone());
    for item in &req.items {
        match memorize_one(&s, item).await {
            Ok(r) => {
                total_facts += r.facts_ingested as i32;
                results.push(serde_json::to_value(r).unwrap_or_else(|_| json!({})));
            }
            Err(e) => results.push(json!({
                "error": e.to_string(),
                "holder": item.holder,
                "text_preview": item.text.chars().take(80).collect::<String>(),
            })),
        }
    }
    let resp = json!({"results": results});
    let elapsed_ms = started.elapsed().as_millis() as u64;
    job_log::record_job(
        &s.pool, &s.settings.consumer_iri, "POST /memorize/batch",
        holder_hint.as_deref(), session_hint.as_deref(),
        200, elapsed_ms,
        &request_json, &resp,
        job_log::JobMetrics {
            facts_ingested: Some(total_facts),
            ..Default::default()
        },
    ).await;
    Json(resp).into_response()
}

#[derive(Debug, thiserror::Error)]
enum MemorizeError {
    #[error("invalid input: {0}")]
    BadInput(String),
    #[error("module: {0}")]
    Module(#[from] donto_memory_core::module::ModuleError),
    #[error("extract: {0}")]
    Extract(#[from] ExtractError),
}

async fn memorize_one(
    s: &Arc<AppState>,
    req: &MemorizeReq,
) -> Result<MemorizeResp, MemorizeError> {
    // Allow empty text when images are attached — the OCR pass will
    // fill it in. Empty text + no images is still an error.
    if req.text.trim().is_empty() && req.images.is_empty() {
        return Err(MemorizeError::BadInput(
            "text is required and must be non-empty (or attach images)".into(),
        ));
    }
    let started = std::time::Instant::now();

    // OCR pass: if images are attached and OCR is enabled, transcribe
    // visible text and prepend it to the memory's text field. The
    // augmented text becomes the episodic chunk AND seeds the
    // structured extraction. Best-effort: an OCR error is logged as a
    // warning and we proceed with no augmentation.
    let mut effective_text = req.text.clone();
    let mut ocr_warnings: Vec<String> = Vec::new();
    if !req.images.is_empty() && s.settings.ocr_enabled {
        if let Some(extractor) = MemoryExtractor::from_settings(&s.settings) {
            match extractor.ocr_images(&req.images).await {
                Ok(transcripts) => {
                    let mut blocks: Vec<String> = Vec::new();
                    for (i, t) in transcripts.iter().enumerate() {
                        let t = t.trim();
                        if !t.is_empty() {
                            blocks.push(format!("[OCR text from image #{}]\n{t}", i + 1));
                        }
                    }
                    if !blocks.is_empty() {
                        let ocr_block = blocks.join("\n\n");
                        effective_text = if effective_text.trim().is_empty() {
                            ocr_block
                        } else {
                            format!("{effective_text}\n\n{ocr_block}")
                        };
                    }
                }
                Err(e) => {
                    warn!(error = %e, "OCR pass failed; continuing without OCR");
                    ocr_warnings.push(format!("ocr failed: {e}"));
                }
            }
        }
    }
    let registry = register_default_modules();
    let episodic: MemoryModuleArc = registry
        .get("mem:module/episodic")
        .ok_or_else(|| MemorizeError::BadInput("episodic module not registered".into()))?;
    let semantic: MemoryModuleArc = registry
        .get("mem:module/semantic-claim")
        .ok_or_else(|| MemorizeError::BadInput("semantic-claim module not registered".into()))?;

    // 1. Always store the raw text as an episodic chunk.
    let episodic_input = IngestInput {
        holder: req.holder.clone(),
        session_id: req.session_id.clone(),
        text: effective_text.clone(),
        modality: req.modality.clone(),
        subject: None,
        predicate: None,
        object_iri: None,
        object_lit: None,
        source_record_iri: None,
        key: None,
        value: None,
    };
    let episodic_record = episodic
        .ingest(&s.substrate, &s.pool, &s.settings.consumer_iri, &episodic_input)
        .await?;

    // Ensure both the episodic context and the (about-to-be-used)
    // semantic-claim context have the self-read assignment in place.
    // Idempotent — a no-op on contexts we've already seen.
    if let Some(sid) = req.session_id.as_deref() {
        let ep_ctx = format!("{}/episodic/session/{sid}", s.settings.consumer_iri);
        let cl_ctx = format!("{}/claims/session/{sid}", s.settings.consumer_iri);
        for ctx in [ep_ctx, cl_ctx] {
            if let Err(e) =
                donto_memory_core::overlays::ensure_memory_self_read_grant(&s.pool, &ctx).await
            {
                warn!(context = %ctx, error = %e, "self-read grant insert failed (non-fatal)");
            }
        }
    }

    let mut warnings: Vec<String> = ocr_warnings;
    let mut facts_extracted = 0usize;
    let mut facts_ingested = 0usize;
    let mut dedup_collisions = 0u32;
    let mut semantic_record_ids: Vec<Uuid> = Vec::new();
    let mut model: Option<String> = None;
    let mut usage = None;
    let mut aperture_yields = Vec::new();
    let mut effective_mode: Option<String> = None;
    let mut extracted_facts: Vec<ExtractedFact> = Vec::new();

    if let Some(supplied) = req.facts.as_ref().filter(|f| !f.is_empty()) {
        // Supplied-facts path: an upstream extractor (OpenCode agent)
        // already produced the facts. Skip the in-process LLM entirely
        // and ingest these directly. The episodic chunk + self-read
        // grant above still applied.
        effective_mode = Some(req.mode.clone().unwrap_or_else(|| "opencode".to_string()));
        facts_extracted = supplied.len();
        model = Some(format!("upstream:{}", effective_mode.as_deref().unwrap_or("opencode")));
        extracted_facts = supplied.clone();
        let outcome = ingest_fact_list(s, &semantic, supplied, req, &episodic_record.record_iri).await;
        facts_ingested = outcome.ingested;
        semantic_record_ids = outcome.record_ids;
        warnings.extend(outcome.warnings);
    } else if req.extract {
        // 2. Optional in-process LLM extraction.
        match MemoryExtractor::from_settings(&s.settings) {
            None => {
                warnings.push(
                    "LLM not configured; episodic stored, no semantic extraction".into(),
                );
            }
            Some(extractor) => {
                let mode = req
                    .mode
                    .as_deref()
                    .unwrap_or(&s.settings.extract_mode)
                    .to_lowercase();
                effective_mode = Some(mode.clone());
                let result = match mode.as_str() {
                    "single" => {
                        extractor
                            .extract_single(
                                &effective_text,
                                &req.holder,
                                req.session_id.as_deref(),
                                Some(&episodic_record.record_iri),
                                &req.images,
                            )
                            .await
                    }
                    "exhaustive" | "multi" | "apertures" => {
                        extractor
                            .extract_exhaustive(
                                &effective_text,
                                &req.holder,
                                req.session_id.as_deref(),
                                Some(&episodic_record.record_iri),
                                &req.images,
                            )
                            .await
                    }
                    "deep" | "sequential" | "iterative" => {
                        let passes = req.passes.unwrap_or(3);
                        extractor
                            .extract_deep(
                                &effective_text,
                                &req.holder,
                                req.session_id.as_deref(),
                                Some(&episodic_record.record_iri),
                                &req.images,
                                passes,
                            )
                            .await
                    }
                    other => {
                        return Err(MemorizeError::BadInput(format!(
                            "unknown extract mode {other:?}; expected single|exhaustive|deep"
                        )));
                    }
                };

                match result {
                    Err(e) => {
                        warn!(error = %e, "LLM extract failed; episodic-only");
                        warnings.push(format!("extract failed: {e}"));
                    }
                    Ok(result) => {
                        facts_extracted = result.facts.len();
                        dedup_collisions = result.dedup_collisions;
                        model = Some(result.model.clone());
                        usage = result.usage.clone();
                        aperture_yields = result.aperture_yields.clone();
                        extracted_facts = result.facts.clone();
                        let outcome = ingest_fact_list(
                            s, &semantic, &result.facts, req, &episodic_record.record_iri,
                        )
                        .await;
                        facts_ingested = outcome.ingested;
                        semantic_record_ids = outcome.record_ids;
                        warnings.extend(outcome.warnings);
                    }
                }
            }
        }
    }

    Ok(MemorizeResp {
        holder: req.holder.clone(),
        session_id: req.session_id.clone(),
        episodic_record_id: episodic_record.record_id,
        episodic_record_iri: episodic_record.record_iri.clone(),
        extracted: req.extract,
        extract_mode: effective_mode,
        facts_extracted,
        facts_ingested,
        dedup_collisions,
        semantic_record_ids,
        model,
        usage,
        aperture_yields,
        facts: extracted_facts,
        elapsed_ms: started.elapsed().as_millis() as u64,
        warnings,
    })
}

/// Outcome of ingesting a list of facts into the semantic-claim module.
struct IngestOutcome {
    ingested: usize,
    errors: usize,
    skipped: usize,
    record_ids: Vec<Uuid>,
    warnings: Vec<String>,
}

/// Ingest a list of already-extracted facts as semantic claims, anchored
/// to the episodic chunk. Shared by both the in-process LLM-extraction
/// path and the supplied-facts path (OpenCode agent), so the ingest
/// behaviour — skip-unigestable, per-fact error capture, progress
/// logging — is identical regardless of where the facts came from.
async fn ingest_fact_list(
    s: &Arc<AppState>,
    semantic: &MemoryModuleArc,
    facts: &[ExtractedFact],
    req: &MemorizeReq,
    episodic_iri: &str,
) -> IngestOutcome {
    tracing::info!(
        holder = req.holder.as_str(),
        session_id = req.session_id.as_deref().unwrap_or("-"),
        facts_to_ingest = facts.len(),
        "starting semantic fact ingest"
    );
    let ingest_started = std::time::Instant::now();
    let mut last_log = std::time::Instant::now();
    let mut out = IngestOutcome {
        ingested: 0,
        errors: 0,
        skipped: 0,
        record_ids: Vec::new(),
        warnings: Vec::new(),
    };
    for (i, fact) in facts.iter().enumerate() {
        // Skip facts emitted without exactly one object — semantic-claim
        // would reject them anyway; filtering keeps the warn flood + the
        // error count clean (it's bad upstream output, not a substrate bug).
        if !fact.is_ingestable() {
            out.skipped += 1;
            continue;
        }
        match ingest_fact(s, semantic, fact, req, episodic_iri).await {
            Ok(id) => {
                out.ingested += 1;
                out.record_ids.push(id);
            }
            Err(e) => {
                out.errors += 1;
                out.warnings.push(format!(
                    "fact ingest failed (subject={}, predicate={}): {e}",
                    fact.subject, fact.predicate
                ));
                if out.errors <= 5 {
                    warn!(
                        holder = req.holder.as_str(),
                        fact_index = i,
                        subject = fact.subject.as_str(),
                        predicate = fact.predicate.as_str(),
                        error = %e,
                        "fact ingest failed"
                    );
                }
            }
        }
        if last_log.elapsed() >= std::time::Duration::from_secs(5) {
            tracing::info!(
                holder = req.holder.as_str(),
                progress = format!("{}/{}", i + 1, facts.len()).as_str(),
                ingested = out.ingested,
                errors = out.errors,
                elapsed_ms = ingest_started.elapsed().as_millis() as u64,
                "ingest progress"
            );
            last_log = std::time::Instant::now();
        }
    }
    if out.skipped > 0 {
        out.warnings.push(format!(
            "{} fact(s) skipped (emitted without exactly one object)",
            out.skipped
        ));
    }
    tracing::info!(
        holder = req.holder.as_str(),
        session_id = req.session_id.as_deref().unwrap_or("-"),
        total = facts.len(),
        ingested = out.ingested,
        errors = out.errors,
        skipped = out.skipped,
        elapsed_ms = ingest_started.elapsed().as_millis() as u64,
        "ingest complete"
    );
    out
}

async fn ingest_fact(
    s: &Arc<AppState>,
    semantic: &MemoryModuleArc,
    fact: &ExtractedFact,
    req: &MemorizeReq,
    source_record_iri: &str,
) -> Result<Uuid, MemorizeError> {
    if fact.subject.trim().is_empty() || fact.predicate.trim().is_empty() {
        return Err(MemorizeError::BadInput("fact missing subject/predicate".into()));
    }
    let modality = fact
        .modality
        .clone()
        .unwrap_or_else(|| req.modality.clone());
    let input = IngestInput {
        holder: req.holder.clone(),
        session_id: req.session_id.clone(),
        text: fact
            .notes
            .clone()
            .unwrap_or_else(|| format!(
                "extracted from episodic {source_record_iri}"
            )),
        modality,
        subject: Some(fact.subject.clone()),
        predicate: Some(fact.predicate.clone()),
        object_iri: fact.object_iri.clone(),
        object_lit: fact.object_lit.clone(),
        source_record_iri: Some(source_record_iri.to_string()),
        key: None,
        value: None,
    };
    let rec = semantic
        .ingest(&s.substrate, &s.pool, &s.settings.consumer_iri, &input)
        .await?;
    Ok(rec.record_id)
}
