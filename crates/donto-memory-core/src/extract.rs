//! LLM-driven memory extraction — single-pass + multi-aperture.
//!
//! Given a raw memory chunk, the extractor calls an OpenAI-compatible
//! chat completion endpoint and parses the response into a list of
//! [`ExtractedFact`]s. Two modes:
//!
//! * **`extract_single`** — one LLM call with a maximalist prompt.
//!   Faster, cheaper, ~20-30 facts per chunk.
//! * **`extract_exhaustive`** — five parallel LLM calls, one per
//!   *aperture* (surface / linguistic / presupposition / inferential
//!   / conceivable). Content-hash deduplicated across the union.
//!   Targets 100+ facts per chunk; substrate's M5 genealogy extractor
//!   pattern. Slower (~30-60 s) and ~5× more tokens, but vastly
//!   more thorough.
//!
//! Configuration via [`crate::Settings`]:
//!   - `llm_base_url` (e.g. `https://openrouter.ai/api/v1`)
//!   - `llm_api_key`
//!   - `llm_model` (default `z-ai/glm-5`)
//!   - `llm_temperature` (default 0.2)

use std::collections::BTreeSet;
use std::time::Duration;

use futures::future::join_all;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{info, warn};

use crate::Settings;

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("LLM not configured (set DONTO_MEMORY_LLM_BASE_URL + DONTO_MEMORY_LLM_API_KEY)")]
    NotConfigured,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("LLM HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("LLM response decode: {0}")]
    Decode(String),
}

/// One extracted ontological statement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFact {
    pub subject: String,
    pub predicate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_iri: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_object_lit",
        skip_serializing_if = "Option::is_none"
    )]
    pub object_lit: Option<serde_json::Value>,
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_f64",
        skip_serializing_if = "Option::is_none"
    )]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modality: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_bool",
        skip_serializing_if = "Option::is_none"
    )]
    pub hypothesis_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aperture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Accept bool, "true"/"false" strings, or 0/1 numerics. Reasoning
/// models occasionally stringify their booleans, which previously
/// killed an entire aperture yield.
fn deserialize_lenient_bool<'de, D>(d: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(b),
        serde_json::Value::String(s) => match s.to_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" | "" => Some(false),
            _ => None,
        },
        serde_json::Value::Number(n) => Some(n.as_f64().map(|x| x != 0.0).unwrap_or(false)),
        _ => None,
    })
}

/// Accept the substrate's `{v, dt}` Literal struct, but also coerce
/// LLM shape-variance — bare strings, numbers, and bools — into a
/// proper Literal with a sensible default datatype. Caught one real
/// failure mode in prod: GLM-5 occasionally returned `object_lit:
/// "alarm history"` instead of `object_lit: {v: "alarm history",
/// dt: "xsd:string"}`, and substrate's 422 rejected the whole fact.
/// Now we wrap it so the fact lands instead of vanishing.
fn deserialize_lenient_object_lit<'de, D>(d: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::{json, Value};
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::Null => None,
        // Already-shaped {v, dt} struct: pass through unchanged.
        Value::Object(_) => Some(v),
        // Bare string → assume xsd:string.
        Value::String(s) => Some(json!({"v": s, "dt": "xsd:string"})),
        // Integer / float → xsd:integer or xsd:decimal.
        Value::Number(n) => {
            let dt = if n.is_i64() || n.is_u64() { "xsd:integer" } else { "xsd:decimal" };
            Some(json!({"v": n, "dt": dt}))
        }
        Value::Bool(b) => Some(json!({"v": b, "dt": "xsd:boolean"})),
        // Arrays don't make sense as a single Literal — drop with None.
        Value::Array(_) => None,
    })
}

/// Accept f64, integers, or stringified numbers. Same rationale as
/// the lenient bool above.
fn deserialize_lenient_f64<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Null => None,
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        serde_json::Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
        _ => None,
    })
}

impl ExtractedFact {
    /// Stable content key for cross-aperture deduplication. Ignores
    /// confidence + modality + notes (those vary per aperture). A
    /// later aperture supplying the same {subject, predicate, object}
    /// is treated as a duplicate.
    pub fn content_key(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.subject.as_bytes());
        h.update(b"\x1f");
        h.update(self.predicate.as_bytes());
        h.update(b"\x1f");
        match (&self.object_iri, &self.object_lit) {
            (Some(i), _) => {
                h.update(b"I");
                h.update(i.as_bytes());
            }
            (None, Some(l)) => {
                h.update(b"L");
                h.update(l.to_string().as_bytes());
            }
            (None, None) => h.update(b"-"),
        }
        hex::encode(h.finalize())
    }

    /// True when this fact has exactly one of (object_iri, object_lit) set —
    /// the substrate's semantic-claim module requires this. LLMs occasionally
    /// emit facts with neither (forgetting the object) or both (over-specified);
    /// those facts are unusable and should be filtered out before ingest
    /// rather than driving a per-fact 400/422.
    pub fn is_ingestable(&self) -> bool {
        self.object_iri.is_some() ^ self.object_lit.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl ExtractionUsage {
    pub fn merge_in(&mut self, other: &ExtractionUsage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub model: String,
    pub elapsed_ms: u64,
    pub facts: Vec<ExtractedFact>,
    pub usage: Option<ExtractionUsage>,
    /// One entry per aperture invocation. Empty for `extract_single`.
    #[serde(default)]
    pub aperture_yields: Vec<ApertureYield>,
    /// Facts dropped because an earlier aperture produced the same
    /// content key. (Cross-aperture dedup count.)
    #[serde(default)]
    pub dedup_collisions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApertureYield {
    pub aperture: String,
    pub raw_facts: u32,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One analytical lens over the source text. Each aperture is a
/// separate LLM call with its own system prompt + confidence band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aperture {
    /// Explicit claims. Anchored, asserted, confidence 0.95–1.0.
    Surface,
    /// Clause-by-clause: every NP→entity, every VP→event, every
    /// modifier→property. Asserted, 0.85–1.0.
    Linguistic,
    /// What the text takes for granted but does not assert. Marked
    /// hypothesis_only, 0.7–0.95.
    Presupposition,
    /// Common-knowledge consequences of stated facts. Modality
    /// inferred, 0.4–0.7.
    Inferential,
    /// Claims that *could* hold given entity types (\"persons have
    /// hair\", \"organisations have employees\"). hypothesis_only,
    /// confidence 0.85 (it is conceivable).
    Conceivable,
}

impl Aperture {
    pub fn id(self) -> &'static str {
        match self {
            Aperture::Surface => "surface",
            Aperture::Linguistic => "linguistic",
            Aperture::Presupposition => "presupposition",
            Aperture::Inferential => "inferential",
            Aperture::Conceivable => "conceivable",
        }
    }

    fn system_prompt(self) -> &'static str {
        match self {
            Aperture::Surface => SURFACE_PROMPT,
            Aperture::Linguistic => LINGUISTIC_PROMPT,
            Aperture::Presupposition => PRESUPPOSITION_PROMPT,
            Aperture::Inferential => INFERENTIAL_PROMPT,
            Aperture::Conceivable => CONCEIVABLE_PROMPT,
        }
    }

    pub const ALL: &'static [Aperture] = &[
        Aperture::Surface,
        Aperture::Linguistic,
        Aperture::Presupposition,
        Aperture::Inferential,
        Aperture::Conceivable,
    ];
}

