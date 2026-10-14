//! Convert a gated selection into a narrow runtime execution directive.

use chrono::{DateTime, Utc};

use super::branch::{
    CandidateId, CapabilityId, CapabilityRef, CapabilityTarget, CausalActionKind, OpId, SideEffectClass,
};
use super::candidate_generator::{CandidateSet, PendingApproval};
use super::decision_engine::EngineDecision;
use super::decision_select::Selection;
use super::gates::GateOutcome;
use super::ids::DecisionId;
use crate::tools::capability::child::ChildCapabilityManifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectedReadPurpose {
    QueryEvidence,
    RouterHistory,
}

#[derive(Debug, Clone)]
pub(crate) enum TurnExecutionDirective {
    CollectApprovalArguments(super::approval_collection::DeferredArgumentProof),
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
    Clarify {
        reason_code: String,
    },
    RequestApproval {
        approval_scope: ApprovalScope,
    },
    Refuse {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "CDP-PLAN retains the internal refusal code; public refusal text must not disclose policy detail"
            )
        )]
        policy_code: String,
    },
}

/// Render only terminal dispositions that need no provider or capability call.
/// Reason codes are internal diagnostics and never enter the reply text.
pub(crate) fn render_terminal_directive(
    directive: &TurnExecutionDirective,
) -> Result<std::borrow::Cow<'static, str>, DirectiveError> {
    match directive {
        TurnExecutionDirective::Clarify { reason_code } => Ok(std::borrow::Cow::Borrowed(match reason_code.as_str() {
            "deadline_exceeded" => "Could you narrow your request so I can finish it in time?",
            "retrieve_evidence" | "hop_stopped" => "What specific information should I use to answer your request?",
            "discover_capability" => "Which action would you like me to take?",
            _ => "Could you clarify the result you want?",
        })),
        TurnExecutionDirective::RequestApproval { approval_scope } => Ok(std::borrow::Cow::Owned(format!(
            "Approval is required for decision {} (effect: {}). Review this decision before resuming it. The operation has not run.",
            approval_scope.decision_id,
            approval_scope.effect.as_str(),
        ))),
        TurnExecutionDirective::Refuse { .. } => Ok(std::borrow::Cow::Borrowed(
            "I can't complete this request under the current policy.",
        )),
        _ => Err(DirectiveError::ActionMismatch),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalScope {
    pub decision_id: DecisionId,
    pub capability_refs: Vec<CapabilityRef>,
    pub op_ids: Vec<OpId>,
    pub effect: SideEffectClass,
    pub owner: String,
    pub workspace: String,
    pub session: Option<String>,
    pub call_digest: Option<[u8; 32]>,
    pub catalog_generation: super::branch::CatalogGeneration,
    pub expires_at: DateTime<Utc>,
    pub single_use: bool,
}

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
        Selection::CollectApprovalArguments(proof) => {
            if proof.capability().generation != context.current_generation
                || decision.prediction.is_some()
                || decision.request_compiled
                || decision.outgoing_child.is_some()
            {
                return Err(DirectiveError::NotAdmitted);
            }
            Ok(TurnExecutionDirective::CollectApprovalArguments(proof.clone()))
        }
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
                    families: match discoverable_families.iter().collect::<Vec<_>>() {
                        families if !families.is_empty() => families,
                        _ => vec![candidate.target()],
                    },
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
                    let decision_bound = manifest.parent_decision_id == decision.decision_id;
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
            let owner = trusted.owner_id.as_ref().ok_or(DirectiveError::ApprovalScopeMismatch)?;
            let call_digest = decision.call_fingerprints.get(pending_candidate_id).copied();
            // Current producers have no independently proven fixed-scope
            // approval. A later producer may justify an optional digest;
            // this path requires a bound operation for every action today.
            if call_digest.is_none() {
                return Err(DirectiveError::ApprovalScopeMismatch);
            }
            let now = Utc::now();
            if context.approval_expires_at <= now || context.approval_expires_at > now + chrono::Duration::minutes(5) {
                return Err(DirectiveError::ApprovalScopeMismatch);
            }
            Ok(TurnExecutionDirective::RequestApproval {
                approval_scope: ApprovalScope {
                    decision_id: decision.decision_id,
                    capability_refs: pending.candidate.required_capabilities().to_vec(),
                    op_ids: op_ids.clone(),
                    effect: pending.candidate.side_effect(),
                    owner: owner.to_string(),
                    workspace: trusted.workspace_id.to_string(),
                    session: trusted.session_key.as_ref().map(ToString::to_string),
                    call_digest,
                    catalog_generation: trusted.catalog_generation,
                    expires_at: context.approval_expires_at,
                    single_use: true,
                },
            })
        }
        Selection::Abstain { action, reason_code } => {
            let candidate_id = CandidateId::new(*action, CapabilityTarget::None, None);
            require_pass(&candidate_id, decision)?;
            match action {
                CausalActionKind::Clarify => Ok(TurnExecutionDirective::Clarify {
                    reason_code: (*reason_code).into(),
                }),
                CausalActionKind::Refuse => Ok(TurnExecutionDirective::Refuse {
                    policy_code: (*reason_code).into(),
                }),
                _ => Err(DirectiveError::ActionMismatch),
            }
        }
    }
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
mod tests {
    use super::*;

