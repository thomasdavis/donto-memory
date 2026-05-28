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
use tracing::warn;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_lit: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hypothesis_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aperture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
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
    temperature: f32,
    http: Client,
}

impl MemoryExtractor {
    pub fn from_settings(settings: &Settings) -> Option<Self> {
        let base_url = settings.llm_base_url.clone()?;
        let api_key = settings.llm_api_key.clone()?;
        let http = Client::builder()
            .timeout(Duration::from_secs(180))
            .user_agent(concat!("donto-memory/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model: settings.llm_model.clone(),
            temperature: settings.llm_temperature,
            http,
        })
    }

    /// Single-pass extraction. ~20–30 facts per chunk, ~one LLM call,
    /// ~$0.005–$0.02 per chunk depending on model.
    pub async fn extract_single(
        &self,
        text: &str,
        holder: &str,
        session_id: Option<&str>,
        source_record_iri: Option<&str>,
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
    ) -> Result<ExtractionResult, ExtractError> {
        let started = std::time::Instant::now();
        let futures = Aperture::ALL.iter().map(|a| {
            let me = self.clone();
            let aperture = *a;
            let text = text.to_string();
            let holder = holder.to_string();
            let session_id = session_id.map(|s| s.to_string());
            let source = source_record_iri.map(|s| s.to_string());
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

    async fn call_one(
        &self,
        system_prompt: &str,
        aperture_id: &str,
        text: &str,
        holder: &str,
        session_id: Option<&str>,
        source_record_iri: Option<&str>,
    ) -> Result<OneCallResult, ExtractError> {
        let started = std::time::Instant::now();
        let user_prompt = build_user_prompt(text, holder, session_id, source_record_iri);
        let req = serde_json::json!({
            "model": self.model,
            "temperature": self.temperature,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
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
        let content = chat
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| ExtractError::Decode("no message content".into()))?;
        let stripped = strip_markdown_code_fence(&content);
        let parsed: FactsEnvelope = serde_json::from_str(&stripped).map_err(|e| {
            warn!(
                aperture = aperture_id,
                error = %e,
                raw = &stripped[..stripped.len().min(400)],
                "LLM returned non-JSON"
            );
            ExtractError::Decode(format!("not valid JSON: {e}"))
        })?;
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
}
