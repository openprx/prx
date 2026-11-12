//! Convert a gated selection into a narrow runtime execution directive.

use chrono::{DateTime, Utc};

use super::branch::{CandidateId, CapabilityId, CapabilityRef, CapabilityTarget, CausalActionKind};

use super::candidate_generator::{CandidateSet, PendingApproval};
use super::decision_engine::EngineDecision;
use super::decision_select::Selection;
use super::gates::GateOutcome;

use crate::tools::capability::child::ChildCapabilityManifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectedReadPurpose {
    QueryEvidence,
    RouterHistory,
}

#[derive(Debug, Clone)]
pub(crate) enum TurnExecutionDirective {
    AnswerDirectly,
    Retrieve {
        sources: Vec<CapabilityRef>,
        purpose: SelectedReadPurpose,
    },
    Discover {
        families: Vec<CapabilityTarget>,
        query: String,
        limit: usize,
    },
    Execute {
        allowed_capability_ids: Vec<CapabilityId>,
    },
    Delegate {
        manifest: ChildCapabilityManifest,
    },
    Clarify {},
    RequestApproval {},
    Refuse {},
}

/// The longest an approval request may stay open: an approval scope expiring
/// later than this after issue is refused as mismatched.
const MAX_APPROVAL_LIFETIME: chrono::TimeDelta = chrono::TimeDelta::minutes(5);

