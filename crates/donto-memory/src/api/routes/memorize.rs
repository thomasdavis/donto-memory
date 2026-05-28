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
use serde_json::json;
use tracing::warn;
use uuid::Uuid;

use crate::api::AppState;

#[derive(Debug, Deserialize)]
pub struct MemorizeReq {
    pub holder: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub text: String,
    #[serde(default = "default_modality")]
    pub modality: String,
    /// If false, skip the LLM extraction step and only store as
    /// episodic. Useful when the caller already has structured
    /// facts and just wants the raw chunk recorded.
    #[serde(default = "default_true")]
    pub extract: bool,
}

#[derive(Debug, Deserialize)]
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
    pub facts_extracted: usize,
    pub facts_ingested: usize,
    pub semantic_record_ids: Vec<Uuid>,
    pub model: Option<String>,
    pub usage: Option<donto_memory_core::extract::ExtractionUsage>,
    pub elapsed_ms: u64,
    pub warnings: Vec<String>,
}

fn default_modality() -> String {
    "model_output".to_string()
}
fn default_true() -> bool {
    true
}

pub async fn memorize(
    State(s): State<Arc<AppState>>,
    Json(req): Json<MemorizeReq>,
) -> impl IntoResponse {
    match memorize_one(&s, &req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(MemorizeError::BadInput(msg)) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
        }
        Err(other) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": other.to_string()})),
        )
            .into_response(),
    }
}

pub async fn memorize_batch(
    State(s): State<Arc<AppState>>,
    Json(req): Json<MemorizeBatchReq>,
) -> impl IntoResponse {
    if req.items.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "items[] is empty"})),
        )
            .into_response();
    }
    let mut results: Vec<serde_json::Value> = Vec::with_capacity(req.items.len());
    for item in req.items {
        match memorize_one(&s, &item).await {
            Ok(r) => results.push(serde_json::to_value(r).unwrap()),
            Err(e) => results.push(json!({
                "error": e.to_string(),
                "holder": item.holder,
                "text_preview": item.text.chars().take(80).collect::<String>(),
            })),
        }
    }
    Json(json!({"results": results})).into_response()
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
    if req.text.trim().is_empty() {
        return Err(MemorizeError::BadInput(
            "text is required and must be non-empty".into(),
        ));
    }
    let started = std::time::Instant::now();
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
        text: req.text.clone(),
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

    let mut warnings: Vec<String> = Vec::new();
    let mut facts_extracted = 0usize;
    let mut facts_ingested = 0usize;
    let mut semantic_record_ids: Vec<Uuid> = Vec::new();
    let mut model: Option<String> = None;
    let mut usage = None;

    if req.extract {
        // 2. Optional LLM extraction.
        match MemoryExtractor::from_settings(&s.settings) {
            None => {
                warnings.push(
                    "LLM not configured; episodic stored, no semantic extraction".into(),
                );
            }
            Some(extractor) => {
                match extractor
                    .extract(
                        &req.text,
                        &req.holder,
                        req.session_id.as_deref(),
                        Some(&episodic_record.record_iri),
                    )
                    .await
                {
                    Err(e) => {
                        warn!(error = %e, "LLM extract failed; episodic-only");
                        warnings.push(format!("extract failed: {e}"));
                    }
                    Ok(result) => {
                        facts_extracted = result.facts.len();
                        model = Some(result.model.clone());
                        usage = result.usage.clone();
                        for fact in &result.facts {
                            match ingest_fact(s, &semantic, fact, req, &episodic_record.record_iri)
                                .await
                            {
                                Ok(id) => {
                                    facts_ingested += 1;
                                    semantic_record_ids.push(id);
                                }
                                Err(e) => {
                                    warnings.push(format!(
                                        "fact ingest failed (subject={}, predicate={}): {e}",
                                        fact.subject, fact.predicate
                                    ));
                                }
                            }
                        }
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
        facts_extracted,
        facts_ingested,
        semantic_record_ids,
        model,
        usage,
        elapsed_ms: started.elapsed().as_millis() as u64,
        warnings,
    })
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
