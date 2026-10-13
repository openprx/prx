pub(crate) mod model_admission;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU8;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};

use uuid::Uuid;

use super::branch::{ApprovalRequirement, CandidateId, CatalogGeneration, CausalCandidate, OpId};

mod canonical;
pub(crate) mod exact_calls;

#[cfg(test)]
mod canonical_tests;
use crate::decision::builtin::weights::{FRESH_ARTIFACT_MS, STALE_ARTIFACT_MS};
use crate::decision::{DecisionAnswer, DecisionKey, KeyDomain, QuestionId};
#[cfg(test)]
use crate::decision::{DecisionRequest, ValidatedDecision};

/// The side-effect mode governing what the CTE pipeline is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum SideEffectMode {
    /// No writes, no tool execution — pure analysis.
    ReadOnly,
    /// Writes require explicit user approval.
    ApprovalRequired,
    /// Writes allowed under policy guard (first-version default is ReadOnly).
    GuardedWrite,
}

impl Default for SideEffectMode {
    fn default() -> Self {
        Self::ReadOnly
    }
}

// ---------------------------------------------------------------------------
// Trusted, observed, predicted and derived turn state (CDP 5.2 and 5.3;
// CDP-PLAN 1.9). This source-owned state replaces the legacy history snapshot.
//
// Nothing here prints private text: every type that holds user, memory,
// catalog or identity text has a `Debug` that shows lengths and counts only
// (D-057 M-5; R-CDP16 M-4).
// ---------------------------------------------------------------------------

/// The surface a turn arrives on: CDP-PLAN 1.12 `SurfaceClass`, one to one
/// (CDP-21 builds it from the turn's origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TurnSurfaceKind {
    Interactive,
    AgentLoop,
    Cron,
    Xin,
    Server,
    Channel,
    Worker,
}

impl TurnSurfaceKind {
    /// The stable code.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::AgentLoop => "agent_loop",
            Self::Cron => "cron",
            Self::Xin => "xin",
            Self::Server => "server",
            Self::Channel => "channel",
            Self::Worker => "worker",
        }
    }
}

/// A constraint code that may leave PRX in the redacted state: a fixed PRX
/// code, never a tool, capability or user name (so no hidden catalog entry
/// can leak through it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ConstraintCode(&'static str);

impl ConstraintCode {
    /// A constraint named by a static code.
    pub(crate) const fn new(code: &'static str) -> Self {
        Self(code)
    }

    /// The code.
    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Remaining budgets of the turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TurnBudgets {
    pub(crate) tokens: u64,
    pub(crate) latency_ms: u64,
    pub(crate) micro_usd: u64,
}

/// What the turn may deliver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DeliveryConstraints {
    pub(crate) interactive: bool,
    pub(crate) can_ask_question: bool,
    pub(crate) approval_channel: bool,
}

/// Every trusted input of a turn: produced by authenticated runtime code,
/// never by a decision backend (CDP 5.2).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TrustedParts {
    pub(crate) workspace_id: Arc<str>,
    pub(crate) owner_id: Option<Arc<str>>,
    pub(crate) session_key: Option<Arc<str>>,
    pub(crate) run_id: Option<Arc<str>>,
    pub(crate) task_id: Option<Arc<str>>,
    pub(crate) channel: Option<Arc<str>>,
    pub(crate) source_event_id: Option<Arc<str>>,
    pub(crate) causation_event_id: Option<Arc<str>>,
    pub(crate) surface: TurnSurfaceKind,
    pub(crate) side_effect_mode: SideEffectMode,
    /// Explicit excluded capability names (global, channel, child manifest).
    /// Exact-model policy has a separate compact commitment below.
    pub(crate) exclusions: Vec<Arc<str>>,
    pub(crate) model_admission: Option<Arc<dyn crate::causal_tree::state::model_admission::ModelAdmissionView>>,
    /// Explicit structural user restrictions, as codes.
    pub(crate) constraints: Vec<ConstraintCode>,
    pub(crate) approval_pending: bool,
    pub(crate) approval_grants: Vec<Arc<str>>,
    pub(crate) budgets: TurnBudgets,
    pub(crate) delivery: DeliveryConstraints,
    pub(crate) catalog_generation: CatalogGeneration,
    pub(crate) policy_version: Arc<str>,
    /// The PMM-resolved model reference of the turn.
    pub(crate) resolved_model: Arc<str>,
}

impl TrustedParts {
    pub(crate) fn owner_isolated(&self) -> bool {
        self.constraints.iter().any(|code| code.as_str() == "owner_isolated")
    }
    pub(crate) fn excludes_entry(
        &self,
        entry: crate::causal_tree::state::model_admission::ModelAdmissionEntry<'_>,
    ) -> bool {
        self.exclusions
            .iter()
            .any(|name| name.as_ref() == entry.public_name || name.as_ref() == entry.root)
            || self
                .model_admission
                .as_ref()
                .is_some_and(|policy| policy.excludes(entry))
    }
}

impl fmt::Debug for TrustedParts {
    /// Identities, exclusions and grants print as presence or counts only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |value: &Option<Arc<str>>| value.is_some();
        f.debug_struct("TrustedParts")
            .field("owner_id_set", &set(&self.owner_id))
            .field("session_key_set", &set(&self.session_key))
            .field("run_id_set", &set(&self.run_id))
            .field("task_id_set", &set(&self.task_id))
            .field("channel_set", &set(&self.channel))
            .field("surface", &self.surface)
            .field("side_effect_mode", &self.side_effect_mode)
            .field("exclusions", &self.exclusions.len())
            .field("constraints", &self.constraints)
            .field("approval_pending", &self.approval_pending)
            .field("approval_grants", &self.approval_grants.len())
            .field("budgets", &self.budgets)
            .field("delivery", &self.delivery)
            .finish_non_exhaustive()
    }
}

