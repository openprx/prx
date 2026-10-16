//! One bounded causal decision over one pinned turn and catalog generation.
//! The service owns hop/retrieval orchestration and the execution boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
pub(crate) mod original_observation;
#[cfg(test)]
mod resume_availability_tests;

use super::branch::{CandidateId, CapabilityRef, CapabilityTarget, CausalActionKind, CausalCandidate};
use super::candidate_generator::{
    CandidatePolicy, CandidateSet, CausalCandidateGenerator, DiscoveredRefs, is_retrieval_source,
};
use super::config::CausalTreeConfig;
#[cfg(test)]
use super::decision_rehearsal::RehearsalEvidence;
use super::decision_rehearsal::{BoundedRehearsal, RehearsalContext, RehearsalError, RehearsalPorts};
use super::decision_score::{
    CandidatePrediction, CandidateScore, CausalBranchScorer, ExtractionError, PredictionEvidence, ScoreError,
    ScoreInputs, ScorerWeights,
};
use super::decision_select::{CausalSelector, SelectError, Selection, SelectorContext};
use super::directive::{DirectiveContext, TurnExecutionDirective, directive_from_selection};
use super::gates::{GateError, GateInputs, GateOutcome, GatePass, GateReport, HardGates};
use super::ids::DecisionId;
use super::questions::{CompileError, CompiledQuestionRequest, CompilerPolicy, QuestionCompiler};
use super::state::{
    CausalState, DerivedCandidate, DerivedState, DuplicateCandidate, ObservedArtifactKind, ObservedStepKind,
    ObservedStepStatus, SideEffectMode, TrustedParts,
};
use crate::capability::CapabilityAvailabilityLevel;
use crate::decision::DecisionError;
#[cfg(test)]
use crate::decision::factory::DecisionBackendFactory;
use crate::decision::factory::{FallbackReason, PredictionResult};
use crate::decision::telemetry::CausalLineage;
use crate::decision::{DecisionAnswer, DecisionKey, GlobalQuestion, RequestLimits, ValidatedDecision};
use crate::security::{AutonomyLevel, SecurityPolicy};
use crate::tools::capability::catalog::{CatalogView, Exposure};
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest, ChildManifestError};
use crate::tools::capability::search::CatalogSearch;
use crate::tools::execution::{ToolExecutionDecision, decide_tool_execution};

/// Borrowed prediction port owned by the engine domain. Its producer keeps
/// physical accounting and transport outside the engine; no boxed future is
/// introduced on builtin/cache or physical paths by this contract.
pub(crate) mod prediction_sealed {
    pub(crate) trait Producer {}
}

pub(crate) trait PredictionPort: prediction_sealed::Producer + Clone + Send + Sync {
    fn remaining_budget(&self) -> Option<(u64, u64)>;
    fn predict<'a>(
        &'a self,
        request: &'a crate::decision::DecisionRequest,
        lineage: &'a CausalLineage,
    ) -> impl std::future::Future<Output = Result<PredictionResult, DecisionError>> + Send + 'a;
}

#[derive(Clone)]
pub(crate) struct EngineInputs<'a, 'catalog, P: PredictionPort> {
    pub state: &'a CausalState,
    pub catalog: &'a CatalogView<'catalog>,
    pub config: &'a CausalTreeConfig,
    pub key: &'a DecisionKey,
    /// Runtime-issued decision id, stable for this turn and its retry hops.
    pub decision_id: DecisionId,
    pub cost_model: &'a dyn super::candidate_generator::CostEstimateSource,
    /// Exact references returned by this turn's selected local discovery hops.
    pub discovered_refs: &'a DiscoveredRefs,
    pub gates: &'a GateInputs<'a>,
    /// New child scope requested by this turn. `gates.child_scope` is the
    /// restriction inherited by the current worker, never the outgoing one.
    pub delegation: Option<(&'a CapabilityConstraints, &'a [CapabilityRef])>,
    /// Runtime's pinned delegation depth limit for this turn.
    pub delegation_max_depth: u8,
    pub prediction: P,
    pub rehearsal_ports: &'a RehearsalPorts<'a>,
    pub request_limits: RequestLimits,
    pub model: Option<&'a str>,
    pub can_deliver_text: bool,
    pub may_delegate: bool,
    pub delegation_target: Option<&'a str>,
    /// Force a fresh terminal-only gate pass after hop exhaustion or no progress.
    pub terminal_only: bool,
    /// A successful selected read with new fresh evidence, or a safe plain
    /// request whose bounded discovery completed, may authorize terminal
    /// Answer after a stopped hop. The service owns this fact.
    pub allow_terminal_answer: bool,
    /// Set once by the caller for the whole decision, including fallback.
    pub deadline: Instant,
    #[cfg(test)]
    pub inject_builtin_failure: bool,
    #[cfg(test)]
    pub after_prediction_pause: Option<Duration>,
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
    /// Code-only, numeric answers copied from the sealed validated response.
    /// Score legends and question bodies can contain user text and are omitted.
    pub audit_trace: Option<PredictionAuditTrace>,
    /// Why a conservative terminal fallback was selected. Retained for the decision record.
    pub fallback_cause: Option<FallbackCause>,
    pub scores: BTreeMap<CandidateId, CandidateScore>,
    /// Real scorer invocations, excluding zero placeholders after fallback.
    pub scored_count: usize,
    #[cfg(test)]
    pub rehearsal: BTreeMap<CandidateId, RehearsalEvidence>,
    pub post_rehearsal: GateReport,
    pub pre_commit: GateReport,
    /// Exact call inputs used by the final gate pass.
    pub call_fingerprints: BTreeMap<CandidateId, [u8; 32]>,
    /// A new sealed strict subset derived from the current turn's authority.
    pub outgoing_child: Option<ChildCapabilityManifest>,
    pub outgoing_parent_allowed: Option<BTreeSet<CapabilityRef>>,
}

/// The validated backend answer as bounded audit data. Every key is a sealed
/// question or option id, never model-authored text.
#[derive(Clone)]
pub(crate) struct PredictionAuditTrace {
    pub(crate) state_fingerprint: String,
    pub(crate) policy_version: String,
    pub(crate) catalog_generation: String,
    pub(crate) answers: BTreeMap<String, PredictionAuditAnswer>,
    pub(crate) omitted_questions: BTreeMap<String, &'static str>,
    pub(crate) forced_choices: BTreeMap<String, String>,
    /// The exact sealed, possibly shrunk request state; kept only by opt-in.
    pub(crate) redacted_projection: Option<String>,
}

