//! Runtime configuration. Read from environment variables with the
//! `DONTO_MEMORY_` prefix.

use serde::Deserialize;

/// Runtime settings.
///
/// Build with [`Settings::from_env`] (reads env + optional `.env`)
/// or via direct construction in tests.
#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    /// Base URL of the donto substrate (dontosrv).
    pub dontosrv_url: String,
    /// Minimum substrate contract version this runtime accepts.
    pub substrate_contract_floor: String,
    /// Postgres DSN (only required for `migrate`, `api`, `worker`).
    pub donto_dsn: Option<String>,

    /// IRI prefix this consumer files under.
    pub consumer_iri: String,
    /// Default holder when none provided.
    pub default_holder: String,

    /// host:port for the API server.
    pub api_bind: String,

    /// Default + max recall limits.
    pub recall_default_limit: i32,
    pub recall_max_limit: i32,
    pub recall_default_action: String,
    pub enable_reconsolidation_enqueue: bool,

    /// Sleep-path worker tunables.
    pub worker_poll_interval_seconds: f64,
    pub worker_batch_size: i32,
    pub worker_claim_ttl_seconds: i64,
    pub reconsolidation_coalesce_window_seconds: i64,

    /// Optional LLM endpoint for sleep-path reflection.
    pub llm_base_url: Option<String>,
    pub llm_api_key: Option<String>,
    pub llm_model: String,
    pub llm_temperature: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dontosrv_url: "http://localhost:7879".to_string(),
            substrate_contract_floor: crate::SUBSTRATE_CONTRACT_FLOOR.to_string(),
            donto_dsn: None,
            consumer_iri: "ctx:memory".to_string(),
            default_holder: "agent:anonymous".to_string(),
            api_bind: "127.0.0.1:7900".to_string(),
            recall_default_limit: 50,
            recall_max_limit: 500,
            recall_default_action: "read_content".to_string(),
            enable_reconsolidation_enqueue: true,
            worker_poll_interval_seconds: 5.0,
            worker_batch_size: 8,
            worker_claim_ttl_seconds: 900,
            reconsolidation_coalesce_window_seconds: 300,
            llm_base_url: None,
            llm_api_key: None,
            llm_model: "z-ai/glm-5".to_string(),
            llm_temperature: 0.2,
        }
    }
}

impl Settings {
    /// Build settings from environment variables.
    ///
    /// Every variable is `DONTO_MEMORY_<UPPER_SNAKE>` of the field
    /// name. Unset variables fall back to [`Settings::default`].
    pub fn from_env() -> Self {
        let mut s = Self::default();
        if let Ok(v) = std::env::var("DONTO_MEMORY_DONTOSRV_URL") {
            s.dontosrv_url = v;
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_SUBSTRATE_CONTRACT_FLOOR") {
            s.substrate_contract_floor = v;
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_DONTO_DSN") {
            s.donto_dsn = Some(v);
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_CONSUMER_IRI") {
            s.consumer_iri = v;
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_DEFAULT_HOLDER") {
            s.default_holder = v;
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_API_BIND") {
            s.api_bind = v;
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_RECALL_DEFAULT_LIMIT") {
            if let Ok(n) = v.parse() {
                s.recall_default_limit = n;
            }
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_RECALL_MAX_LIMIT") {
            if let Ok(n) = v.parse() {
                s.recall_max_limit = n;
            }
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_RECALL_DEFAULT_ACTION") {
            s.recall_default_action = v;
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_ENABLE_RECONSOLIDATION_ENQUEUE") {
            s.enable_reconsolidation_enqueue = matches!(v.as_str(), "true" | "1" | "yes");
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_WORKER_POLL_INTERVAL_SECONDS") {
            if let Ok(n) = v.parse() {
                s.worker_poll_interval_seconds = n;
            }
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_WORKER_BATCH_SIZE") {
            if let Ok(n) = v.parse() {
                s.worker_batch_size = n;
            }
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_WORKER_CLAIM_TTL_SECONDS") {
            if let Ok(n) = v.parse() {
                s.worker_claim_ttl_seconds = n;
            }
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_RECONSOLIDATION_COALESCE_WINDOW_SECONDS") {
            if let Ok(n) = v.parse() {
                s.reconsolidation_coalesce_window_seconds = n;
            }
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_LLM_BASE_URL") {
            s.llm_base_url = Some(v);
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_LLM_API_KEY") {
            s.llm_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_LLM_MODEL") {
            s.llm_model = v;
        }
        if let Ok(v) = std::env::var("DONTO_MEMORY_LLM_TEMPERATURE") {
            if let Ok(n) = v.parse() {
                s.llm_temperature = n;
            }
        }
        s
    }
}
