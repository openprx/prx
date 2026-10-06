#![cfg(test)]
//! Counterfactuals against an already validated actual service context.
#![allow(clippy::expect_used, clippy::panic)]
use super::*;

fn must_reject(service: &TurnDecisionService, proof: &AgentTurnProof, context: &TurnDecisionContext) {
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| semantic_context(
            service, proof, context
        )))
        .is_err(),
        "changed owned fixture must fail the witness"
    );
}
fn first(context: &mut TurnDecisionContext) -> &mut crate::causal_tree::decision_engine::EngineDecision {
    if context.prior_hops.is_empty() {
        &mut context.engine
    } else {
        context.prior_hops.first_mut().expect("nonempty prior hops")
    }
}
pub(super) fn run(
    service: &TurnDecisionService,
    proof: &AgentTurnProof,
    context: &mut Arc<TurnDecisionContext>,
) -> usize {
    use crate::causal_tree::branch::{CandidateId, CandidateSpec, CapabilityTarget, CausalCandidate};
    use crate::causal_tree::decision_engine::PredictionAuditAnswer;
    use crate::causal_tree::gates::GateOutcome;
    let original = semantic_context(service, proof, context);
    let selected = match &context.engine.selection {
        Selection::Abstain { action, .. } => CandidateId::new(*action, CapabilityTarget::None, None),
        _ => panic!("actual selected terminal required"),
    };
    let mut count = 0;
    for pass in 0..3 {
        let owned = Arc::get_mut(context).expect("unique actual fixture");
        let report = match pass {
            0 => match &mut owned.engine.candidates {
                CandidateSet::Legal { report, .. } | CandidateSet::NoLegalPath { report, .. } => report,
            },
            1 => &mut owned.engine.post_rehearsal,
            _ => &mut owned.engine.pre_commit,
        };
        let previous = report
            .outcomes
            .insert(
                selected.clone(),
                GateOutcome::Blocked {
                    gate: 2,
                    reason_code: "fixture_blocked",
                },
            )
            .expect("existing selected gate");
        must_reject(service, proof, context);
        let owned = Arc::get_mut(context).expect("unique fixture after check");
        let report = match pass {
            0 => match &mut owned.engine.candidates {
                CandidateSet::Legal { report, .. } | CandidateSet::NoLegalPath { report, .. } => report,
            },
            1 => &mut owned.engine.post_rehearsal,
            _ => &mut owned.engine.pre_commit,
        };
        report.outcomes.insert(selected.clone(), previous);
        assert_eq!(semantic_context(service, proof, context), original);
        count += 1;
    }
    let hop = first(Arc::get_mut(context).expect("unique trace mutation fixture"));
    let trace = hop.audit_trace.as_mut().expect("actual trace");
    let id = trace
        .answers
        .iter()
        .find_map(|(id, a)| matches!(a, PredictionAuditAnswer::Noul { .. }).then_some(id.clone()))
        .expect("actual numeric Noul");
    let old = trace
        .answers
        .insert(
            id.clone(),
            PredictionAuditAnswer::Noul {
                probability_true: 0.123_456_7,
            },
        )
        .expect("real previous answer");
    must_reject(service, proof, context);
    first(Arc::get_mut(context).expect("restore actual trace"))
        .audit_trace
        .as_mut()
        .expect("trace")
        .answers
        .insert(id, old);
    assert_eq!(semantic_context(service, proof, context), original);
    count += 1;

    let prediction = first(Arc::get_mut(context).expect("unique provenance fixture"))
        .prediction
        .as_mut()
        .expect("actual prediction");
    let old = prediction.served_locality;
    prediction.served_locality = crate::decision::BackendLocality::Loopback;
    must_reject(service, proof, context);
    first(Arc::get_mut(context).expect("restore provenance"))
        .prediction
        .as_mut()
        .expect("prediction")
        .served_locality = old;
    assert_eq!(semantic_context(service, proof, context), original);
    count += 1;

    for generation in [false, true] {
        let hop = first(Arc::get_mut(context).expect("unique ref fixture"));
        let CandidateSet::Legal { candidates, .. } = &mut hop.candidates else {
            panic!("actual legal prediction")
        };
        let index = candidates
            .iter()
            .position(|c| !c.required_capabilities().is_empty())
            .expect("full real ref");
        let old = candidates.get(index).expect("selected actual candidate").clone();
        let mut refs = old.required_capabilities().to_vec();
        if generation {
            refs.first_mut().expect("actual ref").generation = crate::causal_tree::branch::CatalogGeneration([0; 32]);
        } else {
            *refs
                .first_mut()
                .expect("actual ref")
                .fingerprint
                .0
                .first_mut()
                .expect("fixed fingerprint byte") ^= 1;
        }
        *candidates.get_mut(index).expect("selected actual candidate") = CausalCandidate::new(
            old.action(),
            old.target(),
            CandidateSpec {
                namespace: old.candidate_id().namespace(),
                required_capabilities: refs,
                side_effect: old.side_effect(),
                approval: old.approval().clone(),
                estimated_cost: old.estimated_cost(),
                estimated_latency_ms: old.estimated_latency_ms(),
            },
        );
        must_reject(service, proof, context);
        let CandidateSet::Legal { candidates, .. } =
            &mut first(Arc::get_mut(context).expect("restore actual ref")).candidates
        else {
            panic!("legal set")
        };
        *candidates.get_mut(index).expect("selected actual candidate") = old;
        assert_eq!(semantic_context(service, proof, context), original);
        count += 1;
    }
    let connection = rusqlite::Connection::open(service.store.path()).expect("only owned temporary fixture DB");
    let workspace = service.store.workspace_id().expect("fixture workspace");
    let decision = context.engine.decision_id.to_string();
    let old: String = connection
        .query_row(
            "SELECT record_hash FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND decision_id=?3",
            rusqlite::params![workspace.as_ref(), proof.owner_id(), decision],
            |row| row.get(0),
        )
        .expect("actual checksum");
    assert_eq!(
        connection
            .execute(
                "UPDATE decisions SET record_hash=?1 WHERE workspace_id=?2 AND owner_id=?3 AND decision_id=?4",
                rusqlite::params![
                    "fixture_corrupt_checksum",
                    workspace.as_ref(),
                    proof.owner_id(),
                    decision
                ]
            )
            .expect("single fixture update"),
        1
    );
    must_reject(service, proof, context);
    assert_eq!(
        connection
            .execute(
                "UPDATE decisions SET record_hash=?1 WHERE workspace_id=?2 AND owner_id=?3 AND decision_id=?4",
                rusqlite::params![old, workspace.as_ref(), proof.owner_id(), decision]
            )
            .expect("restore fixture checksum"),
        1
    );
    assert_eq!(semantic_context(service, proof, context), original);
    count += 1;
    // Duplicate only the owned temporary row, with a new ID. Known rows and
    // their valid checksums remain unchanged; the full scoped set must catch it.
    let columns: Vec<String> = connection
        .prepare("PRAGMA table_info(decisions)")
        .expect("owned schema")
        .query_map([], |row| row.get(1))
        .expect("owned columns")
        .collect::<rusqlite::Result<_>>()
        .expect("schema columns");
    assert!(columns.iter().any(|name| name == "decision_id"));
    let names = columns
        .iter()
        .map(|name| format!("\"{}\"", name.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    let selected = columns
        .iter()
        .zip(&names)
        .map(|(name, quoted)| {
            if name == "decision_id" {
                "?1".to_owned()
            } else {
                quoted.clone()
            }
        })
        .collect::<Vec<_>>();
    let orphan = DecisionId::new_runtime().to_string();
    let sql = format!(
        "INSERT INTO decisions ({}) SELECT {} FROM decisions WHERE workspace_id=?2 AND owner_id=?3 AND decision_id=?4",
        names.join(","),
        selected.join(",")
    );
    assert_eq!(
        connection
            .execute(
                &sql,
                rusqlite::params![orphan, workspace.as_ref(), proof.owner_id(), decision]
            )
            .expect("single owned orphan counterfactual"),
        1
    );
    must_reject(service, proof, context);
    assert_eq!(
        connection
            .execute(
                "DELETE FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND decision_id=?3",
                rusqlite::params![workspace.as_ref(), proof.owner_id(), orphan]
            )
            .expect("restore only owned temporary orphan"),
        1
    );
    assert_eq!(semantic_context(service, proof, context), original);
    count += 1;
    count
}