impl std::fmt::Debug for PredictionAuditTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PredictionAuditTrace")
            .field("answers", &self.answers)
            .field("omitted_questions", &self.omitted_questions)
            .field("forced_choices", &self.forced_choices)
            .field("has_redacted_projection", &self.redacted_projection.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PredictionAuditAnswer {
    Choice {
        chosen: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        distribution: Vec<f64>,
        expected: f64,
        confidence: f64,
    },
    Noul {
        probability_true: f64,
    },
}

impl PredictionAuditTrace {
    fn capture(
        decision: &ValidatedDecision,
        compiled: &CompiledQuestionRequest,
        record_redacted_projection: bool,
    ) -> Result<Self, ExtractionError> {
        let request = compiled.request();
        if decision.request_id() != request.request_id() {
            return Err(ExtractionError::RequestMismatch);
        }
        let mut answers = BTreeMap::new();
        for id in request.questions().keys() {
            let answer = match decision.answer(id).ok_or(ExtractionError::MissingAnswer)? {
                DecisionAnswer::Choice(value) => PredictionAuditAnswer::Choice {
                    chosen: value.chosen.as_str().to_owned(),
                    probabilities: value
                        .probabilities
                        .iter()
                        .map(|(option, probability)| (option.as_str().to_owned(), probability.get()))
                        .collect(),
                    confidence: value.confidence.get(),
                },
                DecisionAnswer::Score(value) => PredictionAuditAnswer::Score {
                    distribution: value.distribution.iter().map(|probability| probability.get()).collect(),
                    expected: value.expected,
                    confidence: value.confidence.get(),
                },
                DecisionAnswer::Noul(value) => PredictionAuditAnswer::Noul {
                    probability_true: value.probability_true.get(),
                },
            };
            answers.insert(id.as_str().to_owned(), answer);
        }
        let omitted_questions = request
            .manifest()
            .omitted_questions()
            .iter()
            .map(|(id, reason)| (id.as_str().to_owned(), reason.as_str()))
            .collect();
        let forced_choices = [GlobalQuestion::NextAction, GlobalQuestion::CapabilityTarget]
            .into_iter()
            .filter_map(|question| {
                compiled
                    .forced_choice(question)
                    .map(|option| (question.as_str().to_owned(), option.as_str().to_owned()))
            })
            .collect();
        Ok(Self {
            state_fingerprint: request.manifest().state_fingerprint().to_owned(),
            policy_version: request.manifest().policy_version().to_string(),
            catalog_generation: request.manifest().catalog_generation().to_owned(),
            answers,
            omitted_questions,
            forced_choices,
            redacted_projection: record_redacted_projection.then(|| request.state().as_json().to_string()),
        })
    }
}

/// A candidate set and derived state bound to the exact inputs of one hop.
/// The runtime can compare its evidence before allowing another backend call,
/// then consume the same preparation without regenerating candidates.
pub(crate) struct PreparedHop<'a, 'catalog, P: PredictionPort> {
    started_at: Instant,
    inputs: EngineInputs<'a, 'catalog, P>,
    state: CausalState,
    candidates: CandidateSet,
    all: Vec<CausalCandidate>,
    outgoing_child: Option<ChildCapabilityManifest>,
    // Borrowed only from the authenticated resume gate-evidence path.
    resume_target: Option<&'a CapabilityRef>,
}

// The borrowed preparation frame returns before either owned branch proceeds.
// This record retains only the evidence needed for the immediate decision.
struct CollectionReadyRecord {
    proof: super::approval_collection::DeferredArgumentProof,
    post_rehearsal: GateReport,
    pre_commit: GateReport,
}

impl<P: PredictionPort> PreparedHop<'_, '_, P> {
    pub(crate) const fn candidate_count(&self) -> usize {
        self.all.len()
    }

    pub(crate) fn evidence_fingerprint(&self) -> String {
        self.state.evidence_fingerprint(self.inputs.key)
    }

    /// Candidate IDs do not encode the exact bound capability ref. A
    /// same-family discovery may change a legal candidate body without
    /// changing its ID, so the hop loop checks both forms of progress.
    pub(crate) fn candidate_bodies_changed_from(&self, prior: &CausalState) -> bool {
        self.state.derived.candidates() != prior.derived.candidates()
    }

    pub(crate) fn candidate_bodies_changed_from_prepared(&self, prior: &PreparedHop<'_, '_, P>) -> bool {
        self.candidate_bodies_changed_from(&prior.state)
    }

    /// Selected discovery can authenticate an unchanged exact candidate.
    /// Compare borrowed readiness proofs, without creating new candidate sets.
    pub(crate) fn gained_direct_candidate_from_prepared(&self, prior: &PreparedHop<'_, '_, P>) -> bool {
        gained_direct_candidate(
            &self.candidates,
            &prior.candidates,
            |candidate| {
                candidate_is_direct(
                    candidate,
                    self.inputs.catalog,
                    self.resume_target,
                    self.inputs.discovered_refs,
                )
            },
            |candidate| {
                candidate_is_direct(
                    candidate,
                    prior.inputs.catalog,
                    prior.resume_target,
                    prior.inputs.discovered_refs,
                )
            },
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("selected discovery is unavailable for this pinned turn")]
pub(crate) struct DiscoveryIssueError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("selected retrieval is unavailable for this pinned turn")]
pub(crate) struct RetrievalIssueError;

/// Private fields prevent storage authority from being assembled by callers.
/// The runtime phase, not this type alone, enforces exactly one issuance.
#[cfg(feature = "llm-router")]
pub(crate) struct SelectedRouterReadGrant {
    decision_id: DecisionId,
    reference: CapabilityRef,
    purpose: super::state::RouterHistoryReadBinding,
    workspace: Arc<str>,
    owner: Arc<str>,
    session: Arc<str>,
    run: Arc<str>,
    source: Arc<str>,
    deadline: Instant,
}

#[cfg(feature = "llm-router")]
impl SelectedRouterReadGrant {
    pub(crate) fn matches_source(
        &self,
        workspace: &str,
        owner: &str,
        session: &str,
        run: &str,
        source: Option<&str>,
    ) -> bool {
        self.workspace.as_ref() == workspace
            && self.owner.as_ref() == owner
            && self.session.as_ref() == session
            && self.run.as_ref() == run
            && Some(self.source.as_ref()) == source
    }
    pub(crate) fn into_parts(
        self,
    ) -> (
        DecisionId,
        CapabilityRef,
        super::state::RouterHistoryReadBinding,
        Instant,
    ) {
        (self.decision_id, self.reference, self.purpose, self.deadline)
    }
}

impl EngineDecision {
    /// Admit one exact read-only retrieval source after final-gated selection.
    /// The runtime still checks the source proof and deadline at the read.
    pub(crate) fn validate_selected_retrieval(
        &self,
        catalog: &CatalogView<'_>,
        context: &DirectiveContext,
        source_query: &str,
    ) -> Result<super::branch::CapabilityTarget, RetrievalIssueError> {
        if self.state.trusted.router_history().is_some() {
            return Err(RetrievalIssueError);
        }
        self.selected_retrieval(catalog, context, source_query)
            .map(|(family, _)| family)
    }

    fn selected_retrieval(
        &self,
        catalog: &CatalogView<'_>,
        context: &DirectiveContext,
        source_query: &str,
    ) -> Result<(super::branch::CapabilityTarget, CapabilityRef), RetrievalIssueError> {
        let TurnExecutionDirective::Retrieve { sources, purpose } =
            directive_from_selection(self, context).map_err(|_| RetrievalIssueError)?
        else {
            return Err(RetrievalIssueError);
        };
        let expected_purpose = if self.state.trusted.router_history().is_some() {
            super::directive::SelectedReadPurpose::RouterHistory
        } else {
            super::directive::SelectedReadPurpose::QueryEvidence
        };
        if purpose != expected_purpose
            || self.state.trusted.parts().catalog_generation != catalog.generation()
            || self.state.routing_input.as_str() != source_query
            || !self.state.routing_input.supports_selected_read_query()
            || sources.len() != 1
            || Instant::now() >= self.deadline
        {
            return Err(RetrievalIssueError);
        }
        let reference = sources.first().ok_or(RetrievalIssueError)?;
        let entry = catalog.entry_any(&reference.id).ok_or(RetrievalIssueError)?;
        if !catalog.contains(&reference.id)
            || reference.generation != catalog.generation()
            || reference.fingerprint != entry.fingerprint
            || !is_retrieval_source(entry)
            || (entry.family == super::branch::CapabilityTarget::Session
                && self.state.trusted.parts().source_event_id.is_none())
            || self
                .state
                .trusted
                .parts()
                .excludes_entry(catalog.model_admission_entry(entry))
        {
            return Err(RetrievalIssueError);
        }
        Ok((entry.family, reference.clone()))
    }

    /// Runtime calls this only while consuming its unique Pending routing
    /// phase. The opaque grant cannot be constructed from a caller enum.
    #[cfg(feature = "llm-router")]
    pub(crate) fn issue_selected_router_read(
        &self,
        catalog: &CatalogView<'_>,
        context: &DirectiveContext,
        source_query: &str,
    ) -> Result<SelectedRouterReadGrant, RetrievalIssueError> {
        let purpose = self.state.trusted.router_history().ok_or(RetrievalIssueError)?;
        if purpose.version != 1 {
            return Err(RetrievalIssueError);
        }
        let (family, reference) = self.selected_retrieval(catalog, context, source_query)?;
        if family != CapabilityTarget::Memory {
            return Err(RetrievalIssueError);
        }
        let parts = self.state.trusted.parts();
        Ok(SelectedRouterReadGrant {
            decision_id: self.decision_id,
            reference,
            purpose: purpose.clone(),
            workspace: Arc::clone(&parts.workspace_id),
            owner: Arc::clone(parts.owner_id.as_ref().ok_or(RetrievalIssueError)?),
            session: Arc::clone(parts.session_key.as_ref().ok_or(RetrievalIssueError)?),
            run: Arc::clone(parts.run_id.as_ref().ok_or(RetrievalIssueError)?),
            source: Arc::clone(parts.source_event_id.as_ref().ok_or(RetrievalIssueError)?),
            deadline: self.deadline,
        })
    }

    /// Issue exact refs only after final-gated conversion selected Discover.
    /// The search is local metadata work over the same pinned filtered view.
    pub(crate) fn issue_selected_discovery(
        &self,
        catalog: &CatalogView<'_>,
        context: &DirectiveContext,
        source_query: &str,
    ) -> Result<DiscoveredRefs, DiscoveryIssueError> {
        let TurnExecutionDirective::Discover { families, query, limit } =
            directive_from_selection(self, context).map_err(|_| DiscoveryIssueError)?
        else {
            return Err(DiscoveryIssueError);
        };
        let trusted = self.state.trusted.parts();
        if trusted.catalog_generation != catalog.generation()
            || self.state.routing_input.as_str() != source_query
            || query != source_query
            || !self.state.routing_input.supports_selected_read_query()
            || !(1..=64).contains(&limit)
            || families.is_empty()
            || families
                .iter()
                .any(|family| !super::candidate_generator::DiscoveryFamilies::supports(*family))
            || Instant::now() >= self.deadline
        {
            return Err(DiscoveryIssueError);
        }
        let exact_refs = match &self.candidates {
            CandidateSet::Legal {
                candidates,
                exact_mentions,
                ..
            } => candidates
                .iter()
                .filter(|candidate| {
                    exact_mentions.contains(candidate.candidate_id())
                        && candidate.action() == CausalActionKind::UseTool
                        && families.contains(&candidate.target())
                })
                .flat_map(CausalCandidate::required_capabilities)
                .filter(|reference| {
                    catalog.entry_any(&reference.id).is_some_and(|entry| {
                        matches!(entry.exposure, Exposure::Searchable)
                            && !trusted.excludes_entry(catalog.model_admission_entry(entry))
                    })
                })
                .take(limit)
                .cloned()
                .collect::<BTreeSet<_>>(),
            CandidateSet::NoLegalPath { .. } => BTreeSet::new(),
        };
        let refs = if exact_refs.is_empty() {
            CatalogSearch::find_filtered(catalog, &query, &families, limit, |entry| {
                !matches!(
                    entry.public_name.as_str(),
                    "capability_search" | "stay_silent" | "transcript_history_lookup" | "delegate" | "sessions_spawn"
                ) && matches!(entry.exposure, Exposure::Searchable)
                    && !trusted.excludes_entry(catalog.model_admission_entry(entry))
            })
            .into_iter()
            .map(|hit| CapabilityRef {
                id: hit.entry.id.clone(),
                fingerprint: hit.entry.fingerprint,
                generation: hit.entry.generation,
            })
            .collect()
        } else {
            exact_refs
        };
        if Instant::now() >= self.deadline {
            return Err(DiscoveryIssueError);
        }
        Ok(DiscoveredRefs::from_selected_search(refs))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackCause {
    NoLegalPath,
    ExpiredBeforePrediction,
    DecisionFailed,
    DeadlineExceeded,
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
    Extraction(#[from] ExtractionError),
    #[error(transparent)]
    Rehearsal(#[from] RehearsalError),
    #[error(transparent)]
    Score(#[from] ScoreError),
    #[error(transparent)]
    Selection(#[from] SelectError),
    #[error("candidate body or approval scope changed within the pinned turn")]
    CandidateDrift,
    #[error("candidate effect understates a pinned capability effect")]
    EffectUnderstated,
    #[error("an admitted candidate has no cost or latency estimate")]
    MissingEstimate,
    #[error("outgoing child authority does not match this turn")]
    InvalidDelegation,
    #[error(transparent)]
    ChildManifest(#[from] ChildManifestError),
}

pub(crate) struct CausalDecisionEngine;

impl CausalDecisionEngine {
    #[cfg(test)]
    pub(crate) async fn decide_once<P: PredictionPort>(
        inputs: &EngineInputs<'_, '_, P>,
    ) -> Result<EngineDecision, EngineError> {
        Self::decide_prepared(Self::prepare_hop(inputs)?).await
    }

    pub(crate) fn prepare_hop<'a, 'catalog, P: PredictionPort>(
        inputs: &EngineInputs<'a, 'catalog, P>,
    ) -> Result<PreparedHop<'a, 'catalog, P>, EngineError> {
        Self::prepare_hop_with_resume_target(inputs, None)
    }

    /// Only a caller with authenticated, current-snapshot resume evidence may
    /// restore this exact candidate. All normal preparation and gates remain.
    pub(crate) fn prepare_hop_with_resume_target<'a, 'catalog, P: PredictionPort>(
        inputs: &EngineInputs<'a, 'catalog, P>,
        resume_target: Option<&'a crate::causal_tree::branch::CapabilityRef>,
    ) -> Result<PreparedHop<'a, 'catalog, P>, EngineError> {
        let started_at = Instant::now();
        let trusted = inputs.state.trusted.parts();
        if trusted.catalog_generation != inputs.catalog.generation() {
            return Err(EngineError::CandidateDrift);
        }
        if let Some((parent, _)) = inputs.delegation {
            validate_delegation_refs(
                parent,
                inputs.catalog,
                trusted,
                inputs.gates.policy,
                inputs.gates.sender,
                inputs.gates.policy_channel,
                inputs.gates.chat_type,
            )?;
        }
        let outgoing_child = derive_outgoing_child(
            inputs.decision_id,
            inputs.key,
            trusted,
            inputs.gates.policy,
            inputs.delegation_max_depth,
            inputs.gates.child_scope.map(|(current, _)| current),
            inputs.delegation,
        )?;
        let candidate_policy = CandidatePolicy {
            key: inputs.key,
            max_candidates: usize::from(inputs.config.policy.max_candidates),
            can_deliver_text: inputs.can_deliver_text,
            may_delegate: inputs.may_delegate && outgoing_child.is_some(),
            delegation_target: inputs.delegation_target,
            cost_model: inputs.cost_model,
            discovered_refs: inputs.discovered_refs,
        };
        let candidates = if inputs.terminal_only {
            CausalCandidateGenerator::generate_terminal_only(
                inputs.state,
                inputs.catalog,
                &candidate_policy,
                inputs.gates,
            )?
        } else {
            match resume_target {
                Some(target) => CausalCandidateGenerator::generate_with_resume_target(
                    inputs.state,
                    inputs.catalog,
                    &candidate_policy,
                    inputs.gates,
                    Some(target),
                )?,
                None => {
                    CausalCandidateGenerator::generate(inputs.state, inputs.catalog, &candidate_policy, inputs.gates)?
                }
            }
        };
        let mut state = inputs.state.clone();
        state.derived = derive_state(&candidates, inputs.catalog)?;
        let all = gate_candidates(&candidates)?;
        for candidate in &all {
            validate_effect(candidate, inputs.catalog)?;
        }

        Ok(PreparedHop {
            started_at,
            inputs: inputs.clone(),
            state,
            candidates,
            all,
            outgoing_child,
            resume_target,
        })
    }

    /// Select a freshly gated terminal after a read hop stops. This path
    /// consumes a terminal-only preparation and never calls a backend.
    pub(crate) fn decide_terminal_prepared<P: PredictionPort>(
        prepared: PreparedHop<'_, '_, P>,
    ) -> Result<EngineDecision, EngineError> {
        let PreparedHop {
            started_at,
            inputs,
            state,
            candidates,
            all,
            outgoing_child,
            resume_target: _,
        } = prepared;
        if !inputs.terminal_only || inputs.delegation.is_some() || outgoing_child.is_some() {
            return Err(EngineError::CandidateDrift);
        }
        let remaining_budget = remaining_budget(&inputs, &state);
        let post_rehearsal = HardGates::apply_with_remaining_budget(
            GatePass::PostRehearsal,
            &state,
            inputs.catalog,
            &all,
            inputs.gates,
            remaining_budget,
        )?;
        let pre_commit = HardGates::apply_with_remaining_budget(
            GatePass::PreCommit,
            &state,
            inputs.catalog,
            &all,
            inputs.gates,
            remaining_budget,
        )?;
        let pre_commit = meet_gate_reports(&post_rehearsal, pre_commit)?;
        let admitted = match &candidates {
            CandidateSet::Legal { candidates, .. } => candidates,
            CandidateSet::NoLegalPath { terminals, .. } => terminals,
        };
        let passed = |action| {
            admitted.iter().find(|candidate| {
                candidate.action() == action
                    && matches!(
                        pre_commit.outcomes.get(candidate.candidate_id()),
                        Some(GateOutcome::Passed)
                    )
            })
        };
        let has_retrieved_evidence = state.observed.artifacts().iter().any(|artifact| {
            matches!(
                artifact.kind,
                ObservedArtifactKind::Memory | ObservedArtifactKind::Document | ObservedArtifactKind::Session
            ) && artifact.age_ms <= crate::decision::builtin::weights::STALE_ARTIFACT_MS
        });
        let exhausted_discovery = state
            .observed
            .completed_steps()
            .iter()
            .any(|step| step.kind() == ObservedStepKind::Discover && step.status() == ObservedStepStatus::Ok);
        let selection = if inputs.allow_terminal_answer
            && (has_retrieved_evidence || exhausted_discovery)
            && Instant::now() < inputs.deadline
        {
            passed(CausalActionKind::Answer).map(|answer| Selection::Commit {
                candidate_id: answer.candidate_id().clone(),
                propensity: 1.0,
            })
        } else {
            None
        }
        .or_else(|| {
            if state.trusted.parts().delivery.interactive {
                passed(CausalActionKind::Clarify).map(|_| Selection::Abstain {
                    action: CausalActionKind::Clarify,
                    reason_code: "hop_stopped",
                })
            } else {
                None
            }
        })
        .or_else(|| {
            passed(CausalActionKind::Refuse).map(|_| Selection::Abstain {
                action: CausalActionKind::Refuse,
                reason_code: "hop_stopped",
            })
        })
        .ok_or(EngineError::Selection(SelectError::NoTerminalPath))?;
        Ok(EngineDecision {
            decision_id: inputs.decision_id,
            deadline: inputs.deadline,
            elapsed_ms: u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            state,
            candidates,
            selection,
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: Some(FallbackCause::HopStopped),
            scores: BTreeMap::new(),
            scored_count: 0,
            #[cfg(test)]
            rehearsal: BTreeMap::new(),
            post_rehearsal,
            pre_commit,
            call_fingerprints: inputs.gates.call_fingerprints.clone(),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        })
    }

    pub(crate) async fn decide_prepared_with_collection<P: PredictionPort>(
        mut prepared: PreparedHop<'_, '_, P>,
        adapter: Option<super::approval_collection::ApprovalReviewAdapter>,
    ) -> Result<EngineDecision, EngineError> {
        match Self::prepare_collection_routing(&mut prepared, adapter)? {
            Some(ready) => Ok(Self::finish_collection_decision(prepared, ready)),
            None => Self::decide_prepared(prepared).await,
        }
    }

    /// Source eligibility is supplied by the verified service's sealed ingress proof.
    /// No backend sees this result and no executable capability is selected.
    #[inline(never)]
    fn prepare_collection_routing<'a, 'catalog, P: PredictionPort>(
        prepared: &mut PreparedHop<'a, 'catalog, P>,
        adapter: Option<super::approval_collection::ApprovalReviewAdapter>,
    ) -> Result<Option<CollectionReadyRecord>, EngineError> {
        let inputs = &prepared.inputs;
        let trusted = prepared.state.trusted.parts();
        let candidate = adapter
            .filter(|_| {
                !inputs.terminal_only
                    && inputs.delegation.is_none()
                    && prepared.outgoing_child.is_none()
                    && Instant::now() < inputs.deadline
                    && trusted.delivery.interactive
                    && trusted.delivery.approval_channel
            })
            .and_then(|adapter| {
                let policy = CandidatePolicy {
                    key: inputs.key,
                    max_candidates: usize::from(inputs.config.policy.max_candidates),
                    can_deliver_text: inputs.can_deliver_text,
                    may_delegate: false,
                    delegation_target: None,
                    cost_model: inputs.cost_model,
                    discovered_refs: inputs.discovered_refs,
                };
                CausalCandidateGenerator::exact_deferred_mutation(
                    &prepared.state,
                    inputs.catalog,
                    &policy,
                    inputs.gates,
                )
                .map(|candidate| (adapter, candidate))
            });
        let Some((adapter, candidate)) = candidate else {
            return Ok(None);
        };
        let remaining = remaining_budget(inputs, &prepared.state);
        let post_rehearsal = HardGates::apply_with_remaining_budget(
            GatePass::PostRehearsal,
            &prepared.state,
            inputs.catalog,
            std::slice::from_ref(&candidate),
            inputs.gates,
            remaining,
        )?;
        let pre_commit = HardGates::apply_with_remaining_budget(
            GatePass::PreCommit,
            &prepared.state,
            inputs.catalog,
            std::slice::from_ref(&candidate),
            inputs.gates,
            remaining,
        )?;
        if ![&post_rehearsal, &pre_commit].iter().all(|report| {
            matches!(
                report.outcomes.get(candidate.candidate_id()),
                Some(GateOutcome::ApprovalDeferred)
            )
        }) {
            return Ok(None);
        }
        match &mut prepared.candidates {
            CandidateSet::Legal { report, .. } | CandidateSet::NoLegalPath { report, .. } => {
                report
                    .outcomes
                    .insert(candidate.candidate_id().clone(), GateOutcome::ApprovalDeferred);
            }
        }
        let [reference] = candidate.required_capabilities() else {
            return Err(EngineError::CandidateDrift);
        };
        let proof = super::approval_collection::DeferredArgumentProof::issue(
            reference.clone(),
            adapter,
            prepared.state.material_fingerprint(inputs.key),
        );
        Ok(Some(CollectionReadyRecord {
            proof,
            post_rehearsal,
            pre_commit,
        }))
    }

    // Consume the owned hop only after borrowed preparation has returned.
    #[inline(never)]
    fn finish_collection_decision<P: PredictionPort>(
        prepared: PreparedHop<'_, '_, P>,
        ready: CollectionReadyRecord,
    ) -> EngineDecision {
        let inputs = &prepared.inputs;
        let CollectionReadyRecord {
            proof,
            post_rehearsal,
            pre_commit,
        } = ready;
        EngineDecision {
            decision_id: inputs.decision_id,
            deadline: inputs.deadline,
            elapsed_ms: u64::try_from(prepared.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            state: prepared.state,
            candidates: prepared.candidates,
            selection: Selection::CollectApprovalArguments(proof),
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: None,
            scores: BTreeMap::new(),
            scored_count: 0,
            #[cfg(test)]
            rehearsal: BTreeMap::new(),
            post_rehearsal,
            pre_commit,
            call_fingerprints: BTreeMap::new(),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        }
    }

    pub(crate) async fn decide_prepared<P: PredictionPort>(
        prepared: PreparedHop<'_, '_, P>,
    ) -> Result<EngineDecision, EngineError> {
        let PreparedHop {
            started_at,
            inputs,
            state,
            candidates,
            all,
            outgoing_child,
            resume_target,
        } = prepared;
        let inputs = &inputs;
        let trusted = state.trusted.parts();

        let mut prediction = None;
        let mut request_compiled = false;
        let mut audit_trace = None;
        let mut evidence = None;
        let mut fallback_cause = None;
        if matches!(candidates, CandidateSet::Legal { .. }) && Instant::now() < inputs.deadline {
            let compiled = QuestionCompiler::compile(
                &state,
                &candidates,
                &CompilerPolicy {
                    key: inputs.key,
                    model: inputs.model,
                    expires_at: inputs.deadline,
                    limits: inputs.request_limits,
                },
            )?;
            #[cfg(test)]
            original_observation::compiled(inputs.decision_id, &state, &compiled, inputs.request_limits);
            request_compiled = true;
            #[cfg(test)]
            let outcome = if inputs.inject_builtin_failure {
                Ok(Err(DecisionError::Unavailable))
            } else {
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(inputs.deadline),
                    inputs.prediction.predict(compiled.request(), &lineage(trusted)),
                )
                .await
            };
            #[cfg(not(test))]
            let outcome = tokio::time::timeout_at(
                tokio::time::Instant::from_std(inputs.deadline),
                inputs.prediction.predict(compiled.request(), &lineage(trusted)),
            )
            .await;
            match outcome {
                Ok(Ok(result)) if Instant::now() < inputs.deadline => {
                    let terminal_cause = match result.reason {
                        Some(FallbackReason::AllFailed) => Some(FallbackCause::DecisionFailed),
                        Some(FallbackReason::DeadlineExhausted) => Some(FallbackCause::DeadlineExceeded),
                        Some(FallbackReason::Disabled | FallbackReason::BudgetUnproven) | None => None,
                    };
                    if let Some(cause) = terminal_cause {
                        // Keep the factory result for truthful attempt and
                        // accounting records, but never score or execute the
                        // builtin shadow after a configured decision attempt
                        // failed or exhausted its deadline.
                        audit_trace = Some(PredictionAuditTrace::capture(
                            &result.served,
                            &compiled,
                            inputs.config.store.record_redacted_projection,
                        )?);
                        prediction = Some(result);
                        fallback_cause = Some(cause);
                        evidence = Some(terminal_evidence(&candidates));
                    } else {
                        let extracted = PredictionEvidence::extract(&result.served, &compiled, &candidates)?;
                        audit_trace = Some(PredictionAuditTrace::capture(
                            &result.served,
                            &compiled,
                            inputs.config.store.record_redacted_projection,
                        )?);
                        prediction = Some(result);
                        evidence = Some(extracted);
                    }
                }
                Ok(Ok(_)) | Err(_) => {
                    // A failed builtin or exhausted deadline has no trusted
                    // prediction. Hard policy still chooses a typed terminal.
                    fallback_cause = Some(FallbackCause::DeadlineExceeded);
                    evidence = Some(terminal_evidence(&candidates));
                }
                Ok(Err(_)) => {
                    fallback_cause = Some(FallbackCause::DecisionFailed);
                    evidence = Some(terminal_evidence(&candidates));
                }
            }
            #[cfg(test)]
            if prediction.is_some() {
                if let Some(pause) = inputs.after_prediction_pause {
                    let _ = tokio::time::timeout_at(
                        tokio::time::Instant::from_std(inputs.deadline),
                        tokio::time::sleep(pause),
                    )
                    .await;
                }
            }
        } else if matches!(candidates, CandidateSet::Legal { .. }) {
            fallback_cause = Some(FallbackCause::ExpiredBeforePrediction);
            evidence = Some(terminal_evidence(&candidates));
        } else {
            fallback_cause = Some(FallbackCause::NoLegalPath);
        }

        let query_ref = inputs.key.hmac(
            crate::decision::KeyDomain::StateFingerprint,
            &[b"rehearsal-query", state.routing_input.as_str().as_bytes()],
        );
        let rehearsal_context = RehearsalContext {
            principal: trusted.owner_id.as_deref().unwrap_or(inputs.gates.sender),
            query_ref,
            memory_k: 5,
            stage_cap: Duration::from_millis(u64::from(inputs.config.rehearsal.timeout_ms)),
            deadline: tokio::time::Instant::from_std(inputs.deadline),
        };
        let rehearsal = if prediction.is_some() && fallback_cause.is_none() && Instant::now() < inputs.deadline {
            BoundedRehearsal::run(&all, inputs.catalog, inputs.rehearsal_ports, &rehearsal_context).await?
        } else {
            BTreeMap::new()
        };
        let remaining_budget = remaining_budget(inputs, &state);
        let post_rehearsal = HardGates::apply_with_remaining_budget(
            GatePass::PostRehearsal,
            &state,
            inputs.catalog,
            &all,
            inputs.gates,
            remaining_budget,
        )?;
        let pre_commit = HardGates::apply_with_remaining_budget(
            GatePass::PreCommit,
            &state,
            inputs.catalog,
            &all,
            inputs.gates,
            remaining_budget,
        )?;
        let pre_commit = meet_gate_reports(&post_rehearsal, pre_commit)?;

        let mut scores = BTreeMap::new();
        let mut scored_count = 0usize;
        let (mut selection, mut exploration_propensities) = if let Some(evidence) = evidence.as_ref() {
            let weights = if prediction.is_some() && fallback_cause.is_none() && Instant::now() < inputs.deadline {
                Some(ScorerWeights::effective(&inputs.config.scoring)?)
            } else {
                None
            };
            let CandidateSet::Legal {
                candidates: admitted, ..
            } = &candidates
            else {
                return Err(EngineError::CandidateDrift);
            };
            for candidate in admitted.iter().filter(|item| !is_terminal(item.action())) {
                let Some(features) = evidence.candidates.get(candidate.candidate_id()) else {
                    return Err(EngineError::CandidateDrift);
                };
                let score = if let Some(weights) = weights.as_ref().filter(|_| Instant::now() < inputs.deadline) {
                    scored_count = scored_count.saturating_add(1);
                    let cost = candidate.estimated_cost().ok_or(EngineError::MissingEstimate)?;
                    let latency = candidate.estimated_latency_ms().ok_or(EngineError::MissingEstimate)?;
                    let delta = rehearsal
                        .get(candidate.candidate_id())
                        .map_or(0.0, |item| item.bounded_delta);
                    let score_inputs = ScoreInputs {
                        estimated_micro_usd: cost.micro_usd,
                        remaining_micro_usd: remaining_budget.micro_usd,
                        estimated_latency_ms: u64::from(latency),
                        remaining_latency_ms: remaining_budget.latency_ms,
                        rehearsal_delta: delta,
                    };
                    let score = CausalBranchScorer::score(*features, score_inputs, weights)?;
                    #[cfg(test)]
                    original_observation::scored(
                        inputs.decision_id,
                        &state,
                        candidate.candidate_id(),
                        (*features, score_inputs, *weights, score),
                        remaining_budget,
                    );
                    score
                } else {
                    CandidateScore {
                        semantic_utility: 0.0,
                        execution_utility: 0.0,
                    }
                };
                scores.insert(candidate.candidate_id().clone(), score);
            }
            let selection_evidence = if Instant::now() >= inputs.deadline {
                fallback_cause.get_or_insert(FallbackCause::DeadlineExceeded);
                terminal_evidence(&candidates)
            } else {
                evidence.clone()
            };
            if prediction.is_some() && fallback_cause.is_none() && Instant::now() < inputs.deadline {
                CausalSelector::select_with_exploration(
                    &candidates,
                    &scores,
                    &selection_evidence,
                    &pre_commit,
                    &inputs.config.policy,
                    &selector_context(
                        &candidates,
                        inputs.catalog,
                        trusted.delivery.interactive,
                        resume_target,
                        inputs.discovered_refs,
                    ),
                    &inputs.config.evaluation,
                    inputs.key,
                    &inputs.decision_id.to_string(),
                )?
            } else {
                (
                    CausalSelector::select(
                        &candidates,
                        &scores,
                        &selection_evidence,
                        &pre_commit,
                        &inputs.config.policy,
                        &selector_context(
                            &candidates,
                            inputs.catalog,
                            trusted.delivery.interactive,
                            resume_target,
                            inputs.discovered_refs,
                        ),
                    )?,
                    BTreeMap::new(),
                )
            }
        } else {
            (
                CausalSelector::select(
                    &candidates,
                    &scores,
                    &empty_evidence(),
                    &pre_commit,
                    &inputs.config.policy,
                    &selector_context(
                        &candidates,
                        inputs.catalog,
                        trusted.delivery.interactive,
                        resume_target,
                        inputs.discovered_refs,
                    ),
                )?,
                BTreeMap::new(),
            )
        };
        if Instant::now() >= inputs.deadline
            && matches!(selection, Selection::Commit { .. } | Selection::Recover { .. })
        {
            fallback_cause.get_or_insert(FallbackCause::DeadlineExceeded);
            selection = CausalSelector::select(
                &candidates,
                &scores,
                &terminal_evidence(&candidates),
                &pre_commit,
                &inputs.config.policy,
                &selector_context(
                    &candidates,
                    inputs.catalog,
                    trusted.delivery.interactive,
                    resume_target,
                    inputs.discovered_refs,
                ),
            )?;
            exploration_propensities.clear();
        }
        Ok(EngineDecision {
            decision_id: inputs.decision_id,
            deadline: inputs.deadline,
            elapsed_ms: u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            state,
            candidates,
            selection,
            exploration_propensities,
            prediction,
            request_compiled,
            audit_trace,
            fallback_cause,
            scores,
            scored_count,
            #[cfg(test)]
            rehearsal,
            post_rehearsal,
            pre_commit,
            call_fingerprints: inputs.gates.call_fingerprints.clone(),
            outgoing_child,
            outgoing_parent_allowed: inputs.delegation.map(|(parent, _)| parent.allowed.clone()),
        })
    }
}

fn remaining_budget<P: PredictionPort>(
    inputs: &EngineInputs<'_, '_, P>,
    state: &CausalState,
) -> super::state::TurnBudgets {
    let mut remaining = state.trusted.parts().budgets;
    remaining.latency_ms = remaining
        .latency_ms
        .min(u64::try_from(inputs.deadline.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u64::MAX));
    if let Some((tokens, micro_usd)) = inputs.prediction.remaining_budget() {
        remaining.tokens = remaining.tokens.min(tokens);
        remaining.micro_usd = remaining.micro_usd.min(micro_usd);
    }
    remaining
}

fn derive_outgoing_child(
    decision_id: DecisionId,
    decision_key: &DecisionKey,
    trusted: &TrustedParts,
    policy: &SecurityPolicy,
    max_depth: u8,
    current_scope: Option<&ChildCapabilityManifest>,
    delegation: Option<(&CapabilityConstraints, &[CapabilityRef])>,
) -> Result<Option<ChildCapabilityManifest>, EngineError> {
    let Some((parent, requested)) = delegation else {
        return Ok(None);
    };
    if parent.decision_id != decision_id
        || parent.key.hmac(
            crate::decision::KeyDomain::StateFingerprint,
            &[b"outgoing-child-key-v1"],
        ) != decision_key.hmac(
            crate::decision::KeyDomain::StateFingerprint,
            &[b"outgoing-child-key-v1"],
        )
        || parent.catalog_generation != trusted.catalog_generation
        || parent.authority.workspace != trusted.workspace_id.as_ref()
        || trusted.owner_id.as_deref() != Some(parent.authority.owner.as_str())
        || !authority_within_policy(parent, trusted, policy, max_depth)
        || current_scope
            .is_some_and(|current| parent.allowed != current.allowed || parent.authority != current.authority)
    {
        return Err(EngineError::InvalidDelegation);
    }
    ChildCapabilityManifest::derive_strict_subset(parent, requested)
        .map(Some)
        .map_err(EngineError::from)
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

fn validate_delegation_refs(
    parent: &CapabilityConstraints,
    catalog: &CatalogView<'_>,
    trusted: &TrustedParts,
    policy: &SecurityPolicy,
    sender: &str,
    policy_channel: &str,
    chat_type: &str,
) -> Result<(), EngineError> {
    for reference in &parent.allowed {
        let Some(entry) = catalog.entry_any(&reference.id) else {
            return Err(EngineError::InvalidDelegation);
        };
        if !catalog.contains(&reference.id)
            || reference.generation != catalog.generation()
            || reference.fingerprint != entry.fingerprint
            || matches!(entry.exposure, Exposure::Unavailable { .. })
            || entry.availability.level < CapabilityAvailabilityLevel::Configured
            || trusted.excludes_entry(catalog.model_admission_entry(entry))
            || (trusted.channel.is_some() && !entry.channel_eligible)
            || decide_tool_execution(policy, entry.policy_tool_name(), sender, policy_channel, chat_type)
                == ToolExecutionDecision::Deny
        {
            return Err(EngineError::InvalidDelegation);
        }
    }
    Ok(())
}

const fn is_terminal(action: CausalActionKind) -> bool {
    matches!(
        action,
        CausalActionKind::Clarify | CausalActionKind::RequestApproval | CausalActionKind::Refuse
    )
}

fn derive_state(set: &CandidateSet, catalog: &CatalogView<'_>) -> Result<DerivedState, DuplicateCandidate> {
    let CandidateSet::Legal {
        candidates,
        exact_mentions,
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
                summary: Arc::from(
                    candidate
                        .required_capabilities()
                        .first()
                        .and_then(|reference| catalog.entry_any(&reference.id))
                        .map_or("Fixed terminal", |entry| entry.summary.as_str()),
                ),
                exact_mention: exact_mentions.contains(candidate.candidate_id()),
                available: true,
            })
            .collect(),
    )
}

fn gate_candidates(set: &CandidateSet) -> Result<Vec<CausalCandidate>, EngineError> {
    let mut all = Vec::new();
    let mut seen = BTreeMap::<CandidateId, CausalCandidate>::new();
    let mut append = |candidate: &CausalCandidate| -> Result<(), EngineError> {
        if let Some(prior) = seen.get(candidate.candidate_id()) {
            if prior != candidate {
                return Err(EngineError::CandidateDrift);
            }
            return Ok(());
        }
        seen.insert(candidate.candidate_id().clone(), candidate.clone());
        all.push(candidate.clone());
        Ok(())
    };
    match set {
        CandidateSet::Legal {
            candidates,
            approval_scopes,
            ..
        } => {
            for candidate in candidates {
                append(candidate)?;
            }
            for pending in approval_scopes.values().flatten() {
                append(&pending.candidate)?;
            }
        }
        CandidateSet::NoLegalPath {
            terminals,
            approval_only,
            ..
        } => {
            for candidate in terminals {
                append(candidate)?;
            }
            for pending in approval_only {
                append(&pending.candidate)?;
            }
        }
    }
    Ok(all)
}

fn validate_effect(candidate: &CausalCandidate, catalog: &CatalogView<'_>) -> Result<(), EngineError> {
    for reference in candidate.required_capabilities() {
        if reference.generation != catalog.generation() {
            return Err(EngineError::CandidateDrift);
        }
        let Some(entry) = catalog.entry_any(&reference.id) else {
            return Err(EngineError::CandidateDrift);
        };
        if entry.generation != catalog.generation()
            || entry.fingerprint != reference.fingerprint
            || !catalog.contains(&entry.id)
        {
            return Err(EngineError::CandidateDrift);
        }
        if candidate.side_effect() < entry.effect {
            return Err(EngineError::EffectUnderstated);
        }
    }
    Ok(())
}

/// A later pass may narrow a candidate, never restore one that failed after
/// rehearsal. Approval OpIds must match in both passes.
fn meet_gate_reports(post: &GateReport, mut current: GateReport) -> Result<GateReport, EngineError> {
    if post.gate_set_hash != current.gate_set_hash || post.outcomes.len() != current.outcomes.len() {
        return Err(EngineError::CandidateDrift);
    }
    for (id, outcome) in &post.outcomes {
        let Some(now) = current.outcomes.get_mut(id) else {
            return Err(EngineError::CandidateDrift);
        };
        match (outcome, &*now) {
            (GateOutcome::Passed, _) => {}
            (GateOutcome::ApprovalOnly { op_ids: earlier }, GateOutcome::ApprovalOnly { op_ids: later })
                if earlier == later => {}
            (GateOutcome::ApprovalOnly { .. }, GateOutcome::Passed) => *now = outcome.clone(),
            (
                GateOutcome::Blocked { .. } | GateOutcome::ApprovalDeferred,
                GateOutcome::Passed | GateOutcome::ApprovalOnly { .. },
            ) => *now = outcome.clone(),
            (GateOutcome::ApprovalOnly { .. }, GateOutcome::ApprovalOnly { .. }) => {
                return Err(EngineError::CandidateDrift);
            }
            _ => {}
        }
    }
    Ok(current)
}

/// Preserve the runtime's lazy evidence, candidate-body, readiness order.
pub(crate) fn discovery_has_no_progress(
    same_evidence: impl FnOnce() -> bool,
    same_candidate_bodies: impl FnOnce() -> bool,
    gained_direct_readiness: impl FnOnce() -> bool,
) -> bool {
    same_evidence() && same_candidate_bodies() && !gained_direct_readiness()
}

fn gained_direct_candidate(
    current: &CandidateSet,
    previous: &CandidateSet,
    is_current_direct: impl Fn(&CausalCandidate) -> bool,
    is_previous_direct: impl Fn(&CausalCandidate) -> bool,
) -> bool {
    let CandidateSet::Legal {
        candidates: current, ..
    } = current
    else {
        return false;
    };
    let previous = match previous {
        CandidateSet::Legal { candidates, .. } => candidates.as_slice(),
        CandidateSet::NoLegalPath { .. } => &[],
    };
    current.iter().any(|candidate| {
        is_current_direct(candidate)
            && !previous
                .iter()
                .any(|prior| prior.candidate_id() == candidate.candidate_id() && is_previous_direct(prior))
    })
}

fn candidate_is_direct(
    candidate: &CausalCandidate,
    catalog: &CatalogView<'_>,
    resume_target: Option<&CapabilityRef>,
    discovered_refs: &DiscoveredRefs,
) -> bool {
    if candidate.required_capabilities().is_empty() {
        return candidate.action() == CausalActionKind::Answer
            && candidate.target() == super::branch::CapabilityTarget::None;
    }
    candidate.required_capabilities().iter().all(|reference| {
        catalog.entry_any(&reference.id).is_some_and(|entry| {
            matches!(entry.exposure, Exposure::Direct)
                || (candidate.action() == CausalActionKind::Retrieve && is_retrieval_source(entry))
                // Final-gated selected search exposes only exact current refs.
                // Selectability never substitutes for an execution grant.
                || (candidate.action() == CausalActionKind::UseTool
                    && matches!(entry.exposure, Exposure::Searchable)
                    && catalog.contains(&reference.id)
                    && reference.generation == catalog.generation()
                    && reference.generation == entry.generation
                    && reference.fingerprint == entry.fingerprint
                    && matches!(entry.family, super::branch::CapabilityTarget::Skill
                        | super::branch::CapabilityTarget::Mcp | super::branch::CapabilityTarget::NativeTool
                        | super::branch::CapabilityTarget::Wasm | super::branch::CapabilityTarget::Hook)
                    && discovered_refs.contains(reference))
                // A freshly authenticated exact resume already owns
                // this concrete capability. It need not rediscover
                // a Filesystem entry, and does not expose any peer.
                || (candidate.action() == CausalActionKind::UseTool
                    && candidate.required_capabilities().len() == 1
                    && resume_target.is_some_and(|target| target == reference)
                    && reference.generation == catalog.generation()
                    && reference.generation == entry.generation
                    && reference.fingerprint == entry.fingerprint
                    && matches!(entry.exposure, Exposure::Searchable))
        })
    })
}

fn exact_candidate_without_discovery_is_current(candidate: &CausalCandidate, catalog: &CatalogView<'_>) -> bool {
    candidate.action() == CausalActionKind::UseTool
        && candidate.required_capabilities().len() == 1
        && !super::candidate_generator::DiscoveryFamilies::supports(candidate.target())
        && candidate.required_capabilities().iter().all(|reference| {
            catalog.entry_any(&reference.id).is_some_and(|entry| {
                matches!(entry.exposure, Exposure::Searchable)
                    && catalog.contains(&reference.id)
                    && entry.family == candidate.target()
                    && reference.generation == catalog.generation()
                    && reference.generation == entry.generation
                    && reference.fingerprint == entry.fingerprint
            })
        })
}

fn selector_context(
    set: &CandidateSet,
    catalog: &CatalogView<'_>,
    interactive: bool,
    resume_target: Option<&CapabilityRef>,
    discovered_refs: &DiscoveredRefs,
) -> SelectorContext {
    let direct_candidates = match set {
        CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } => candidates
            .iter()
            .filter(|candidate| {
                // An exact or source-configured delegator already has a
                // derived, sealed child manifest. It does not need a catalog
                // discovery hop to become ready, while an unmentioned Direct
                // delegator retains the ordinary prediction path.
                (candidate.action() == CausalActionKind::Delegate && exact_mentions.contains(candidate.candidate_id()))
                    // Some executable catalog families deliberately have no
                    // capability-search port. An exact source mention can
                    // select their one current visible ref, but cannot make a
                    // stale, excluded, compound, or discoverable-family ref
                    // direct.
                    || (exact_mentions.contains(candidate.candidate_id())
                        && exact_candidate_without_discovery_is_current(candidate, catalog))
                    || candidate_is_direct(candidate, catalog, resume_target, discovered_refs)
            })
            .map(|candidate| candidate.candidate_id().clone())
            .collect(),
        CandidateSet::NoLegalPath { .. } => BTreeSet::new(),
    };
    SelectorContext {
        interactive,
        direct_candidates,
        discovery_completed: discovered_refs.iter().next().is_some(),
    }
}

fn empty_evidence() -> PredictionEvidence {
    PredictionEvidence {
        candidates: BTreeMap::<CandidateId, CandidatePrediction>::new(),
        action_confidence: 0.0,
        target_confidence: 0.0,
        target_probabilities: BTreeMap::new(),
        chosen_action: "none".to_string(),
        chosen_target: "none".to_string(),
        evidence_sufficient: 0.0,
        needs_fresh_state: 0.0,
    }
}

fn terminal_evidence(set: &CandidateSet) -> PredictionEvidence {
    let mut evidence = empty_evidence();
    if let CandidateSet::Legal { candidates, .. } = set {
        for candidate in candidates.iter().filter(|item| !is_terminal(item.action())) {
            evidence.candidates.insert(
                candidate.candidate_id().clone(),
                CandidatePrediction {
                    choice_probability: 0.0,
                    applicability_probability: 0.0,
                    success_probability: 0.0,
                    expected_gain: 0.0,
                    gain_uncertainty: 1.0,
                    semantic_risk: 1.0,
                    risk_uncertainty: 1.0,
                },
            );
        }
    }
    evidence
}

fn lineage(trusted: &super::state::TrustedParts) -> CausalLineage {
    CausalLineage {
        workspace_id: Arc::clone(&trusted.workspace_id),
        owner_id: trusted.owner_id.clone(),
        session_key: trusted.session_key.clone(),
        run_id: trusted.run_id.clone(),
        task_id: trusted.task_id.clone(),
        source_event_id: trusted.source_event_id.clone(),
        causation_event_id: trusted.causation_event_id.clone(),
        decision_id: None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::super::branch::{
        ApprovalRequirement, CandidateSpec, CapabilityId, CapabilityTarget, CostEstimate, OpId, SchemaFingerprint,
        SideEffectClass,
    };
    use super::super::state::{TrustedState, fixtures};
    use super::*;
    use crate::decision::backend::DecisionBackend;
    use crate::decision::builtin::BuiltinDecisionBackend;
    use crate::decision::factory::SlotLimits;
    use crate::decision::outbound::GuardedBody;
    use crate::decision::outbound::OutboundSecrets;
    use crate::decision::validate::ModelVersionPolicy;
    use crate::decision::{
        BackendIdentity, BackendKind, BackendLocality, DecisionError, DecisionRequest, DecisionResponse,
        DecisionResponseValidator, ValidationContext,
    };
    use crate::security::AutonomyLevel;
    use crate::security::SecurityPolicy;
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
    use crate::tools::capability::child::AuthorityEnvelope;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DelayedBackend {
        calls: Arc<AtomicUsize>,
        identity: BackendIdentity,
    }

    #[async_trait]
    impl DecisionBackend for DelayedBackend {
        fn identity(&self) -> &BackendIdentity {
            &self.identity
        }
        fn locality(&self) -> BackendLocality {
            BackendLocality::Loopback
        }
        fn version_policy(&self) -> ModelVersionPolicy {
            ModelVersionPolicy::Unpinned
        }
        async fn evaluate(
            &self,
            _request: &DecisionRequest,
            _body: &GuardedBody,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(4)).await;
            Err(DecisionError::Timeout)
        }
    }

    fn candidate(action: CausalActionKind) -> CausalCandidate {
        CausalCandidate::new(
            action,
            CapabilityTarget::None,
            CandidateSpec {
                namespace: None,
                required_capabilities: Vec::new(),
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        )
    }

    fn report(outcomes: BTreeMap<CandidateId, GateOutcome>) -> GateReport {
        GateReport {
            outcomes,
            gate_set_hash: [7; 32],
        }
    }

    #[test]
    fn validated_audit_trace_keeps_numeric_scores_and_forced_codes_without_state_text() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "prx_private_goal_sentinel");
        let working = fixtures::candidate(&key, CausalActionKind::UseTool, CapabilityTarget::Mcp, None);
        let refusal = fixtures::candidate(&key, CausalActionKind::Refuse, CapabilityTarget::None, None);
        state.derived = DerivedState::new(vec![working, refusal]).expect("valid candidate fixture");
        let artifacts = (0..8)
            .map(|index| fixtures::artifact(&format!("artifact-{index}-{}", "a".repeat(220)), index * 10))
            .collect();
        state.observed = super::super::state::ObservedState::from_runtime_records(Vec::new(), artifacts, 0);
        let candidates = state
            .derived
            .candidates()
            .iter()
            .map(|item| item.candidate.clone())
            .collect();
        let set = CandidateSet::Legal {
            candidates,
            exact_mentions: BTreeSet::new(),
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: BTreeMap::new(),
            report: report(BTreeMap::new()),
        };
        let mut policy = CompilerPolicy {
            key: &key,
            model: None,
            expires_at: Instant::now() + Duration::from_secs(30),
            limits: RequestLimits::default(),
        };
        let baseline = QuestionCompiler::compile(&state, &set, &policy).expect("valid compiled fixture");
        let mut shrunk = None;
        for bytes in (2_000..=8_000).step_by(256) {
            policy.limits.max_request_bytes = bytes;
            if let Ok(compiled) = QuestionCompiler::compile(&state, &set, &policy) {
                if !compiled
                    .request()
                    .manifest()
                    .projection_audit()
                    .truncated_fields
                    .is_empty()
                {
                    shrunk = Some(compiled);
                    break;
                }
            }
        }
        let compiled = shrunk.expect("sealed request shrinks artifact summaries");
        assert_ne!(
            baseline.request().state().as_json(),
            compiled.request().state().as_json()
        );
        let response = BuiltinDecisionBackend::answer(compiled.request()).expect("builtin answer");
        let validated = DecisionResponseValidator::validate(
            compiled.request(),
            response,
            &ValidationContext {
                received_at: Instant::now(),
                max_response_bytes: 256 * 1024,
                version_policy: ModelVersionPolicy::Unpinned,
                local_backend: BuiltinDecisionBackend::identity(),
            },
        )
        .expect("valid response");
        PredictionEvidence::extract(&validated, &compiled, &set).expect("bound evidence");
        let trace = PredictionAuditTrace::capture(&validated, &compiled, true).expect("bound audit trace");
        let sealed_projection = compiled.request().state().as_json().to_string();
        assert_eq!(trace.redacted_projection.as_deref(), Some(sealed_projection.as_str()));
        assert!(
            PredictionAuditTrace::capture(&validated, &compiled, false)
                .expect("bound audit trace without projection")
                .redacted_projection
                .is_none()
        );
        let Some(PredictionAuditAnswer::Score {
            distribution,
            expected,
            confidence,
        }) = trace.answers.get("semantic_risk")
        else {
            panic!("expected numeric risk score");
        };
        assert_eq!(distribution.len(), 5);
        assert!(
            distribution
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        );
        assert!(expected.is_finite() && (0.0..=4.0).contains(expected));
        assert!(confidence.is_finite() && (0.0..=1.0).contains(confidence));
        assert_eq!(
            trace.forced_choices.get("next_action").map(String::as_str),
            Some("use_tool")
        );
        assert_eq!(
            trace.omitted_questions.get("next_action"),
            Some(&"single_legal_candidate")
        );
        assert!(!format!("{trace:?}").contains("prx_private_goal_sentinel"));
        assert!(!format!("{trace:?}").contains("very low"));
    }

    #[test]
    fn outgoing_child_is_new_strict_subset_for_root_and_nested_worker() {
        let key = fixtures::key();
        let mut trusted = fixtures::trusted();
        trusted.owner_id = Some(Arc::from("prx_user"));
        trusted.workspace_id = Arc::from("prx_workspace");
        trusted.budgets.micro_usd = 1_000_000;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        let security = SecurityPolicy::default();
        let generation = trusted.catalog_generation;
        let reference = |name| CapabilityRef {
            id: CapabilityId::new(CapabilityTarget::NativeTool, "prx", name).unwrap(),
            fingerprint: SchemaFingerprint([1; 32]),
            generation,
        };
        let spawn = CapabilityRef {
            id: CapabilityId::new(CapabilityTarget::Session, "prx", "delegate").unwrap(),
            fingerprint: SchemaFingerprint([2; 32]),
            generation,
        };
        let read = reference("read");
        let search = reference("search");
        let other = reference("other");
        let root_id = DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap();
        let root = CapabilityConstraints {
            decision_id: root_id,
            allowed: BTreeSet::from([spawn.clone(), read.clone(), search.clone(), other]),
            authority: AuthorityEnvelope {
                owner: trusted.owner_id.as_ref().unwrap().to_string(),
                workspace: trusted.workspace_id.to_string(),
                level: AutonomyLevel::Full,
                max_cost_cents: 100,
                remaining_depth: 2,
            },
            catalog_generation: generation,
            key: Arc::clone(&key),
        };
        let mut read_only_policy = SecurityPolicy::default();
        read_only_policy.autonomy = AutonomyLevel::ReadOnly;
        let overprivileged = CapabilityConstraints {
            decision_id: root_id,
            allowed: root.allowed.clone(),
            authority: AuthorityEnvelope {
                level: AutonomyLevel::Full,
                ..root.authority.clone()
            },
            catalog_generation: generation,
            key: Arc::clone(&key),
        };
        assert!(matches!(
            derive_outgoing_child(
                root_id,
                &key,
                &trusted,
                &read_only_policy,
                2,
                None,
                Some((&overprivileged, std::slice::from_ref(&read)))
            ),
            Err(EngineError::InvalidDelegation)
        ));
        assert!(matches!(
            derive_outgoing_child(
                root_id,
                &key,
                &trusted,
                &security,
                1,
                None,
                Some((&root, std::slice::from_ref(&read)))
            ),
            Err(EngineError::InvalidDelegation)
        ));
        let first = derive_outgoing_child(
            root_id,
            &key,
            &trusted,
            &security,
            2,
            None,
            Some((&root, &[spawn.clone(), read.clone(), search.clone()])),
        )
        .unwrap()
        .unwrap();
        assert!(first.allowed.contains(&spawn), "this child may spawn a nested worker");
        assert_eq!(first.authority.remaining_depth, 1);
        first.verify(&root).unwrap();

        let nested_id = DecisionId::parse("0192f0c4-0000-7000-8000-000000000002").unwrap();
        let nested = CapabilityConstraints {
            decision_id: nested_id,
            allowed: first.allowed.clone(),
            authority: first.authority.clone(),
            catalog_generation: generation,
            key: Arc::clone(&key),
        };
        let second = derive_outgoing_child(
            nested_id,
            &key,
            &trusted,
            &security,
            2,
            Some(&first),
            Some((&nested, std::slice::from_ref(&read))),
        )
        .unwrap()
        .unwrap();
        assert_eq!(second.allowed, BTreeSet::from([read.clone()]));
        assert_eq!(second.authority.remaining_depth, 0);
        second.verify(&nested).unwrap();

        let mismatched = CapabilityConstraints {
            decision_id: nested_id,
            allowed: BTreeSet::from([read.clone(), search.clone()]),
            authority: nested.authority.clone(),
            catalog_generation: generation,
            key: Arc::clone(&key),
        };
        assert!(matches!(
            derive_outgoing_child(
                nested_id,
                &key,
                &trusted,
                &security,
                2,
                Some(&first),
                Some((&mismatched, &[]))
            ),
            Err(EngineError::InvalidDelegation)
        ));
        let wrong_key = fixtures::key();
        let wrong = CapabilityConstraints {
            decision_id: nested_id,
            allowed: nested.allowed.clone(),
            authority: nested.authority.clone(),
            catalog_generation: generation,
            key: wrong_key,
        };
        assert!(matches!(
            derive_outgoing_child(
                nested_id,
                &key,
                &trusted,
                &security,
                2,
                Some(&first),
                Some((&wrong, std::slice::from_ref(&read)))
            ),
            Err(EngineError::InvalidDelegation)
        ));

        let delegate = CausalCandidate::new(
            CausalActionKind::Delegate,
            CapabilityTarget::Session,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![spawn.clone()],
                side_effect: SideEffectClass::ProcessExec,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let delegate_id = delegate.candidate_id().clone();
        let gates = report(BTreeMap::from([(delegate_id.clone(), GateOutcome::Passed)]));
        let mut nested_state = fixtures::state(&key, "neutral request");
        nested_state.trusted = TrustedState::from_runtime(trusted.clone());
        let mut decision = EngineDecision {
            decision_id: nested_id,
            deadline: Instant::now() + Duration::from_secs(5),
            elapsed_ms: 0,
            state: nested_state,
            candidates: CandidateSet::Legal {
                candidates: vec![delegate],
                exact_mentions: BTreeSet::new(),
                truncated: Vec::new(),
                discoverable_families: Default::default(),
                approval_scopes: BTreeMap::new(),
                report: gates.clone(),
            },
            selection: Selection::Commit {
                candidate_id: delegate_id,
                propensity: 1.0,
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: None,
            scores: BTreeMap::new(),
            scored_count: 0,
            rehearsal: BTreeMap::new(),
            post_rehearsal: gates.clone(),
            pre_commit: gates,
            call_fingerprints: BTreeMap::new(),
            outgoing_child: Some(second),
            outgoing_parent_allowed: Some(nested.allowed),
        };
        let context = super::super::directive::DirectiveContext {
            current_generation: generation,
            approval_expires_at: chrono::Utc::now() + chrono::Duration::minutes(1),
            discovery_limit: 4,
        };
        assert!(matches!(
            super::super::directive::directive_from_selection(&decision, &context),
            Ok(super::super::directive::TurnExecutionDirective::Delegate { manifest })
                if manifest.parent_decision_id == nested_id && !manifest.allowed.contains(&spawn)
        ));
        // A separately derived child without spawn cannot emit Delegate.
        let no_spawn = derive_outgoing_child(
            root_id,
            &key,
            &trusted,
            &security,
            2,
            None,
            Some((&root, &[read.clone(), search])),
        )
        .unwrap()
        .unwrap();
        assert!(!no_spawn.allowed.contains(&spawn));
        let no_spawn_parent = CapabilityConstraints {
            decision_id: nested_id,
            allowed: no_spawn.allowed.clone(),
            authority: no_spawn.authority.clone(),
            catalog_generation: generation,
            key: Arc::clone(&key),
        };
        decision.outgoing_child = derive_outgoing_child(
            nested_id,
            &key,
            &trusted,
            &security,
            2,
            Some(&no_spawn),
            Some((&no_spawn_parent, &[read])),
        )
        .unwrap();
        decision.outgoing_parent_allowed = Some(no_spawn_parent.allowed);
        assert!(matches!(
            super::super::directive::directive_from_selection(&decision, &context),
            Err(super::super::directive::DirectiveError::InvalidChildScope)
        ));
    }

    #[test]
    fn outgoing_child_rejects_excluded_catalog_reference() {
        struct PrxReadTool;
        #[async_trait]
        impl crate::tools::traits::Tool for PrxReadTool {
            fn name(&self) -> &str {
                "prx_read"
            }
            fn description(&self) -> &str {
                "Read local data"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object","properties":{}})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::traits::ToolResult> {
                Ok(crate::tools::traits::ToolResult {
                    success: true,
                    output: String::new(),
                    error: None,
                })
            }
        }
        let tools: Vec<Box<dyn crate::tools::traits::Tool>> = vec![Box::new(PrxReadTool)];
        let visible = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let id = visible
            .view(&CatalogScope::default())
            .entries()
            .first()
            .unwrap()
            .id
            .clone();
        let mut excluded_surface = SurfacePolicy::default();
        excluded_surface.excluded_names.insert("prx_read".to_string());
        let excluded = CapabilityCatalog::build(&tools, &CatalogSources::default(), &excluded_surface).unwrap();
        let view = excluded.view(&CatalogScope::default());
        let entry = view.entry_any(&id).unwrap();
        let reference = CapabilityRef {
            id,
            fingerprint: entry.fingerprint,
            generation: view.generation(),
        };
        let key = fixtures::key();
        let mut trusted = fixtures::trusted();
        trusted.owner_id = Some(Arc::from("prx_user"));
        trusted.workspace_id = Arc::from("prx_workspace");
        trusted.catalog_generation = view.generation();
        let parent = CapabilityConstraints {
            decision_id: DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap(),
            allowed: BTreeSet::from([reference]),
            authority: AuthorityEnvelope {
                owner: trusted.owner_id.as_ref().unwrap().to_string(),
                workspace: trusted.workspace_id.to_string(),
                level: AutonomyLevel::ReadOnly,
                max_cost_cents: 0,
                remaining_depth: 1,
            },
            catalog_generation: view.generation(),
            key,
        };
        assert!(matches!(
            validate_delegation_refs(
                &parent,
                &view,
                &trusted,
                &SecurityPolicy::default(),
                "prx_user",
                "terminal",
                "dm"
            ),
            Err(EngineError::InvalidDelegation)
        ));
    }

    #[test]
    fn post_rehearsal_denial_cannot_be_restored_by_pre_commit() {
        let id = candidate(CausalActionKind::Answer).candidate_id().clone();
        let passed = report(BTreeMap::from([(id.clone(), GateOutcome::Passed)]));
        let blocked = GateOutcome::Blocked {
            gate: 3,
            reason_code: "unavailable_or_stale",
        };
        let denied = report(BTreeMap::from([(id.clone(), blocked.clone())]));
        assert!(matches!(
            meet_gate_reports(&passed, passed.clone()).unwrap().outcomes.get(&id),
            Some(GateOutcome::Passed)
        ));
        assert_eq!(
            meet_gate_reports(&denied, passed).unwrap().outcomes.get(&id),
            Some(&blocked)
        );
    }

    #[test]
    fn approval_scope_cannot_be_restored_or_rebound() {
        let id = candidate(CausalActionKind::Answer).candidate_id().clone();
        let first = GateOutcome::ApprovalOnly {
            op_ids: vec![OpId::new("prx.read")],
        };
        let other = GateOutcome::ApprovalOnly {
            op_ids: vec![OpId::new("prx.write")],
        };
        let post = report(BTreeMap::from([(id.clone(), first.clone())]));
        let passed = report(BTreeMap::from([(id.clone(), GateOutcome::Passed)]));
        assert_eq!(
            meet_gate_reports(&post, passed).unwrap().outcomes.get(&id),
            Some(&first)
        );
        assert!(matches!(
            meet_gate_reports(&post, report(BTreeMap::from([(id, other)]))),
            Err(EngineError::CandidateDrift)
        ));
    }

    #[test]
    fn exhausted_prediction_has_no_working_choice_mass() {
        let answer = candidate(CausalActionKind::Answer);
        let id = answer.candidate_id().clone();
        let set = CandidateSet::Legal {
            candidates: vec![
                answer,
                candidate(CausalActionKind::Clarify),
                candidate(CausalActionKind::Refuse),
            ],
            exact_mentions: BTreeSet::new(),
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: BTreeMap::new(),
            report: report(BTreeMap::new()),
        };
        let evidence = terminal_evidence(&set);
        assert_eq!(
            evidence
                .candidates
                .get(&id)
                .expect("working candidate")
                .choice_probability,
            0.0
        );
        assert_eq!(evidence.action_confidence, 0.0);
        assert_eq!(evidence.target_confidence, 0.0);
    }

    #[test]
    fn selected_read_issuers_reject_over_bound_exact_source_queries() {
        struct SourceTool(&'static str);
        #[async_trait]
        impl crate::tools::traits::Tool for SourceTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "Read neutral evidence"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object","properties":{}})
            }
            fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
                if self.0 == "memory_search" {
                    &[crate::tools::traits::ToolCategory::Memory]
                } else {
                    &[]
                }
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::traits::ToolResult> {
                panic!("read issuer tests never dispatch tools")
            }
        }
        let key = fixtures::key();
        for (action, family, name) in [
            (CausalActionKind::Discover, CapabilityTarget::NativeTool, "image_info"),
            (CausalActionKind::Retrieve, CapabilityTarget::Memory, "memory_search"),
            (
                CausalActionKind::Retrieve,
                CapabilityTarget::Document,
                "document_search",
            ),
            (
                CausalActionKind::Retrieve,
                CapabilityTarget::Session,
                "sessions_history",
            ),
        ] {
            let tools: Vec<Box<dyn crate::tools::traits::Tool>> = vec![Box::new(SourceTool(name))];
            let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default())
                .expect("actual selected-read catalog");
            let view = catalog.view(&CatalogScope::default());
            let entry = view.iter_entries().next().expect("selected read source");
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            };
            let candidate = CausalCandidate::new(
                action,
                family,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: vec![reference],
                    side_effect: SideEffectClass::ReadOnly,
                    approval: ApprovalRequirement::None,
                    estimated_cost: None,
                    estimated_latency_ms: None,
                },
            );
            let id = candidate.candidate_id().clone();
            let gates = report(BTreeMap::from([(id.clone(), GateOutcome::Passed)]));
            let mut state = fixtures::state(&key, "neutral query");
            let mut trusted = fixtures::trusted();
            trusted.catalog_generation = view.generation();
            state.trusted = TrustedState::from_runtime(trusted);
            // Synthetic selected decisions exercise the final issuer boundary,
            // independently of ordinary generator availability filtering.
            let mut decision = EngineDecision {
                decision_id: DecisionId::new_runtime(),
                deadline: Instant::now() + Duration::from_secs(5),
                elapsed_ms: 0,
                state,
                candidates: CandidateSet::Legal {
                    candidates: vec![candidate],
                    exact_mentions: BTreeSet::new(),
                    truncated: Vec::new(),
                    discoverable_families: Default::default(),
                    approval_scopes: BTreeMap::new(),
                    report: gates.clone(),
                },
                selection: Selection::Commit {
                    candidate_id: id,
                    propensity: 1.0,
                },
                exploration_propensities: BTreeMap::new(),
                prediction: None,
                request_compiled: false,
                audit_trace: None,
                fallback_cause: None,
                scores: BTreeMap::new(),
                scored_count: 0,
                rehearsal: BTreeMap::new(),
                post_rehearsal: gates.clone(),
                pre_commit: gates,
                call_fingerprints: BTreeMap::new(),
                outgoing_child: None,
                outgoing_parent_allowed: None,
            };
            let context = DirectiveContext {
                current_generation: view.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            };
            for (query, supported) in [
                (String::new(), true),
                ("neutral query".to_owned(), true),
                ("a".repeat(1024), true),
                ("\u{e9}".repeat(512), true),
                ("a".repeat(1025), false),
                (format!("{}a", "\u{e9}".repeat(512)), false),
            ] {
                decision.state.routing_input = super::super::state::RawRoutingInput::new(query.clone());
                let allowed = if action == CausalActionKind::Discover {
                    decision.issue_selected_discovery(&view, &context, &query).is_ok()
                } else {
                    decision.validate_selected_retrieval(&view, &context, &query).is_ok()
                };
                assert_eq!(
                    allowed,
                    supported,
                    "action={}, UTF-8 bytes={}",
                    action.as_str(),
                    query.len()
                );
                if action == CausalActionKind::Retrieve {
                    assert!(
                        decision
                            .validate_selected_retrieval(&view, &context, "foreign query")
                            .is_err()
                    );
                    if supported {
                        assert_eq!(
                            decision.validate_selected_retrieval(&view, &context, &query).unwrap(),
                            family
                        );
                    }
                }
            }
            if family == CapabilityTarget::Session {
                let mut trusted = decision.state.trusted.parts().clone();
                trusted.source_event_id = None;
                decision.state.trusted = super::super::state::TrustedState::from_runtime(trusted);
                decision.state.routing_input = super::super::state::RawRoutingInput::new("neutral query".to_owned());
                assert!(
                    decision
                        .validate_selected_retrieval(&view, &context, "neutral query")
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn selected_discovery_issues_only_pinned_visible_allowed_refs() {
        struct PrxSearchableTool;
        #[async_trait]
        impl crate::tools::traits::Tool for PrxSearchableTool {
            fn name(&self) -> &str {
                "prx_searchable"
            }
            fn description(&self) -> &str {
                "Searchable PRX fixture"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object","properties":{}})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::traits::ToolResult> {
                Ok(crate::tools::traits::ToolResult {
                    success: true,
                    output: String::new(),
                    error: None,
                })
            }
        }
        struct PrxPeerTool;
        #[async_trait]
        impl crate::tools::traits::Tool for PrxPeerTool {
            fn name(&self) -> &str {
                "prx_peer"
            }
            fn description(&self) -> &str {
                "Searchable peer fixture"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object","properties":{}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::traits::ToolResult> {
                Ok(crate::tools::traits::ToolResult {
                    success: true,
                    output: String::new(),
                    error: None,
                })
            }
        }
        let tools: Vec<Box<dyn crate::tools::traits::Tool>> = vec![Box::new(PrxSearchableTool), Box::new(PrxPeerTool)];
        let catalog = CapabilityCatalog::build(
            &tools,
            &CatalogSources::default(),
            &SurfacePolicy {
                channel_allowed: true,
                ..Default::default()
            },
        )
        .unwrap();
        let view = catalog.view(&CatalogScope::default());
        let entry = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "prx_searchable")
            .expect("searchable fixture");
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "prx_searchable");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = view.generation();
        state.trusted = TrustedState::from_runtime(trusted.clone());
        let candidate = CausalCandidate::new(
            CausalActionKind::Discover,
            CapabilityTarget::NativeTool,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference.clone()],
                side_effect: SideEffectClass::ReadOnly,
                approval: ApprovalRequirement::None,
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let id = candidate.candidate_id().clone();
        let gates = report(BTreeMap::from([(id.clone(), GateOutcome::Passed)]));
        let mut decision = EngineDecision {
            decision_id: DecisionId::new_runtime(),
            deadline: Instant::now() + Duration::from_secs(5),
            elapsed_ms: 0,
            state,
            candidates: CandidateSet::Legal {
                candidates: vec![candidate],
                exact_mentions: BTreeSet::new(),
                truncated: Vec::new(),
                discoverable_families: Default::default(),
                approval_scopes: BTreeMap::new(),
                report: gates.clone(),
            },
            selection: Selection::Commit {
                candidate_id: id.clone(),
                propensity: 1.0,
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: None,
            scores: BTreeMap::new(),
            scored_count: 0,
            rehearsal: BTreeMap::new(),
            post_rehearsal: gates.clone(),
            pre_commit: gates,
            call_fingerprints: BTreeMap::new(),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        };
        let context = DirectiveContext {
            current_generation: view.generation(),
            approval_expires_at: chrono::Utc::now(),
            discovery_limit: 1,
        };
        assert!(
            decision
                .issue_selected_discovery(&view, &context, "prx_searchable")
                .unwrap()
                .contains(&reference)
        );
        let selected_refs = decision
            .issue_selected_discovery(&view, &context, "prx_searchable")
            .unwrap();
        // Reproduce the alias failure using actual preparations and authenticated
        // discovery. Scores are deterministic fixture inputs to the real selector.
        {
            let mut hop_state = decision.state.clone();
            let mut parts = hop_state.trusted.parts().clone();
            parts.side_effect_mode = SideEffectMode::GuardedWrite;
            parts.delivery.interactive = false;
            parts.delivery.can_ask_question = false;
            parts.delivery.approval_channel = false;
            hop_state.trusted = TrustedState::from_runtime(parts);
            let policy = SecurityPolicy::default();
            let fingerprints = BTreeMap::new();
            let replayed = BTreeSet::new();
            let gate_inputs = GateInputs {
                policy: &policy,
                owner_id: "owner-1",
                sender: "prx_user",
                policy_channel: "terminal",
                chat_type: "dm",
                model_ref: "anthropic:claude",
                model_can_chat: true,
                model_can_call_tools: true,
                decision_endpoint_ready: true,
                child_scope: None,
                verified_approval_calls: BTreeMap::new(),
                call_fingerprints: &fingerprints,
                failed_call_counts: BTreeMap::new(),
                replayed_non_idempotent: &replayed,
                retry_limit: Some(0),
            };
            let cost = super::super::candidate_generator::CapabilityCostModel {
                per_capability: view
                    .iter_entries()
                    .map(|entry| {
                        (
                            CapabilityRef {
                                id: entry.id.clone(),
                                fingerprint: entry.fingerprint,
                                generation: entry.generation,
                            },
                            (
                                CostEstimate {
                                    tokens: 1,
                                    micro_usd: 0,
                                },
                                1,
                            ),
                        )
                    })
                    .collect(),
                estimates: CapabilityTarget::ALL
                    .into_iter()
                    .map(|target| {
                        (
                            target,
                            (
                                CostEstimate {
                                    tokens: 1,
                                    micro_usd: 0,
                                },
                                1,
                            ),
                        )
                    })
                    .collect(),
            };
            let config = CausalTreeConfig::default();
            let backend = DecisionBackendFactory::from_slots(
                Vec::new(),
                Arc::clone(&key),
                OutboundSecrets::default(),
                SlotLimits::default(),
            );
            let ports = RehearsalPorts {
                memory: None,
                catalog: None,
                skill: None,
            };
            let no_refs = DiscoveredRefs::empty();
            let inputs = EngineInputs {
                state: &hop_state,
                catalog: &view,
                config: &config,
                key: &key,
                decision_id: DecisionId::new_runtime(),
                cost_model: &cost,
                discovered_refs: &no_refs,
                gates: &gate_inputs,
                delegation: None,
                delegation_max_depth: 0,
                prediction: backend.prediction_port(None),
                rehearsal_ports: &ports,
                request_limits: RequestLimits::default(),
                model: Some("prx-model"),
                can_deliver_text: true,
                may_delegate: false,
                delegation_target: None,
                terminal_only: false,
                allow_terminal_answer: false,
                deadline: Instant::now() + Duration::from_secs(5),
                inject_builtin_failure: false,
                after_prediction_pause: None,
            };
            let previous = CausalDecisionEngine::prepare_hop(&inputs).unwrap();
            let CandidateSet::Legal { candidates, report, .. } = &previous.candidates else {
                panic!("exact alias fixture must have legal candidates");
            };
            let exact = candidates
                .iter()
                .find(|candidate| candidate.action() == CausalActionKind::UseTool)
                .expect("exact mention survives before discovery");
            let exact_id = exact.candidate_id().clone();
            let mut evidence = empty_evidence();
            evidence.action_confidence = 1.0;
            evidence.target_confidence = 1.0;
            evidence.chosen_action = "use_tool".to_owned();
            evidence.chosen_target = "native_tool".to_owned();
            evidence.target_probabilities.insert(CapabilityTarget::NativeTool, 1.0);
            evidence.evidence_sufficient = 1.0;
            let mut scores = BTreeMap::new();
            for candidate in candidates {
                let chosen = candidate.candidate_id() == &exact_id;
                let probability = if chosen { 1.0 } else { 0.0 };
                scores.insert(
                    candidate.candidate_id().clone(),
                    CandidateScore {
                        semantic_utility: probability,
                        execution_utility: probability,
                    },
                );
                evidence.candidates.insert(
                    candidate.candidate_id().clone(),
                    CandidatePrediction {
                        choice_probability: probability,
                        applicability_probability: probability,
                        success_probability: probability,
                        expected_gain: probability,
                        gain_uncertainty: 0.0,
                        semantic_risk: 0.0,
                        risk_uncertainty: 0.0,
                    },
                );
            }
            let selected = CausalSelector::select(
                &previous.candidates,
                &scores,
                &evidence,
                report,
                &config.policy,
                &selector_context(&previous.candidates, &view, false, None, &no_refs),
            )
            .unwrap();
            assert!(
                matches!(&selected, Selection::Recover { candidate_id, reason_code: "discover_capability" }
                if candidate_id == &id)
            );
            decision.selection = selected;
            let authenticated = decision
                .issue_selected_discovery(&view, &context, "prx_searchable")
                .unwrap();
            assert!(authenticated.contains(&reference));
            let mut accumulated = DiscoveredRefs::empty();
            assert!(
                accumulated.extend_selected(authenticated),
                "this is not an empty/repeated search"
            );
            let next_inputs = EngineInputs {
                discovered_refs: &accumulated,
                ..inputs.clone()
            };
            let current = CausalDecisionEngine::prepare_hop(&next_inputs).unwrap();
            let same_evidence = current.evidence_fingerprint() == previous.evidence_fingerprint();
            let same_bodies = !current.candidate_bodies_changed_from_prepared(&previous);
            assert!(same_evidence && same_bodies, "Q44 leaves the same exact candidate body");
            let old_stop = same_evidence && same_bodies;
            assert!(
                old_stop,
                "the actual old two-operand runtime predicate stops after Discover"
            );
            let old_terminal_inputs = EngineInputs {
                terminal_only: old_stop,
                ..next_inputs.clone()
            };
            let old_terminal = CausalDecisionEngine::decide_terminal_prepared(
                CausalDecisionEngine::prepare_hop(&old_terminal_inputs).unwrap(),
            )
            .unwrap();
            assert_eq!(old_terminal.fallback_cause, Some(FallbackCause::HopStopped));
            assert!(matches!(
                old_terminal.selection,
                Selection::Abstain {
                    action: CausalActionKind::Refuse,
                    reason_code: "hop_stopped",
                }
            ));
            assert!(
                !discovery_has_no_progress(
                    || current.evidence_fingerprint() == previous.evidence_fingerprint(),
                    || !current.candidate_bodies_changed_from_prepared(&previous),
                    || current.gained_direct_candidate_from_prepared(&previous),
                ),
                "the runtime connective must retain authenticated readiness progress"
            );
            assert!(
                discovery_has_no_progress(|| same_evidence, || same_bodies, || false),
                "negative mutation removing readiness recognition reproduces the old stop"
            );
            let CandidateSet::Legal { report, .. } = &current.candidates else {
                panic!("authenticated preparation remains legal");
            };
            assert!(matches!(CausalSelector::select(
                &current.candidates, &scores, &evidence, report, &config.policy,
                &selector_context(&current.candidates, &view, false, None, &accumulated),
            ).unwrap(), Selection::Commit { candidate_id, .. } if candidate_id == exact_id));
        }
        let use_candidate = CausalCandidate::new(
            CausalActionKind::UseTool,
            entry.family,
            CandidateSpec {
                namespace: None,
                required_capabilities: vec![reference.clone()],
                side_effect: entry.effect,
                approval: entry.approval.clone(),
                estimated_cost: None,
                estimated_latency_ms: None,
            },
        );
        let use_id = use_candidate.candidate_id().clone();
        let mut available_set = CandidateSet::Legal {
            candidates: vec![use_candidate],
            exact_mentions: BTreeSet::new(),
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: BTreeMap::new(),
            report: report(BTreeMap::new()),
        };
        assert!(
            selector_context(&available_set, &view, true, None, &DiscoveredRefs::empty())
                .direct_candidates
                .is_empty()
        );
        assert_eq!(
            selector_context(&available_set, &view, true, None, &selected_refs).direct_candidates,
            BTreeSet::from([use_id])
        );
        // Genuine selected refs make this unchanged body directly selectable.
        // Repeat discovery and readiness loss must not authorize another hop.
        let empty_refs = DiscoveredRefs::empty();
        let readiness_progress = |current_refs: &DiscoveredRefs, previous_refs: &DiscoveredRefs| {
            gained_direct_candidate(
                &available_set,
                &available_set,
                |candidate| candidate_is_direct(candidate, &view, None, current_refs),
                |candidate| candidate_is_direct(candidate, &view, None, previous_refs),
            )
        };
        assert!(readiness_progress(&selected_refs, &empty_refs));
        assert!(!readiness_progress(&selected_refs, &selected_refs));
        assert!(!readiness_progress(&empty_refs, &selected_refs));
        for stale_schema in [true, false] {
            let mut stale = reference.clone();
            if stale_schema {
                stale.fingerprint.0[0] ^= 1;
            } else {
                stale.generation.0[0] ^= 1;
            }
            let refs = DiscoveredRefs::from_refs_for_test(BTreeSet::from([stale]));
            assert!(!readiness_progress(&refs, &empty_refs));
            assert!(
                selector_context(&available_set, &view, true, None, &refs)
                    .direct_candidates
                    .is_empty()
            );
        }
        let peer_entry = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "prx_peer")
            .unwrap();
        let peer = CapabilityRef {
            id: peer_entry.id.clone(),
            fingerprint: peer_entry.fingerprint,
            generation: peer_entry.generation,
        };
        assert_eq!(entry.family, CapabilityTarget::NativeTool);
        assert_eq!(peer_entry.family, CapabilityTarget::NativeTool);
        assert!(matches!(peer_entry.exposure, Exposure::Searchable));
        assert_ne!(reference.id, peer.id);
        assert!(!selected_refs.contains(&peer));
        let peer_only = DiscoveredRefs::from_refs_for_test(BTreeSet::from([peer.clone()]));
        assert!(!readiness_progress(&peer_only, &empty_refs));
        assert!(
            selector_context(&available_set, &view, true, None, &peer_only)
                .direct_candidates
                .is_empty()
        );
        let mut compound_set = CandidateSet::Legal {
            candidates: Vec::new(),
            exact_mentions: BTreeSet::new(),
            truncated: Vec::new(),
            discoverable_families: Default::default(),
            approval_scopes: BTreeMap::new(),
            report: report(BTreeMap::new()),
        };
        if let CandidateSet::Legal { candidates, .. } = &mut compound_set {
            candidates.push(CausalCandidate::new(
                CausalActionKind::UseTool,
                entry.family,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: vec![reference.clone(), peer],
                    side_effect: entry.effect,
                    approval: entry.approval.clone(),
                    estimated_cost: None,
                    estimated_latency_ms: None,
                },
            ));
        }
        assert!(
            selector_context(&compound_set, &view, true, None, &selected_refs)
                .direct_candidates
                .is_empty()
        );
        let excluded_view = catalog.view(&CatalogScope {
            families: BTreeSet::from([CapabilityTarget::Memory]),
            channel: false,
        });
        assert!(!gained_direct_candidate(
            &available_set,
            &available_set,
            |candidate| candidate_is_direct(candidate, &excluded_view, None, &selected_refs),
            |candidate| candidate_is_direct(candidate, &view, None, &empty_refs),
        ));
        assert!(
            selector_context(&available_set, &excluded_view, true, None, &selected_refs)
                .direct_candidates
                .is_empty()
        );
        for action in [CausalActionKind::Discover, CausalActionKind::Delegate] {
            if let CandidateSet::Legal { candidates, .. } = &mut available_set {
                candidates[0] = CausalCandidate::new(
                    action,
                    entry.family,
                    CandidateSpec {
                        namespace: None,
                        required_capabilities: vec![reference.clone()],
                        side_effect: entry.effect,
                        approval: entry.approval.clone(),
                        estimated_cost: None,
                        estimated_latency_ms: None,
                    },
                );
            }
            assert!(
                selector_context(&available_set, &view, true, None, &selected_refs)
                    .direct_candidates
                    .is_empty()
            );
        }
        assert!(matches!(entry.exposure, Exposure::Searchable));
        assert!(
            decision
                .issue_selected_discovery(&view, &context, "foreign query")
                .is_err()
        );
        let foreign = DirectiveContext {
            current_generation: super::super::branch::CatalogGeneration([7; 32]),
            approval_expires_at: chrono::Utc::now(),
            discovery_limit: 1,
        };
        assert!(
            decision
                .issue_selected_discovery(&view, &foreign, "prx_searchable")
                .is_err()
        );
        decision.pre_commit.outcomes.insert(
            id.clone(),
            GateOutcome::Blocked {
                gate: 2,
                reason_code: "excluded",
            },
        );
        assert!(
            decision
                .issue_selected_discovery(&view, &context, "prx_searchable")
                .is_err()
        );
        decision.pre_commit.outcomes.insert(id.clone(), GateOutcome::Passed);
        trusted.exclusions.push(Arc::from("prx_searchable"));
        decision.state.trusted = TrustedState::from_runtime(trusted);
        assert!(
            !decision
                .issue_selected_discovery(&view, &context, "prx_searchable")
                .unwrap()
                .contains(&reference)
        );
        decision.selection = Selection::Abstain {
            action: CausalActionKind::Refuse,
            reason_code: "not_selected",
        };
        assert!(
            decision
                .issue_selected_discovery(&view, &context, "prx_searchable")
                .is_err()
        );
        decision.selection = Selection::Commit {
            candidate_id: id,
            propensity: 1.0,
        };
        decision.deadline = Instant::now();
        assert!(
            decision
                .issue_selected_discovery(&view, &context, "prx_searchable")
                .is_err()
        );
    }

    #[tokio::test]
    async fn expired_turn_skips_backend_and_returns_a_typed_terminal() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.owner_id = Some(Arc::from("prx_user"));
        trusted.workspace_id = Arc::from("prx_workspace");
        trusted.catalog_generation = *catalog.generation();
        state.trusted = TrustedState::from_runtime(trusted);
        let view = catalog.view(&CatalogScope::default());
        let cost = super::super::candidate_generator::CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([(
                CapabilityTarget::None,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            )]),
        };
        let security = SecurityPolicy::default();
        let call_fingerprints = BTreeMap::new();
        let replayed = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "prx_user",
            sender: "prx_user",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: false,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &call_fingerprints,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &replayed,
            retry_limit: Some(0),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = DecisionBackendFactory::from_slots(
            vec![Box::new(DelayedBackend {
                calls: Arc::clone(&calls),
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("mock-model"),
                },
            })],
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits {
                timeout: Duration::from_secs(5),
                ..SlotLimits::default()
            },
        );
        let ports = RehearsalPorts {
            memory: None,
            catalog: None,
            skill: None,
        };
        let discovered_refs = DiscoveredRefs::empty();
        let mut inputs = EngineInputs {
            state: &state,
            catalog: &view,
            config: &CausalTreeConfig::default(),
            key: &key,
            decision_id: DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap(),
            cost_model: &cost,
            discovered_refs: &discovered_refs,
            gates: &gates,
            delegation: None,
            delegation_max_depth: 2,
            prediction: backend.prediction_port(None),
            rehearsal_ports: &ports,
            request_limits: RequestLimits::default(),
            model: Some("mock-model"),
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            terminal_only: false,
            allow_terminal_answer: false,
            deadline: Instant::now()
                .checked_sub(Duration::from_millis(1))
                .expect("current instant supports a one millisecond lookback"),
            inject_builtin_failure: false,
            after_prediction_pause: None,
        };
        let result = CausalDecisionEngine::decide_once(&inputs).await.unwrap();
        assert!(result.prediction.is_none());
        assert!(!result.request_compiled);
        assert!(result.audit_trace.is_none());
        assert_eq!(result.scored_count, 0, "expired prediction cannot claim scoring");
        assert_eq!(result.fallback_cause, Some(FallbackCause::ExpiredBeforePrediction));
        assert!(result.rehearsal.is_empty());
        assert!(matches!(result.candidates, CandidateSet::Legal { .. }));
        assert!(matches!(
            result.selection,
            Selection::Abstain {
                action: CausalActionKind::Clarify | CausalActionKind::Refuse,
                ..
            }
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "expired request must not touch the backend"
        );
        let near_start = Instant::now();
        inputs.deadline = near_start + Duration::from_secs(2);
        let near_deadline = CausalDecisionEngine::decide_once(&inputs).await.unwrap();
        assert!(
            matches!(
                near_deadline.selection,
                Selection::Abstain {
                    action: CausalActionKind::Clarify | CausalActionKind::Refuse,
                    ..
                }
            ),
            "selection={:?} fallback={:?} attempts={:?} calls={}",
            near_deadline.selection,
            near_deadline.fallback_cause,
            near_deadline.prediction.as_ref().map(|result| &result.attempts),
            calls.load(Ordering::SeqCst)
        );
        assert_eq!(near_deadline.fallback_cause, Some(FallbackCause::DeadlineExceeded));
        assert!(near_deadline.request_compiled);
        assert!(
            near_start.elapsed() < Duration::from_secs(3),
            "one deadline bounds the backend call"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "near-deadline request must attempt exactly once"
        );
        inputs.deadline = Instant::now() + Duration::from_secs(2);
        let prepared = CausalDecisionEngine::prepare_hop(&inputs).expect("same pinned preflight");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "preflight must not call a backend");
        assert_eq!(
            prepared.evidence_fingerprint(),
            near_deadline.state.evidence_fingerprint(&key),
            "the prepared hop derives the same visible candidates"
        );
        let prepared_result = CausalDecisionEngine::decide_prepared(prepared)
            .await
            .expect("prepared decision");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "consume attempts once");
        assert_eq!(prepared_result.selection, near_deadline.selection);
        assert_eq!(prepared_result.pre_commit.outcomes, near_deadline.pre_commit.outcomes);
        assert_eq!(
            prepared_result.pre_commit.gate_set_hash,
            near_deadline.pre_commit.gate_set_hash
        );
        assert_eq!(
            prepared_result.state.evidence_fingerprint(&key),
            near_deadline.state.evidence_fingerprint(&key)
        );
        let no_cost = super::super::candidate_generator::CapabilityCostModel::default();
        inputs.cost_model = &no_cost;
        inputs.deadline = Instant::now() + Duration::from_secs(1);
        let no_legal = CausalDecisionEngine::decide_once(&inputs).await.unwrap();
        assert!(matches!(no_legal.candidates, CandidateSet::NoLegalPath { .. }));
        assert_eq!(no_legal.fallback_cause, Some(FallbackCause::NoLegalPath));
        assert!(!no_legal.request_compiled);
        assert!(matches!(
            no_legal.selection,
            Selection::Abstain {
                action: CausalActionKind::Refuse,
                ..
            }
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "NoLegalPath must not touch the backend"
        );
        let builtin = DecisionBackendFactory::from_slots(
            Vec::new(),
            Arc::clone(&key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        inputs.prediction = builtin.prediction_port(None);
        inputs.cost_model = &cost;
        inputs.inject_builtin_failure = true;
        inputs.deadline = Instant::now() + Duration::from_secs(1);
        let failed_builtin = CausalDecisionEngine::decide_once(&inputs).await.unwrap();
        assert_eq!(failed_builtin.fallback_cause, Some(FallbackCause::DecisionFailed));
        assert!(failed_builtin.request_compiled);
        assert!(matches!(failed_builtin.selection, Selection::Abstain { .. }));
        assert!(failed_builtin.prediction.is_none());
        assert_eq!(failed_builtin.scored_count, 0, "builtin failure cannot claim scoring");

        inputs.inject_builtin_failure = false;
        inputs.after_prediction_pause = Some(Duration::from_secs(1));
        let late_start = Instant::now();
        inputs.deadline = late_start + Duration::from_millis(500);
        let late_valid = CausalDecisionEngine::decide_once(&inputs).await.unwrap();
        assert!(
            late_valid.prediction.is_some(),
            "builtin response was valid before the bounded pause"
        );
        assert_eq!(late_valid.fallback_cause, Some(FallbackCause::DeadlineExceeded));
        assert!(late_valid.request_compiled);
        assert_eq!(
            late_valid.scored_count, 0,
            "expired post-prediction work uses zero placeholders"
        );
        assert!(matches!(late_valid.selection, Selection::Abstain { .. }));
        assert!(
            late_start.elapsed() < Duration::from_secs(1),
            "later work shares the same deadline"
        );

        inputs.terminal_only = true;
        inputs.allow_terminal_answer = true;
        inputs.after_prediction_pause = None;
        inputs.deadline = Instant::now() + Duration::from_secs(1);
        inputs.prediction = backend.prediction_port(None);
        let before_terminal = calls.load(Ordering::SeqCst);
        let prepared = CausalDecisionEngine::prepare_hop(&inputs).expect("fresh terminal candidates");
        let terminal = CausalDecisionEngine::decide_terminal_prepared(prepared).expect("fresh terminal gate pass");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before_terminal,
            "terminal recovery is local"
        );
        assert!(terminal.prediction.is_none());
        assert_eq!(terminal.fallback_cause, Some(FallbackCause::HopStopped));
        let CandidateSet::Legal { candidates, .. } = &terminal.candidates else {
            panic!("terminal-only set must retain an admitted terminal");
        };
        assert!(candidates.iter().all(|candidate| matches!(
            candidate.action(),
            CausalActionKind::Answer | CausalActionKind::Clarify | CausalActionKind::Refuse
        )));
        let directive = directive_from_selection(
            &terminal,
            &DirectiveContext {
                current_generation: view.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
        )
        .expect("the selected terminal passed the final gate");
        assert!(matches!(directive, TurnExecutionDirective::AnswerDirectly));

        let mut empty = state.clone();
        empty.observed = super::super::state::ObservedState::from_runtime_records(Vec::new(), Vec::new(), 0);
        inputs.state = &empty;
        let prepared = CausalDecisionEngine::prepare_hop(&inputs).expect("fresh empty terminal candidates");
        let empty_terminal = CausalDecisionEngine::decide_terminal_prepared(prepared).expect("gated empty terminal");
        assert_eq!(calls.load(Ordering::SeqCst), before_terminal, "empty recovery is local");
        let directive = directive_from_selection(
            &empty_terminal,
            &DirectiveContext {
                current_generation: view.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
        )
        .expect("empty recovery is gated");
        assert!(matches!(
            directive,
            TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
        ));

        let mut stale = state.clone();
        let mut artifacts = stale.observed.artifacts().to_vec();
        artifacts[0].age_ms = crate::decision::builtin::weights::STALE_ARTIFACT_MS + 1;
        stale.observed = super::super::state::ObservedState::from_runtime_records(Vec::new(), artifacts, 0);
        inputs.state = &stale;
        let prepared = CausalDecisionEngine::prepare_hop(&inputs).expect("stale-only terminal candidates");
        let stale_terminal = CausalDecisionEngine::decide_terminal_prepared(prepared).expect("gated stale terminal");
        let directive = directive_from_selection(
            &stale_terminal,
            &DirectiveContext {
                current_generation: view.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
        )
        .expect("stale-only recovery is gated");
        assert!(matches!(
            directive,
            TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
        ));

        let mut discovered = state.clone();
        let mut artifacts = discovered.observed.artifacts().to_vec();
        artifacts[0].kind = ObservedArtifactKind::Discovery;
        discovered.observed = super::super::state::ObservedState::from_runtime_records(Vec::new(), artifacts, 0);
        inputs.state = &discovered;
        let prepared = CausalDecisionEngine::prepare_hop(&inputs).expect("discovery-only terminal candidates");
        let discovery_terminal =
            CausalDecisionEngine::decide_terminal_prepared(prepared).expect("gated discovery terminal");
        let directive = directive_from_selection(
            &discovery_terminal,
            &DirectiveContext {
                current_generation: view.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
        )
        .expect("discovery-only recovery is gated");
        assert!(matches!(
            directive,
            TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
        ));

        inputs.state = &state;
        inputs.deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("expired instant");
        let prepared = CausalDecisionEngine::prepare_hop(&inputs).expect("expired terminal candidates");
        let expired_terminal =
            CausalDecisionEngine::decide_terminal_prepared(prepared).expect("gated expired terminal");
        let directive = directive_from_selection(
            &expired_terminal,
            &DirectiveContext {
                current_generation: view.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
        )
        .expect("expired recovery is gated");
        assert!(matches!(
            directive,
            TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
        ));

        let mut blocked = state.clone();
        let mut trusted = blocked.trusted.parts().clone();
        trusted.budgets.tokens = 0;
        blocked.trusted = TrustedState::from_runtime(trusted);
        inputs.state = &blocked;
        inputs.deadline = Instant::now() + Duration::from_secs(1);
        let prepared = CausalDecisionEngine::prepare_hop(&inputs).expect("budget-blocked terminal candidates");
        let blocked_terminal = CausalDecisionEngine::decide_terminal_prepared(prepared).expect("gated budget terminal");
        let directive = directive_from_selection(
            &blocked_terminal,
            &DirectiveContext {
                current_generation: view.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
        )
        .expect("budget-blocked recovery is gated");
        assert!(matches!(
            directive,
            TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
        ));
        let CandidateSet::NoLegalPath { report, .. } = &blocked_terminal.candidates else {
            panic!("budget-blocked Answer must not remain legal");
        };
        assert!(matches!(
            report.outcomes.get(&CandidateId::new(
                CausalActionKind::Answer,
                CapabilityTarget::None,
                None
            )),
            Some(GateOutcome::Blocked { gate: 6, .. })
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before_terminal,
            "all recovery cases stay local"
        );
    }
}
