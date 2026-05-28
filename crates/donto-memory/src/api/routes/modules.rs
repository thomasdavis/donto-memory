use std::sync::Arc;

use axum::{extract::State, response::IntoResponse, Json};
use donto_memory_core::module::register_default_modules;
use donto_memory_core::overlays;
use serde_json::json;
use std::collections::BTreeMap;

use crate::api::AppState;

pub async fn list(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let reg = register_default_modules();
    let runtime: Vec<serde_json::Value> = reg
        .all()
        .iter()
        .map(|m| {
            let spec = m.spec();
            json!({
                "module_iri": spec.module_iri,
                "form": spec.form,
                "function": spec.function,
                "label": spec.label,
                "description": spec.description,
                "version": spec.version,
                "source": "runtime",
            })
        })
        .collect();

    let db_rows = overlays::list_modules(&s.pool).await.unwrap_or_default();
    let mut by_iri: BTreeMap<String, serde_json::Value> = runtime
        .iter()
        .map(|v| (v["module_iri"].as_str().unwrap().to_string(), v.clone()))
        .collect();
    for r in db_rows {
        match by_iri.get_mut(&r.module_iri) {
            Some(v) => {
                v["enabled_in_db"] = json!(r.enabled);
                v["db_version"] = json!(r.version);
            }
            None => {
                by_iri.insert(
                    r.module_iri.clone(),
                    json!({
                        "module_iri": r.module_iri,
                        "form": r.form,
                        "function": r.function,
                        "label": r.label,
                        "description": r.description,
                        "version": r.version,
                        "source": "db_only",
                        "enabled_in_db": r.enabled,
                    }),
                );
            }
        }
    }
    Json(json!({"modules": by_iri.into_values().collect::<Vec<_>>()}))
}
