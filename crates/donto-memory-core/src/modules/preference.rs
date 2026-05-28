//! Preference memory — user preferences that never silently overwrite.

use async_trait::async_trait;
use deadpool_postgres::Pool;
use serde_json::json;
use uuid::Uuid;

use crate::module::{IngestInput, MemoryModule, ModuleError, ModuleSpec};
use crate::overlays;
use crate::substrate::SubstrateClient;
use crate::types::{
    MemoryRecord, MemoryRecordRef, ModuleForm, ModuleFunction, RecallQuery, RecallRow,
};

const PREFERS: &str = "mem:pref/prefers";
const PREF_OF: &str = "mem:pref/of";
const PREF_VALUE: &str = "mem:pref/value";

#[derive(Debug)]
pub struct PreferenceModule;

impl PreferenceModule {
    pub fn spec_static() -> ModuleSpec {
        ModuleSpec {
            module_iri: "mem:module/preference".to_string(),
            form: ModuleForm::Structured,
            function: ModuleFunction::Preference,
            label: "Preference".to_string(),
            description:
                "User preferences. Updates are append-only: a new preference \
                 is a new statement with a `supersedes` argument edge to any \
                 prior preference on the same key.".to_string(),
            version: "v0.1.0".to_string(),
        }
    }
}

#[async_trait]
impl MemoryModule for PreferenceModule {
    fn spec(&self) -> &ModuleSpec {
        use std::sync::OnceLock;
        static SPEC: OnceLock<ModuleSpec> = OnceLock::new();
        SPEC.get_or_init(PreferenceModule::spec_static)
    }

    async fn ingest(
        &self,
        substrate: &SubstrateClient,
        pool: &Pool,
        consumer_iri: &str,
        input: &IngestInput,
    ) -> Result<MemoryRecord, ModuleError> {
        if input.holder.is_empty() {
            return Err(ModuleError::Invalid("holder required".into()));
        }
        let key = input
            .key
            .as_deref()
            .ok_or_else(|| ModuleError::Invalid("key required".into()))?;
        let value = input
            .value
            .as_deref()
            .ok_or_else(|| ModuleError::Invalid("value required".into()))?;

        let ctx = format!("{consumer_iri}/preferences/holder/{}", input.holder);
        substrate
            .ensure_context(&ctx, "custom", "permissive", None)
            .await?;

        let pref_subject = format!(
            "{consumer_iri}/preference/{}/{}",
            input.holder,
            slug(key)
        );

        // Look up prior currently-believed preference for (subject, prefers).
        let prior = substrate
            .recall(
                &input.holder,
                "read_metadata",
                Some(&pref_subject),
                Some(PREFERS),
                None,
                None,
                "asserted",
                0,
                None,
                None,
                None,
                1,
                false,
            )
            .await?;
        let prior_stmt: Option<Uuid> = prior.rows.first().map(|r| r.statement_id);
        let prior_value: Option<String> = prior
            .rows
            .first()
            .and_then(|r| r.object_lit.as_ref())
            .and_then(|v| v.get("v"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());

        // New preference statement.
        let assert = substrate
            .assert_statement(
                &pref_subject,
                PREFERS,
                None,
                Some(&json!({"v": value, "dt": "xsd:string"})),
                &ctx,
                "asserted",
                2, // evidence-supported by user assertion
                None,
                None,
            )
            .await?;
        let new_stmt = assert.statement_id;

        // Supersedes edge to the prior preference, if it disagrees.
        if let (Some(p_stmt), Some(p_val)) = (prior_stmt, prior_value) {
            if p_val.as_str() != value {
                substrate
                    .add_argument(
                        new_stmt,
                        p_stmt,
                        "supersedes",
                        &ctx,
                        Some(1.0),
                        Some(&json!({"reason": format!("new value: {value}")})),
                    )
                    .await?;
            }
        }

        // Indexability claims.
        substrate
            .assert_statement(
                &pref_subject,
                PREF_OF,
                Some(&input.holder),
                None,
                &ctx,
                "asserted",
                0,
                None,
                None,
            )
            .await?;
        substrate
            .assert_statement(
                &pref_subject,
                PREF_VALUE,
                None,
                Some(&json!({"v": key, "dt": "xsd:string"})),
                &ctx,
                "asserted",
                0,
                None,
                None,
            )
            .await?;

        let record_iri = format!(
            "{consumer_iri}/preference-record/{}/{}/{}",
            input.holder,
            slug(key),
            new_stmt
        );
        let metadata = json!({
            "key": key,
            "value": value,
            "prior_statement_id": prior_stmt.map(|u| u.to_string()),
            "informational_text": if input.text.is_empty() { serde_json::Value::Null } else { json!(input.text) },
        });
        let record_id = overlays::create_record(
            pool,
            &record_iri,
            &self.spec().module_iri,
            &MemoryRecordRef { statement_id: Some(new_stmt), ..Default::default() },
            Some(&input.holder),
            input.session_id.as_deref(),
            None,
            &metadata,
        )
        .await?;
        overlays::get_record(pool, record_id)
            .await?
            .ok_or_else(|| ModuleError::Invalid("record vanished post-insert".into()))
    }

    async fn retrieve(
        &self,
        substrate: &SubstrateClient,
        consumer_iri: &str,
        query: &RecallQuery,
    ) -> Result<Vec<RecallRow>, ModuleError> {
        let scope = json!({
            "include": [format!("{consumer_iri}/preferences/holder/{}", query.holder)]
        });
        let resp = substrate
            .recall(
                &query.holder,
                &query.action,
                None,
                Some(PREFERS),
                None,
                Some(&scope),
                &query.polarity,
                query.min_maturity,
                query.as_of_tx,
                None,
                query.lens_name.as_deref(),
                query.limit,
                query.permitted_only,
            )
            .await?;
        let mut out = Vec::new();
        for (i, mut row) in resp.rows.into_iter().enumerate() {
            if let Some(q) = &query.query {
                let q_lower = q.to_lowercase();
                let hay = format!(
                    "{} {}",
                    row.subject,
                    row.object_lit
                        .as_ref()
                        .and_then(|v| v.get("v"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                )
                .to_lowercase();
                if !hay.contains(&q_lower) {
                    continue;
                }
            }
            row.module_iri = Some(self.spec().module_iri.clone());
            row.rank = Some((i + 1) as i32);
            row.score = Some(1.0 / (i + 1) as f64);
            out.push(row);
        }
        Ok(out)
    }
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}
