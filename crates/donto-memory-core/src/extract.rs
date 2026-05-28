//! LLM-driven memory extraction.
//!
//! Given a raw memory chunk, the extractor calls an OpenAI-
//! compatible chat completion endpoint and parses the response into
//! a list of [`ExtractedFact`]s. The caller is responsible for
//! ingesting the facts (typically via the [`crate::modules::SemanticClaimModule`]).
//!
//! Configuration comes from [`crate::Settings`]:
//!   * `llm_base_url` (e.g. `https://openrouter.ai/api/v1`)
//!   * `llm_api_key`
//!   * `llm_model` (default `z-ai/glm-5`)
//!   * `llm_temperature` (default 0.2)
//!
//! If `llm_base_url` is unset, [`MemoryExtractor::new`] returns
//! `None`. Callers should fall back to episodic-only storage in
//! that case.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
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
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub modality: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub model: String,
    pub elapsed_ms: u64,
    pub facts: Vec<ExtractedFact>,
    pub usage: Option<ExtractionUsage>,
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
    /// Build an extractor from settings. Returns `None` if the LLM
    /// is unconfigured — callers fall back to episodic-only.
    pub fn from_settings(settings: &Settings) -> Option<Self> {
        let base_url = settings.llm_base_url.clone()?;
        let api_key = settings.llm_api_key.clone()?;
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
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

    /// Call the LLM and parse out a list of [`ExtractedFact`]s.
    ///
    /// `holder`, `session_id`, and `record_iri` are passed in as
    /// context so the model can mint stable IRIs (e.g. preferring
    /// `mem:<holder>/<entity-name>` over a generic `ex:`).
    pub async fn extract(
        &self,
        text: &str,
        holder: &str,
        session_id: Option<&str>,
        source_record_iri: Option<&str>,
    ) -> Result<ExtractionResult, ExtractError> {
        let prompt = build_prompt(text, holder, session_id, source_record_iri);
        let started = std::time::Instant::now();
        let req = serde_json::json!({
            "model": self.model,
            "temperature": self.temperature,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": prompt},
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
            warn!(error = %e, "raw content: {}", &stripped[..stripped.len().min(400)]);
            ExtractError::Decode(format!("not valid JSON: {e}"))
        })?;
        Ok(ExtractionResult {
            model: chat.model.unwrap_or_else(|| self.model.clone()),
            elapsed_ms: started.elapsed().as_millis() as u64,
            facts: parsed.facts,
            usage: chat.usage.map(|u| ExtractionUsage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                total_tokens: u.total_tokens.unwrap_or(0),
            }),
        })
    }
}

const SYSTEM_PROMPT: &str = "You are a memory-extraction service for an \
agentic memory system built on the donto evidence substrate. Given a \
raw memory chunk (an utterance, an observation, a thought), you \
extract every ontological statement the chunk *implies*. Be thorough \
— surface claims, presuppositions, entity types, relationships, \
properties, temporal anchors. Reuse existing donto vocabulary when \
the predicate is obvious (ex:knownAs, ex:bornIn, ex:occurredAt, \
ex:locatedIn, ex:hasName, ex:metAt, ex:mentionedAt, ...). \
\n\nReturn STRICT JSON of shape {\"facts\": [...]}. Each fact has:\n\
  - subject (IRI string, prefix:local form)\n\
  - predicate (IRI string)\n\
  - exactly one of object_iri (IRI string) or object_lit ({v, dt}, \
    where dt is an xsd:* datatype IRI)\n\
  - confidence (0..1)\n\
  - modality (one of: descriptive, inferred, oral_history, \
    model_output, community_protocol; default model_output)\n\
\nNo prose. No markdown. No explanation. STRICT JSON only.";

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

fn build_prompt(
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
        "holder: {holder}\n{session_block}{src_block}\nchunk:\n{text}\n\n\
        Return STRICT JSON of the form {{\"facts\": [...]}} as documented in the system prompt."
    )
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
}
