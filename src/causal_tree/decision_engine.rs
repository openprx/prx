//! The legacy decision hop record of one pinned turn.
//!
//! The prediction hop and its selection pipeline were retired by R9: every
//! turn opens prediction-free with one committed Answer candidate. The hop
//! record stays for the decision-record writers, terminal replay and legacy
//! recovery until Part 2. Advisory prediction runs in
//! `runtime::decision_advisory` through
//! `decision_engine::prediction_port::PredictionPort`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::causal_tree::candidate_generator::CandidateSet;
use crate::causal_tree::decision_select::Selection;
use crate::causal_tree::gates::{GateError, GateOutcome, GateReport};
use crate::causal_tree::ids::DecisionId;
use crate::decision_engine::candidate::{CandidateId, CandidateSpec, CausalCandidate};
use crate::decision_engine::decision_score::CandidateScore;
use crate::decision_engine::prediction::factory::PredictionResult;
use crate::decision_engine::questions::CompileError;
use crate::decision_engine::state::{CausalState, DuplicateCandidate};
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest, ChildManifestError};
use crate::tools::capability::contract::{
    ApprovalRequirement, CapabilityRef, CapabilityTarget, CausalActionKind, CostEstimate, SideEffectClass,
};
use crate::tools::capability::turn_state::{SideEffectMode, TrustedParts};

pub(crate) struct EngineDecision {
    pub decision_id: DecisionId,
    pub deadline: Instant,
    /// Preparation through final selection, measured on one monotonic clock.
    pub elapsed_ms: u64,
    pub state: CausalState,
    pub candidates: CandidateSet,
    pub selection: Selection,
    /// Probability of every candidate in an eligible exploration band.
    pub exploration_propensities: BTreeMap<CandidateId, f64>,
    pub prediction: Option<PredictionResult>,
    /// A sealed protocol request existed even if it timed out or failed.
    pub request_compiled: bool,
    /// Why a conservative terminal fallback was selected. Retained for the decision record.
    pub fallback_cause: Option<FallbackCause>,
    pub scores: BTreeMap<CandidateId, CandidateScore>,
    pub post_rehearsal: GateReport,
    pub pre_commit: GateReport,
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
    /// Trusted state and G6 slice for a disabled engine: no prediction port
    /// and no compiled request. A committed Legal Answer with a known cost
    /// lets later execution stamp the call budget and publish the Direct
    /// catalog surface.
    pub(crate) fn without_prediction(state: CausalState, deadline: Instant, decision_id: DecisionId) -> Self {
        let answer = CausalCandidate::new(
            CausalActionKind::Answer,
            CapabilityTarget::None,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: Some(CostEstimate {
                    tokens: 0,
                    micro_usd: 0,
                }),
                estimated_latency_ms: Some(0),
            },
        );
        let answer_id = answer.candidate_id().clone();
        let gates = GateReport {
            outcomes: BTreeMap::from([(answer_id.clone(), GateOutcome::Passed)]),
        };
        Self {
            decision_id,
            deadline,
            elapsed_ms: 0,
            state,
            candidates: CandidateSet::Legal {
                candidates: vec![answer],
                exact_mentions: BTreeSet::new(),
                approval_scopes: BTreeMap::new(),
                report: gates.clone(),
            },
            selection: Selection::Commit {
                candidate_id: answer_id,
                propensity: 1.0,
                reason_code: Some("engine_disabled"),
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            fallback_cause: None,
            scores: BTreeMap::new(),
            post_rehearsal: gates.clone(),
            pre_commit: gates,
            outgoing_child: None,
            outgoing_parent_allowed: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackCause {
    HopStopped,
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
