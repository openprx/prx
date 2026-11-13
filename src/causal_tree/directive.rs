//! Convert a gated selection into a narrow runtime execution directive.

use super::branch::CandidateId;
use crate::tools::capability::contract::{CapabilityId, CausalActionKind};

use super::candidate_generator::CandidateSet;
use super::decision_engine::EngineDecision;
use super::decision_select::Selection;
use super::gates::GateOutcome;

use crate::tools::capability::child::ChildCapabilityManifest;

#[derive(Debug, Clone)]
pub(crate) enum TurnExecutionDirective {
    AnswerDirectly,
    Retrieve,
    Discover,
    Execute { allowed_capability_ids: Vec<CapabilityId> },
    Delegate { manifest: ChildCapabilityManifest },
}

pub(crate) struct DirectiveContext {
    /// The service must supply the same pinned generation at the execution boundary.
    pub current_generation: crate::tools::capability::contract::CatalogGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DirectiveError {
    #[error("decision selection {kind} is not executable")]
    NonExecutableSelection { kind: &'static str },
    #[error("selected candidate is absent or did not pass the final gate")]
    NotAdmitted,
    #[error("selected action cannot be converted to a directive")]
    ActionMismatch,
    #[error("delegation manifest is absent or invalid")]
    InvalidChildScope,
}

pub(crate) fn directive_from_selection(
    decision: &EngineDecision,
    context: &DirectiveContext,
) -> Result<TurnExecutionDirective, DirectiveError> {
    let trusted = decision.state.trusted.parts();
    // Execution boundaries still enforce their own live deadlines and
    // generation parity.
    if context.current_generation != trusted.catalog_generation {
        return Err(DirectiveError::NotAdmitted);
    }
    let Selection::Commit { candidate_id, .. } = &decision.selection;
    let CandidateSet::Legal { candidates, .. } = &decision.candidates else {
        return Err(DirectiveError::NotAdmitted);
    };
    let candidate = candidates
        .iter()
        .find(|item| item.candidate_id() == candidate_id)
        .ok_or(DirectiveError::NotAdmitted)?;
    require_pass(candidate_id, decision)?;
    match candidate.action() {
        CausalActionKind::Answer => Ok(TurnExecutionDirective::AnswerDirectly),
        CausalActionKind::Retrieve => Ok(TurnExecutionDirective::Retrieve),
        CausalActionKind::Discover => Ok(TurnExecutionDirective::Discover),
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
