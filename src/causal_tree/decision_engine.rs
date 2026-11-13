//! The legacy decision hop record of one pinned turn.
//!
//! The prediction hop and its selection pipeline were retired by R9: every
//! turn opens prediction-free with one committed Answer candidate. The hop
//! record stays for the decision-record writers, terminal replay and legacy
//! recovery until Part 2. Advisory prediction runs in
//! `runtime::decision_advisory` through [`PredictionPort`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use super::branch::{CandidateId, CandidateSpec, CausalCandidate};
use super::candidate_generator::CandidateSet;
use super::decision_score::CandidateScore;
use super::decision_select::Selection;
use super::gates::{GateError, GateOutcome, GateReport};
use super::ids::DecisionId;
use super::questions::CompileError;
use super::state::{CausalState, DerivedCandidate, DerivedState, DuplicateCandidate, SideEffectMode, TrustedParts};
use crate::decision::DecisionError;
use crate::decision::factory::PredictionResult;
use crate::decision::telemetry::CausalLineage;
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::catalog::CatalogView;
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest, ChildManifestError};
use crate::tools::capability::contract::{
    ApprovalRequirement, CapabilityRef, CapabilityTarget, CausalActionKind, CostEstimate, SideEffectClass,
};

/// Borrowed prediction port owned by the engine domain. Its producer keeps
/// physical accounting and transport outside the engine; no boxed future is
/// introduced on builtin/cache or physical paths by this contract.
pub(crate) mod prediction_sealed {
    pub(crate) trait Producer {}
}

pub(crate) trait PredictionPort: prediction_sealed::Producer + Clone + Send + Sync {
    fn predict<'a>(
        &'a self,
        request: &'a crate::decision::DecisionRequest,
        lineage: &'a CausalLineage,
    ) -> impl std::future::Future<Output = Result<PredictionResult, DecisionError>> + Send + 'a;
}

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

/// The derived state of an admitted candidate set: what the question
/// compiler projects and checks against the set before sealing a request.
pub(crate) fn derive_state(set: &CandidateSet, catalog: &CatalogView<'_>) -> Result<DerivedState, DuplicateCandidate> {
    let CandidateSet::Legal {
        candidates,
        exact_mentions,
        report,
        ..
    } = set
    else {
        return DerivedState::new(Vec::new());
    };
    DerivedState::new(
        candidates
            .iter()
            .map(|candidate| DerivedCandidate {
                candidate: candidate.clone(),
                summary: if candidate.action() == CausalActionKind::UseTool
                    && candidate.target() == crate::tools::capability::contract::CapabilityTarget::Filesystem
                    && candidate.required_capabilities().len() > 1
                {
                    let names = candidate
                        .required_capabilities()
                        .iter()
                        .filter_map(|reference| catalog.entry_any(&reference.id))
                        .map(|entry| entry.public_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Arc::from(format!("Native workspace file operations: {names}"))
                } else {
                    Arc::from(
                        candidate
                            .required_capabilities()
                            .first()
                            .and_then(|reference| catalog.entry_any(&reference.id))
                            .map_or("Fixed terminal", |entry| entry.summary.as_str()),
                    )
                },
                exact_mention: exact_mentions.contains(candidate.candidate_id()),
                available: true,
                effective_approval_required: match report.outcomes.get(candidate.candidate_id()) {
                    Some(GateOutcome::Passed) => Some(false),
                    Some(GateOutcome::ApprovalOnly { .. } | GateOutcome::ApprovalDeferred) => Some(true),
                    _ => None,
                },
            })
            .collect(),
    )
}
