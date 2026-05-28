//! Thin reqwest-based client for dontosrv.
//!
//! Every donto-memory operation that touches the substrate goes
//! through this client. No direct Postgres traffic at runtime
//! (migrations are the only exception, via [`crate::overlays`]).

use chrono::{DateTime, NaiveDate, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::RecallRow;

#[derive(Debug, Error)]
pub enum SubstrateError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("substrate {status}: {body}")]
    Status { status: u16, body: String },
    #[error("substrate JSON decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("substrate contract floor not met: actual={actual}, required={floor}")]
    ContractFloor { actual: String, floor: String },
}

/// reqwest-based async client for a remote donto substrate.
#[derive(Debug, Clone)]
pub struct SubstrateClient {
    base_url: String,
    http: Client,
}

impl SubstrateClient {
    /// Build a new client.
    pub fn new(base_url: impl Into<String>) -> Result<Self, SubstrateError> {
        let http = Client::builder()
            .user_agent(concat!("donto-memory/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, SubstrateError> {
        let resp = self.http.get(self.url(path)).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SubstrateError::Status {
                status: status.as_u16(),
                body: body.chars().take(256).collect(),
            });
        }
        Ok(resp.json::<T>().await?)
    }

    async fn get_text(&self, path: &str) -> Result<String, SubstrateError> {
        let resp = self.http.get(self.url(path)).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SubstrateError::Status {
                status: status.as_u16(),
                body: body.chars().take(256).collect(),
            });
        }
        Ok(body)
    }

    async fn post<B: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, SubstrateError> {
        let resp = self
            .http
            .post(self.url(path))
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SubstrateError::Status {
                status: status.as_u16(),
                body: body.chars().take(256).collect(),
            });
        }
        Ok(resp.json::<T>().await?)
    }

    // -- contract handshake ------------------------------------------

    pub async fn contract_version(&self) -> Result<serde_json::Value, SubstrateError> {
        self.get("/discovery/contract-version").await
    }

    /// Refuse to run against a substrate older than `floor`.
    pub async fn assert_contract_floor(&self, floor: &str) -> Result<(), SubstrateError> {
        let info: serde_json::Value = self.contract_version().await?;
        let actual = info
            .get("contract_version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if semver_less(&actual, floor) {
            return Err(SubstrateError::ContractFloor {
                actual,
                floor: floor.to_string(),
            });
        }
        tracing::info!(
            actual = %actual, floor = %floor,
            "substrate contract floor satisfied"
        );
        Ok(())
    }

    // -- write path --------------------------------------------------

    pub async fn ensure_context(
        &self,
        iri: &str,
        kind: &str,
        mode: &str,
        parent: Option<&str>,
    ) -> Result<serde_json::Value, SubstrateError> {
        #[derive(Serialize)]
        struct Req<'a> {
            iri: &'a str,
            kind: &'a str,
            mode: &'a str,
            parent: Option<&'a str>,
        }
        self.post("/contexts/ensure", &Req { iri, kind, mode, parent })
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn assert_statement(
        &self,
        subject: &str,
        predicate: &str,
        object_iri: Option<&str>,
        object_lit: Option<&serde_json::Value>,
        context: &str,
        polarity: &str,
        maturity: i32,
        valid_from: Option<NaiveDate>,
        valid_to: Option<NaiveDate>,
    ) -> Result<AssertResp, SubstrateError> {
        #[derive(Serialize)]
        struct Req<'a> {
            subject: &'a str,
            predicate: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            object_iri: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            object_lit: Option<&'a serde_json::Value>,
            context: &'a str,
            polarity: &'a str,
            maturity: i32,
            #[serde(skip_serializing_if = "Option::is_none")]
            valid_from: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            valid_to: Option<String>,
        }
        let req = Req {
            subject,
            predicate,
            object_iri,
            object_lit,
            context,
            polarity,
            maturity,
            valid_from: valid_from.map(|d| d.to_string()),
            valid_to: valid_to.map(|d| d.to_string()),
        };
        self.post("/assert", &req).await
    }

    pub async fn retract(
        &self,
        statement_id: uuid::Uuid,
    ) -> Result<serde_json::Value, SubstrateError> {
        #[derive(Serialize)]
        struct Req<'a> {
            statement_id: &'a uuid::Uuid,
        }
        self.post("/retract", &Req { statement_id: &statement_id })
            .await
    }

    pub async fn add_argument(
        &self,
        source_statement_id: uuid::Uuid,
        target_statement_id: uuid::Uuid,
        relation: &str,
        context: &str,
        strength: Option<f64>,
        evidence: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, SubstrateError> {
        #[derive(Serialize)]
        struct Req<'a> {
            source_statement_id: &'a uuid::Uuid,
            target_statement_id: &'a uuid::Uuid,
            relation: &'a str,
            context: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            strength: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            evidence: Option<&'a serde_json::Value>,
        }
        let req = Req {
            source_statement_id: &source_statement_id,
            target_statement_id: &target_statement_id,
            relation,
            context,
            strength,
            evidence,
        };
        self.post("/arguments/assert", &req).await
    }

    // -- read path ---------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub async fn recall(
        &self,
        holder: &str,
        action: &str,
        subject: Option<&str>,
        predicate: Option<&str>,
        object_iri: Option<&str>,
        scope: Option<&serde_json::Value>,
        polarity: &str,
        min_maturity: i32,
        as_of_tx: Option<DateTime<Utc>>,
        as_of_valid: Option<NaiveDate>,
        lens_name: Option<&str>,
        limit: i32,
        permitted_only: bool,
    ) -> Result<RecallResp, SubstrateError> {
        #[derive(Serialize)]
        struct Req<'a> {
            holder: &'a str,
            action: &'a str,
            subject: Option<&'a str>,
            predicate: Option<&'a str>,
            object_iri: Option<&'a str>,
            scope: Option<&'a serde_json::Value>,
            polarity: &'a str,
            min_maturity: i32,
            as_of_tx: Option<String>,
            as_of_valid: Option<String>,
            lens_name: Option<&'a str>,
            limit: i32,
            permitted_only: bool,
        }
        let req = Req {
            holder,
            action,
            subject,
            predicate,
            object_iri,
            scope,
            polarity,
            min_maturity,
            as_of_tx: as_of_tx.map(|t| t.to_rfc3339()),
            as_of_valid: as_of_valid.map(|d| d.to_string()),
            lens_name,
            limit,
            permitted_only,
        };
        self.post("/recall", &req).await
    }

    pub async fn dontoql(&self, query: &str) -> Result<serde_json::Value, SubstrateError> {
        #[derive(Serialize)]
        struct Req<'a> {
            query: &'a str,
        }
        self.post("/dontoql", &Req { query }).await
    }

    // -- policy ------------------------------------------------------

    pub async fn effective_actions(
        &self,
        target_kind: &str,
        target_id: &str,
    ) -> Result<serde_json::Value, SubstrateError> {
        let path = format!(
            "/policy/effective/{}/{}",
            urlencoding(target_kind),
            urlencoding(target_id)
        );
        self.get(&path).await
    }

    // -- discovery ---------------------------------------------------

    pub async fn substrate_health(&self) -> Result<serde_json::Value, SubstrateError> {
        self.get("/discovery/substrate-health").await
    }

    pub async fn overlays(&self) -> Result<serde_json::Value, SubstrateError> {
        self.get("/discovery/overlays").await
    }

    pub async fn dontoql_grammar(&self) -> Result<String, SubstrateError> {
        self.get_text("/discovery/dontoql-grammar").await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssertResp {
    pub statement_id: uuid::Uuid,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecallResp {
    #[serde(default)]
    pub holder: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub lens: Option<String>,
    #[serde(default)]
    pub as_of: Option<DateTime<Utc>>,
    #[serde(default)]
    pub rows: Vec<RecallRow>,
    #[serde(default)]
    pub row_count: i32,
}

/// Minimal URL-component encoder: percent-encodes characters outside
/// the unreserved set per RFC 3986.
fn urlencoding(s: &str) -> String {
    const UNRESERVED: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Lexical-numeric semver-ish less-than. Compares the leading dotted
/// numeric prefix; suffix (`-m10`, `-beta.1`) is compared lexically
/// when the numeric prefix matches.
fn semver_less(a: &str, b: &str) -> bool {
    let (a_num, a_tail) = split_semver(a);
    let (b_num, b_tail) = split_semver(b);
    if a_num != b_num {
        return a_num < b_num;
    }
    a_tail < b_tail
}

fn split_semver(s: &str) -> (Vec<u32>, &str) {
    let (head, tail) = match s.find('-') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let nums = head
        .split('.')
        .filter_map(|p| p.parse::<u32>().ok())
        .collect();
    (nums, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compare() {
        assert!(semver_less("0.0.5", "0.1.0-m10"));
        assert!(semver_less("0.1.0-alpha", "0.1.0-m10"));
        assert!(!semver_less("0.1.0-m10", "0.1.0-m10"));
        assert!(!semver_less("0.2.0", "0.1.0-m10"));
    }

    #[test]
    fn url_encode_keeps_unreserved() {
        assert_eq!(urlencoding("doc:123"), "doc%3A123");
        assert_eq!(urlencoding("plain"), "plain");
    }
}
