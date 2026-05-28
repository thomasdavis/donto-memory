//! Episodic memory — verbatim event/chunk recall.

use async_trait::async_trait;
use chrono::Utc;
use deadpool_postgres::Pool;
use serde_json::json;
use uuid::Uuid;

use crate::module::{IngestInput, MemoryModule, ModuleError, ModuleSpec};
use crate::overlays;
use crate::substrate::SubstrateClient;
use crate::types::{
    MemoryRecord, MemoryRecordRef, ModuleForm, ModuleFunction, RecallQuery, RecallRow,
};

const EP_CHUNK: &str = "mem:episodic/chunk";
const EP_RECORDED: &str = "mem:episodic/recorded_at";
const EP_HOLDER: &str = "mem:episodic/holder";

#[derive(Debug)]
pub struct EpisodicModule;

impl EpisodicModule {
    pub fn spec_static() -> ModuleSpec {
        ModuleSpec {
            module_iri: "mem:module/episodic".to_string(),
            form: ModuleForm::Token,
            function: ModuleFunction::Experiential,
            label: "Episodic".to_string(),
            description:
                "Verbatim event / chunk recall. Each ingest writes a single \
                 mem:episodic/chunk statement with the raw text as the \
                 object literal.".to_string(),
            version: "v0.1.0".to_string(),
        }
    }
}

#[async_trait]
impl MemoryModule for EpisodicModule {
    fn spec(&self) -> &ModuleSpec {
        // Build once per program — leaked into a static.
        use std::sync::OnceLock;
        static SPEC: OnceLock<ModuleSpec> = OnceLock::new();
        SPEC.get_or_init(EpisodicModule::spec_static)
    }

    async fn ingest(
        &self,
        substrate: &SubstrateClient,
        pool: &Pool,
        consumer_iri: &str,
        input: &IngestInput,
    ) -> Result<MemoryRecord, ModuleError> {
        if input.text.trim().is_empty() {
            return Err(ModuleError::Invalid(
                "episodic ingest requires non-empty text".into(),
            ));
        }
        let session = input.session_id.as_deref().unwrap_or("default");
        let ctx = format!("{consumer_iri}/episodic/session/{session}");
        substrate
            .ensure_context(&ctx, "custom", "permissive", None)
            .await?;

        let record_uuid = Uuid::new_v4();
        let record_iri = format!("{consumer_iri}/episodic/{record_uuid}");
        let subject_iri = record_iri.clone();

        let chunk = substrate
            .assert_statement(
                &subject_iri,
                EP_CHUNK,
                None,
                Some(&json!({"v": input.text, "dt": "xsd:string"})),
                &ctx,
                "asserted",
                0,
                None,
                None,
            )
            .await?;
        let chunk_stmt_id = chunk.statement_id;

        let now = Utc::now().to_rfc3339();
        substrate
            .assert_statement(
                &subject_iri,
                EP_RECORDED,
                None,
                Some(&json!({"v": now, "dt": "xsd:dateTime"})),
                &ctx,
                "asserted",
                0,
                None,
                None,
            )
            .await?;

        if !input.holder.is_empty() {
            substrate
                .assert_statement(
                    &subject_iri,
                    EP_HOLDER,
                    Some(&input.holder),
                    None,
                    &ctx,
                    "asserted",
                    0,
                    None,
                    None,
                )
                .await?;
        }

        let metadata = json!({
            "modality": input.modality,
            "text_len": input.text.len(),
        });
        let r#ref = MemoryRecordRef {
            statement_id: Some(chunk_stmt_id),
            ..Default::default()
        };
        let record_id = overlays::create_record(
            pool,
            &record_iri,
            &self.spec().module_iri,
            &r#ref,
            Some(&input.holder),
            Some(session),
            None,
            &metadata,
        )
        .await?;
        let rec = overlays::get_record(pool, record_id)
            .await?
            .ok_or_else(|| ModuleError::Invalid("record vanished post-insert".into()))?;
        Ok(rec)
    }

    async fn retrieve(
        &self,
        substrate: &SubstrateClient,
        consumer_iri: &str,
        query: &RecallQuery,
    ) -> Result<Vec<RecallRow>, ModuleError> {
        let scope = match &query.session_id {
            Some(s) => json!({"include": [format!("{consumer_iri}/episodic/session/{s}")]}),
            None => json!({"include": [format!("{consumer_iri}/episodic")]}),
        };
        let resp = substrate
            .recall(
                &query.holder,
                &query.action,
                None,
                Some(EP_CHUNK),
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
                let lit_value = row
                    .object_lit
                    .as_ref()
                    .and_then(|v| v.get("v"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                if !lit_value.to_lowercase().contains(&q.to_lowercase()) {
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