#[derive(Debug, Clone)]
pub struct MemoryExtractor {
    base_url: String,
    api_key: String,
    model: String,
    /// Optional separate model used when images are present in the
    /// /memorize call. When `None`, the regular `model` is used for
    /// both text-only and multimodal calls (works fine if `model` is
    /// already vision-capable).
    vision_model: Option<String>,
    temperature: f32,
    http: Client,
}

impl MemoryExtractor {
    pub fn from_settings(settings: &Settings) -> Option<Self> {
        let base_url = settings.llm_base_url.clone()?;
        let api_key = settings.llm_api_key.clone()?;
        let http = Client::builder()
            // 15-minute timeout per LLM call. Deep-mode passes 5+ have
            // a 5-10KB prior_facts_block which makes GLM-5 slow (often
            // 3-7 min). Previously 180s, which truncated every pass.
            .timeout(Duration::from_secs(900))
            .user_agent(concat!("donto-memory/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model: settings.llm_model.clone(),
            vision_model: settings.llm_vision_model.clone(),
            temperature: settings.llm_temperature,
            http,
        })
    }

    /// OCR every image and return one transcript per image (empty
    /// string when no text is visible).
    ///
    /// Implemented as a single multimodal LLM call against the
    /// configured `vision_model` with a tight OCR-only prompt. The
    /// model is asked to return JSON of shape
    /// `{"transcripts": ["...", "...", ...]}` — one string per
    /// image, in order. This is one network round-trip regardless of
    /// the number of images.
    ///
    /// Callers should treat an OCR error as best-effort: if the call
    /// fails, fall back to no-OCR rather than aborting the whole
    /// memorize flow.
    pub async fn ocr_images(&self, images: &[String]) -> Result<Vec<String>, ExtractError> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let chosen_model = self
            .vision_model
            .as_deref()
            .unwrap_or(self.model.as_str());

