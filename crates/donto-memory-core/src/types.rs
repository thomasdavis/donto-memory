//! Shared types — wire format for the API and module layer.
//!
//! Breaking changes here bump the major version.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Polarity of an asserted claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Polarity {
    Asserted,
    Negated,
    Absent,
    Unknown,
}

impl Polarity {
    pub fn as_str(self) -> &'static str {
        match self {
            Polarity::Asserted => "asserted",
            Polarity::Negated => "negated",
            Polarity::Absent => "absent",
            Polarity::Unknown => "unknown",
        }
    }
}

impl Default for Polarity {
    fn default() -> Self {
        Self::Asserted
    }
}

/// Trust Kernel action.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    ReadMetadata,
    ReadContent,
    Quote,
    ViewAnchorLocation,
    DeriveClaims,
    DeriveEmbeddings,
    Translate,
    Summarize,
    ExportClaims,
    ExportSources,
    ExportAnchors,
    TrainModel,
    PublishRelease,
    ShareWithThirdParty,
    FederatedQuery,
    RequestDeletion,
}

impl PolicyAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadMetadata => "read_metadata",
            Self::ReadContent => "read_content",
            Self::Quote => "quote",
            Self::ViewAnchorLocation => "view_anchor_location",
            Self::DeriveClaims => "derive_claims",
            Self::DeriveEmbeddings => "derive_embeddings",
            Self::Translate => "translate",
            Self::Summarize => "summarize",
            Self::ExportClaims => "export_claims",
            Self::ExportSources => "export_sources",
            Self::ExportAnchors => "export_anchors",
            Self::TrainModel => "train_model",
            Self::PublishRelease => "publish_release",
            Self::ShareWithThirdParty => "share_with_third_party",
            Self::FederatedQuery => "federated_query",
            Self::RequestDeletion => "request_deletion",
        }
    }
}

/// A literal-object value: typed JSON + optional language tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Literal {
    pub v: serde_json::Value,
    pub dt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

impl Literal {
    pub fn string(s: impl Into<String>) -> Self {
        Self {
            v: serde_json::Value::String(s.into()),
            dt: "xsd:string".to_string(),
            lang: None,
        }
    }
    pub fn integer(n: i64) -> Self {
        Self {
            v: serde_json::Value::Number(n.into()),
            dt: "xsd:integer".to_string(),
            lang: None,
        }
    }
}

/// Memory module form. Mirrors `donto_x_memory_module.form` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModuleForm {
    Token,
    Structured,
    Parametric,
    Dream,
}

impl ModuleForm {
    pub fn as_str(self) -> &'static str {
        match self {
            ModuleForm::Token => "token",
            ModuleForm::Structured => "structured",
            ModuleForm::Parametric => "parametric",
            ModuleForm::Dream => "dream",
        }
    }
}

/// Memory module function. Mirrors `donto_x_memory_module.function` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModuleFunction {
    Factual,
    Experiential,
    Procedural,
    Preference,
    Working,
}

impl ModuleFunction {
    pub fn as_str(self) -> &'static str {
        match self {
            ModuleFunction::Factual => "factual",
            ModuleFunction::Experiential => "experiential",
            ModuleFunction::Procedural => "procedural",
            ModuleFunction::Preference => "preference",
            ModuleFunction::Working => "working",
        }
    }
}

/// Access kind on a memory record. Mirrors `donto_x_memory_access.access_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessKind {
    Retrieved,
    Surfaced,
    Cited,
    Ignored,
    Corrected,
}

impl AccessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AccessKind::Retrieved => "retrieved",
            AccessKind::Surfaced => "surfaced",
            AccessKind::Cited => "cited",
            AccessKind::Ignored => "ignored",
            AccessKind::Corrected => "corrected",
        }
    }
}

/// Substrate anchor for a memory record. Exactly one of the three is `Some`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecordRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_iri: Option<String>,
}

/// A memory record — one unit of memory in donto-memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub record_id: Uuid,
    pub record_iri: String,
    pub module_iri: String,
    #[serde(flatten)]
    pub r#ref: MemoryRecordRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_iri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_iri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_policy_iri: Option<String>,
    pub tx_lo: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_hi: Option<DateTime<Utc>>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Recall input — POST /recall request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecallQuery {
    pub holder: String,
    #[serde(default = "default_action")]
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_iri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_iris: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of_tx: Option<DateTime<Utc>>,
    #[serde(default = "default_polarity")]
    pub polarity: String,
    #[serde(default)]
    pub min_maturity: i32,
    #[serde(default = "default_limit")]
    pub limit: i32,
    #[serde(default = "default_true")]
    pub permitted_only: bool,
}

fn default_action() -> String {
    "read_content".to_string()
}
fn default_polarity() -> String {
    "asserted".to_string()
}
fn default_limit() -> i32 {
    50
}
fn default_true() -> bool {
    true
}

/// One row in a Memory Evidence Bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallRow {
    pub statement_id: Uuid,
    pub subject: String,
    pub predicate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_iri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_lit: Option<serde_json::Value>,
    pub context: String,
    pub polarity: String,
    pub maturity: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_lo: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_hi: Option<NaiveDate>,
    pub tx_lo: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_hi: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_object: Option<String>,
    #[serde(default)]
    pub effective_actions: BTreeMap<String, bool>,
    pub action_allowed: bool,

    // donto-memory layer additions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_iri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_iri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<i32>,
}

/// Composed Memory Evidence Bundle returned by POST /recall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvidenceBundle {
    pub holder: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
    pub rows: Vec<RecallRow>,
    pub row_count: i32,
    pub modules_used: Vec<String>,
    pub policy_report: serde_json::Value,
}
