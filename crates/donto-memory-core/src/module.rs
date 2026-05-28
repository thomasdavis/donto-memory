//! Memory module trait + process-wide registry.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use deadpool_postgres::Pool;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::overlays::OverlayError;
use crate::substrate::{SubstrateClient, SubstrateError};
use crate::types::{MemoryRecord, ModuleForm, ModuleFunction, RecallQuery, RecallRow};

#[derive(Debug, Error)]
pub enum ModuleError {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("substrate: {0}")]
    Substrate(#[from] SubstrateError),
    #[error("overlay: {0}")]
    Overlay(#[from] OverlayError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Declarative spec for a module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleSpec {
    pub module_iri: String,
    pub form: ModuleForm,
    pub function: ModuleFunction,
    pub label: String,
    pub description: String,
    pub version: String,
}

/// Inputs to ingest. Modules consume their own subset of fields.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestInput {
    pub holder: String,
    pub session_id: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default = "default_modality")]
    pub modality: String,

    // Module-specific optional fields:
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub object_iri: Option<String>,
    #[serde(default)]
    pub object_lit: Option<serde_json::Value>,
    #[serde(default)]
    pub source_record_iri: Option<String>,

    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

fn default_modality() -> String {
    "model_output".to_string()
}

#[async_trait]
pub trait MemoryModule: Send + Sync + std::fmt::Debug + 'static {
    fn spec(&self) -> &ModuleSpec;

    /// Ingest a unit of memory.
    ///
    /// Implementations call `substrate.assert_statement(...)` (one or
    /// more times) for the evidence and then
    /// `overlays::create_record(...)` for the consumer overlay row.
    /// They MUST NOT write directly into substrate core tables.
    async fn ingest(
        &self,
        substrate: &SubstrateClient,
        pool: &Pool,
        consumer_iri: &str,
        input: &IngestInput,
    ) -> Result<MemoryRecord, ModuleError>;

    /// Retrieve candidate rows for a recall query.
    ///
    /// The module is responsible for narrowing scope/predicate to its
    /// form/function. Policy gating + identity-lens resolution happen
    /// substrate-side via `substrate.recall(...)`.
    async fn retrieve(
        &self,
        substrate: &SubstrateClient,
        consumer_iri: &str,
        query: &RecallQuery,
    ) -> Result<Vec<RecallRow>, ModuleError>;
}

/// Shared reference to a registered module instance.
pub type MemoryModuleArc = Arc<dyn MemoryModule>;

/// Process-wide module registry.
#[derive(Debug, Default)]
pub struct ModuleRegistry {
    inner: RwLock<BTreeMap<String, MemoryModuleArc>>,
}

impl ModuleRegistry {
    pub fn register(&self, module: MemoryModuleArc) {
        let iri = module.spec().module_iri.clone();
        self.inner.write().unwrap().insert(iri, module);
    }

    pub fn get(&self, iri: &str) -> Option<MemoryModuleArc> {
        self.inner.read().unwrap().get(iri).cloned()
    }

    pub fn all(&self) -> Vec<MemoryModuleArc> {
        self.inner.read().unwrap().values().cloned().collect()
    }

    pub fn iris(&self) -> Vec<String> {
        self.inner.read().unwrap().keys().cloned().collect()
    }
}

/// Process-wide static registry. Modules register here at startup
/// (via `register_default_modules()` or consumer code).
pub static MODULE_REGISTRY: OnceLock<ModuleRegistry> = OnceLock::new();

/// Initialise the registry with the three default modules. Idempotent.
pub fn register_default_modules() -> &'static ModuleRegistry {
    let reg = MODULE_REGISTRY.get_or_init(ModuleRegistry::default);
    if reg.all().is_empty() {
        reg.register(Arc::new(crate::modules::episodic::EpisodicModule));
        reg.register(Arc::new(crate::modules::semantic_claim::SemanticClaimModule));
        reg.register(Arc::new(crate::modules::preference::PreferenceModule));
    }
    reg
}