        let mut parts: Vec<serde_json::Value> = Vec::with_capacity(1 + images.len());
        parts.push(serde_json::json!({
            "type": "text",
            "text": OCR_PROMPT,
        }));
        for url in images {
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": url },
            }));
        }

        let req = serde_json::json!({
            "model": chosen_model,
            "temperature": 0.0,  // deterministic transcription
            "max_tokens": 4000,
            "messages": [
                {"role": "system", "content": OCR_SYSTEM_PROMPT},
                {"role": "user", "content": parts},
            ],
            "response_format": { "type": "json_object" },
        });
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ExtractError::Status {
                status: status.as_u16(),
                body: body.chars().take(512).collect(),
            });
        }
        let chat: ChatCompletion = resp
            .json()
            .await
            .map_err(|e| ExtractError::Decode(e.to_string()))?;
        let msg = chat
            .choices
            .first()
            .map(|c| &c.message)
            .ok_or_else(|| ExtractError::Decode("no message in OCR response".into()))?;
        let content = msg
            .content
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .or(msg.reasoning_content.as_ref().filter(|s| !s.trim().is_empty()))
            .cloned()
            .ok_or_else(|| ExtractError::Decode("no OCR content".into()))?;

        #[derive(Deserialize)]
        struct OcrEnvelope {
            #[serde(default)]
            transcripts: Vec<String>,
        }
        let stripped = strip_markdown_code_fence(&content);
        let parsed: OcrEnvelope = serde_json::from_str(&stripped)
            .map_err(|e| ExtractError::Decode(format!("OCR not valid JSON: {e}")))?;
        // Pad to one entry per image even if model returned fewer.
        let mut out = parsed.transcripts;
        while out.len() < images.len() {
            out.push(String::new());
        }
        out.truncate(images.len());
        Ok(out)
    }

    /// Single-pass extraction. ~20–30 facts per chunk, ~one LLM call,
    /// ~$0.005–$0.02 per chunk depending on model.
    ///
    /// `images` is a list of http(s) URLs or `data:image/...;base64,…`
    /// URLs. When non-empty, the call switches to OpenAI multimodal
    /// message format and (if configured) uses `vision_model`
    /// instead of `model`.
    pub async fn extract_single(
        &self,
        text: &str,
        holder: &str,
        session_id: Option<&str>,
        source_record_iri: Option<&str>,
        images: &[String],
    ) -> Result<ExtractionResult, ExtractError> {
        let started = std::time::Instant::now();
        let yield_ = self
            .call_one(
                SINGLE_PROMPT,
                "single",
                text,
                holder,
                session_id,
                source_record_iri,
                images,
            )
            .await?;
        Ok(ExtractionResult {
            model: yield_.model,
            elapsed_ms: started.elapsed().as_millis() as u64,
            facts: yield_.facts,
            usage: yield_.usage,
            aperture_yields: vec![ApertureYield {
                aperture: "single".to_string(),
                raw_facts: yield_.raw_count,
                elapsed_ms: yield_.elapsed_ms,
                error: None,
            }],
            dedup_collisions: 0,
        })
    }

    /// Multi-aperture extraction. Five LLM calls in parallel,
    /// content-hash deduplicated across the union. Targets 100+
    /// facts on a typical memory chunk. ~30–60 s, ~5× the token cost
    /// of `extract_single`.
    pub async fn extract_exhaustive(
        &self,
        text: &str,
        holder: &str,
        session_id: Option<&str>,
        source_record_iri: Option<&str>,
        images: &[String],
    ) -> Result<ExtractionResult, ExtractError> {
        let started = std::time::Instant::now();
        let futures = Aperture::ALL.iter().map(|a| {
            let me = self.clone();
            let aperture = *a;
            let text = text.to_string();
            let holder = holder.to_string();
            let session_id = session_id.map(|s| s.to_string());
            let source = source_record_iri.map(|s| s.to_string());
            let images = images.to_vec();
            async move {
                let id = aperture.id();
                let result = me
                    .call_one(
                        aperture.system_prompt(),
                        id,
                        &text,
                        &holder,
                        session_id.as_deref(),
                        source.as_deref(),
                        &images,
                    )
                    .await;
                (aperture, result)
            }
        });
        let results = join_all(futures).await;

        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut merged_usage: Option<ExtractionUsage> = None;
        let mut aperture_yields: Vec<ApertureYield> = Vec::new();
        let mut all_facts: Vec<ExtractedFact> = Vec::new();
        let mut dedup_collisions = 0u32;
        let mut model = self.model.clone();

        for (aperture, res) in results {
            match res {
                Ok(y) => {
                    aperture_yields.push(ApertureYield {
                        aperture: aperture.id().to_string(),
                        raw_facts: y.raw_count,
                        elapsed_ms: y.elapsed_ms,
                        error: None,
                    });
                    if let Some(u) = y.usage {
                        match merged_usage.as_mut() {
                            None => merged_usage = Some(u),
                            Some(acc) => acc.merge_in(&u),
                        }
                    }
                    model = y.model;
                    for mut fact in y.facts {
                        if fact.aperture.is_none() {
                            fact.aperture = Some(aperture.id().to_string());
                        }
                        let key = fact.content_key();
                        if seen.insert(key) {
                            all_facts.push(fact);
                        } else {
                            dedup_collisions += 1;
                        }
                    }
                }
                Err(e) => {
                    warn!(aperture = aperture.id(), error = %e, "aperture call failed");
                    aperture_yields.push(ApertureYield {
                        aperture: aperture.id().to_string(),
                        raw_facts: 0,
                        elapsed_ms: 0,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        Ok(ExtractionResult {
            model,
            elapsed_ms: started.elapsed().as_millis() as u64,
            facts: all_facts,
            usage: merged_usage,
            aperture_yields,
            dedup_collisions,
        })
    }

    /// Sequential multi-pass extraction. Runs `passes` LLM calls one
    /// after the other (no parallelism). Each pass sees the union of
    /// facts produced by earlier passes and is asked to find new
    /// angles, deeper implications, additional entities. No rigid
    /// per-pass system prompts — the SINGLE maximalist prompt is reused
    /// every pass and divergence is driven entirely by showing the
    /// model what's already covered.
    ///
    /// Content-hash deduplication runs at the end across the union.
    /// Each fact is tagged with `aperture = "pass_<n>"` so the job
    /// page can show per-pass attribution. `passes` is clamped to
    /// [1, 10]; 3 is a reasonable default.
    pub async fn extract_deep(
        &self,
        text: &str,
        holder: &str,
        session_id: Option<&str>,
        source_record_iri: Option<&str>,
        images: &[String],
        passes: u32,
    ) -> Result<ExtractionResult, ExtractError> {
        let started = std::time::Instant::now();
        let passes = passes.clamp(1, 10);

        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut merged_usage: Option<ExtractionUsage> = None;
        let mut pass_yields: Vec<ApertureYield> = Vec::new();
        let mut all_facts: Vec<ExtractedFact> = Vec::new();
        let mut dedup_collisions = 0u32;
        let mut model = self.model.clone();

        info!(
            holder = holder,
            session_id = session_id.unwrap_or("-"),
            passes = passes,
            text_chars = text.chars().count(),
            images = images.len(),
            "deep extract starting"
        );

        for pass_n in 1..=passes {
            let pass_id = format!("pass_{pass_n}");
            let prior_count = all_facts.len();
            let prior_block = if all_facts.is_empty() {
                None
            } else {
                Some(format_prior_facts_block(&all_facts))
            };
            info!(
                pass = pass_id.as_str(),
                pass_n = pass_n,
                of = passes,
                prior_facts = prior_count,
                cumulative_unique = all_facts.len(),
                "deep pass starting"
            );
            let pass_started = std::time::Instant::now();
            let result = self
                .call_one_with_context(
                    SINGLE_PROMPT,
                    &pass_id,
                    text,
                    holder,
                    session_id,
                    source_record_iri,
                    images,
                    prior_block.as_deref(),
                )
                .await;

            match result {
                Ok(y) => {
                    let mut added = 0u32;
                    let mut collided = 0u32;
                    pass_yields.push(ApertureYield {
                        aperture: pass_id.clone(),
                        raw_facts: y.raw_count,
                        elapsed_ms: y.elapsed_ms,
                        error: None,
                    });
                    if let Some(u) = y.usage {
                        match merged_usage.as_mut() {
                            None => merged_usage = Some(u),
                            Some(acc) => acc.merge_in(&u),
                        }
                    }
                    model = y.model;
                    for mut fact in y.facts {
                        // Always overwrite the aperture so the pass
                        // label is authoritative even when the LLM
                        // suggested one in the JSON.
                        fact.aperture = Some(pass_id.clone());
                        let key = fact.content_key();
                        if seen.insert(key) {
                            all_facts.push(fact);
                            added += 1;
                        } else {
                            dedup_collisions += 1;
                            collided += 1;
                        }
                    }
                    info!(
                        pass = pass_id.as_str(),
                        elapsed_ms = pass_started.elapsed().as_millis() as u64,
                        raw_facts = y.raw_count,
                        new_unique = added,
                        dedup_collisions_in_pass = collided,
                        cumulative_unique = all_facts.len(),
                        "deep pass complete"
                    );
                }
                Err(e) => {
                    warn!(
                        pass = pass_id.as_str(),
                        elapsed_ms = pass_started.elapsed().as_millis() as u64,
                        error = %e,
                        "deep pass failed"
                    );
                    pass_yields.push(ApertureYield {
                        aperture: pass_id,
                        raw_facts: 0,
                        elapsed_ms: 0,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        let total_elapsed = started.elapsed().as_millis() as u64;
        let successful_passes = pass_yields.iter().filter(|y| y.error.is_none()).count();
        let failed_passes = pass_yields.iter().filter(|y| y.error.is_some()).count();
        info!(
            holder = holder,
            session_id = session_id.unwrap_or("-"),
            total_elapsed_ms = total_elapsed,
            passes_attempted = passes,
            passes_successful = successful_passes,
            passes_failed = failed_passes,
            total_unique_facts = all_facts.len(),
            total_dedup_collisions = dedup_collisions,
            "deep extract finished"
        );

        Ok(ExtractionResult {
            model,
            elapsed_ms: total_elapsed,
            facts: all_facts,
            usage: merged_usage,
            aperture_yields: pass_yields,
            dedup_collisions,
        })
    }

    async fn call_one(
        &self,
        system_prompt: &str,
        aperture_id: &str,
        text: &str,
        holder: &str,
        session_id: Option<&str>,
        source_record_iri: Option<&str>,
        images: &[String],
    ) -> Result<OneCallResult, ExtractError> {
        self.call_one_with_context(
            system_prompt,
            aperture_id,
            text,
            holder,
            session_id,
            source_record_iri,
            images,
            None,
        )
        .await
    }

    /// Same as `call_one` but optionally prepends a "prior facts" block
    /// to the user prompt. Used by `extract_deep` to encourage each
    /// pass to find facts the previous passes missed without dictating
    /// a specific extraction lens.
    #[allow(clippy::too_many_arguments)]
    async fn call_one_with_context(
        &self,
        system_prompt: &str,
        aperture_id: &str,
        text: &str,
        holder: &str,
        session_id: Option<&str>,
        source_record_iri: Option<&str>,
        images: &[String],
        prior_facts_block: Option<&str>,
    ) -> Result<OneCallResult, ExtractError> {
        let started = std::time::Instant::now();
        let base_prompt = build_user_prompt(text, holder, session_id, source_record_iri);
        let user_prompt = match prior_facts_block {
            Some(prior) if !prior.is_empty() => format!("{prior}\n\n{base_prompt}"),
            _ => base_prompt,
        };

        // When images are attached, switch to the OpenAI multimodal
        // message format: `content` becomes an array of parts. Each
        // image_url accepts an http(s) URL or a data: URL with a
        // base64 payload (`data:image/png;base64,...`). When no
        // images are present we keep the simpler string-content
        // shape — non-vision models still accept this.
        let user_message_content = if images.is_empty() {
            serde_json::json!(user_prompt)
        } else {
            let mut parts: Vec<serde_json::Value> = Vec::with_capacity(1 + images.len());
            parts.push(serde_json::json!({"type": "text", "text": user_prompt}));
            for url in images {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": url },
                }));
            }
            serde_json::json!(parts)
        };

        // If the runtime has a separate vision model configured and
        // images are present, prefer it. Otherwise stay on the text
        // model (multimodal-capable models accept text-only requests
        // fine).
        let chosen_model = if !images.is_empty() && self.vision_model.is_some() {
            self.vision_model.as_deref().unwrap()
        } else {
            self.model.as_str()
        };

        // Reasoning models (z-ai/glm-5 etc.) sometimes return null
        // `content` when the reasoning budget is too tight to leave
        // room for the JSON output. We give them a generous overall
        // budget and don't cap the reasoning channel separately.
        let req = serde_json::json!({
            "model": chosen_model,
            "temperature": self.temperature,
            "max_tokens": 8000,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_message_content},
            ],
            "response_format": { "type": "json_object" },
        });
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ExtractError::Status {
                status: status.as_u16(),
                body: body.chars().take(512).collect(),
            });
        }
        let chat: ChatCompletion = resp
            .json()
            .await
            .map_err(|e| ExtractError::Decode(e.to_string()))?;
        // Some reasoning models put their JSON in `reasoning_content`
        // when `content` ends up null (the model "thought" the answer
        // but never committed it as a visible turn). Try both fields.
        let msg = chat
            .choices
            .first()
            .map(|c| &c.message)
            .ok_or_else(|| ExtractError::Decode("no message in response".into()))?;
        let content = msg
            .content
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .or(msg.reasoning_content.as_ref().filter(|s| !s.trim().is_empty()))
            .or(msg.reasoning.as_ref().filter(|s| !s.trim().is_empty()))
            .cloned()
            .ok_or_else(|| ExtractError::Decode("no message content".into()))?;
        let stripped = strip_markdown_code_fence(&content);
        // First attempt: parse as-is. Most calls land here.
        let parsed: FactsEnvelope = match serde_json::from_str(&stripped) {
            Ok(env) => env,
            Err(first_err) => {
                // The model frequently truncates mid-string when it runs
                // out of max_tokens. Salvage the prefix: walk forward to
                // find the `"facts": [`, then peel off complete `{…}`
                // objects until we hit something that won't parse.
                let recovered = recover_truncated_facts(&stripped);
                if !recovered.is_empty() {
                    warn!(
                        aperture = aperture_id,
                        recovered = recovered.len(),
                        first_error = %first_err,
                        "LLM JSON truncated; recovered partial facts"
                    );
                    FactsEnvelope { facts: recovered }
                } else {
                    warn!(
                        aperture = aperture_id,
                        error = %first_err,
                        raw = &stripped[..stripped.len().min(400)],
                        "LLM returned non-JSON, no salvage"
                    );
                    return Err(ExtractError::Decode(format!(
                        "not valid JSON: {first_err}"
                    )));
                }
            }
        };
        Ok(OneCallResult {
            model: chat.model.unwrap_or_else(|| self.model.clone()),
            raw_count: parsed.facts.len() as u32,
            facts: parsed.facts,
            usage: chat.usage.map(|u| ExtractionUsage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                total_tokens: u.total_tokens.unwrap_or(0),
            }),
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}

struct OneCallResult {
    model: String,
    raw_count: u32,
    facts: Vec<ExtractedFact>,
    usage: Option<ExtractionUsage>,
    elapsed_ms: u64,
}

// ---------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------

const COMMON_FRAGMENT: &str = "Return STRICT JSON of shape \
{\"facts\": [{...}, ...]}. Each fact has: subject (IRI string, \
prefix:local form), predicate (IRI string), exactly one of \
object_iri (IRI) or object_lit ({v, dt}), confidence (0..1), \
modality (descriptive | inferred | reconstructed | elicited | \
oral_history | model_output | community_protocol), and optionally \
hypothesis_only (boolean). Reuse existing donto vocabulary where \
obvious (ex:hasName, ex:knownAs, ex:locatedIn, ex:bornIn, \
ex:occurredAt, ex:metAt, ex:hasOccupation, ex:hasFather, ex:hasChild, \
ex:residesIn, rdf:type). Coin new ex:* predicates when needed. \
\n\n\
SUPPRESS STRUCTURAL BOILERPLATE. The donto-memory runtime already \
records, in its own overlay tables and via context naming: which \
agent holds the memory, the episodic chunk's record IRI, the \
holder's identity, the session IRI, the timestamp, and the platform. \
DO NOT emit facts that merely restate the holder, the agent, the \
session IRI, the platform (e.g. Discord), the episodic-chunk-as-\
EpisodicMemoryChunk typing, or guild/channel-ids parsed out of the \
session string. Focus 95% of your output on facts implied by the \
MESSAGE CONTENT itself; let donto handle the metadata. \n\n\
USE CANONICAL ENTITY IRIs. For chat messages of the form \
\"<user> in #<channel>: <body>\", normalize entities to: \
discord:user:<user>, discord:channel:<channel>, \
discord:message:<id-or-uuid-if-unknown>. Never mint user IRIs as \
bare handles or with a different prefix; cross-message recall \
depends on these being stable. \n\n\
LENGTH-CONDITION YIELD. Aim for one content fact per 2-3 words of \
input body (excluding the \"<user> in #<channel>: \" prefix). \
Under-yield rather than over-yield on short utterances — a future \
call can fill gaps if needed. A 3-word body should produce at most \
~5 content facts plus the user/channel/message linkages. \n\n\
No prose, no markdown. STRICT JSON only.";

const SINGLE_PROMPT: &str = "You are an aggressive ontological \
extractor for an agentic memory system. Given a raw memory chunk \
(an utterance, observation, or thought), extract every implied \
statement: surface claims, presuppositions, entity types, \
relationships, properties, temporal anchors, places, people. Be \
thorough — 30+ statements from a sentence-length chunk is normal, \
100+ from a paragraph is the target. Decompose every clause; do \
not summarise. Each named entity gets type assertions and \
property assertions even if implied. Return STRICT JSON of shape \
{\"facts\": [...]} as documented below. \n\n";

const SURFACE_PROMPT: &str = "Surface aperture. Extract ONLY the \
explicitly-stated facts from the chunk. One fact per stated claim. \
High confidence (0.9–1.0). Asserted modality. Do NOT speculate. \
Do NOT add type-of assertions unless the text explicitly types the \
entity. \"I met Annie at the festival\" yields: (agent, ex:met, \
annie), (agent, ex:metAt, festival). No presuppositions; another \
aperture handles those.\n\n";

const LINGUISTIC_PROMPT: &str = "Linguistic aperture. Decompose \
EVERY clause of the chunk. Every noun phrase becomes an entity \
claim (rdf:type, ex:hasName). Every verb phrase becomes an event \
or relation claim. Every modifier (adjective, adverb, possessive) \
becomes a property claim. Be exhaustive: a single sentence often \
yields 20+ linguistic facts. Confidence 0.85–1.0. Asserted modality. \
\"Annie's father was a fisherman who worked Lizard Island in the \
1880s\" yields: (annie, ex:hasFather, X), (X, rdf:type, ex:Person), \
(X, rdf:type, ex:Fisherman), (X, ex:hasOccupation, fisherman), \
(X, ex:workedAt, lizard-island), (lizard-island, rdf:type, ex:Place), \
(X, ex:workedDuring, 1880s)...\n\n";

const PRESUPPOSITION_PROMPT: &str = "Presupposition aperture. \
Extract what the chunk takes for granted but does not assert. \
\"I met Annie at X\" presupposes Annie exists, X exists, X is a \
place capable of hosting a meeting, the meeting occurred, the agent \
is capable of meeting people. Mark every fact hypothesis_only=true. \
Confidence 0.7–0.95. Modality inferred. \
A typical sentence has 10+ presuppositions. Be thorough — the \
existence of every named entity is a presupposition; the type of \
every named entity is often a presupposition.\n\n";

const INFERENTIAL_PROMPT: &str = "Inferential aperture. Extract \
claims that follow from the stated facts via common knowledge. \
\"Born 1990 in Sydney\" allows: (person, rdf:type, ex:Australian), \
(person, ex:alive_in, 2026), (person, ex:approximate_age, 36), \
(sydney, ex:locatedIn, australia). Confidence 0.4–0.7. Modality \
inferred. Be careful — only inferences a generic reasonable person \
would make from the stated facts.\n\n";

const CONCEIVABLE_PROMPT: &str = "Conceivable aperture. Extract \
claims that COULD plausibly hold given the entity types in the \
chunk, without being inferable from the stated facts. \"persons \
have hair, fingers, parents\". \"festivals have attendees, dates, \
locations, organisers\". \"places have coordinates, populations\". \
Mark every fact hypothesis_only=true. Confidence ~0.85 (it is \
conceivable). Modality inferred. Be wildly thorough — this aperture \
floods the candidate space. 30+ facts from a single named entity is \
fine.\n\n";

const OCR_SYSTEM_PROMPT: &str = "You are an OCR engine. You will be \
shown one or more images. For each image, transcribe EVERY word \
visible — UI labels, screenshots, signs, handwritten notes, captions, \
watermarks, numbers, code. Preserve line breaks where they matter. \
If an image has no visible text, return an empty string for that \
index. Do NOT describe the image. Do NOT add commentary. Return \
STRICT JSON of shape {\"transcripts\": [\"text from image 1\", \
\"text from image 2\", ...]} with exactly one entry per input image, \
in the same order they were provided.";

const OCR_PROMPT: &str =
    "Transcribe every word visible in each of the following image(s). \
Return STRICT JSON {\"transcripts\":[...]}, one entry per image.";

fn build_user_prompt(
    text: &str,
    holder: &str,
    session_id: Option<&str>,
    source_record_iri: Option<&str>,
) -> String {
    let session_block = session_id
        .map(|s| format!("session_id: {s}\n"))
        .unwrap_or_default();
    let src_block = source_record_iri
        .map(|s| format!("source_record_iri: {s}\n"))
        .unwrap_or_default();
    format!(
        "holder: {holder}\n{session_block}{src_block}\nchunk:\n{text}\n\n{COMMON_FRAGMENT}"
    )
}

/// Format already-extracted facts as a context block for subsequent
/// deep-mode passes. Deliberately light-touch: lists the (subject,
/// predicate, object) tuples and asks for new angles. Does NOT
/// prescribe a specific extraction lens (linguistic, presupposition,
/// inferential, etc.) — the model picks its own divergence.
///
/// Truncates to the most recent 300 facts to keep prompt size bounded
/// while still giving the model enough signal to avoid repetition.
fn format_prior_facts_block(facts: &[ExtractedFact]) -> String {
    // Cap the included facts to the most-recent 300. `start` is the
    // index of the first fact to include; the slice `&facts[start..]`
    // gives at most 300 entries. Bounds the prompt size so even on a
    // 7-pass deep extraction with 600+ accumulated facts we don't
    // blow past max_tokens on the input side.
    let start = facts.len().saturating_sub(300);
    let mut s = String::with_capacity(facts.len() * 80);
    s.push_str(
        "Earlier passes over this same chunk already extracted the facts below. \
Your job in this pass is to find EVERY remaining fact the previous passes missed. \
Do NOT repeat anything in the list — content-hash dedup will drop repeats anyway, \
so your job is pure novelty. Push harder: deeper inferences, unstated assumptions, \
additional entities (including abstract/conceptual ones, time/place anchors, \
counterfactuals), alternate framings, finer-grained properties, temporal and \
spatial nuance, causal and dependency links, contrastive readings, parts of named \
entities, generic-class facts (\"X is a Y\", \"Y has property Z\"), metalinguistic \
facts about the utterance itself (sentence count, mood, register, sentiment, \
politeness, addressee, speech act), pragmatic implicatures, conventional \
implicatures, scalar implicatures, conversational maxims, intent, plan, \
prerequisite, consequence, related concepts in the same domain, related \
practitioners, related tools/standards/formats, the user's evident expertise level, \
the user's evident emotional state, the user's evident workflow, the user's \
evident dependencies, the user's evident substitutes-avoided, the user's evident \
counterfactual world (\"would be lost without X\"), domain knowledge implied. \
Aim for 30-60+ NEW facts in this pass. Repeat content will be dropped — your \
incentive is breadth + novelty. Only return {\"facts\": []} if you genuinely \
cannot think of one more angle.\n\nALREADY EXTRACTED (subject | predicate | \
object):\n",
    );
    for fact in &facts[start..] {
        let obj = match (&fact.object_iri, &fact.object_lit) {
            (Some(i), _) => i.clone(),
            (None, Some(l)) => l.to_string(),
            (None, None) => "—".to_string(),
        };
        s.push_str("- ");
        s.push_str(&fact.subject);
        s.push_str(" | ");
        s.push_str(&fact.predicate);
        s.push_str(" | ");
        s.push_str(&obj);
        s.push('\n');
    }
    s
}

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning-model fallback: some providers put the actual answer
    /// here when `content` is null after a long reasoning pass.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct FactsEnvelope {
    #[serde(default)]
    facts: Vec<ExtractedFact>,
}

/// Salvage extracted facts from a JSON response truncated mid-string
/// (the EOF-mid-string error reasoning models produce when they
/// exhaust max_tokens).
///
/// Strategy: find `"facts"` array opening `[`, then walk the body
/// pulling out complete `{…}` objects via brace+string-aware
/// bracket-matching. Stop at the first object that doesn't parse.
/// Returns whatever prefix of valid facts we got — usually 90+% of
/// what the model intended.
fn recover_truncated_facts(raw: &str) -> Vec<ExtractedFact> {
    // Locate the array start. The model is told to emit `{"facts":[…]}`
    // so the array key is always `facts`.
    let array_start = match raw.find("\"facts\"").and_then(|i| raw[i..].find('[').map(|j| i + j)) {
        Some(p) => p + 1, // position after `[`
        None => return Vec::new(),
    };
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = array_start;
    while i < bytes.len() {
        // Skip whitespace + commas.
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'{' {
            break;
        }
        let obj_start = i;
        // Walk forward tracking string vs structural context until the
        // matching brace closes.
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escape = false;
        while i < bytes.len() {
            let c = bytes[i];
            if in_string {
                if escape {
                    escape = false;
                } else if c == b'\\' {
                    escape = true;
                } else if c == b'"' {
                    in_string = false;
                }
            } else if c == b'"' {
                in_string = true;
            } else if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    i += 1;
                    break;
                }
            }
            i += 1;
        }
        if depth != 0 {
            // Object was truncated mid-stream. We're done.
            break;
        }
        let slice = &raw[obj_start..i];
        match serde_json::from_str::<ExtractedFact>(slice) {
            Ok(f) => out.push(f),
            Err(_) => break,
        }
    }
    out
}

fn strip_markdown_code_fence(s: &str) -> String {
    let trimmed = s.trim();
    let body = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest
    } else {
        trimmed
    };
    body.trim().trim_end_matches("```").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_fenced_blocks() {
        let raw = "```json\n{\"facts\":[]}\n```";
        assert_eq!(strip_markdown_code_fence(raw), r#"{"facts":[]}"#);
    }

    #[test]
    fn passes_through_bare_json() {
        let raw = r#"{"facts":[]}"#;
        assert_eq!(strip_markdown_code_fence(raw), raw);
    }

    #[test]
    fn extractor_disabled_when_no_url() {
        let mut s = Settings::default();
        s.llm_base_url = None;
        assert!(MemoryExtractor::from_settings(&s).is_none());
    }

    #[test]
    fn content_key_dedupes_same_triple_across_apertures() {
        let a = ExtractedFact {
            subject: "ex:s".into(),
            predicate: "ex:p".into(),
            object_iri: Some("ex:o".into()),
            object_lit: None,
            confidence: Some(0.9),
            modality: Some("asserted".into()),
            hypothesis_only: None,
            aperture: Some("surface".into()),
            notes: None,
        };
        let b = ExtractedFact {
            confidence: Some(0.7),
            modality: Some("inferred".into()),
            aperture: Some("linguistic".into()),
            ..a.clone()
        };
        assert_eq!(a.content_key(), b.content_key());
    }

    #[test]
    fn content_key_differs_on_object_change() {
        let a = ExtractedFact {
            subject: "ex:s".into(),
            predicate: "ex:p".into(),
            object_iri: Some("ex:o1".into()),
            object_lit: None,
            confidence: None,
            modality: None,
            hypothesis_only: None,
            aperture: None,
            notes: None,
        };
        let b = ExtractedFact {
            object_iri: Some("ex:o2".into()),
            ..a.clone()
        };
        assert_ne!(a.content_key(), b.content_key());
    }

    #[test]
    fn all_apertures_present() {
        assert_eq!(Aperture::ALL.len(), 5);
        let ids: Vec<&str> = Aperture::ALL.iter().map(|a| a.id()).collect();
        for expected in [
            "surface",
            "linguistic",
            "presupposition",
            "inferential",
            "conceivable",
        ] {
            assert!(ids.contains(&expected), "{expected} missing");
        }
    }

    /// Regression test for the lenient bool deserializer. Reasoning
    /// models like z-ai/glm-5 occasionally emit `"true"` (string)
    /// instead of `true` (bool); this previously killed an entire
    /// aperture yield with `invalid type: string "true", expected a
    /// boolean`.
    #[test]
    fn extracted_fact_accepts_stringified_bool() {
        let raw = r#"{
            "subject":"ex:s","predicate":"ex:p","object_iri":"ex:o",
            "hypothesis_only":"true","confidence":0.7
        }"#;
        let f: ExtractedFact = serde_json::from_str(raw).unwrap();
        assert_eq!(f.hypothesis_only, Some(true));
    }

    /// Real prod incident 2026-05-30: GLM-5 returned `object_lit:
    /// "alarm history"` (bare string) instead of the documented
    /// `{v, dt}` shape. Substrate rejected with 422 and lost the
    /// fact. The lenient deserializer must wrap bare strings as
    /// xsd:string so the fact lands.
    #[test]
    fn extracted_fact_lenient_object_lit_bare_string() {
        let raw = r#"{
            "subject":"ex:s","predicate":"ex:p",
            "object_lit":"alarm history"
        }"#;
        let f: ExtractedFact = serde_json::from_str(raw).unwrap();
        let lit = f.object_lit.expect("object_lit should have been wrapped, not dropped");
        assert_eq!(lit["v"], "alarm history");
        assert_eq!(lit["dt"], "xsd:string");
    }

    #[test]
    fn extracted_fact_lenient_object_lit_bare_number() {
        let raw = r#"{"subject":"ex:s","predicate":"ex:p","object_lit":42}"#;
        let f: ExtractedFact = serde_json::from_str(raw).unwrap();
        let lit = f.object_lit.expect("number must be wrapped");
        assert_eq!(lit["v"], 42);
        assert_eq!(lit["dt"], "xsd:integer");
    }

    #[test]
    fn is_ingestable_neither_object_set() {
        let f = ExtractedFact {
            subject: "ex:s".into(), predicate: "ex:p".into(),
            object_iri: None, object_lit: None,
            confidence: None, modality: None, hypothesis_only: None,
            aperture: None, notes: None,
        };
        assert!(!f.is_ingestable(), "fact with no object must be filtered out");
    }

    #[test]
    fn is_ingestable_both_objects_set() {
        let f = ExtractedFact {
            subject: "ex:s".into(), predicate: "ex:p".into(),
            object_iri: Some("ex:x".into()),
            object_lit: Some(serde_json::json!({"v":"y","dt":"xsd:string"})),
            confidence: None, modality: None, hypothesis_only: None,
            aperture: None, notes: None,
        };
        assert!(!f.is_ingestable(), "fact with both objects set must be filtered out");
    }

    #[test]
    fn is_ingestable_one_object_set() {
        let iri = ExtractedFact {
            subject: "ex:s".into(), predicate: "ex:p".into(),
            object_iri: Some("ex:x".into()), object_lit: None,
            confidence: None, modality: None, hypothesis_only: None,
            aperture: None, notes: None,
        };
        assert!(iri.is_ingestable());
        let lit = ExtractedFact {
            subject: "ex:s".into(), predicate: "ex:p".into(),
            object_iri: None,
            object_lit: Some(serde_json::json!({"v":"y","dt":"xsd:string"})),
            confidence: None, modality: None, hypothesis_only: None,
            aperture: None, notes: None,
        };
        assert!(lit.is_ingestable());
    }

    #[test]
    fn extracted_fact_lenient_object_lit_struct_passthrough() {
        let raw = r#"{
            "subject":"ex:s","predicate":"ex:p",
            "object_lit":{"v":"hello","dt":"xsd:string"}
        }"#;
        let f: ExtractedFact = serde_json::from_str(raw).unwrap();
        let lit = f.object_lit.unwrap();
        assert_eq!(lit["v"], "hello");
        assert_eq!(lit["dt"], "xsd:string");
    }

    /// Salvage facts from a truncated LLM JSON response. This is the
    /// EOF-mid-string failure reported in the live job log on
    /// 2026-05-30: the model produced ~150 lines of valid facts then
    /// got cut off mid-value on the last one. We must recover the
    /// prefix.
    #[test]
    fn recovers_truncated_facts_at_end_of_array() {
        let raw = r#"{
            "facts": [
                {"subject":"ex:a","predicate":"ex:p","object_iri":"ex:o1"},
                {"subject":"ex:b","predicate":"ex:p","object_iri":"ex:o2"},
                {"subject":"ex:c","predicate":"ex:p","object_iri":"ex:THIS_IS_TRU"#;
        let out = recover_truncated_facts(raw);
        assert_eq!(out.len(), 2, "expected 2 salvageable facts; got {:?}", out);
        assert_eq!(out[0].subject, "ex:a");
        assert_eq!(out[1].subject, "ex:b");
    }

    /// A truncation in the middle of a string value mid-object must
    /// not produce a half-fact. Stop at the last complete `}`.
    #[test]
    fn recovers_handles_truncated_value() {
        let raw = r#"{"facts":[{"subject":"ex:a","predicate":"ex:p","object_lit":{"v":"unter"#;
        let out = recover_truncated_facts(raw);
        assert!(out.is_empty(), "no complete fact yet → must be empty");
    }

    /// Nested braces inside a value must not confuse the matcher.
    #[test]
    fn recovers_handles_nested_braces() {
        let raw = r#"{"facts":[{"subject":"ex:a","predicate":"ex:p","object_lit":{"v":1,"dt":"xsd:integer"}},{"subject":"ex:b","predicate":"ex:p","object_lit":{"v":"incomplete"#;
        let out = recover_truncated_facts(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].subject, "ex:a");
    }

    /// Strings with escaped quotes must not break the matcher.
    #[test]
    fn recovers_handles_escaped_quotes() {
        let raw = r#"{"facts":[{"subject":"ex:a","predicate":"ex:p","object_lit":{"v":"he said \"hi\""}},{"subject":"ex:trun"#;
        let out = recover_truncated_facts(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].subject, "ex:a");
    }

    /// Confidence sometimes comes back as a string. Same justification.
    #[test]
    fn extracted_fact_accepts_stringified_confidence() {
        let raw = r#"{
            "subject":"ex:s","predicate":"ex:p","object_iri":"ex:o",
            "confidence":"0.85"
        }"#;
        let f: ExtractedFact = serde_json::from_str(raw).unwrap();
        assert!((f.confidence.unwrap() - 0.85).abs() < 1e-9);
    }
}
