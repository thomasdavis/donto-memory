//! Reciprocal-rank fusion across module outputs.

use std::collections::BTreeMap;
use uuid::Uuid;

use crate::types::RecallRow;

/// Reciprocal-rank fuse a map of `module_iri → ranked rows`.
///
/// `k` is the RRF constant (60 is standard). Returns a single
/// list sorted by descending fused score; output rows have `rank`
/// re-assigned to their final position.
pub fn rrf_fuse(per_module: BTreeMap<String, Vec<RecallRow>>, k: i32) -> Vec<RecallRow> {
    let mut acc: BTreeMap<Uuid, (f64, RecallRow)> = BTreeMap::new();
    for (_module_iri, rows) in per_module {
        for row in rows {
            let sid = row.statement_id;
            let rank = row.rank.unwrap_or(1);
            let rrf = 1.0 / ((k + rank) as f64);
            match acc.remove(&sid) {
                None => {
                    let mut r = row;
                    r.score = Some(rrf);
                    acc.insert(sid, (rrf, r));
                }
                Some((prev_score, prev_row)) => {
                    let new_score = prev_score + rrf;
                    let prev_rank = prev_row.rank.unwrap_or(1);
                    let mut chosen = if rank < prev_rank { row } else { prev_row };
                    chosen.score = Some(new_score);
                    acc.insert(sid, (new_score, chosen));
                }
            }
        }
    }
    let mut out: Vec<RecallRow> = acc.into_values().map(|(_score, row)| row).collect();
    out.sort_by(|a, b| {
        b.score
            .unwrap_or(0.0)
            .partial_cmp(&a.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (i, row) in out.iter_mut().enumerate() {
        row.rank = Some((i + 1) as i32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn row(sid: &str, rank: i32) -> RecallRow {
        RecallRow {
            statement_id: Uuid::parse_str(sid).unwrap(),
            subject: "ex:s".into(),
            predicate: "ex:p".into(),
            object_iri: None,
            object_lit: None,
            context: "ctx:t".into(),
            polarity: "asserted".into(),
            maturity: 1,
            valid_lo: None,
            valid_hi: None,
            tx_lo: Utc::now(),
            tx_hi: None,
            resolved_subject: None,
            resolved_object: None,
            effective_actions: Default::default(),
            action_allowed: true,
            record_iri: None,
            module_iri: Some("mem:test".into()),
            score: None,
            rank: Some(rank),
        }
    }

    #[test]
    fn unique_statements_survive() {
        let a = "11111111-1111-1111-1111-111111111111";
        let b = "22222222-2222-2222-2222-222222222222";
        let mut input = BTreeMap::new();
        input.insert("m1".into(), vec![row(a, 1)]);
        input.insert("m2".into(), vec![row(b, 1)]);
        let fused = rrf_fuse(input, 60);
        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn overlap_boosts_score() {
        let a = "33333333-3333-3333-3333-333333333333";
        let b = "44444444-4444-4444-4444-444444444444";
        let mut input = BTreeMap::new();
        input.insert("m1".into(), vec![row(a, 1), row(b, 2)]);
        input.insert("m2".into(), vec![row(a, 1)]);
        let fused = rrf_fuse(input, 60);
        // `a` was at rank 1 in both → higher fused score than b at rank 2.
        assert_eq!(fused[0].statement_id, Uuid::parse_str(a).unwrap());
        assert_eq!(fused[1].statement_id, Uuid::parse_str(b).unwrap());
        assert!(fused[0].score.unwrap() > fused[1].score.unwrap());
    }

    #[test]
    fn ranks_reassigned() {
        let a = "55555555-5555-5555-5555-555555555555";
        let b = "66666666-6666-6666-6666-666666666666";
        let c = "77777777-7777-7777-7777-777777777777";
        let mut input = BTreeMap::new();
        input.insert("m1".into(), vec![row(a, 1), row(b, 2), row(c, 3)]);
        let fused = rrf_fuse(input, 60);
        assert_eq!(fused.iter().map(|r| r.rank).collect::<Vec<_>>(), vec![Some(1), Some(2), Some(3)]);
    }
}