    #[test]
    fn terminal_renderer_never_exposes_reason_or_capability_names() {
        let hidden = "HIDDEN-CAP-9V";
        for directive in [
            TurnExecutionDirective::Clarify {
                reason_code: hidden.to_string(),
            },
            TurnExecutionDirective::Refuse {
                policy_code: hidden.to_string(),
            },
        ] {
            let reply = render_terminal_directive(&directive).expect("terminal reply");
            assert!(!reply.contains(hidden));
            assert!(!reply.is_empty());
        }
        assert_eq!(
            render_terminal_directive(&TurnExecutionDirective::Clarify {
                reason_code: "deadline_exceeded".to_string(),
            }),
            Ok(std::borrow::Cow::Borrowed(
                "Could you narrow your request so I can finish it in time?"
            ))
        );
        assert!(matches!(
            render_terminal_directive(&TurnExecutionDirective::AnswerDirectly),
            Err(DirectiveError::ActionMismatch)
        ));
    }
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::super::branch::{
        ApprovalRequirement, CandidateSpec, CatalogGeneration, CausalCandidate, SchemaFingerprint,
    };
    use super::super::candidate_generator::NoLegalReason;
    use super::super::decision_engine::FallbackCause;
    use super::super::gates::GateReport;
    use super::super::state::{TrustedState, fixtures};

    #[test]
    fn approval_scope_uses_pinned_decision_and_call_digest() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.owner_id = Some(Arc::from("prx_user"));
        trusted.workspace_id = Arc::from("prx_workspace");
        state.trusted = TrustedState::from_runtime(trusted);
        let generation = state.trusted.parts().catalog_generation;
        let id = DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap();
        let reference = CapabilityRef {
            id: CapabilityId::new(CapabilityTarget::NativeTool, "prx", "read").unwrap(),
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
        let mut decision = EngineDecision {
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
        let context = DirectiveContext {
            current_generation: generation,
            approval_expires_at: Utc::now() + chrono::Duration::minutes(1),
            discovery_limit: 4,
        };
        let TurnExecutionDirective::RequestApproval { approval_scope } =
            directive_from_selection(&decision, &context).unwrap()
        else {
            panic!("expected approval directive")
        };
        assert_eq!(approval_scope.decision_id, id);
        assert_eq!(approval_scope.owner, "prx_user");
        assert_eq!(approval_scope.workspace, "prx_workspace");
        assert_eq!(approval_scope.call_digest, Some([6; 32]));
        assert_eq!(approval_scope.capability_refs, vec![reference]);
        assert_eq!(approval_scope.op_ids, vec![op_id]);
        assert!(approval_scope.single_use);

        let stale = DirectiveContext {
            current_generation: CatalogGeneration([8; 32]),
            ..context
        };
        assert!(matches!(
            directive_from_selection(&decision, &stale),
            Err(DirectiveError::NotAdmitted)
        ));
        decision.call_fingerprints.clear();
        assert!(matches!(
            directive_from_selection(
                &decision,
                &DirectiveContext {
                    current_generation: generation,
                    approval_expires_at: Utc::now() + chrono::Duration::minutes(1),
                    discovery_limit: 4,
                }
            ),
            Err(DirectiveError::ApprovalScopeMismatch)
        ));
        decision.deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("test clock supports one millisecond subtraction");
        decision.selection = Selection::Abstain {
            action: CausalActionKind::Refuse,
            reason_code: "deadline_exceeded",
        };
        assert!(matches!(
            directive_from_selection(&decision, &DirectiveContext {
                current_generation: generation,
                approval_expires_at: Utc::now() + chrono::Duration::minutes(1),
                discovery_limit: 4,
            }),
            Ok(TurnExecutionDirective::Refuse { policy_code }) if policy_code == "deadline_exceeded"
        ));
    }
}
