//! The legacy decision hop record of one pinned turn.
//!
//! The prediction hop and its selection pipeline were retired by R9: every
//! turn opens prediction-free with one committed Answer candidate. The hop
//! record stays for the decision-record writers, terminal replay and legacy
//! recovery until Part 2. Advisory prediction runs in
//! `runtime::decision_advisory` through
//! `decision_engine::prediction_port::PredictionPort`.

use std::collections::BTreeSet;
use std::time::Instant;

use crate::causal_tree::gates::GateError;
use crate::causal_tree::ids::DecisionId;
use crate::decision_engine::questions::CompileError;
use crate::decision_engine::state::{CausalState, DuplicateCandidate};
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest, ChildManifestError};
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::turn_state::{SideEffectMode, TrustedParts};

pub(crate) struct EngineDecision {
    pub decision_id: DecisionId,
    pub deadline: Instant,
    pub state: CausalState,
    /// A new sealed strict subset derived from the current turn's authority.
    pub outgoing_child: Option<ChildCapabilityManifest>,
    pub outgoing_parent_allowed: Option<BTreeSet<CapabilityRef>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("selected discovery is unavailable for this pinned turn")]
pub(crate) struct DiscoveryIssueError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("selected retrieval is unavailable for this pinned turn")]
pub(crate) struct RetrievalIssueError;

impl EngineDecision {
    /// Trusted state of a turn opened without a prediction port.
    pub(crate) const fn without_prediction(state: CausalState, deadline: Instant, decision_id: DecisionId) -> Self {
        Self {
            decision_id,
            deadline,
            state,
            outgoing_child: None,
            outgoing_parent_allowed: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EngineError {
    #[error(transparent)]
    Gates(#[from] GateError),
    #[error(transparent)]
    Derived(#[from] DuplicateCandidate),
    #[error(transparent)]
    Compiler(#[from] CompileError),
    #[error(transparent)]
    ChildManifest(#[from] ChildManifestError),
}

pub(crate) fn authority_within_policy(
    parent: &CapabilityConstraints,
    trusted: &TrustedParts,
    policy: &SecurityPolicy,
    max_depth: u8,
) -> bool {
    let level_allowed = match policy.autonomy {
        AutonomyLevel::ReadOnly => parent.authority.level == AutonomyLevel::ReadOnly,
        AutonomyLevel::Supervised => parent.authority.level != AutonomyLevel::Full,
        AutonomyLevel::Full => true,
    };
    level_allowed
        && (trusted.side_effect_mode != SideEffectMode::ReadOnly || parent.authority.level == AutonomyLevel::ReadOnly)
        && parent.authority.max_cost_cents <= policy.max_cost_per_day_cents
        && u64::from(parent.authority.max_cost_cents).saturating_mul(10_000) <= trusted.budgets.micro_usd
        && parent.authority.remaining_depth <= max_depth
}