/// The trusted part of a turn state. Its fields are private and it has no
/// setter; the only constructor is [`TrustedState::from_runtime`], whose
/// single production caller CDP-21's guard AG-28 pins to the turn decision
/// service. No backend output type converts into it (V3.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustedState(TrustedParts, Option<Arc<RouterHistoryReadBinding>>);

/// Trusted runtime purpose, included in canonical state only for the routing
/// phase. It grants no access until the corresponding candidate is selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RouterHistoryReadBinding {
    pub(crate) domain_sha256: [u8; 32],
    pub(crate) query_sha256: [u8; 32],
    pub(crate) version: u8,
}

impl TrustedState {
    /// The trusted state of a turn, from runtime-authenticated parts.
    pub(crate) const fn from_runtime(parts: TrustedParts) -> Self {
        Self(parts, None)
    }

    #[cfg(feature = "llm-router")]
    pub(crate) fn with_router_history(mut self, purpose: RouterHistoryReadBinding) -> Self {
        self.1 = Some(Arc::new(purpose));
        self
    }

    pub(crate) fn router_history(&self) -> Option<&RouterHistoryReadBinding> {
        self.1.as_deref()
    }

    /// The trusted parts, read-only.
    pub(crate) const fn parts(&self) -> &TrustedParts {
        &self.0
    }
}

/// The kind of a completed step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ObservedStepKind {
    Tool,
    Provider,
    Retrieve,
    Discover,
    /// Router training-history work is audited separately and is not query
    /// evidence for a user answer or an ordinary Memory Retrieve candidate.
    RouterHistory,
}

impl ObservedStepKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Provider => "provider",
            Self::Retrieve => "retrieve",
            Self::Discover => "discover",
            Self::RouterHistory => "router_history",
        }
    }
}

/// The terminal status of a completed step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ObservedStepStatus {
    Ok,
    Failed,
    Denied,
    Cancelled,
}

impl ObservedStepStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One completed step, as the runtime recorded its execution: the kind, the
/// terminal status, the candidate it executed (when it executed one) and the
/// event that recorded it.
///
/// It is trust-bearing: a `Failed` step that names a candidate marks that
/// candidate `failed_before`. So its fields are private, its only
/// constructor is [`ObservedStep::recorded_by_runtime`], and AG-28 pins that
/// constructor's production callers to the turn decision service (D-066
/// N-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedStep {
    kind: ObservedStepKind,
    status: ObservedStepStatus,
    candidate: Option<CandidateId>,
    event_id: Option<Arc<str>>,
}

impl ObservedStep {
    /// A completed step as the runtime recorded it.
    pub(crate) const fn recorded_by_runtime(
        kind: ObservedStepKind,
        status: ObservedStepStatus,
        candidate: Option<CandidateId>,
        event_id: Option<Arc<str>>,
    ) -> Self {
        Self {
            kind,
            status,
            candidate,
            event_id,
        }
    }

    /// The kind of step.
    pub(crate) const fn kind(&self) -> ObservedStepKind {
        self.kind
    }

    /// Its terminal status.
    pub(crate) const fn status(&self) -> ObservedStepStatus {
        self.status
    }

    /// The candidate it executed, if it executed one.
    pub(crate) const fn candidate(&self) -> Option<&CandidateId> {
        self.candidate.as_ref()
    }

    /// The event that recorded it.
    pub(crate) const fn event_id(&self) -> &Option<Arc<str>> {
        &self.event_id
    }
}

/// The kind of an artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ObservedArtifactKind {
    Memory,
    Document,
    Session,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Canonical artifact kind; live discovery metadata must not become answer evidence"
        )
    )]
    Discovery,
}

impl ObservedArtifactKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Document => "document",
            Self::Session => "session",
            Self::Discovery => "discovery",
        }
    }
}

/// A bounded artifact summary with its provenance.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ObservedArtifact {
    pub(crate) kind: ObservedArtifactKind,
    pub(crate) age_ms: u64,
    /// Runtime summaries are redacted and bounded to 256 characters before
    /// entering the state; projection rechecks both properties defensively.
    pub(crate) summary: String,
    /// Preserve projection accounting for redaction performed before storage.
    pub(crate) summary_was_redacted: bool,
    /// Preserve projection accounting for a pre-storage summary cut.
    pub(crate) summary_was_truncated: bool,
    pub(crate) source_event_id: Option<Arc<str>>,
}

impl fmt::Debug for ObservedArtifact {
    /// The summary may contain private source text: only its length prints.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservedArtifact")
            .field("kind", &self.kind)
            .field("age_ms", &self.age_ms)
            .field("summary_len", &self.summary.len())
            .field("summary_was_redacted", &self.summary_was_redacted)
            .field("summary_was_truncated", &self.summary_was_truncated)
            .field("source_event_id", &self.source_event_id)
            .finish()
    }
}

/// How old an artifact is, in the classes a decision distinguishes: the
/// builtin's fresh and stale thresholds (`weights::FRESH_ARTIFACT_MS`,
/// `weights::STALE_ARTIFACT_MS`). The material fingerprint holds the class,
/// not the exact age, so a state that only grew older within a class is not
/// materially different (R-CDP16 L-4).
pub(crate) const fn artifact_age_class(age_ms: u64) -> &'static str {
    if age_ms < FRESH_ARTIFACT_MS {
        "fresh"
    } else if age_ms > STALE_ARTIFACT_MS {
        "stale"
    } else {
        "aging"
    }
}

/// What completed executions observed (CDP 5.2).
///
/// Built only by [`ObservedState::from_runtime_records`] from the runtime's records of
/// completed steps, and read-only afterwards. A failure is a completed step
/// whose status is `Failed` and which names the candidate it executed, so
/// [`ObservedState::failed_candidates`] can only come from such a record,
/// never from a backend answer (R-CDP16 L-2). CDP-21 records the steps;
/// its guard AG-28 pins the constructor's production callers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ObservedState {
    completed_steps: Vec<ObservedStep>,
    artifacts: Vec<ObservedArtifact>,
    retry_count: u32,
    exact_call_observations: Option<Arc<exact_calls::ExactCallObservations>>,
}

