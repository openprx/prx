//! Capture-time refusal classification over a legacy engine decision.
//!
//! Only the `GateOutcome`-dependent specialization stays here until Part 2
//! retires it with its last caller (historical terminal replay). The reasons,
//! hints and the reply renderer are execution-owned in `agent::refusal`.

use crate::agent::refusal::{RefusalReason, unpriced_reason};
use crate::causal_tree::candidate_generator::CandidateSet;
use crate::causal_tree::decision_engine::EngineDecision;
use crate::causal_tree::gates::GateOutcome;
use crate::decision_engine::candidate::CausalCandidate;
use crate::tools::capability::contract::CausalActionKind;

/// Specialize a selector/gate code with the pinned decision's G6 facts.
pub(crate) fn classify(code: &str, decision: &EngineDecision) -> RefusalReason {
    let reason = RefusalReason::from_code(code);
    if matches!(
        reason,
        RefusalReason::NoLegalPath
            | RefusalReason::Budget
            | RefusalReason::UnpricedModel { .. }
            | RefusalReason::InvalidPrice
            | RefusalReason::Unknown
    ) {
        refine_budget(decision).unwrap_or(reason)
    } else {
        reason
    }
}

fn refine_budget(decision: &EngineDecision) -> Option<RefusalReason> {
    let generator_report = match &decision.candidates {
        CandidateSet::Legal { report, .. } | CandidateSet::NoLegalPath { report, .. } => report,
    };
    let bodies: Vec<&CausalCandidate> = match &decision.candidates {
        CandidateSet::Legal { candidates, .. } => candidates.iter().collect(),
        CandidateSet::NoLegalPath { terminals, .. } => terminals.iter().collect(),
    };
    let mut unpriced = false;
    let mut exhausted = false;
    for (id, outcome) in generator_report
        .outcomes
        .iter()
        .chain(decision.pre_commit.outcomes.iter())
    {
        if id.action() == CausalActionKind::Refuse {
            continue;
        }
        let GateOutcome::Blocked { reason_code, .. } = outcome else {
            continue;
        };
        match *reason_code {
            "unpriced_model" => unpriced = true,
            "budget" => match bodies.iter().find(|candidate| candidate.candidate_id() == id) {
                Some(candidate) if candidate.estimated_cost().is_none() => unpriced = true,
                Some(_) | None => exhausted = true,
            },
            _ => {}
        }
    }
    if unpriced {
        let parts = decision.state.trusted.parts();
        return Some(unpriced_reason(
            parts.resolved_provider.as_ref(),
            parts.resolved_model_id.as_ref(),
        ));
    }
    if exhausted {
        return Some(RefusalReason::Budget);
    }
    None
}
