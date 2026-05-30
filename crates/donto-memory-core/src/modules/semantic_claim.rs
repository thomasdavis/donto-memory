//! Semantic-claim memory — extracted typed claims.

use async_trait::async_trait;
use deadpool_postgres::Pool;
use serde_json::json;

use crate::module::{IngestInput, MemoryModule, ModuleError, ModuleSpec};
use crate::overlays;
use crate::substrate::SubstrateClient;
use crate::types::{
    MemoryRecord, MemoryRecordRef, ModuleForm, ModuleFunction, RecallQuery, RecallRow,
};

const DERIVED_FROM: &str = "mem:claim/derived_from";

#[derive(Debug)]
pub struct SemanticClaimModule;

impl SemanticClaimModule {
    pub fn spec_static() -> ModuleSpec {
        ModuleSpec {
            module_iri: "mem:module/semantic-claim".to_string(),
            form: ModuleForm::Structured,
            function: ModuleFunction::Factual,
            label: "Semantic Claim".to_string(),
            description:
                "Extracted typed claims (subject/predicate/object). Each \
                 claim becomes one substrate statement; records anchor to \
                 the statement_id.".to_string(),
            version: "v0.1.0".to_string(),
        }
    }
}

#[async_trait]
impl MemoryModule for SemanticClaimModule {
    fn spec(&self) -> &ModuleSpec {
        use std::sync::OnceLock;
        static SPEC: OnceLock<ModuleSpec> = OnceLock::new();
        SPEC.get_or_init(SemanticClaimModule::spec_static)
    }

    async fn ingest(
        &self,
        substrate: &SubstrateClient,
        pool: &Pool,
        consumer_iri: &str,
        input: &IngestInput,
    ) -> Result<MemoryRecord, ModuleError> {
        let subject = input
            .subject
            .as_deref()
            .ok_or_else(|| ModuleError::Invalid("subject required".into()))?;
        let predicate = input
            .predicate
            .as_deref()
            .ok_or_else(|| ModuleError::Invalid("predicate required".into()))?;
        if input.object_iri.is_some() == input.object_lit.is_some() {
            return Err(ModuleError::Invalid(
                "exactly one of object_iri or object_lit must be set".into(),
            ));
        }

        let session = input.session_id.as_deref().unwrap_or("default");
        let ctx = format!("{consumer_iri}/claims/session/{session}");
        substrate
            .ensure_context(&ctx, "custom", "permissive", None)
            .await?;

        let assert = substrate
            .assert_statement(
                subject,
                predicate,
                input.object_iri.as_deref(),
                input.object_lit.as_ref(),
                &ctx,
                "asserted",
                1, // candidate by default
                None,
                None,
            )
            .await?;
        let stmt_id = assert.statement_id;

        if let Some(src) = input.source_record_iri.as_deref() {
            substrate
                .assert_statement(
                    &stmt_id.to_string(),
                    DERIVED_FROM,
                    Some(src),
                    None,
                    &ctx,
                    "asserted",
                    0,
                    None,
                    None,
                )
                .await?;
        }

        let record_iri = format!("{consumer_iri}/claim/{stmt_id}");
        let metadata = json!({
            "modality": input.modality,
            "informational_text": if input.text.is_empty() { serde_json::Value::Null } else { json!(input.text) },
            "source_record_iri": input.source_record_iri,
        });
        let record_id = overlays::create_record(
            pool,
            &record_iri,
            &self.spec().module_iri,
            &MemoryRecordRef { statement_id: Some(stmt_id), ..Default::default() },
            Some(&input.holder),
            Some(session),
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
        pool: &Pool,
        consumer_iri: &str,
        query: &RecallQuery,
    ) -> Result<Vec<RecallRow>, ModuleError> {
        // Same fan-out pattern as episodic — see comment there.
        let includes: Vec<String> = if let Some(s) = &query.session_id {
            vec![format!("{consumer_iri}/claims/session/{s}")]
        } else {
            let sessions = overlays::list_sessions_for_holder(
                pool,
                &query.holder,
                &self.spec().module_iri,
            )
            .await?;
            sessions
                .into_iter()
                .map(|s| format!("{consumer_iri}/claims/session/{s}"))
                .collect()
        };
        if includes.is_empty() {
            return Ok(Vec::new());
        }
        let scope = json!({"include": includes});
        let resp = substrate
            .recall(
                &query.holder,
                &query.action,
                query.subject.as_deref(),
                query.predicate.as_deref(),
                query.object_iri.as_deref(),
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
                let hay_subject = row.subject.to_lowercase();
                let hay_lit = row
                    .object_lit
                    .as_ref()
                    .and_then(|v| v.get("v"))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default();
                if !hay_subject.contains(&q_lower) && !hay_lit.contains(&q_lower) {
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