impl ObservedState {
    /// The observations of a turn so far.
    pub(crate) const fn from_runtime_records(
        completed_steps: Vec<ObservedStep>,
        artifacts: Vec<ObservedArtifact>,
        retry_count: u32,
    ) -> Self {
        Self {
            completed_steps,
            artifacts,
            retry_count,
            exact_call_observations: None,
        }
    }

    pub(crate) fn with_exact_call_observations(
        mut self,
        facts: Option<Arc<exact_calls::ExactCallObservations>>,
    ) -> Self {
        self.exact_call_observations = facts;
        self
    }

    pub(crate) const fn exact_call_observations(&self) -> Option<&Arc<exact_calls::ExactCallObservations>> {
        self.exact_call_observations.as_ref()
    }

    /// The completed steps, in order.
    pub(crate) fn completed_steps(&self) -> &[ObservedStep] {
        &self.completed_steps
    }

    /// The artifacts, in order.
    pub(crate) fn artifacts(&self) -> &[ObservedArtifact] {
        &self.artifacts
    }

    /// Retries so far.
    pub(crate) const fn retry_count(&self) -> u32 {
        self.retry_count
    }

    /// The candidates whose execution failed earlier in the turn: the
    /// candidates of the completed steps with status `Failed`, deduplicated
    /// and in id order. The projection marks exactly these candidates
    /// `failed_before` (D-061 M-3).
    pub(crate) fn failed_candidates(&self) -> BTreeSet<&CandidateId> {
        self.completed_steps
            .iter()
            .filter(|step| step.kind != ObservedStepKind::RouterHistory && step.status == ObservedStepStatus::Failed)
            .filter_map(ObservedStep::candidate)
            .collect()
    }
}

/// Validated predictions of one decision (CDP 5.2). Built only from a
/// [`crate::decision::ValidatedDecision`] that answers the given request; nothing maps a
/// predicted value into the trusted or observed state, and the state
/// fingerprints never read it (V3.14). CDP-21 stores it on the hop's state
/// and CDP-22's `DecisionRecordBuilder` reads it into the decision record.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PredictedState {
    request_id: Arc<str>,
    answers: BTreeMap<QuestionId, DecisionAnswer>,
}

/// A validated decision offered for another request (R-CDP16 L-8).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the decision answers another request")]
pub(crate) struct ForeignDecision;

#[cfg(test)]
impl PredictedState {
    /// The answers of `decision` to the questions of `request`.
    ///
    /// # Errors
    /// [`ForeignDecision`] when `decision` answers another request.
    pub(crate) fn from_validated(
        decision: &ValidatedDecision,
        request: &DecisionRequest,
    ) -> Result<Self, ForeignDecision> {
        if decision.request_id() != request.request_id() {
            return Err(ForeignDecision);
        }
        let answers = request
            .questions()
            .keys()
            .filter_map(|id| decision.answer(id).map(|answer| (id.clone(), answer.clone())))
            .collect();
        Ok(Self {
            request_id: Arc::from(decision.request_id()),
            answers,
        })
    }

    /// The id of the request the predictions answer.
    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    /// The prediction for question `id`.
    pub(crate) fn answer(&self, id: &QuestionId) -> Option<&DecisionAnswer> {
        self.answers.get(id)
    }
}

/// One candidate with the catalog facts the projection sends: the bounded
/// family summary, whether the user named it exactly, and availability.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DerivedCandidate {
    pub(crate) candidate: CausalCandidate,
    /// The catalog's compact summary (sanitized, at most 160 characters in
    /// the projection).
    pub(crate) summary: Arc<str>,
    pub(crate) exact_mention: bool,
    pub(crate) available: bool,
}

impl fmt::Debug for DerivedCandidate {
    /// The catalog summary prints as its length only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DerivedCandidate")
            .field("candidate", &self.candidate)
            .field("summary_len", &self.summary.len())
            .field("exact_mention", &self.exact_mention)
            .field("available", &self.available)
            .finish()
    }
}

/// Two derived candidates share one id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("candidate {} is listed twice", .0.as_str())]
pub(crate) struct DuplicateCandidate(pub(crate) CandidateId);

/// Values computed deterministically from trusted, observed and validated
/// predicted inputs (CDP 5.2). CDP-18 and CDP-20 add gate and score results.
///
/// Its candidates have distinct ids: [`DerivedState::new`] refuses a repeat,
/// so neither the fingerprint nor the projection depends on which of two
/// entries comes first (R-CDP16 L-4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DerivedState {
    candidates: Vec<DerivedCandidate>,
}

impl DerivedState {
    /// The derived state of these candidates.
    ///
    /// # Errors
    /// [`DuplicateCandidate`] when two candidates share an id.
    pub(crate) fn new(candidates: Vec<DerivedCandidate>) -> Result<Self, DuplicateCandidate> {
        let mut seen = BTreeSet::new();
        for derived in &candidates {
            if !seen.insert(derived.candidate.candidate_id()) {
                return Err(DuplicateCandidate(derived.candidate.candidate_id().clone()));
            }
        }
        Ok(Self { candidates })
    }

    /// The candidates, in the order given.
    pub(crate) fn candidates(&self) -> &[DerivedCandidate] {
        &self.candidates
    }
}

/// The exact text of a turn before memory enrichment, tool refresh or prompt
/// compilation. It stays inside PRX: it is not `Serialize`, its `Debug`
/// shows only the length and a hash prefix, and only the redacted projection
/// of it ever leaves (CDP 5.3).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RawRoutingInput(String);

impl RawRoutingInput {
    /// Routing text remains valid above this bound. Selected Retrieve and
    /// Discover require the exact UTF-8 source query to fit the local read port.
    pub(crate) const fn supports_selected_read_query(&self) -> bool {
        self.0.len() <= 1_024
    }

