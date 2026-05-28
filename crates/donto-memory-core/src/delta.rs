//! DontoDelta — the substrate-blessed delta vocabulary.
//!
//! A DontoDelta is the set of *append-only* operations donto-memory
//! may apply to a substrate during reconsolidation. No `DELETE`, no
//! `UPDATE_CLAIM`, no silent rewrite — if a recall reveals a memory
//! drifted from truth, the sleep path emits a new claim + an
//! argument edge of relation `supersedes` back to the old.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::Literal;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DontoDeltaOp {
    AssertClaim {
        subject: String,
        predicate: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object_iri: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object_lit: Option<Literal>,
        context: String,
        #[serde(default = "default_polarity")]
        polarity: String,
        #[serde(default = "default_maturity")]
        maturity: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        valid_from: Option<NaiveDate>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        valid_to: Option<NaiveDate>,
        #[serde(default)]
        derived_from_record_ids: Vec<Uuid>,
    },
    AssertFrame {
        frame_type: String,
        context: String,
        #[serde(default)]
        roles: Vec<serde_json::Value>,
        #[serde(default)]
        derived_from_record_ids: Vec<Uuid>,
    },
    AddArgument {
        source_statement_id: Uuid,
        target_statement_id: Uuid,
        relation: ArgumentRelation,
        context: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strength: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<serde_json::Value>,
    },
    AddIdentityEdge {
        left_symbol_iri: String,
        right_symbol_iri: String,
        relation: IdentityRelation,
        confidence: f64,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
    CloseValidTime {
        statement_id: Uuid,
        new_valid_to: NaiveDate,
    },
    CloseTxTime {
        statement_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    UpdateConfidence {
        statement_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine_confidence: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        calibrated_confidence: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        human_confidence: Option<f64>,
    },
    ScheduleReview {
        statement_id: Uuid,
        #[serde(default = "default_obligation_kind")]
        obligation_kind: String,
        #[serde(default)]
        priority: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rationale: Option<String>,
    },
    LinkDerivedArtifact {
        artifact_iri: String,
        source_record_ids: Vec<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

fn default_polarity() -> String {
    "asserted".to_string()
}
fn default_maturity() -> i32 {
    1
}
fn default_obligation_kind() -> String {
    "needs_review".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentRelation {
    Supports,
    Rebuts,
    Undercuts,
    Qualifies,
    Endorses,
    Supersedes,
    PotentiallySame,
    SameReferent,
    SameEvent,
}

impl ArgumentRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supports => "supports",
            Self::Rebuts => "rebuts",
            Self::Undercuts => "undercuts",
            Self::Qualifies => "qualifies",
            Self::Endorses => "endorses",
            Self::Supersedes => "supersedes",
            Self::PotentiallySame => "potentially_same",
            Self::SameReferent => "same_referent",
            Self::SameEvent => "same_event",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityRelation {
    SameReferent,
    PossiblySameReferent,
    DistinctReferent,
    NotEnoughInformation,
}

/// A batch of substrate-bound operations issued by reconsolidation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DontoDelta {
    #[serde(default = "Uuid::new_v4")]
    pub delta_id: Uuid,
    #[serde(default = "Utc::now")]
    pub issued_at: DateTime<Utc>,
    #[serde(default = "default_issuer")]
    pub issued_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(default)]
    pub ops: Vec<DontoDeltaOp>,
}

fn default_issuer() -> String {
    "donto-memory".to_string()
}

impl DontoDelta {
    pub fn new(issued_by: impl Into<String>) -> Self {
        Self {
            delta_id: Uuid::new_v4(),
            issued_at: Utc::now(),
            issued_by: issued_by.into(),
            rationale: None,
            ops: Vec::new(),
        }
    }
    pub fn push(&mut self, op: DontoDeltaOp) -> &mut Self {
        self.ops.push(op);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json() {
        let mut d = DontoDelta::new("test");
        d.push(DontoDeltaOp::AssertClaim {
            subject: "ex:s".to_string(),
            predicate: "ex:p".to_string(),
            object_iri: Some("ex:o".to_string()),
            object_lit: None,
            context: "ctx:t".to_string(),
            polarity: "asserted".to_string(),
            maturity: 1,
            valid_from: None,
            valid_to: None,
            derived_from_record_ids: vec![],
        });
        d.push(DontoDeltaOp::AddArgument {
            source_statement_id: Uuid::new_v4(),
            target_statement_id: Uuid::new_v4(),
            relation: ArgumentRelation::Supersedes,
            context: "ctx:t".to_string(),
            strength: Some(0.9),
            evidence: None,
        });
        let s = serde_json::to_string(&d).unwrap();
        let back: DontoDelta = serde_json::from_str(&s).unwrap();
        assert_eq!(back.ops.len(), 2);
    }
}