pub(crate) struct DirectiveContext {
    /// The service must supply the same pinned generation at the execution boundary.
    pub current_generation: super::branch::CatalogGeneration,
    pub approval_expires_at: DateTime<Utc>,
    pub discovery_limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DirectiveError {
    #[error("selected candidate is absent or did not pass the final gate")]
    NotAdmitted,
    #[error("selected action cannot be converted to a directive")]
    ActionMismatch,
    #[error("approval scope is incomplete or changed after admission")]
    ApprovalScopeMismatch,
    #[error("delegation manifest is absent or invalid")]
    InvalidChildScope,
}

pub(crate) fn directive_from_selection(
    decision: &EngineDecision,
    context: &DirectiveContext,
) -> Result<TurnExecutionDirective, DirectiveError> {
    let trusted = decision.state.trusted.parts();
    // The decision engine replaces an executable selection with its terminal
    // fallback when the decision deadline has expired before finalization.
    // Re-reading the clock here makes an already final-gated selection depend
    // on scheduler delay between those adjacent steps. Execution boundaries
    // still enforce their own live deadlines and generation parity.
    if context.current_generation != trusted.catalog_generation {
        return Err(DirectiveError::NotAdmitted);
    }
    match &decision.selection {
        Selection::Commit { candidate_id, .. } | Selection::Recover { candidate_id, .. } => {
            let CandidateSet::Legal {
                candidates,
                discoverable_families,
                ..
            } = &decision.candidates
            else {
                return Err(DirectiveError::NotAdmitted);
            };
            let candidate = candidates
                .iter()
                .find(|item| item.candidate_id() == candidate_id)
                .ok_or(DirectiveError::NotAdmitted)?;
            require_pass(candidate_id, decision)?;
            if matches!(decision.selection, Selection::Recover { .. })
                && !matches!(
                    candidate.action(),
                    CausalActionKind::Answer | CausalActionKind::Retrieve | CausalActionKind::Discover
                )
            {
                return Err(DirectiveError::ActionMismatch);
            }
            match candidate.action() {
                CausalActionKind::Answer => Ok(TurnExecutionDirective::AnswerDirectly),
                CausalActionKind::Retrieve => Ok(TurnExecutionDirective::Retrieve {
                    sources: candidate.required_capabilities().to_vec(),
                    purpose: if decision.state.trusted.router_history().is_some() {
                        SelectedReadPurpose::RouterHistory
                    } else {
                        SelectedReadPurpose::QueryEvidence
                    },
                }),
                CausalActionKind::Discover => Ok(TurnExecutionDirective::Discover {
                    // The builtin's language-neutral candidate searches all
                    // eligible families. A concrete selected family remains
                    // valid for exact recovery and issuer revalidation.
                    families: selected_discovery_family(decision)?.map_or_else(
                        || match discoverable_families.iter().collect::<Vec<_>>() {
                            families if !families.is_empty() => families,
                            _ => vec![candidate.target()],
                        },
                        |family| vec![family],
                    ),
                    query: decision.state.routing_input.as_str().to_string(),
                    limit: match context.discovery_limit {
                        1..=64 => context.discovery_limit,
                        _ => return Err(DirectiveError::ActionMismatch),
                    },
                }),
                CausalActionKind::UseTool => {
                    if candidate.required_capabilities().is_empty() {
                        return Err(DirectiveError::ActionMismatch);
                    }
                    let ids = candidate
                        .required_capabilities()
                        .iter()
                        .map(|reference| reference.id.clone())
                        .collect();
                    Ok(TurnExecutionDirective::Execute {
                        allowed_capability_ids: ids,
                    })
                }
                CausalActionKind::Delegate => {
                    let manifest = decision
                        .outgoing_child
                        .as_ref()
                        .ok_or(DirectiveError::InvalidChildScope)?;
                    let parent_allowed = decision
                        .outgoing_parent_allowed
                        .as_ref()
                        .ok_or(DirectiveError::InvalidChildScope)?;
                    let decision_bound = manifest.parent_authority_id == decision.decision_id.into();
                    let generation_bound = manifest.catalog_generation == trusted.catalog_generation;
                    let workspace_bound = manifest.authority.workspace == trusted.workspace_id.as_ref();
                    let owner_bound = trusted.owner_id.as_deref() == Some(manifest.authority.owner.as_str());
                    let out_of_scope = candidate
                        .required_capabilities()
                        .iter()
                        .any(|reference| !parent_allowed.contains(reference));
                    if !(decision_bound && generation_bound && workspace_bound && owner_bound) || out_of_scope {
                        return Err(DirectiveError::InvalidChildScope);
                    }
                    Ok(TurnExecutionDirective::Delegate {
                        manifest: manifest.clone(),
                    })
                }
                _ => Err(DirectiveError::ActionMismatch),
            }
        }
        Selection::RequestApproval {
            terminal_id,
            pending_candidate_id,
            op_ids,
            ..
        } => {
            require_pass(terminal_id, decision)?;
            let pending = pending_approval(&decision.candidates, terminal_id, pending_candidate_id)
                .ok_or(DirectiveError::ApprovalScopeMismatch)?;
            if op_ids.is_empty()
                || &pending.op_ids != op_ids
                || pending.candidate.required_capabilities().is_empty()
                || !matches!(decision.pre_commit.outcomes.get(pending_candidate_id),
                    Some(GateOutcome::ApprovalOnly { op_ids: gated }) if gated == op_ids)
            {
                return Err(DirectiveError::ApprovalScopeMismatch);
            }
            trusted.owner_id.as_ref().ok_or(DirectiveError::ApprovalScopeMismatch)?;
            let call_digest = decision.call_fingerprints.get(pending_candidate_id).copied();
            // Current producers have no independently proven fixed-scope
            // approval. A later producer may justify an optional digest;
            // this path requires a bound operation for every action today.
            if call_digest.is_none() {
                return Err(DirectiveError::ApprovalScopeMismatch);
            }
            let now = Utc::now();
            if context.approval_expires_at <= now || context.approval_expires_at > now + MAX_APPROVAL_LIFETIME {
                return Err(DirectiveError::ApprovalScopeMismatch);
            }
            Ok(TurnExecutionDirective::RequestApproval {})
        }
        Selection::Abstain { action, .. } => {
            let candidate_id = CandidateId::new(*action, CapabilityTarget::None, None);
            require_pass(&candidate_id, decision)?;

            match action {
                CausalActionKind::Answer => Ok(TurnExecutionDirective::AnswerDirectly),
                CausalActionKind::Clarify => Ok(TurnExecutionDirective::Clarify {}),
                CausalActionKind::Refuse => Ok(TurnExecutionDirective::Refuse {}),
                _ => Err(DirectiveError::ActionMismatch),
            }
        }
    }
}

/// The low-utility recovery may inspect only the validated chosen family.
/// This narrows metadata search; it grants no execution authority.
pub(crate) fn selected_discovery_family(decision: &EngineDecision) -> Result<Option<CapabilityTarget>, DirectiveError> {
    if !matches!(
        decision.selection,
        Selection::Recover {
            reason_code: "discover_unrepresented_operation",
            ..
        }
    ) {
        return Ok(None);
    }
    if !decision.served_prediction_succeeded() {
        return Err(DirectiveError::NotAdmitted);
    }
    let trace = decision.audit_trace.as_ref().ok_or(DirectiveError::NotAdmitted)?;
    let key = crate::decision::GlobalQuestion::CapabilityTarget.as_str();
    let chosen = match trace.answers.get(key) {
        Some(super::decision_engine::PredictionAuditAnswer::Choice { chosen, .. }) => chosen,
        None => trace.forced_choices.get(key).ok_or(DirectiveError::NotAdmitted)?,
        _ => return Err(DirectiveError::NotAdmitted),
    };
    let family = CapabilityTarget::ALL
        .into_iter()
        .find(|family| family.as_str() == chosen)
        .filter(|family| super::candidate_generator::DiscoveryFamilies::supports(*family))
        .ok_or(DirectiveError::NotAdmitted)?;
    let CandidateSet::Legal {
        discoverable_families,
        candidates,
        ..
    } = &decision.candidates
    else {
        return Err(DirectiveError::NotAdmitted);
    };
    let eligible = discoverable_families.iter().any(|eligible| eligible == family)
        || (discoverable_families.iter().next().is_none()
            && candidates
                .iter()
                .any(|candidate| candidate.action() == CausalActionKind::Discover && candidate.target() == family));
    if !eligible {
        return Err(DirectiveError::NotAdmitted);
    }
    Ok(Some(family))
}

fn require_pass(candidate_id: &CandidateId, decision: &EngineDecision) -> Result<(), DirectiveError> {
    let (admitted, report) = match &decision.candidates {
        CandidateSet::Legal { candidates, report, .. } => (candidates, report),
        CandidateSet::NoLegalPath { terminals, report, .. } => (terminals, report),
    };
    if admitted
        .iter()
        .any(|candidate| candidate.candidate_id() == candidate_id)
        && matches!(report.outcomes.get(candidate_id), Some(GateOutcome::Passed))
        && matches!(
            decision.pre_commit.outcomes.get(candidate_id),
            Some(GateOutcome::Passed)
        )
    {
        Ok(())
    } else {
        Err(DirectiveError::NotAdmitted)
    }
}

fn pending_approval<'a>(
    set: &'a CandidateSet,
    terminal_id: &CandidateId,
    pending_id: &CandidateId,
) -> Option<&'a PendingApproval> {
    match set {
        CandidateSet::Legal { approval_scopes, .. } => approval_scopes
            .get(terminal_id)?
            .iter()
            .find(|item| item.candidate.candidate_id() == pending_id),
        CandidateSet::NoLegalPath { approval_only, .. } => approval_only.iter().find(|item| {
            item.candidate.candidate_id() == pending_id
                && CandidateId::new(CausalActionKind::RequestApproval, item.candidate.target(), None) == *terminal_id
        }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::causal_tree::branch::{ApprovalRequirement, CandidateSpec, CausalCandidate, SchemaFingerprint};
    use crate::causal_tree::branch::{OpId, SideEffectClass};
    use crate::causal_tree::candidate_generator::NoLegalReason;
    use crate::causal_tree::decision_engine::FallbackCause;
    use crate::causal_tree::gates::GateReport;
    use crate::causal_tree::ids::DecisionId;
    use crate::causal_tree::state::{TrustedState, fixtures};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    pub(crate) fn recorded_request_approval_hop() -> (EngineDecision, crate::causal_tree::store::DecisionRecord) {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.owner_id = Some(Arc::from("prx_user"));
        trusted.workspace_id = Arc::from("prx_workspace");
        state.trusted = TrustedState::from_runtime(trusted);
        let generation = state.trusted.parts().catalog_generation;
        let id = DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").expect("test: historical approval fixture");
        let reference = CapabilityRef {
            id: CapabilityId::new(CapabilityTarget::NativeTool, "prx", "read")
                .expect("test: historical approval fixture"),
            fingerprint: SchemaFingerprint([3; 32]),
            generation,
        };
        let op_id = OpId::new("prx.operation");
        let pending = CausalCandidate::new(
            CausalActionKind::UseTool,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference.clone()],
                side_effect: SideEffectClass::LocalWrite,
                approval: ApprovalRequirement::Required {
                    op_ids: vec![op_id.clone()],
                },
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let terminal_id = CandidateId::new(CausalActionKind::RequestApproval, CapabilityTarget::NativeTool, None);
        let refusal_id = CandidateId::new(CausalActionKind::Refuse, CapabilityTarget::None, None);
        let terminal = CausalCandidate::new(
            CausalActionKind::RequestApproval,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let pending_id = pending.candidate_id().clone();
        let refusal = CausalCandidate::new(
            CausalActionKind::Refuse,
            CapabilityTarget::None,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let gates = GateReport {
            outcomes: BTreeMap::from([
                (terminal_id.clone(), GateOutcome::Passed),
                (refusal_id, GateOutcome::Passed),
                (
                    pending_id.clone(),
                    GateOutcome::ApprovalOnly {
                        op_ids: vec![op_id.clone()],
                    },
                ),
            ]),
            gate_set_hash: [4; 32],
        };
        let decision = EngineDecision {
            decision_id: id,
            deadline: Instant::now() + Duration::from_secs(5),
            elapsed_ms: 0,
            state,
            candidates: CandidateSet::NoLegalPath {
                reason: NoLegalReason::NoLegalCandidate,
                terminals: vec![terminal, refusal],
                approval_only: vec![PendingApproval {
                    candidate: pending,
                    op_ids: vec![op_id.clone()],
                }],
                report: gates.clone(),
            },
            selection: Selection::RequestApproval {
                terminal_id,
                pending_candidate_id: pending_id.clone(),
                op_ids: vec![op_id.clone()],
                reason_code: "approval_only",
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: Some(FallbackCause::NoLegalPath),
            scores: BTreeMap::new(),
            scored_count: 0,
            rehearsal: BTreeMap::new(),
            post_rehearsal: gates.clone(),
            pre_commit: gates,
            call_fingerprints: BTreeMap::from([(pending_id, [6; 32])]),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        };
        let record = crate::causal_tree::outcome::DecisionRecordBuilder::build_hop(&decision, &key, None, true, false)
            .expect("test: historical approval record");
        (decision, record)
    }
    #[test]
    fn approval_scope_uses_pinned_decision_and_call_digest() {
        let (decision, _) = recorded_request_approval_hop();
        let context = DirectiveContext {
            current_generation: decision.state.trusted.parts().catalog_generation,
            approval_expires_at: Utc::now() + chrono::Duration::minutes(1),
            discovery_limit: 4,
        };
        assert!(
            matches!(
                directive_from_selection(&decision, &context),
                Ok(TurnExecutionDirective::RequestApproval { .. })
            ),
            "test: structurally valid exact-call approval"
        );
    }
}