    /// Wrap the raw text.
    pub(crate) const fn new(text: String) -> Self {
        Self(text)
    }

    /// The raw text, for code inside PRX.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RawRoutingInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digest = Sha256::digest(self.0.as_bytes());
        write!(
            f,
            "RawRoutingInput(len={}, sha256={})",
            self.0.len(),
            hex::encode(digest.get(..4).unwrap_or_default())
        )
    }
}

/// Where a decision hop sits in its turn. The first hop has no prior
/// decision; every later hop has a hop number of at least 1 and names the
/// decision before it. One type, so the two can never disagree (the state
/// v1 rule "null exactly on hop 0"; R-CDP16 M-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HopLineage {
    /// Hop 0.
    First,
    /// A later hop.
    After { hop: NonZeroU8, prior_decision_id: Uuid },
}

impl HopLineage {
    /// The hop number, 0 for the first hop.
    pub(crate) const fn hop(self) -> u8 {
        match self {
            Self::First => 0,
            Self::After { hop, .. } => hop.get(),
        }
    }

    /// The previous decision, `None` on the first hop.
    pub(crate) const fn prior_decision_id(self) -> Option<Uuid> {
        match self {
            Self::First => None,
            Self::After { prior_decision_id, .. } => Some(prior_decision_id),
        }
    }
}

/// The causal state of one decision hop (CDP 5.3). Its `Debug` prints no
/// private text: every part redacts itself.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CausalState {
    pub(crate) trusted: TrustedState,
    pub(crate) observed: ObservedState,
    pub(crate) predicted: Option<PredictedState>,
    pub(crate) derived: DerivedState,
    /// Whether the admitted source belongs to a conversation with an earlier turn.
    pub(crate) has_conversation_history: bool,
    /// The latest user turn, exactly as received.
    pub(crate) routing_input: RawRoutingInput,
    /// The current goal.
    pub(crate) goal: RawRoutingInput,
    /// The hop number and the prior decision: lineage, so not material.
    pub(crate) lineage: HopLineage,
    /// When the state was built (ms since the epoch); a timestamp, so not
    /// material.
    pub(crate) built_at_ms: u64,
}

impl CausalState {
    /// The keyed fingerprint of everything that decides the turn: trusted,
    /// observed and derived inputs and the raw texts. Lineage and time are
    /// not material: the run and task ids, the source, causation, step and
    /// artifact event ids, the build time, the exact artifact ages (only
    /// their [`artifact_age_class`] counts), the hop number and the prior
    /// decision id (R-CDP16 M-1). It never reads predicted values. Every
    /// executed hop adds its own completed step, which is material, so this
    /// fingerprint cannot tell that a hop made no progress; the hop loop
    /// uses [`CausalState::evidence_fingerprint`] for that (D-066 N-1).
    /// Lowercase hex of
    /// `HMAC(DecisionKey, StateFingerprint, "material" ‖ canonical JSON)`.
    pub(crate) fn material_fingerprint(&self, key: &DecisionKey) -> String {
        hex::encode(key.hmac(
            KeyDomain::StateFingerprint,
            &[b"material", self.canonical(false).as_bytes()],
        ))
    }

    /// The keyed fingerprint of the evidence the turn has observed (D-066
    /// N-1 and its amendment): the set of artifact contents (kind and
    /// summary), the set of completed steps that did not succeed (kind,
    /// status and candidate), and the set of candidate ids the turn can
    /// choose from, so a hop that discovers a new candidate is progress even
    /// when it records no artifact. A successful retrieve or discover step
    /// adds nothing of its own; its evidence is the artifacts and
    /// candidates it returned. CDP-21's hop loop compares
    /// this fingerprint before and after a Retrieve or Discover hop and
    /// stops when it is unchanged: a hop that found nothing new made no
    /// progress. Sets, so the order and repeats of entries do not matter.
    /// Lowercase hex of
    /// `HMAC(DecisionKey, StateFingerprint, "evidence" ‖ canonical JSON)`.
    pub(crate) fn evidence_fingerprint(&self, key: &DecisionKey) -> String {
        let artifacts: BTreeSet<String> = self
            .observed
            .artifacts()
            .iter()
            .map(|artifact| json!([artifact.kind.as_str(), artifact.summary]).to_string())
            .collect();
        let unsuccessful: BTreeSet<String> = self
            .observed
            .completed_steps()
            .iter()
            .filter(|step| step.kind() != ObservedStepKind::RouterHistory && step.status() != ObservedStepStatus::Ok)
            .map(|step| {
                json!([
                    step.kind().as_str(),
                    step.status().as_str(),
                    step.candidate().map(CandidateId::as_str),
                ])
                .to_string()
            })
            .collect();
        let candidates: BTreeSet<&str> = self
            .derived
            .candidates()
            .iter()
            .map(|derived| derived.candidate.candidate_id().as_str())
            .collect();
        let mut canonical = json!({
            "artifacts": artifacts,
            "unsuccessful_steps": unsuccessful,
            "candidates": candidates,
        });
        if let Some(facts) = self.observed.exact_call_observations()
            && let serde_json::Value::Object(fields) = &mut canonical
        {
            fields.insert("exact_call_observations".to_owned(), json!(facts.as_ref()));
        }
        let canonical = canonical.to_string();
        hex::encode(key.hmac(KeyDomain::StateFingerprint, &[b"evidence", canonical.as_bytes()]))
    }

    /// The keyed fingerprint of the full record: the material inputs plus
    /// the lineage, the event ids and the times.
    #[cfg(test)]
    pub(crate) fn record_fingerprint(&self, key: &DecisionKey) -> String {
        hex::encode(key.hmac(
            KeyDomain::StateFingerprint,
            &[b"record", self.canonical(true).as_bytes()],
        ))
    }

