use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

#[cfg(test)]
use serde_json::json;
use sha2::{Digest as _, Sha256};

use uuid::Uuid;

use crate::decision_engine::candidate::{CandidateId, CausalCandidate};
use crate::tools::capability::contract::{ApprovalRequirement, OpId};

mod canonical;
use crate::tools::capability::turn_state::{RouterHistoryReadBinding, SideEffectMode, TrustedState, exact_calls};
#[cfg(test)]
use crate::tools::capability::{
    contract::CatalogGeneration,
    turn_state::{ConstraintCode, DeliveryConstraints, TrustedParts, TurnBudgets, TurnSurfaceKind, model_admission},
};

#[cfg(test)]
mod canonical_tests;
#[cfg(test)]
mod exact_calls_tests;
use crate::security::runtime_key::{DecisionKey, KeyDomain};
/// The kind of a completed step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ObservedStepKind {
    Tool,
    Provider,
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
}

impl ObservedStep {
    /// A completed step as the runtime recorded it.
    pub(crate) const fn recorded_by_runtime(
        kind: ObservedStepKind,
        status: ObservedStepStatus,
        candidate: Option<CandidateId>,
    ) -> Self {
        Self {
            kind,
            status,
            candidate,
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
    retry_count: u32,
    exact_call_observations: Option<Arc<exact_calls::ExactCallObservations>>,
}

impl ObservedState {
    /// The observations of a turn so far.
    pub(crate) const fn from_runtime_records(completed_steps: Vec<ObservedStep>, retry_count: u32) -> Self {
        Self {
            completed_steps,
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
    /// Projection-only verdict of current admission. Unknown fixtures retain
    /// the intrinsic requirement; execution always rechecks the original gate.
    pub(crate) effective_approval_required: Option<bool>,
}

impl fmt::Debug for DerivedCandidate {
    /// The catalog summary prints as its length only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DerivedCandidate")
            .field("candidate", &self.candidate)
            .field("summary_len", &self.summary.len())
            .field("exact_mention", &self.exact_mention)
            .field("available", &self.available)
            .field("effective_approval_required", &self.effective_approval_required)
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

/// Where a decision hop sits in its turn. Since R9 every turn opens
/// prediction-free with exactly one hop, so the only lineage is hop 0 with
/// no prior decision (the state v1 rule "null exactly on hop 0"; R-CDP16
/// M-2). The wire keeps both fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HopLineage {
    /// Hop 0.
    First,
}

impl HopLineage {
    /// The hop number, 0 for the first hop.
    pub(crate) const fn hop(self) -> u8 {
        match self {
            Self::First => 0,
        }
    }

    /// The previous decision, `None` on the first hop.
    pub(crate) const fn prior_decision_id(self) -> Option<Uuid> {
        match self {
            Self::First => None,
        }
    }
}

/// The causal state of one decision hop (CDP 5.3). Its `Debug` prints no
/// private text: every part redacts itself.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CausalState {
    pub(crate) trusted: TrustedState,
    pub(crate) observed: ObservedState,
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

fn fingerprint_from_canonical(
    key: &DecisionKey,
    domain: &[u8],
    canonical: Result<String, serde_json::Error>,
) -> Result<String, serde_json::Error> {
    let canonical = canonical?;
    Ok(hex::encode(
        key.hmac(KeyDomain::StateFingerprint, &[domain, canonical.as_bytes()]),
    ))
}

impl CausalState {
    /// The keyed fingerprint of everything that decides the turn: trusted,
    /// observed and derived inputs and the raw texts. Lineage and time are
    /// not material: the run and task ids, the source, causation and step
    /// event ids, the build time, the hop number and the prior decision id
    /// (R-CDP16 M-1). The canonical `artifacts` sequences stay empty: no
    /// runtime path records an artifact since R9. It never reads predicted
    /// values. Every executed hop adds its own completed step, which is
    /// material.
    /// Lowercase hex of
    /// `HMAC(DecisionKey, StateFingerprint, "material" ‖ canonical JSON)`.
    pub(crate) fn material_fingerprint(&self, key: &DecisionKey) -> Result<String, serde_json::Error> {
        fingerprint_from_canonical(key, b"material", self.canonical())
    }

    /// Canonical JSON of the fingerprinted inputs: object keys in a fixed
    /// order, sets (exclusions, constraints, grants, approval op ids,
    /// candidates) deduplicated and sorted so neither their listing order
    /// nor a repeat matters, sequences (steps) in order, no
    /// floats.
    fn canonical(&self) -> Result<String, serde_json::Error> {
        canonical::serialize(self)
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
    use super::*;
    use crate::decision_engine::candidate::{CandidateSpec, Ns8};
    use crate::tools::capability::contract::{
        ApprovalRequirement, CapabilityTarget, CausalActionKind, CostEstimate, SideEffectClass,
    };

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
            resolved_provider: Arc::from("anthropic"),
            resolved_model_id: Arc::from("claude"),
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
            effective_approval_required: None,
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
        ObservedStep::recorded_by_runtime(kind, status, None)
    }

    /// A step that executed `candidate`.
    pub(crate) fn executed(status: ObservedStepStatus, candidate: &CandidateId) -> ObservedStep {
        ObservedStep::recorded_by_runtime(ObservedStepKind::Tool, status, Some(candidate.clone()))
    }

    pub(crate) fn observed() -> ObservedState {
        ObservedState::from_runtime_records(vec![step(ObservedStepKind::Tool, ObservedStepStatus::Ok)], 0)
    }

    pub(crate) fn state(key: &DecisionKey, turn: &str) -> CausalState {
        CausalState {
            trusted: TrustedState::from_runtime(trusted()),
            observed: observed(),
            derived: derived(candidates(key)),
            has_conversation_history: false,
            routing_input: RawRoutingInput::new(turn.to_string()),
            goal: RawRoutingInput::new("Answer the question.".to_string()),
            lineage: HopLineage::First,
            built_at_ms: 1_000,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod turn_state_tests {
    use super::fixtures::{candidate, candidates, derived, executed, key, observed, state, step};
    use super::*;
    use crate::tools::capability::contract::{CapabilityTarget, CausalActionKind};

    fn with_trusted(state: &CausalState, edit: impl FnOnce(&mut TrustedParts)) -> CausalState {
        let mut parts = state.trusted.parts().clone();
        edit(&mut parts);
        let mut changed = state.clone();
        changed.trusted = TrustedState::from_runtime(parts);
        changed
    }

    /// R-CDP16 M-1: lineage and time are not material. A state that differs
    /// only in the run and task ids, the event ids and the build time has
    /// the same material fingerprint.
    #[test]
    fn lineage_and_time_are_not_material() {
        let key = key();
        let base = state(&key, "hello");
        let material = base
            .material_fingerprint(&key)
            .expect("test: canonical state serializes");
        assert_eq!(material.len(), 64);

        let mut later = with_trusted(&base, |parts| {
            parts.run_id = Some(Arc::from("run-2"));
            parts.task_id = Some(Arc::from("task-9"));
            parts.source_event_id = Some(Arc::from("event-9"));
            parts.causation_event_id = Some(Arc::from("event-8"));
        });
        later.built_at_ms = 99_000;
        let renamed = ObservedStep::recorded_by_runtime(ObservedStepKind::Tool, ObservedStepStatus::Ok, None);
        later.observed = ObservedState::from_runtime_records(vec![renamed], 0);
        assert_eq!(
            later
                .material_fingerprint(&key)
                .expect("test: canonical state serializes"),
            material,
            "not material"
        );
    }

    /// A new failure, a new catalog generation, a new approval grant, a new
    /// routing input, a retry or an exact mention is material.
    #[test]
    fn new_inputs_are_material() {
        let key = key();
        let base = state(&key, "hello");
        let material = base
            .material_fingerprint(&key)
            .expect("test: canonical state serializes");
        let github = candidates(&key)[2].candidate.candidate_id().clone();
        let failed = executed(ObservedStepStatus::Failed, &github);
        let changes: Vec<CausalState> = vec![
            {
                let mut changed = base.clone();
                changed.observed = ObservedState::from_runtime_records(vec![failed], 0);
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
                changed.observed = ObservedState::from_runtime_records(base.observed.completed_steps().to_vec(), 1);
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
            assert_ne!(
                changed
                    .material_fingerprint(&key)
                    .expect("test: canonical state serializes"),
                material,
                "change {index}"
            );
        }
        let other_key = super::fixtures::key();
        assert_ne!(
            base.material_fingerprint(&other_key)
                .expect("test: canonical state serializes"),
            material,
            "keyed per workspace"
        );
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
        assert_eq!(
            shuffled
                .material_fingerprint(&key)
                .expect("test: canonical state serializes"),
            base.material_fingerprint(&key)
                .expect("test: canonical state serializes")
        );
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
            0,
        );
        assert_eq!(observed.failed_candidates().into_iter().collect::<Vec<_>>(), [&github]);
        let mut failed = base.clone();
        failed.observed = observed;
        assert_ne!(
            failed
                .material_fingerprint(&key)
                .expect("test: canonical state serializes"),
            base.material_fingerprint(&key)
                .expect("test: canonical state serializes")
        );
        // Which candidate failed is material, not only that a step failed.
        let failed_on = |candidate: &CandidateId| {
            let mut changed = base.clone();
            changed.observed =
                ObservedState::from_runtime_records(vec![executed(ObservedStepStatus::Failed, candidate)], 0);
            changed
                .material_fingerprint(&key)
                .expect("test: canonical state serializes")
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

    /// The only hop lineage is hop 0 with no prior decision.
    #[test]
    fn the_hop_lineage_pairs_hop_and_prior() {
        let key = key();
        let first = state(&key, "hello");
        assert_eq!((first.lineage.hop(), first.lineage.prior_decision_id()), (0, None));
    }

    /// D-057 M-5 and R-CDP16 M-4: `Debug` of a state prints no raw text: not
    /// the routing input, the goal, a catalog summary, an
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
            "catalog-sentinel-8",
        ] {
            assert!(!printed.contains(sentinel), "{sentinel} printed: {printed}");
        }
    }
    #[test]
    fn fingerprint_error_never_becomes_a_match() {
        let error = serde_json::from_str::<serde_json::Value>("{").expect_err("test: invalid JSON");
        let key = fixtures::key();
        assert!(
            fingerprint_from_canonical(&key, b"material", Err(error)).is_err(),
            "serialization error must not mint a usable material fingerprint"
        );
    }
}