    /// Canonical JSON of the fingerprinted inputs: object keys in a fixed
    /// order, sets (exclusions, constraints, grants, approval op ids,
    /// candidates) deduplicated and sorted so neither their listing order
    /// nor a repeat matters, sequences (steps, artifacts) in order, no
    /// floats.
    #[allow(clippy::expect_used)] // Fixed primitive fields serialize infallibly into an in-memory String.
    fn canonical(&self, record: bool) -> String {
        canonical::serialize(self, record).expect("canonical primitive serialization invariant")
    }

    /// The derived candidates ordered by candidate id (ids are distinct).
    fn derived_sorted(&self) -> impl Iterator<Item = &DerivedCandidate> {
        let mut candidates: Vec<&DerivedCandidate> = self.derived.candidates().iter().collect();
        candidates.sort_by(|left, right| left.candidate.candidate_id().cmp(right.candidate.candidate_id()));
        candidates.into_iter()
    }
}

/// A complete turn state for the domain and projection tests.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod fixtures {
    use super::super::branch::{
        ApprovalRequirement, CandidateSpec, CapabilityTarget, CausalActionKind, CostEstimate, Ns8, SideEffectClass,
    };
    use super::*;

    pub(crate) fn key() -> Arc<DecisionKey> {
        let dir = tempfile::tempdir().expect("test: tempdir");
        DecisionKey::load_or_create(dir.path()).expect("test: key")
    }

    pub(crate) fn trusted() -> TrustedParts {
        TrustedParts {
            workspace_id: Arc::from("ws-1"),
            owner_id: Some(Arc::from("owner-1")),
            session_key: Some(Arc::from("session-1")),
            run_id: Some(Arc::from("run-1")),
            task_id: None,
            channel: Some(Arc::from("cli")),
            source_event_id: Some(Arc::from("event-1")),
            causation_event_id: Some(Arc::from("event-0")),
            surface: TurnSurfaceKind::Interactive,
            side_effect_mode: SideEffectMode::ReadOnly,
            exclusions: vec![Arc::from("shell"), Arc::from("browser")],
            model_admission: None,
            constraints: vec![
                ConstraintCode::new("no_destructive_filesystem"),
                ConstraintCode::new("external_send_requires_approval"),
            ],
            approval_pending: false,
            approval_grants: Vec::new(),
            budgets: TurnBudgets {
                tokens: 8_000,
                latency_ms: 1_500,
                micro_usd: 5_000,
            },
            delivery: DeliveryConstraints {
                interactive: true,
                can_ask_question: true,
                approval_channel: true,
            },
            catalog_generation: CatalogGeneration([7; 32]),
            policy_version: Arc::from("policy-1"),
            resolved_model: Arc::from("anthropic:claude"),
        }
    }

    pub(crate) fn candidate(
        key: &DecisionKey,
        action: CausalActionKind,
        target: CapabilityTarget,
        namespace: Option<&str>,
    ) -> DerivedCandidate {
        DerivedCandidate {
            candidate: CausalCandidate::new(
                action,
                target,
                CandidateSpec {
                    namespace: namespace.map(|name| Ns8::of(key, name)),
                    required_capabilities: Vec::new(),
                    side_effect: SideEffectClass::ReadOnly,
                    approval: ApprovalRequirement::None,
                    estimated_cost: Some(CostEstimate {
                        tokens: 100,
                        micro_usd: 50,
                    }),
                    estimated_latency_ms: Some(200),
                },
            ),
            summary: Arc::from("A capability family."),
            exact_mention: false,
            available: true,
        }
    }

    pub(crate) fn candidates(key: &DecisionKey) -> Vec<DerivedCandidate> {
        vec![
            candidate(key, CausalActionKind::Answer, CapabilityTarget::None, None),
            candidate(key, CausalActionKind::Retrieve, CapabilityTarget::Memory, None),
            candidate(key, CausalActionKind::UseTool, CapabilityTarget::Mcp, Some("github")),
        ]
    }

    pub(crate) fn derived(candidates: Vec<DerivedCandidate>) -> DerivedState {
        DerivedState::new(candidates).expect("test: distinct candidates")
    }

    pub(crate) fn step(kind: ObservedStepKind, status: ObservedStepStatus) -> ObservedStep {
        ObservedStep::recorded_by_runtime(kind, status, None, Some(Arc::from("event-step-1")))
    }

    /// A step that executed `candidate`.
    pub(crate) fn executed(status: ObservedStepStatus, candidate: &CandidateId) -> ObservedStep {
        ObservedStep::recorded_by_runtime(
            ObservedStepKind::Tool,
            status,
            Some(candidate.clone()),
            Some(Arc::from("event-step-1")),
        )
    }

    pub(crate) fn artifact(summary: &str, age_ms: u64) -> ObservedArtifact {
        ObservedArtifact {
            kind: ObservedArtifactKind::Memory,
            age_ms,
            summary: summary.to_string(),
            summary_was_redacted: false,
            summary_was_truncated: false,
            source_event_id: Some(Arc::from("event-artifact-1")),
        }
    }

    pub(crate) fn observed() -> ObservedState {
        ObservedState::from_runtime_records(
            vec![step(ObservedStepKind::Retrieve, ObservedStepStatus::Ok)],
            vec![artifact("Two notes about the release plan.", 1_000)],
            0,
        )
    }

    pub(crate) fn state(key: &DecisionKey, turn: &str) -> CausalState {
        CausalState {
            trusted: TrustedState::from_runtime(trusted()),
            observed: observed(),
            predicted: None,
            derived: derived(candidates(key)),
            has_conversation_history: false,
            routing_input: RawRoutingInput::new(turn.to_string()),
            goal: RawRoutingInput::new("Answer the question.".to_string()),
            lineage: HopLineage::First,
            built_at_ms: 1_000,
        }
    }

    /// The hop after `state`, with `prior` as the previous decision.
    pub(crate) fn next_hop(state: &CausalState, prior: Uuid) -> HopLineage {
        let hop = NonZeroU8::new(state.lineage.hop().saturating_add(1)).unwrap_or(NonZeroU8::MIN);
        HopLineage::After {
            hop,
            prior_decision_id: prior,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod turn_state_tests {
    use std::time::{Duration, Instant};

    use super::super::branch::{CapabilityTarget, CausalActionKind};
    use super::fixtures::{artifact, candidate, candidates, derived, executed, key, next_hop, observed, state, step};
    use super::*;
    use crate::decision::{
        BackendIdentity, BackendKind, ChoiceAnswer, DecisionQuestion, DecisionResponse, DecisionResponseValidator,
        DecisionUsage, GlobalQuestion, ModelVersionPolicy, OptionId, Probability, RequestLimits, RequestParts,
        StructuredValue, ValidationContext,
    };

    fn with_trusted(state: &CausalState, edit: impl FnOnce(&mut TrustedParts)) -> CausalState {
        let mut parts = state.trusted.parts().clone();
        edit(&mut parts);
        let mut changed = state.clone();
        changed.trusted = TrustedState::from_runtime(parts);
        changed
    }

    /// R-CDP16 M-1: lineage and time are not material. A state that differs
    /// only in the hop, the prior decision, the run and task ids, the event
    /// ids, the build time and the exact artifact age (within its class) has
    /// the same material fingerprint and a different record fingerprint.
    #[test]
    fn lineage_and_time_are_not_material() {
        let key = key();
        let base = state(&key, "hello");
        let material = base.material_fingerprint(&key);
        let record = base.record_fingerprint(&key);
        assert_eq!(material.len(), 64);
        assert_ne!(material, record);

        let mut later = with_trusted(&base, |parts| {
            parts.run_id = Some(Arc::from("run-2"));
            parts.task_id = Some(Arc::from("task-9"));
            parts.source_event_id = Some(Arc::from("event-9"));
            parts.causation_event_id = Some(Arc::from("event-8"));
        });
        later.lineage = next_hop(&base, Uuid::from_u128(7));
        later.built_at_ms = 99_000;
        let mut aged = artifact("Two notes about the release plan.", 2_000);
        aged.source_event_id = Some(Arc::from("event-artifact-9"));
        let renamed = ObservedStep::recorded_by_runtime(
            ObservedStepKind::Retrieve,
            ObservedStepStatus::Ok,
            None,
            Some(Arc::from("event-step-9")),
        );
        later.observed = ObservedState::from_runtime_records(vec![renamed], vec![aged], 0);
        assert_eq!(later.material_fingerprint(&key), material, "not material");
        assert_ne!(later.record_fingerprint(&key), record, "but recorded");
    }

    /// D-066 N-1, the hop loop simulated: decide Retrieve, execute it, then
    /// compare the evidence fingerprint. A retrieval that returned nothing
    /// adds a successful step but no evidence, so the loop stops, even
    /// though the material fingerprint changed; a retrieval that returned
    /// an artifact, a failed retrieval and a discovery that found something
    /// are progress.
    #[test]
    fn the_hop_loop_stops_when_a_hop_found_no_new_evidence() {
        let key = key();
        // The first hop has done nothing yet: no step, one artifact from the
        // session.
        let mut before = state(&key, "what changed in the release?");
        before.observed = ObservedState::from_runtime_records(Vec::new(), before.observed.artifacts().to_vec(), 0);
        let before = before;
        let hop = |status, found: Option<(ObservedArtifactKind, &str)>, kind| {
            let mut after = before.clone();
            after.lineage = next_hop(&before, Uuid::from_u128(9));
            let mut steps = before.observed.completed_steps().to_vec();
            steps.push(ObservedStep::recorded_by_runtime(
                kind,
                status,
                None,
                Some(Arc::from("event-hop")),
            ));
            let mut artifacts = before.observed.artifacts().to_vec();
            if let Some((kind, summary)) = found {
                let mut found = artifact(summary, 10);
                found.kind = kind;
                artifacts.push(found);
            }
            after.observed = ObservedState::from_runtime_records(steps, artifacts, before.observed.retry_count());
            after
        };
        let evidence = before.evidence_fingerprint(&key);

        // D-066 amendment: a Discover hop that only adds a candidate is
        // progress too.
        let mut discovered = hop(ObservedStepStatus::Ok, None, ObservedStepKind::Discover);
        let mut list = before.derived.candidates().to_vec();
        list.push(candidate(
            &key,
            CausalActionKind::UseTool,
            CapabilityTarget::Web,
            Some("search"),
        ));
        discovered.derived = derived(list);
        assert_ne!(
            discovered.evidence_fingerprint(&key),
            evidence,
            "a discovered candidate is new evidence"
        );

        let empty = hop(ObservedStepStatus::Ok, None, ObservedStepKind::Retrieve);
        assert_eq!(empty.evidence_fingerprint(&key), evidence, "no new evidence: stop");
        assert_ne!(
            empty.material_fingerprint(&key),
            before.material_fingerprint(&key),
            "the step itself is material, which is why the loop does not use it"
        );

        for progress in [
            hop(
                ObservedStepStatus::Ok,
                Some((ObservedArtifactKind::Memory, "The release moved to Friday.")),
                ObservedStepKind::Retrieve,
            ),
            hop(ObservedStepStatus::Failed, None, ObservedStepKind::Retrieve),
            hop(
                ObservedStepStatus::Ok,
                Some((ObservedArtifactKind::Discovery, "A calendar skill.")),
                ObservedStepKind::Discover,
            ),
        ] {
            assert_ne!(progress.evidence_fingerprint(&key), evidence, "{progress:?}");
        }

        let mut repeated = before.clone();
        let mut artifacts = before.observed.artifacts().to_vec();
        artifacts.extend(before.observed.artifacts().iter().cloned());
        artifacts.reverse();
        repeated.observed =
            ObservedState::from_runtime_records(before.observed.completed_steps().to_vec(), artifacts, 0);
        assert_eq!(
            repeated.evidence_fingerprint(&key),
            evidence,
            "a set: order and repeats do not matter"
        );
        assert_ne!(before.evidence_fingerprint(&super::fixtures::key()), evidence, "keyed");
    }

    /// New evidence, a new failure, a new catalog generation, a new approval
    /// grant, a new routing input or an artifact crossing an age class is
    /// material.
    #[test]
    fn new_inputs_are_material() {
        let key = key();
        let base = state(&key, "hello");
        let material = base.material_fingerprint(&key);
        let github = candidates(&key)[2].candidate.candidate_id().clone();
        let failed = executed(ObservedStepStatus::Failed, &github);
        let changes: Vec<CausalState> = vec![
            {
                let mut changed = base.clone();
                changed.observed = ObservedState::from_runtime_records(
                    base.observed.completed_steps().to_vec(),
                    vec![
                        artifact("Two notes about the release plan.", 1_000),
                        artifact("A third note.", 1_000),
                    ],
                    0,
                );
                changed
            },
            {
                let mut changed = base.clone();
                changed.observed =
                    ObservedState::from_runtime_records(vec![failed], base.observed.artifacts().to_vec(), 0);
                changed
            },
            {
                let mut changed = base.clone();
                changed.observed = ObservedState::from_runtime_records(
                    base.observed.completed_steps().to_vec(),
                    vec![artifact("Two notes about the release plan.", 2 * 60 * 60 * 1000)],
                    0,
                );
                changed
            },
            with_trusted(&base, |parts| parts.catalog_generation = CatalogGeneration([8; 32])),
            with_trusted(&base, |parts| parts.approval_grants = vec![Arc::from("grant-1")]),
            {
                let mut changed = base.clone();
                changed.routing_input = RawRoutingInput::new("hello!".to_string());
                changed
            },
            {
                let mut changed = base.clone();
                changed.observed = ObservedState::from_runtime_records(
                    base.observed.completed_steps().to_vec(),
                    base.observed.artifacts().to_vec(),
                    1,
                );
                changed
            },
            {
                let mut changed = base.clone();
                let mut list = candidates(&key);
                list[0].exact_mention = true;
                changed.derived = derived(list);
                changed
            },
        ];
        for (index, changed) in changes.iter().enumerate() {
            assert_ne!(changed.material_fingerprint(&key), material, "change {index}");
        }
        let other_key = super::fixtures::key();
        assert_ne!(base.material_fingerprint(&other_key), material, "keyed per workspace");
    }

    /// I-01.4 and R-CDP16 L-4: neither the listing order of a set nor a
    /// repeated entry changes the fingerprint.
    #[test]
    fn the_listing_order_and_repeats_of_sets_do_not_matter() {
        let key = key();
        let base = state(&key, "hello");
        let mut reversed = candidates(&key);
        reversed.reverse();
        let mut shuffled = with_trusted(&base, |parts| {
            parts.exclusions.reverse();
            parts.exclusions.push(Arc::from("shell"));
            parts.constraints.reverse();
            parts.constraints.push(ConstraintCode::new("no_destructive_filesystem"));
        });
        shuffled.derived = derived(reversed);
        assert_eq!(shuffled.material_fingerprint(&key), base.material_fingerprint(&key));
        assert_eq!(shuffled.record_fingerprint(&key), base.record_fingerprint(&key));
    }

    /// R-CDP16 L-2: a failure comes only from a completed step with status
    /// `Failed` that names its candidate; other statuses and steps without a
    /// candidate mark nothing, and a failure is material.
    #[test]
    fn failed_candidates_come_only_from_failed_steps() {
        let key = key();
        let base = state(&key, "hello");
        let list = candidates(&key);
        let (answer, github) = (
            list[0].candidate.candidate_id().clone(),
            list[2].candidate.candidate_id().clone(),
        );
        let observed = ObservedState::from_runtime_records(
            vec![
                executed(ObservedStepStatus::Failed, &github),
                executed(ObservedStepStatus::Failed, &github),
                executed(ObservedStepStatus::Denied, &answer),
                executed(ObservedStepStatus::Ok, &answer),
                step(ObservedStepKind::Tool, ObservedStepStatus::Failed),
            ],
            Vec::new(),
            0,
        );
        assert_eq!(observed.failed_candidates().into_iter().collect::<Vec<_>>(), [&github]);
        let mut failed = base.clone();
        failed.observed = observed;
        assert_ne!(failed.material_fingerprint(&key), base.material_fingerprint(&key));
        // Which candidate failed is material, not only that a step failed.
        let failed_on = |candidate: &CandidateId| {
            let mut changed = base.clone();
            changed.observed = ObservedState::from_runtime_records(
                vec![executed(ObservedStepStatus::Failed, candidate)],
                Vec::new(),
                0,
            );
            changed.material_fingerprint(&key)
        };
        assert_ne!(failed_on(&github), failed_on(&answer));
    }

    /// R-CDP16 L-4: two candidates with one id are refused.
    #[test]
    fn the_derived_state_refuses_a_repeated_candidate() {
        let key = key();
        let twice = vec![
            candidate(&key, CausalActionKind::UseTool, CapabilityTarget::Mcp, Some("github")),
            candidate(&key, CausalActionKind::UseTool, CapabilityTarget::Mcp, Some("github")),
        ];
        let repeated = twice[0].candidate.candidate_id().clone();
        assert_eq!(DerivedState::new(twice), Err(DuplicateCandidate(repeated)));
    }

    /// The hop lineage gives hop 0 with no prior, and a later hop with one.
    #[test]
    fn the_hop_lineage_pairs_hop_and_prior() {
        let key = key();
        let first = state(&key, "hello");
        assert_eq!((first.lineage.hop(), first.lineage.prior_decision_id()), (0, None));
        let prior = Uuid::from_u128(5);
        let after = next_hop(&first, prior);
        assert_eq!((after.hop(), after.prior_decision_id()), (1, Some(prior)));
    }

    fn sealed(key: &DecisionKey, material: &str) -> (DecisionRequest, QuestionId, DecisionAnswer) {
        let criteria: BTreeMap<OptionId, StructuredValue> = ["owner_id", "approval_pending", "catalog_generation"]
            .into_iter()
            .map(|code| {
                (
                    OptionId::code(code),
                    StructuredValue::try_from(json!({"set": code, "to": "attacker"})).expect("test: structured"),
                )
            })
            .collect();
        let id = QuestionId::global(GlobalQuestion::NextAction);
        let request = DecisionRequest::seal(
            RequestParts {
                state: StructuredValue::try_from(json!({"owner_id": "attacker"})).expect("test: state"),
                questions: BTreeMap::from([(
                    id.clone(),
                    DecisionQuestion::Choice {
                        instructions: StructuredValue::try_from(json!("Which trusted field?")).expect("test: text"),
                        criteria,
                    },
                )]),
                model: None,
                state_fingerprint: material.to_string(),
                candidate_set_hash: "c".to_string(),
                policy_version: Arc::from("policy-1"),
                catalog_generation: "g".to_string(),
                omitted_questions: Vec::new(),
                approval_pending: false,
            },
            key,
            Instant::now() + Duration::from_mins(1),
            &RequestLimits::default(),
        )
        .expect("test: seal");
        let probability = |value| Probability::new(value).expect("test: probability");
        let answer = DecisionAnswer::Choice(ChoiceAnswer {
            chosen: OptionId::code("owner_id"),
            probabilities: BTreeMap::from([
                (OptionId::code("approval_pending"), probability(0.1)),
                (OptionId::code("catalog_generation"), probability(0.1)),
                (OptionId::code("owner_id"), probability(0.8)),
            ]),
            confidence: probability(0.9),
        });
        (request, id, answer)
    }

    fn validated(request: &DecisionRequest, id: &QuestionId, answer: &DecisionAnswer) -> ValidatedDecision {
        let response = DecisionResponse {
            backend: "builtin".to_string(),
            resolved_model: "m".to_string(),
            answers: BTreeMap::from([(id.clone(), answer.clone())]),
            usage: DecisionUsage::default(),
            provider_request_id: None,
            elapsed_ms: 1,
            raw_bytes: 0,
            response_sha256: [0; 32],
        };
        let context = ValidationContext {
            received_at: Instant::now(),
            max_response_bytes: 1 << 20,
            version_policy: ModelVersionPolicy::Unpinned,
            local_backend: BackendIdentity {
                kind: BackendKind::Builtin,
                provider: None,
                configured_model: Arc::from("builtin-v1"),
            },
        };
        DecisionResponseValidator::validate(request, response, &context).expect("test: validate")
    }

    /// V3.14 `backend_cannot_set_trusted_state`: a validated decision whose
    /// options are named like trusted fields becomes predicted state only;
    /// the trusted parts and both fingerprints are unchanged. R-CDP16 L-8:
    /// a decision for another request is refused.
    #[test]
    fn backend_cannot_set_trusted_state() {
        let key = key();
        let mut turn = state(&key, "hello");
        let trusted_before = turn.trusted.clone();
        let material_before = turn.material_fingerprint(&key);
        let record_before = turn.record_fingerprint(&key);

        let (request, id, answer) = sealed(&key, &material_before);
        let decision = validated(&request, &id, &answer);
        turn.predicted = Some(PredictedState::from_validated(&decision, &request).expect("test: same request"));
        let predicted = turn.predicted.as_ref().expect("test: predicted");
        assert_eq!(predicted.answer(&id), Some(&answer));
        assert_eq!(predicted.request_id(), request.request_id());
        assert_eq!(turn.trusted, trusted_before);
        assert_eq!(turn.material_fingerprint(&key), material_before);
        assert_eq!(turn.record_fingerprint(&key), record_before);

        let (other, _, _) = sealed(&key, &material_before);
        assert_ne!(other.request_id(), request.request_id());
        assert_eq!(PredictedState::from_validated(&decision, &other), Err(ForeignDecision));
    }

    /// D-057 M-5 and R-CDP16 M-4: `Debug` of a state prints no raw text: not
    /// the routing input, the goal, an artifact or catalog summary, an
    /// identity, an exclusion or a grant.
    #[test]
    fn debug_output_carries_no_private_text() {
        let raw = RawRoutingInput::new("my secret plan".to_string());
        let printed = format!("{raw:?}");
        assert!(printed.starts_with("RawRoutingInput(len=14, sha256="), "{printed}");
        assert!(!printed.contains("secret"));

        let key = key();
        let mut turn = with_trusted(&state(&key, "my secret plan"), |parts| {
            parts.owner_id = Some(Arc::from("owner-sentinel-1"));
            parts.session_key = Some(Arc::from("session-sentinel-2"));
            parts.workspace_id = Arc::from("workspace-sentinel-3");
            parts.exclusions = vec![Arc::from("exclusion-sentinel-4")];
            parts.approval_grants = vec![Arc::from("grant-sentinel-5")];
        });
        turn.goal = RawRoutingInput::new("goal-sentinel-6".to_string());
        turn.observed =
            ObservedState::from_runtime_records(Vec::new(), vec![artifact("password=hunter2-sentinel-7", 10)], 0);
        let mut list = candidates(&key);
        list[0].summary = Arc::from("catalog-sentinel-8");
        turn.derived = derived(list);
        let printed = format!("{turn:?} {:?}", observed());
        for sentinel in [
            "my secret plan",
            "owner-sentinel-1",
            "session-sentinel-2",
            "workspace-sentinel-3",
            "exclusion-sentinel-4",
            "grant-sentinel-5",
            "goal-sentinel-6",
            "hunter2-sentinel-7",
            "catalog-sentinel-8",
            "release plan",
        ] {
            assert!(!printed.contains(sentinel), "{sentinel} printed: {printed}");
        }
    }
}
