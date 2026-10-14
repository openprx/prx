//! Deterministic capability-backed candidate enumeration for the decision plane.
//!
//! Kept separate from the `llm-router`-only legacy expander so the new
//! decision domain also compiles in bare builds.

use std::collections::{BTreeMap, BTreeSet};

use super::branch::{
    ApprovalRequirement, CandidateId, CandidateSpec, CapabilityRef, CapabilityTarget, CausalActionKind,
    CausalCandidate, CostEstimate, Ns8, OpId, SideEffectClass,
};
use super::gates::{GateError, GateInputs, GateOutcome, GatePass, GateReport, HardGates};
use super::state::{CausalState, SideEffectMode};
use crate::decision::DecisionKey;
use crate::tools::capability::catalog::{CapabilityEntry, CatalogView, Exposure};

/// The selected Memory hop uses the runtime's scoped lexical read port, not
/// the tool's `execute` method. Keep its catalog anchor exact so a write tool
/// in the broad Memory category cannot be mislabeled as Retrieve.
pub(crate) fn is_memory_retrieval_source(entry: &CapabilityEntry) -> bool {
    entry.family == CapabilityTarget::Memory
        && entry.public_name == "memory_search"
        && entry.root == "memory_search"
        && entry.effect == SideEffectClass::ReadOnly
        // Ordinary Agent catalogs publish this read tool as Searchable. A
        // selected Retrieve invokes the separate scoped read port, never the
        // tool executor; Q-9 requires it without a preliminary Discover.
        && matches!(entry.exposure, Exposure::Direct | Exposure::Searchable)
}

pub(crate) fn is_document_retrieval_source(entry: &CapabilityEntry) -> bool {
    entry.family == CapabilityTarget::Document
        && entry.public_name == "document_search"
        && entry.root == "document_search"
        && entry.effect == SideEffectClass::ReadOnly
        && matches!(entry.exposure, Exposure::Direct | Exposure::Searchable)
}

pub(crate) fn is_session_retrieval_source(entry: &CapabilityEntry) -> bool {
    entry.family == CapabilityTarget::Session
        && entry.public_name == "sessions_history"
        && entry.root == "sessions_history"
        && entry.effect == SideEffectClass::ReadOnly
        && matches!(entry.exposure, Exposure::Direct | Exposure::Searchable)
}

pub(crate) fn is_retrieval_source(entry: &CapabilityEntry) -> bool {
    is_memory_retrieval_source(entry) || is_document_retrieval_source(entry) || is_session_retrieval_source(entry)
}

/// Exact turn-scoped refs. Production insertion is limited to a validated
/// selected search in the sibling decision-engine module.
#[derive(Debug, Clone, Default)]
pub(crate) struct DiscoveredRefs {
    refs: BTreeSet<CapabilityRef>,
}

impl DiscoveredRefs {
    pub(crate) const fn empty() -> Self {
        Self { refs: BTreeSet::new() }
    }

    /// References from selected, final-gated discovery hops only.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &CapabilityRef> {
        self.refs.iter()
    }

    pub(crate) fn contains(&self, reference: &CapabilityRef) -> bool {
        self.refs.contains(reference)
    }

    /// Preserve only refs issued by final-gated selected searches in this
    /// turn. No discovered ref is an execution grant by itself.
    pub(crate) fn extend_selected(&mut self, selected: Self) -> bool {
        let before = self.refs.len();
        self.refs.extend(selected.refs);
        self.refs.len() != before
    }

    pub(super) const fn from_selected_search(refs: BTreeSet<CapabilityRef>) -> Self {
        Self { refs }
    }

    #[cfg(test)]
    pub(crate) const fn from_refs_for_test(refs: BTreeSet<CapabilityRef>) -> Self {
        Self { refs }
    }
}

/// Test fixture for exact capability estimates. Production prices pinned
/// entries on demand through `RuntimeCostModel`; missing estimates stay
/// unknown and are rejected by G6.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct CapabilityCostModel {
    /// Exact catalog binding. A same-family estimate cannot price a different
    /// schema or capability once this map is populated by the turn service.
    pub per_capability: BTreeMap<CapabilityRef, (CostEstimate, u32)>,
    /// Terminal estimate and legacy test fixtures. Production capability
    /// estimates are keyed by `CapabilityRef` above.
    pub estimates: BTreeMap<CapabilityTarget, (CostEstimate, u32)>,
}

/// Price only a capability the generator actually considers. A missing
/// estimate remains unknown and is rejected by the budget gate.
pub(crate) trait CostEstimateSource: Send + Sync {
    fn estimate_entry(&self, entry: &CapabilityEntry) -> Option<(CostEstimate, u32)>;
    fn estimate_terminal(&self) -> Option<(CostEstimate, u32)>;
}

#[cfg(test)]
impl CostEstimateSource for CapabilityCostModel {
    fn estimate_entry(&self, entry: &CapabilityEntry) -> Option<(CostEstimate, u32)> {
        self.per_capability
            .get(&CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .copied()
    }

    fn estimate_terminal(&self) -> Option<(CostEstimate, u32)> {
        self.estimates.get(&CapabilityTarget::None).copied()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CandidatePolicy<'a> {
    pub key: &'a DecisionKey,
    pub max_candidates: usize,
    pub can_deliver_text: bool,
    pub may_delegate: bool,
    /// A source-bound configured delegation target; this never bypasses gates.
    pub delegation_target: Option<&'a str>,
    pub cost_model: &'a dyn CostEstimateSource,
    /// Runtime-discovered refs admitted for non-exact searchable tool use.
    pub discovered_refs: &'a DiscoveredRefs,
}

/// Fixed-size eligibility facts, independent of prediction truncation and of
/// whether the original source text can itself serve as a bounded hop query.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DiscoveryFamilies([bool; 5]);

impl DiscoveryFamilies {
    const ALL: [CapabilityTarget; 5] = [
        CapabilityTarget::Skill,
        CapabilityTarget::Mcp,
        CapabilityTarget::NativeTool,
        CapabilityTarget::Wasm,
        CapabilityTarget::Hook,
    ];

    pub(crate) fn iter(&self) -> impl Iterator<Item = CapabilityTarget> + '_ {
        Self::ALL
            .into_iter()
            .zip(self.0)
            .filter_map(|(family, eligible)| eligible.then_some(family))
    }
}

#[derive(Debug)]
pub(crate) enum CandidateSet {
    Legal {
        discoverable_families: DiscoveryFamilies,
        candidates: Vec<CausalCandidate>,
        exact_mentions: BTreeSet<CandidateId>,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "CDP-PLAN retains bounded truncation diagnostics; production selection consumes the retained candidates"
            )
        )]
        truncated: Vec<CandidateId>,
        approval_scopes: BTreeMap<CandidateId, Vec<PendingApproval>>,
        report: GateReport,
    },
    NoLegalPath {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Preserve the no-legal-path classification; authorization uses gates and approval scopes"
            )
        )]
        reason: NoLegalReason,
        /// Admitted terminal bodies retained for fresh post/pre-commit gates.
        terminals: Vec<CausalCandidate>,
        approval_only: Vec<PendingApproval>,
        report: GateReport,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoLegalReason {
    NoLegalCandidate,
    ApprovalScopeUnavailable,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingApproval {
    pub candidate: CausalCandidate,
    pub op_ids: Vec<OpId>,
}

pub(crate) struct CausalCandidateGenerator;

impl CausalCandidateGenerator {
    /// Exact one-capability proposal; never inserts it into the legal executable set.
    pub(super) fn exact_deferred_mutation(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        gates: &GateInputs<'_>,
    ) -> Option<CausalCandidate> {
        // Necessary-only prefilter over the catalog's actual mention terms.
        // ASCII matching streams bytes without allocation. Non-ASCII takes
        // the original Unicode matcher (including contextual case mapping).
        let raw = state.routing_input.as_str();
        let possible = catalog
            .iter_entries()
            .filter(|entry| {
                entry.family == CapabilityTarget::Filesystem
                    && matches!(entry.public_name.as_str(), "file_write" | "file_edit")
            })
            .any(|entry| {
                entry.exact_mention_terms.iter().any(|term| {
                    !term.is_empty()
                        && (!raw.is_ascii()
                            || !term.is_ascii()
                            || raw
                                .as_bytes()
                                .windows(term.len())
                                .any(|part| part.eq_ignore_ascii_case(term.as_bytes())))
                })
            });
        if !possible {
            return None;
        }
        let folded = state.routing_input.as_str().to_lowercase();
        let mut exact = catalog.iter_entries().filter(|entry| exact_mention(&folded, entry));
        let entry = exact.next()?;
        if exact.next().is_some()
            || entry.family != CapabilityTarget::Filesystem
            || !matches!(entry.public_name.as_str(), "file_write" | "file_edit")
        {
            return None;
        }
        let candidate = from_entry(
            CausalActionKind::UseTool,
            entry.family,
            entry,
            policy,
            Some(Ns8::of(policy.key, &entry.namespace)),
        );
        (HardGates::check_one(GatePass::PrePrediction, state, catalog, &candidate, gates)
            == GateOutcome::ApprovalDeferred)
            .then_some(candidate)
    }

    pub(crate) fn generate(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        gates: &GateInputs<'_>,
    ) -> Result<CandidateSet, GateError> {
        Self::generate_mode(state, catalog, policy, gates, false, None)
    }

    /// Rebuild a fresh candidate set for a turn that has exhausted useful
    /// read hops. Terminal candidates still pass the ordinary three gates.
    pub(crate) fn generate_terminal_only(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        gates: &GateInputs<'_>,
    ) -> Result<CandidateSet, GateError> {
        Self::generate_mode(state, catalog, policy, gates, true, None)
    }

    /// The runtime supplies only the exact target of an authenticated resume.
    /// This restores candidate intent, never an execution or discovery grant.
    pub(super) fn generate_with_resume_target(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        gates: &GateInputs<'_>,
        resume_target: Option<&CapabilityRef>,
    ) -> Result<CandidateSet, GateError> {
        Self::generate_mode(state, catalog, policy, gates, false, resume_target)
    }

    fn generate_mode(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        gates: &GateInputs<'_>,
        terminal_only: bool,
        resume_target: Option<&CapabilityRef>,
    ) -> Result<CandidateSet, GateError> {
        if policy.max_candidates < 3 {
            return Err(GateError::InvalidCandidateLimit);
        }
        let mut discoverable_families = DiscoveryFamilies::default();
        let mut exact_mentions = BTreeSet::new();
        let mut proposed = Vec::new();
        let folded_input = if terminal_only || policy.delegation_target.is_some() {
            String::new()
        } else {
            state.routing_input.as_str().to_lowercase()
        };
        let routing_history = state.trusted.router_history().is_some();
        if !terminal_only && routing_history && state.routing_input.supports_selected_read_query() {
            let sources = catalog.iter_entries().filter(|entry| is_memory_retrieval_source(entry));
            if let Some(candidate) = select_family_candidate(
                CausalActionKind::Retrieve,
                CapabilityTarget::Memory,
                sources,
                state,
                catalog,
                policy,
                gates,
            )? {
                proposed.push(candidate);
            }
        }
        if !terminal_only && !routing_history {
            if policy.delegation_target.is_none() {
                // Raw text affects candidates only through exact mentions.
                // Authenticated resume supplies its separately verified exact
                // target; neither path itself grants execution authority.
                for family in CapabilityTarget::ALL {
                    let mut approval_only = None;
                    let mut selected = None;
                    for entry in catalog.iter_entries().filter(|entry| {
                        entry.family == family
                            && !matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn")
                            && (exact_mention(&folded_input, entry)
                                || resume_target.is_some_and(|reference| {
                                    reference.id == entry.id
                                        && reference.fingerprint == entry.fingerprint
                                        && reference.generation == entry.generation
                                }))
                    }) {
                        let candidate = from_entry(
                            CausalActionKind::UseTool,
                            family,
                            entry,
                            policy,
                            Some(Ns8::of(policy.key, &entry.namespace)),
                        );
                        let admission =
                            HardGates::check_one(GatePass::PrePrediction, state, catalog, &candidate, gates);
                        match admission {
                            GateOutcome::Passed => {
                                selected = Some(candidate);
                                break;
                            }
                            GateOutcome::ApprovalOnly { op_ids } if !op_ids.is_empty() && approval_only.is_none() => {
                                approval_only = Some(candidate);
                            }
                            _ => {}
                        }
                    }
                    if let Some(candidate) = selected.or(approval_only) {
                        exact_mentions.insert(candidate.candidate_id().clone());
                        proposed.push(candidate);
                    }
                }

                for family in CapabilityTarget::ALL {
                    let executable = catalog.iter_entries().filter(|entry| {
                        entry.family == family
                            && !matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn")
                            && available_for_fallback(entry, policy)
                    });
                    if let Some(candidate) = select_family_candidate(
                        CausalActionKind::UseTool,
                        family,
                        executable,
                        state,
                        catalog,
                        policy,
                        gates,
                    )? {
                        // An admitted exact mention already represents this entry.
                        // Preserve family fallback for other entries and unmatched input.
                        let exact_entry_present = proposed.iter().any(|prior: &CausalCandidate| {
                            exact_mentions.contains(prior.candidate_id())
                                && prior.required_capabilities().iter().any(|exact| {
                                    candidate
                                        .required_capabilities()
                                        .iter()
                                        .any(|fallback| exact.id == fallback.id)
                                })
                        });
                        if !exact_entry_present
                            && !proposed
                                .iter()
                                .any(|prior: &CausalCandidate| prior.candidate_id() == candidate.candidate_id())
                        {
                            proposed.push(candidate);
                        }
                    }
                }
                if state.routing_input.supports_selected_read_query() {
                    for family in [
                        CapabilityTarget::Memory,
                        CapabilityTarget::Document,
                        CapabilityTarget::Session,
                    ] {
                        if family == CapabilityTarget::Session && state.trusted.parts().source_event_id.is_none() {
                            continue;
                        }
                        let safe_sources = catalog
                            .iter_entries()
                            .filter(|entry| entry.family == family && is_retrieval_source(entry));
                        if let Some(candidate) = select_family_candidate(
                            CausalActionKind::Retrieve,
                            family,
                            safe_sources,
                            state,
                            catalog,
                            policy,
                            gates,
                        )? {
                            proposed.push(candidate);
                        }
                    }
                }
                // A later capability_search call supplies its own bounded query.
                // Only initial Discover hops require the raw source to fit that
                // query contract; both paths use the same capability gates.
                for (eligible, family) in discoverable_families.0.iter_mut().zip(DiscoveryFamilies::ALL) {
                    let searchable = catalog
                        .iter_entries()
                        .filter(|entry| entry.family == family && matches!(entry.exposure, Exposure::Searchable));
                    if let Some(candidate) = select_family_candidate(
                        CausalActionKind::Discover,
                        family,
                        searchable,
                        state,
                        catalog,
                        policy,
                        gates,
                    )? {
                        *eligible = HardGates::check_one(GatePass::PrePrediction, state, catalog, &candidate, gates)
                            == GateOutcome::Passed;
                        if state.routing_input.supports_selected_read_query() {
                            proposed.push(candidate);
                        }
                    }
                }
            }
            if policy.may_delegate {
                let exact_delegator = catalog.iter_entries().find(|entry| {
                    matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn")
                        && exact_mention(&folded_input, entry)
                });
                let delegates = catalog.iter_entries().filter(|entry| {
                    (entry.public_name == "delegate" || entry.public_name == "sessions_spawn")
                        && policy
                            .delegation_target
                            .is_none_or(|target| entry.public_name == target)
                        && exact_delegator.is_none_or(|exact| entry.id == exact.id)
                });
                if let Some(candidate) = select_family_candidate(
                    CausalActionKind::Delegate,
                    CapabilityTarget::Session,
                    delegates,
                    state,
                    catalog,
                    policy,
                    gates,
                )? {
                    if policy.delegation_target.is_some() || exact_delegator.is_some() {
                        exact_mentions.insert(candidate.candidate_id().clone());
                    }
                    proposed.push(candidate);
                }
            }
        }
        if !routing_history && policy.can_deliver_text && policy.delegation_target.is_none() {
            proposed.push(terminal(CausalActionKind::Answer, policy));
        }
        if state.trusted.parts().delivery.interactive && state.trusted.parts().delivery.can_ask_question {
            proposed.push(terminal(CausalActionKind::Clarify, policy));
        }
        proposed.push(terminal(CausalActionKind::Refuse, policy));

        // A duplicate id is a generator bug and is rejected by HardGates.
        let initial = HardGates::apply(GatePass::PrePrediction, state, catalog, &proposed, gates)?;
        let mut approval_only = Vec::new();
        let mut approval_scope_missing = false;
        for candidate in proposed.iter().filter(|_| !terminal_only) {
            if let Some(GateOutcome::ApprovalOnly { op_ids }) = initial.outcomes.get(candidate.candidate_id()) {
                if state.trusted.parts().delivery.approval_channel && !op_ids.is_empty() {
                    // No current producer binds tool arguments, a Discover
                    // query or a Delegate manifest without an exact scope.
                    if gates.call_fingerprints.contains_key(candidate.candidate_id()) {
                        approval_only.push(PendingApproval {
                            candidate: candidate.clone(),
                            op_ids: op_ids.clone(),
                        });
                    } else {
                        approval_scope_missing = true;
                    }
                }
            }
        }
        let mut approval_scopes = BTreeMap::<CandidateId, Vec<PendingApproval>>::new();
        if state.trusted.parts().delivery.approval_channel && !approval_only.is_empty() {
            let Some(refusal) = proposed.pop() else {
                return Err(GateError::MissingRefusal);
            };
            let mut targets = BTreeSet::new();
            for pending in &approval_only {
                let target = pending.candidate.target();
                if targets.insert(target) {
                    proposed.push(approval_candidate(target));
                }
                approval_scopes
                    .entry(CandidateId::new(CausalActionKind::RequestApproval, target, None))
                    .or_default()
                    .push(pending.clone());
            }
            proposed.push(refusal);
        }
        let report = HardGates::apply(GatePass::PrePrediction, state, catalog, &proposed, gates)?;
        let mut legal = Vec::new();
        for candidate in proposed {
            if matches!(report.outcomes.get(candidate.candidate_id()), Some(GateOutcome::Passed)) {
                legal.push(candidate);
            }
        }
        let nonterminal = legal.iter().any(|candidate| {
            !matches!(
                candidate.action(),
                CausalActionKind::Refuse | CausalActionKind::Clarify | CausalActionKind::RequestApproval
            )
        });
        if !nonterminal {
            // Generic supervised calls need arguments before exact OpIds can
            // be derived. CDPB-08 owns the non-executable proposal path.
            let mut reason = if !terminal_only && approval_scope_missing {
                NoLegalReason::ApprovalScopeUnavailable
            } else {
                NoLegalReason::NoLegalCandidate
            };
            if !terminal_only && approval_only.is_empty() && policy.delegation_target.is_none() {
                for family in CapabilityTarget::ALL {
                    for entry in catalog.iter_entries().filter(|entry| entry.family == family) {
                        if !available_for_fallback(entry, policy) && !exact_mention(&folded_input, entry) {
                            continue;
                        }
                        let candidate = from_entry(CausalActionKind::UseTool, family, entry, policy, None);
                        let admission =
                            HardGates::check_one(GatePass::PrePrediction, state, catalog, &candidate, gates);
                        if admission == GateOutcome::ApprovalDeferred {
                            reason = NoLegalReason::ApprovalScopeUnavailable;
                            break;
                        }
                    }
                }
            }
            return Ok(CandidateSet::NoLegalPath {
                reason,
                terminals: legal,
                approval_only,
                report,
            });
        }
        let cap = policy.max_candidates;
        let mut truncated = Vec::new();
        let mut kept = Vec::new();
        // Refuse and legal Clarify are guaranteed space. Other candidates
        // retain the fixed enumeration priority above.
        let reserved = legal
            .iter()
            .filter(|candidate| matches!(candidate.action(), CausalActionKind::Refuse | CausalActionKind::Clarify))
            .count();
        for candidate in legal {
            if matches!(candidate.action(), CausalActionKind::Refuse | CausalActionKind::Clarify)
                || kept.len() < cap.saturating_sub(reserved)
            {
                kept.push(candidate);
            } else {
                truncated.push(candidate.candidate_id().clone());
            }
        }
        exact_mentions.retain(|id| kept.iter().any(|candidate| candidate.candidate_id() == id));
        approval_scopes.retain(|id, _| kept.iter().any(|candidate| candidate.candidate_id() == id));
        Ok(CandidateSet::Legal {
            discoverable_families,
            candidates: kept,
            exact_mentions,
            truncated,
            approval_scopes,
            report,
        })
    }
}

fn select_family_candidate<'a>(
    action: CausalActionKind,
    family: CapabilityTarget,
    entries: impl IntoIterator<Item = &'a CapabilityEntry>,
    state: &CausalState,
    catalog: &CatalogView<'_>,
    policy: &CandidatePolicy<'_>,
    gates: &GateInputs<'_>,
) -> Result<Option<CausalCandidate>, GateError> {
    let mut approval_only = None;
    for entry in entries {
        // G9 will reject a write effect under a read-only turn. This
        // transient probe cannot become Passed or ApprovalOnly, so avoid
        // constructing and pricing it during family fallback selection.
        if state.trusted.parts().side_effect_mode == SideEffectMode::ReadOnly
            && entry.effect > SideEffectClass::ReadOnly
        {
            continue;
        }
        // These borrowed facts can only reject a transient family probe.
        // Keep the full gates authoritative for every proposed candidate and
        // never interpret a model bitmap from a different catalog layout.
        let trusted = state.trusted.parts();
        if catalog.surface_excludes(entry)
            || trusted
                .exclusions
                .iter()
                .any(|name| name.as_ref() == entry.public_name || name.as_ref() == entry.root)
            || trusted.model_admission.as_ref().is_some_and(|policy| {
                policy.matches_layout(catalog.generation().0, catalog.model_admission_layout())
                    && policy.excludes(catalog.model_admission_entry(entry))
            })
            || trusted.channel.is_some() && !entry.channel_eligible
        {
            continue;
        }
        let candidate = from_entry(action, family, entry, policy, None);
        let outcome = HardGates::check_one(GatePass::PrePrediction, state, catalog, &candidate, gates);
        match outcome {
            GateOutcome::Passed => return Ok(Some(candidate)),
            GateOutcome::ApprovalOnly { op_ids } if !op_ids.is_empty() && approval_only.is_none() => {
                approval_only = Some(candidate);
            }
            _ => {}
        }
    }
    Ok(approval_only)
}

fn from_entry(
    action: CausalActionKind,
    family: CapabilityTarget,
    entry: &CapabilityEntry,
    policy: &CandidatePolicy<'_>,
    namespace: Option<Ns8>,
) -> CausalCandidate {
    let reference = CapabilityRef {
        id: entry.id.clone(),
        fingerprint: entry.fingerprint,
        generation: entry.generation,
    };
    let estimate = policy.cost_model.estimate_entry(entry);
    CausalCandidate::new(
        action,
        family,
        CandidateSpec {
            namespace,
            required_capabilities: vec![reference],
            side_effect: entry.effect,
            approval: entry.approval.clone(),
            estimated_cost: estimate.map(|(cost, _)| cost),
            estimated_latency_ms: estimate.map(|(_, latency)| latency),
        },
    )
}

fn terminal(action: CausalActionKind, policy: &CandidatePolicy<'_>) -> CausalCandidate {
    let estimate = policy.cost_model.estimate_terminal();
    CausalCandidate::new(
        action,
        CapabilityTarget::None,
        CandidateSpec {
            namespace: None,
            required_capabilities: Vec::new(),
            side_effect: SideEffectClass::ReadOnly,
            approval: ApprovalRequirement::None,
            estimated_cost: if action != CausalActionKind::Answer {
                Some(CostEstimate {
                    tokens: 0,
                    micro_usd: 0,
                })
            } else {
                estimate.map(|(cost, _)| cost)
            },
            estimated_latency_ms: if action != CausalActionKind::Answer {
                Some(0)
            } else {
                estimate.map(|(_, latency)| latency)
            },
        },
    )
}

fn approval_candidate(target: CapabilityTarget) -> CausalCandidate {
    CausalCandidate::new(
        CausalActionKind::RequestApproval,
        target,
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
    )
}

fn available_for_fallback(entry: &CapabilityEntry, policy: &CandidatePolicy<'_>) -> bool {
    !matches!(entry.exposure, Exposure::Searchable)
        || policy.discovered_refs.contains(&CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        })
}

fn exact_mention(folded_text: &str, entry: &CapabilityEntry) -> bool {
    entry
        .exact_mention_terms
        .iter()
        .any(|name| contains_bounded(folded_text, name))
}

fn contains_bounded(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    text.match_indices(name).any(|(start, matched)| {
        let before = text[..start].chars().next_back();
        let after = text[start + matched.len()..].chars().next();
        before.is_none_or(|character| !character.is_alphanumeric() && character != '_')
            && after.is_none_or(|character| !character.is_alphanumeric() && character != '_')
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct CountingCost<'a> {
        inner: &'a CapabilityCostModel,
        entries: AtomicUsize,
    }

    impl CostEstimateSource for CountingCost<'_> {
        fn estimate_entry(&self, entry: &CapabilityEntry) -> Option<(CostEstimate, u32)> {
            self.entries.fetch_add(1, Ordering::Relaxed);
            self.inner.estimate_entry(entry)
        }

        fn estimate_terminal(&self) -> Option<(CostEstimate, u32)> {
            self.inner.estimate_terminal()
        }
    }
    static EMPTY_DISCOVERED_REFS: std::sync::LazyLock<DiscoveredRefs> = std::sync::LazyLock::new(DiscoveredRefs::empty);
    use crate::causal_tree::state::{RawRoutingInput, TrustedState, fixtures};
    use crate::security::SecurityPolicy;
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
    use crate::tools::mcp::McpCapabilitySpec;
    use crate::tools::traits::ToolSpec;
    use crate::tools::traits::{Tool, ToolResult};
    use async_trait::async_trait;
    use serde_json::Value;
    use serde_json::json;

    fn fixture_cost_for_catalog(catalog: &CapabilityCatalog, mut cost: CapabilityCostModel) -> CapabilityCostModel {
        let view = catalog.view(&CatalogScope::default());
        for entry in view.entries() {
            let estimate = cost.estimates.get(&entry.family).or_else(|| {
                (entry.public_name == "delegate" || entry.public_name == "sessions_spawn")
                    .then(|| cost.estimates.get(&CapabilityTarget::Session))
                    .flatten()
            });
            if let Some(estimate) = estimate {
                cost.per_capability.insert(
                    CapabilityRef {
                        id: entry.id.clone(),
                        fingerprint: entry.fingerprint,
                        generation: entry.generation,
                    },
                    *estimate,
                );
            }
        }
        cost
    }

    #[test]
    fn provider_outage_with_no_legal_path_yields_typed_refusal() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "unmatched request");
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = CapabilityCostModel {
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
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let security = SecurityPolicy::default();
        let calls_seen = BTreeSet::<[u8; 32]>::new();
        let calls = BTreeMap::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: false,
            model_can_call_tools: false,
            decision_endpoint_ready: false,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &calls_seen,
            retry_limit: Some(2),
        };
        let view = catalog.view(&CatalogScope::default());
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        assert!(matches!(generated, CandidateSet::NoLegalPath { approval_only, .. } if approval_only.is_empty()));
    }

    #[test]
    fn supervised_generic_write_without_arguments_reports_scope_unavailable() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("file_write"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([(
                CapabilityTarget::NativeTool,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            )]),
        };
        let cost = fixture_cost_for_catalog(&catalog, cost);
        let view = catalog.view(&CatalogScope::default());
        let entry = view.entries().first().expect("one tool in fixture");
        let discovered = DiscoveredRefs::from_refs_for_test(BTreeSet::from([CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        }]));
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 3,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &discovered,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Supervised;
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(2),
        };
        let undiscovered_policy = CandidatePolicy {
            discovered_refs: &EMPTY_DISCOVERED_REFS,
            ..policy
        };
        let undiscovered = CausalCandidateGenerator::generate(&state, &view, &undiscovered_policy, &gates).unwrap();
        assert!(matches!(
            undiscovered,
            CandidateSet::NoLegalPath {
                reason: NoLegalReason::NoLegalCandidate,
                ..
            }
        ));
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        assert!(matches!(generated, CandidateSet::NoLegalPath {
            reason: NoLegalReason::ApprovalScopeUnavailable,
            approval_only,
            ..
        } if approval_only.is_empty()));
    }

    #[test]
    fn exact_mention_requires_identifier_boundaries() {
        assert!(contains_bounded("use prx_read now", "prx_read"));
        assert!(!contains_bounded("use prx_reader now", "prx_read"));
        assert!(!contains_bounded("use other_prx_read now", "prx_read"));
    }

    #[test]
    fn pinned_exact_terms_match_live_lowercase_for_unicode_aliases_and_boundaries() {
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "\u{130}-Write".into(),
            state_digest: [1; 32],
            spec: ToolSpec {
                name: "mcp__prx_server__write".into(),
                description: "PRX alias fixture".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("\u{130}-Node"))];
        let catalog = CapabilityCatalog::build(&tools, &sources, &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        for entry in view.entries() {
            for input in [
                "use i\u{307}-node now",
                "use \u{130}-node now",
                "call i\u{307}-write!",
                "call other_i\u{307}-write now",
                "call i\u{307}-writer now",
                "mcp__prx_server__write",
            ] {
                let folded = input.to_lowercase();
                let old = std::iter::once(entry.id.as_str())
                    .chain(std::iter::once(entry.public_name.as_str()))
                    .chain(entry.aliases.iter().map(String::as_str))
                    .any(|name| contains_bounded(&folded, &name.to_lowercase()));
                assert_eq!(
                    exact_mention(&folded, entry),
                    old,
                    "input={input}, entry={}",
                    entry.id.as_str()
                );
            }
        }
        assert!(
            view.entries()
                .iter()
                .any(|entry| { entry.exact_mention_terms.iter().any(|term| term == "i\u{307}-write") })
        );
    }

    #[test]
    fn fixed_terminal_candidate_generation_has_inventory_independent_allocations() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut baseline = None;
        for size in [0, 1, 64, 256, 256, 64, 1, 0] {
            let sources = CatalogSources {
                mcp: (0..size)
                    .map(|index| McpCapabilitySpec {
                        server: "prx_server".into(),
                        tool: format!("write_{index}"),
                        state_digest: [0; 32],
                        spec: ToolSpec {
                            name: format!("mcp__prx_server__write_{index}"),
                            description: "PRX unrelated write fixture".into(),
                            parameters: json!({"type":"object","properties":{}}),
                        },
                    })
                    .collect(),
                ..CatalogSources::default()
            };
            let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
            let mut trusted = fixtures::trusted();
            trusted.catalog_generation = *catalog.generation();
            trusted.owner_id = Some(std::sync::Arc::from("prx_user"));
            trusted.resolved_model = std::sync::Arc::from("prx_model");
            trusted.delivery.interactive = false;
            trusted.delivery.can_ask_question = true;
            state.trusted = TrustedState::from_runtime(trusted);
            let cost = CapabilityCostModel {
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
            let policy = CandidatePolicy {
                key: &key,
                max_candidates: 3,
                can_deliver_text: true,
                may_delegate: false,
                delegation_target: None,
                cost_model: &cost,
                discovered_refs: &EMPTY_DISCOVERED_REFS,
            };
            let security = SecurityPolicy::default();
            let calls = BTreeMap::new();
            let history = BTreeSet::new();
            let gates = GateInputs {
                policy: &security,
                owner_id: "prx_user",
                sender: "prx_user",
                policy_channel: "terminal",
                chat_type: "dm",
                model_ref: "prx_model",
                model_can_chat: true,
                model_can_call_tools: true,
                decision_endpoint_ready: true,
                child_scope: None,
                verified_approval_calls: BTreeMap::new(),
                call_fingerprints: &calls,
                failed_call_counts: BTreeMap::new(),
                replayed_non_idempotent: &history,
                retry_limit: Some(2),
            };
            let view = catalog.view(&CatalogScope::default());
            let (generated, allocations) = crate::decision::builtin::allocation_counter::count(|| {
                CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap()
            });
            let shape = format!("{generated:?}");
            let CandidateSet::Legal { candidates, .. } = &generated else {
                panic!("expected terminal candidates");
            };
            assert_eq!(candidates.len(), 2);
            assert!(
                candidates
                    .iter()
                    .all(|candidate| candidate.required_capabilities().is_empty())
            );
            if let Some((expected_shape, expected_allocations)) = &baseline {
                assert_eq!(
                    &shape, expected_shape,
                    "complete candidates and gate reports at size {size}"
                );
                assert_eq!(
                    &allocations, expected_allocations,
                    "generator allocations at size {size}"
                );
            } else {
                baseline = Some((shape, allocations));
            }
        }
    }

    #[test]
    fn noninteractive_surface_never_emits_clarify_even_if_question_flag_is_set() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let catalog = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.delivery.interactive = false;
        trusted.delivery.can_ask_question = true;
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = CapabilityCostModel {
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
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 3,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let security = SecurityPolicy::default();
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(2),
        };
        let generated =
            CausalCandidateGenerator::generate(&state, &catalog.view(&CatalogScope::default()), &policy, &gates)
                .unwrap();
        let CandidateSet::Legal { candidates, .. } = generated else {
            panic!("expected answer");
        };
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.action() != CausalActionKind::Clarify)
        );
    }

    #[test]
    fn approval_only_path_carries_concrete_operation_scope() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "unmatched request");
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "write".into(),
            state_digest: [1; 32],
            spec: ToolSpec {
                name: "mcp__prx_server__write".into(),
                description: "Write remote data".into(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        });
        let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([(
                CapabilityTarget::Mcp,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            )]),
        };
        let cost = fixture_cost_for_catalog(&catalog, cost);
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Supervised;
        let calls_seen = BTreeSet::<[u8; 32]>::new();
        let calls = BTreeMap::new();
        let mut gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &calls_seen,
            retry_limit: Some(2),
        };
        let view = catalog.view(&CatalogScope::default());
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        assert!(
            matches!(generated, CandidateSet::NoLegalPath {
                reason: NoLegalReason::ApprovalScopeUnavailable,
                approval_only, ..
            } if approval_only.is_empty()),
            "a proposed capability has no action-bound approval scope"
        );
        let calls_with = BTreeMap::from([(
            CandidateId::new(CausalActionKind::Discover, CapabilityTarget::Mcp, None),
            [3; 32],
        )]);
        gates.call_fingerprints = &calls_with;
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        assert!(matches!(generated, CandidateSet::NoLegalPath { approval_only, .. }
            if approval_only.iter().any(|pending| !pending.op_ids.is_empty())));
        gates.model_can_chat = false;
        gates.decision_endpoint_ready = false;
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        let CandidateSet::NoLegalPath {
            approval_only, report, ..
        } = generated
        else {
            panic!("expected fixed-template closure");
        };
        assert!(!approval_only.is_empty());
        let approval_id = CandidateId::new(CausalActionKind::RequestApproval, CapabilityTarget::Mcp, None);
        assert_eq!(report.outcomes.get(&approval_id), Some(&GateOutcome::Passed));
        gates.model_can_chat = true;
        gates.decision_endpoint_ready = true;
        let mut restricted = state.trusted.parts().clone();
        restricted.delivery.approval_channel = false;
        state.trusted = TrustedState::from_runtime(restricted);
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        assert!(matches!(generated, CandidateSet::NoLegalPath { approval_only, .. } if approval_only.is_empty()));
        let mut restored = state.trusted.parts().clone();
        restored.delivery.approval_channel = true;
        state.trusted = TrustedState::from_runtime(restored);
        gates.model_can_call_tools = false;
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        assert!(matches!(generated, CandidateSet::NoLegalPath { approval_only, .. } if approval_only.is_empty()));
        gates.model_can_call_tools = true;
        let mut legal_cost = cost.clone();
        legal_cost.estimates.insert(
            CapabilityTarget::None,
            (
                CostEstimate {
                    tokens: 1,
                    micro_usd: 0,
                },
                1,
            ),
        );
        let legal_policy = CandidatePolicy {
            cost_model: &legal_cost,
            ..policy
        };
        let generated = CausalCandidateGenerator::generate(&state, &view, &legal_policy, &gates).unwrap();
        let CandidateSet::Legal {
            approval_scopes,
            candidates,
            ..
        } = generated
        else {
            panic!("expected legal answer");
        };
        let request = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::RequestApproval)
            .unwrap();
        let scopes = approval_scopes.get(request.candidate_id()).unwrap();
        assert!(
            scopes
                .iter()
                .any(|pending| pending.candidate.target() == CapabilityTarget::Mcp
                    && !pending.op_ids.is_empty()
                    && !pending.candidate.required_capabilities().is_empty())
        );

        // Exact mention also proposes UseTool. The same argumentless turn
        // must not publish a grant for either that call or discovery.
        gates.call_fingerprints = &calls;
        state.routing_input = RawRoutingInput::new("mcp__prx_server__write".into());
        let generated = CausalCandidateGenerator::generate(&state, &view, &legal_policy, &gates).unwrap();
        let CandidateSet::Legal {
            approval_scopes,
            candidates,
            ..
        } = generated
        else {
            panic!("expected a terminal answer path");
        };
        assert!(approval_scopes.is_empty());
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.action() != CausalActionKind::RequestApproval)
        );
    }

    #[test]
    fn explicit_delegator_selects_only_requested_entrypoint_and_keeps_hard_gates() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "review project without a tool-name mention");
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(ProbeTool("delegate")),
            Box::new(ProbeTool("sessions_spawn")),
            Box::new(ProbeTool("file_read")),
        ];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = fixture_cost_for_catalog(
            &catalog,
            CapabilityCostModel {
                per_capability: BTreeMap::new(),
                estimates: [
                    CapabilityTarget::Session,
                    CapabilityTarget::NativeTool,
                    CapabilityTarget::None,
                ]
                .into_iter()
                .map(|family| {
                    (
                        family,
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
            },
        );
        let mut policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: true,
            may_delegate: true,
            delegation_target: Some("sessions_spawn"),
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let security = SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Full,
            ..SecurityPolicy::default()
        };
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let mut gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(2),
        };
        let view = catalog.view(&CatalogScope::default());
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } = generated
        else {
            panic!("configured delegator must have a legal candidate");
        };
        let delegate = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::Delegate)
            .unwrap();
        assert_eq!(delegate.required_capabilities().len(), 1);
        let entry = view
            .entry_any(&delegate.required_capabilities().first().unwrap().id)
            .unwrap();
        assert_eq!(entry.public_name, "sessions_spawn");
        assert!(exact_mentions.contains(delegate.candidate_id()));
        assert!(candidates.iter().all(|candidate| matches!(
            candidate.action(),
            CausalActionKind::Delegate
                | CausalActionKind::Clarify
                | CausalActionKind::Refuse
                | CausalActionKind::RequestApproval
        )));
        for missing_target in [false, true] {
            gates.model_can_call_tools = missing_target;
            policy.delegation_target = Some(if missing_target {
                "prx_missing"
            } else {
                "sessions_spawn"
            });
            let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
            let candidates = match generated {
                CandidateSet::Legal { candidates, .. } => candidates,
                CandidateSet::NoLegalPath { terminals, .. } => terminals,
            };
            assert!(
                candidates.iter().all(|candidate| matches!(
                    candidate.action(),
                    CausalActionKind::Clarify | CausalActionKind::Refuse
                ))
            );
        }

        gates.model_can_call_tools = true;
        policy.delegation_target = None;
        state.routing_input = RawRoutingInput::new("use sessions_spawn for this task".into());
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } = generated
        else {
            panic!("named delegator must have a legal candidate");
        };
        let delegate = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::Delegate)
            .unwrap();
        let entry = view
            .entry_any(&delegate.required_capabilities().first().unwrap().id)
            .unwrap();
        assert_eq!(entry.public_name, "sessions_spawn");
        assert!(exact_mentions.contains(delegate.candidate_id()));
    }

    #[test]
    fn delegate_without_an_action_scope_cannot_offer_approval() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "neutral request");
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_service".into(),
            tool: "delegate".into(),
            state_digest: [2; 32],
            spec: ToolSpec {
                name: "delegate".into(),
                description: "Delegate a task".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([(
                CapabilityTarget::Session,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            )]),
        };
        let cost = fixture_cost_for_catalog(&catalog, cost);
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: false,
            may_delegate: true,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Supervised;
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(2),
        };
        let view = catalog.view(&CatalogScope::default());
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        let CandidateSet::NoLegalPath {
            reason,
            approval_only,
            report,
            ..
        } = generated
        else {
            panic!("expected a typed terminal path");
        };
        assert_eq!(reason, NoLegalReason::ApprovalScopeUnavailable);
        assert!(approval_only.is_empty());
        assert!(matches!(
            report.outcomes.get(&CandidateId::new(
                CausalActionKind::Delegate,
                CapabilityTarget::Session,
                None
            )),
            Some(GateOutcome::ApprovalOnly { .. })
        ));
        let terminal = CausalCandidateGenerator::generate_terminal_only(&state, &view, &policy, &gates)
            .expect("fresh terminal path");
        let CandidateSet::NoLegalPath {
            reason,
            terminals,
            approval_only,
            report,
        } = terminal
        else {
            panic!("terminal-only recovery must not inherit work diagnostics");
        };
        assert_eq!(reason, NoLegalReason::NoLegalCandidate);
        assert!(approval_only.is_empty());
        assert!(
            terminals
                .iter()
                .all(|candidate| matches!(candidate.action(), CausalActionKind::Clarify | CausalActionKind::Refuse))
        );
        assert!(
            terminals
                .iter()
                .any(|candidate| candidate.action() == CausalActionKind::Refuse)
        );
        assert!(
            report
                .outcomes
                .keys()
                .all(|id| { terminals.iter().any(|candidate| candidate.candidate_id() == id) })
        );
    }

    struct ProbeTool(&'static str);

    struct MemoryProbeTool(&'static str);

    #[async_trait]
    impl Tool for MemoryProbeTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "PRX memory fixture"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
            &[crate::tools::traits::ToolCategory::Memory]
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn selected_read_candidates_require_a_representable_exact_source_query() {
        let key = fixtures::key();
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(MemoryProbeTool("memory_search")),
            Box::new(ProbeTool("image_info")),
            Box::new(ProbeTool("file_read")),
        ];
        let surface = SurfacePolicy {
            allowed_tools: None,
            direct_names: BTreeSet::from(["file_read".to_owned()]),
            ..SurfacePolicy::default()
        };
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &surface)
            .expect("read-query boundary catalog");
        let view = catalog.view(&CatalogScope::default());
        let cost = fixture_cost_for_catalog(
            &catalog,
            CapabilityCostModel {
                per_capability: BTreeMap::new(),
                estimates: [
                    CapabilityTarget::Memory,
                    CapabilityTarget::NativeTool,
                    CapabilityTarget::None,
                ]
                .into_iter()
                .map(|family| {
                    (
                        family,
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
            },
        );
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Full;
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "prx_user",
            sender: "prx_user",
            policy_channel: "cli",
            chat_type: "private",
            model_ref: "ollama:prx-local-model",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(0),
        };
        let mut expected_supported = None;
        let mut expected_other = None;
        for (query, supported) in [
            (String::new(), true),
            ("neutral query".to_owned(), true),
            ("a".repeat(1024), true),
            ("\u{e9}".repeat(512), true),
            ("a".repeat(1025), false),
            (format!("{}a", "\u{e9}".repeat(512)), false),
            ("a".repeat(16384), false),
        ] {
            let mut state = fixtures::state(&key, &query);
            let mut trusted = fixtures::trusted();
            trusted.catalog_generation = view.generation();
            trusted.owner_id = Some(Arc::from("prx_user"));
            trusted.resolved_model = Arc::from("ollama:prx-local-model");
            trusted.channel = None;
            trusted.side_effect_mode = SideEffectMode::GuardedWrite;
            trusted.constraints.clear();
            state.trusted = TrustedState::from_runtime(trusted);
            let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates)
                .expect("normal gated generation for valid raw routing state");
            let CandidateSet::Legal { candidates, report, .. } = generated else {
                panic!("bounded tools and Answer must remain legal");
            };
            let (reads, other): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|candidate| {
                matches!(
                    candidate.action(),
                    CausalActionKind::Retrieve | CausalActionKind::Discover
                )
            });
            assert!(
                other
                    .iter()
                    .any(|candidate| candidate.action() == CausalActionKind::UseTool)
            );
            assert!(
                other
                    .iter()
                    .any(|candidate| candidate.action() == CausalActionKind::Answer)
            );
            for candidate in other.iter().chain(&reads) {
                assert_eq!(
                    report.outcomes.get(candidate.candidate_id()),
                    Some(&GateOutcome::Passed)
                );
            }
            if supported {
                assert!(
                    reads
                        .iter()
                        .any(|candidate| candidate.action() == CausalActionKind::Retrieve)
                );
                assert!(
                    reads
                        .iter()
                        .any(|candidate| candidate.action() == CausalActionKind::Discover)
                );
                if let Some(expected) = &expected_supported {
                    assert_eq!(&reads, expected, "full refs and order must remain stable");
                } else {
                    expected_supported = Some(reads);
                }
            } else {
                assert!(
                    reads.is_empty(),
                    "unrepresentable exact source query must not advertise local Reads"
                );
            }
            if let Some(expected) = &expected_other {
                assert_eq!(&other, expected, "UseTool and terminal bodies must remain stable");
            } else {
                expected_other = Some(other);
            }
        }
    }

    #[test]
    fn retrieve_admits_only_exact_read_only_memory_search_on_default_searchable_surface() {
        let key = fixtures::key();
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(MemoryProbeTool("memory_forget")),
            Box::new(MemoryProbeTool("memory_search")),
            Box::new(MemoryProbeTool("memory_store")),
        ];
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
            .expect("memory catalog");
        let view = catalog.view(&CatalogScope::default());
        let eligible: Vec<_> = view
            .entries()
            .iter()
            .filter(|entry| is_memory_retrieval_source(entry))
            .collect();
        assert_eq!(eligible.len(), 1);
        let search = eligible.first().expect("one read-only search source");
        assert_eq!(search.public_name, "memory_search");
        assert!(matches!(search.exposure, Exposure::Searchable));
        assert_eq!(search.effect, SideEffectClass::ReadOnly);
        for name in ["memory_forget", "memory_store"] {
            let write = view
                .entries()
                .iter()
                .find(|entry| entry.public_name == name)
                .expect("write fixture");
            assert_eq!(write.effect, SideEffectClass::LocalWrite);
        }
        let document_registry: Vec<Box<dyn Tool>> = vec![Box::new(MemoryProbeTool("document_search"))];
        let document_catalog = CapabilityCatalog::build(
            &document_registry,
            &CatalogSources::default(),
            &SurfacePolicy::default(),
        )
        .expect("document fixture");
        assert!(
            document_catalog
                .view(&CatalogScope::default())
                .entries()
                .iter()
                .all(|entry| !is_memory_retrieval_source(entry))
        );
        let document_view = document_catalog.view(&CatalogScope::default());
        let document = document_view
            .entries()
            .iter()
            .copied()
            .find(|entry| entry.public_name == "document_search")
            .expect("document anchor");
        assert!(is_document_retrieval_source(&document));
        let mut changed = (*document).clone();
        changed.effect = SideEffectClass::LocalWrite;
        assert!(!is_document_retrieval_source(&changed));
        changed = (*document).clone();
        changed.root = "document_import".to_owned();
        assert!(!is_document_retrieval_source(&changed));
        let mut state = fixtures::state(&key, "prx memory question");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = view.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        trusted.constraints.clear();
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = fixture_cost_for_catalog(
            &catalog,
            CapabilityCostModel {
                per_capability: BTreeMap::new(),
                estimates: BTreeMap::from([
                    (
                        CapabilityTarget::Memory,
                        (
                            CostEstimate {
                                tokens: 1,
                                micro_usd: 0,
                            },
                            1,
                        ),
                    ),
                    (
                        CapabilityTarget::None,
                        (
                            CostEstimate {
                                tokens: 1,
                                micro_usd: 0,
                            },
                            1,
                        ),
                    ),
                ]),
            },
        );
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Full;
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "cli",
            chat_type: "private",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(0),
        };
        let write = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "memory_forget")
            .expect("first write source");
        assert_eq!(
            view.entries().first().expect("ordered catalog").public_name,
            "memory_forget"
        );
        let wrongly_labeled = from_entry(
            CausalActionKind::Retrieve,
            CapabilityTarget::Memory,
            write,
            &policy,
            None,
        );
        let old_gate = HardGates::apply(
            GatePass::PrePrediction,
            &state,
            &view,
            std::slice::from_ref(&wrongly_labeled),
            &gates,
        )
        .expect("permissive gate");
        assert_eq!(
            old_gate.outcomes.get(wrongly_labeled.candidate_id()),
            Some(&GateOutcome::Passed),
            "the old broad Memory loop could select a write tool as Retrieve"
        );
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        let CandidateSet::Legal { candidates, .. } = generated else {
            panic!("safe Memory retrieval should be legal: {generated:?}");
        };
        let retrieves: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.action() == CausalActionKind::Retrieve)
            .collect();
        assert_eq!(retrieves.len(), 1);
        let selected = retrieves.first().expect("one selected Retrieve");
        let source = selected.required_capabilities().first().expect("Retrieve source");
        assert_eq!(source.id, search.id);
    }

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Read local evidence"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn read_only_family_selection_skips_write_probe_before_pricing() {
        let key = fixtures::key();
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_write"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let entry = *view.entries().first().expect("one write capability");
        assert!(entry.effect > SideEffectClass::ReadOnly);
        let cost = fixture_cost_for_catalog(
            &catalog,
            CapabilityCostModel {
                per_capability: BTreeMap::new(),
                estimates: BTreeMap::from([(
                    CapabilityTarget::NativeTool,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                )]),
            },
        );
        let counting_cost = CountingCost {
            inner: &cost,
            entries: AtomicUsize::new(0),
        };
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 3,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &counting_cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Full;
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(2),
        };
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        state.trusted = TrustedState::from_runtime(trusted.clone());
        assert_eq!(
            HardGates::check_one(
                GatePass::PrePrediction,
                &state,
                &view,
                &from_entry(CausalActionKind::Discover, entry.family, entry, &policy, None),
                &gates,
            ),
            GateOutcome::Blocked {
                gate: 9,
                reason_code: "explicit_restriction",
            }
        );
        counting_cost.entries.store(0, Ordering::Relaxed);
        let skipped = select_family_candidate(
            CausalActionKind::Discover,
            entry.family,
            [entry],
            &state,
            &view,
            &policy,
            &gates,
        )
        .unwrap();
        assert!(skipped.is_none());
        assert_eq!(counting_cost.entries.load(Ordering::Relaxed), 0);

        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let considered = select_family_candidate(
            CausalActionKind::Discover,
            entry.family,
            [entry],
            &state,
            &view,
            &policy,
            &gates,
        )
        .unwrap();
        assert!(considered.is_some());
        assert_eq!(counting_cost.entries.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn exact_cost_binding_cannot_price_another_same_family_capability() {
        let key = fixtures::key();
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read")), Box::new(ProbeTool("prx_search"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let entries = view.entries();
        assert_eq!(entries.len(), 2);
        let first = *entries.first().expect("first fixture capability");
        let second = *entries.get(1).expect("second fixture capability");
        assert_eq!(first.family, second.family);
        let first_ref = CapabilityRef {
            id: first.id.clone(),
            fingerprint: first.fingerprint,
            generation: first.generation,
        };
        let exact = CostEstimate {
            tokens: 17,
            micro_usd: 3,
        };
        let cost = CapabilityCostModel {
            per_capability: BTreeMap::from([(first_ref, (exact, 77))]),
            estimates: BTreeMap::from([(
                first.family,
                (
                    CostEstimate {
                        tokens: 1,
                        micro_usd: 0,
                    },
                    1,
                ),
            )]),
        };
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let priced = from_entry(CausalActionKind::UseTool, first.family, first, &policy, None);
        let other = from_entry(CausalActionKind::UseTool, second.family, second, &policy, None);
        assert_eq!(priced.estimated_cost(), Some(exact));
        assert_eq!(priced.estimated_latency_ms(), Some(77));
        assert_eq!(other.estimated_cost(), None);
        assert_eq!(other.estimated_latency_ms(), None);
        let empty_exact = CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: cost.estimates.clone(),
        };
        let empty_policy = CandidatePolicy {
            cost_model: &empty_exact,
            ..policy
        };
        let unpriced = from_entry(CausalActionKind::UseTool, first.family, first, &empty_policy, None);
        assert_eq!(
            unpriced.estimated_cost(),
            None,
            "a family estimate never authorizes a capability"
        );
    }

    #[test]
    fn exact_mention_dominates_same_entry_family_and_unmatched_input_keeps_fallback() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "first unrelated request");
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read")), Box::new(ProbeTool("prx_search"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([
                (
                    CapabilityTarget::None,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                ),
                (
                    CapabilityTarget::NativeTool,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                ),
            ]),
        };
        let cost = fixture_cost_for_catalog(&catalog, cost);
        let counting_cost = CountingCost {
            inner: &cost,
            entries: AtomicUsize::new(0),
        };
        let view = catalog.view(&CatalogScope::default());
        let discovered_all: BTreeSet<_> = view
            .entries()
            .iter()
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .collect();
        let discovered_all = DiscoveredRefs::from_refs_for_test(discovered_all);
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &counting_cost,
            discovered_refs: &discovered_all,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Full;
        let calls_seen = BTreeSet::<[u8; 32]>::new();
        let calls = BTreeMap::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &calls_seen,
            retry_limit: Some(2),
        };
        let CandidateSet::Legal { candidates, .. } =
            CausalCandidateGenerator::generate(&state, &view, &policy, &gates).expect("baseline")
        else {
            panic!("legal baseline");
        };
        let baseline = candidates
            .iter()
            .find(|candidate| candidate.action() == CausalActionKind::UseTool)
            .expect("unmatched input has family fallback");
        let fallback_id = &baseline.required_capabilities().first().expect("fallback entry").id;
        let first = view
            .entries()
            .iter()
            .find(|entry| &entry.id == fallback_id)
            .expect("fallback catalog entry");
        let other = view
            .entries()
            .iter()
            .find(|entry| &entry.id != fallback_id)
            .expect("different same-family entry");
        for (input, blocked, expected_exact, expected_tools) in [
            (format!("use {}", first.public_name), false, true, 1),
            ("unrelated request".to_owned(), false, false, 1),
            (format!("use {}", other.public_name), false, true, 2),
            (format!("use {}", first.public_name), true, false, 1),
        ] {
            state.routing_input = RawRoutingInput::new(input);
            let mut trusted = state.trusted.parts().clone();
            trusted.exclusions.clear();
            if blocked {
                trusted.exclusions.push(first.public_name.clone().into());
            }
            state.trusted = TrustedState::from_runtime(trusted);
            let CandidateSet::Legal {
                candidates,
                exact_mentions,
                ..
            } = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).expect("legal fixture")
            else {
                panic!("expected admitted capability");
            };
            let tools = candidates
                .iter()
                .filter(|candidate| candidate.action() == CausalActionKind::UseTool)
                .collect::<Vec<_>>();
            assert_eq!(tools.len(), expected_tools);
            assert_eq!(!exact_mentions.is_empty(), expected_exact);
            for entry in [first, other] {
                let count = tools
                    .iter()
                    .filter(|candidate| {
                        candidate
                            .required_capabilities()
                            .iter()
                            .any(|reference| reference.id == entry.id)
                    })
                    .count();
                assert!(count <= 1, "exact and family cannot duplicate an entry");
            }
            if blocked {
                assert!(
                    tools.iter().all(|candidate| candidate
                        .required_capabilities()
                        .iter()
                        .all(|reference| reference.id != first.id)),
                    "blocked exact entry remains excluded"
                );
                assert!(
                    tools.iter().any(|candidate| candidate
                        .required_capabilities()
                        .iter()
                        .any(|reference| reference.id == other.id)),
                    "different fallback survives blocked exact mention"
                );
            } else {
                assert!(
                    tools.iter().any(|candidate| candidate
                        .required_capabilities()
                        .iter()
                        .any(|reference| reference.id == first.id)),
                    "original family entry remains represented"
                );
            }
        }
    }

    #[test]
    fn unrelated_user_text_preserves_candidates_and_exact_name_adds_only_its_pair() {
        let key = fixtures::key();
        let mut state = fixtures::state(&key, "first unrelated request");
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_read")), Box::new(ProbeTool("prx_search"))];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted);
        let cost = CapabilityCostModel {
            per_capability: BTreeMap::new(),
            estimates: BTreeMap::from([
                (
                    CapabilityTarget::None,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                ),
                (
                    CapabilityTarget::NativeTool,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                ),
            ]),
        };
        let cost = fixture_cost_for_catalog(&catalog, cost);
        let counting_cost = CountingCost {
            inner: &cost,
            entries: AtomicUsize::new(0),
        };
        let view = catalog.view(&CatalogScope::default());
        let discovered_all: BTreeSet<_> = view
            .entries()
            .iter()
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .collect();
        let discovered_all = DiscoveredRefs::from_refs_for_test(discovered_all);
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 12,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &counting_cost,
            discovered_refs: &discovered_all,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Full;
        let calls_seen = BTreeSet::<[u8; 32]>::new();
        let calls = BTreeMap::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &calls_seen,
            retry_limit: Some(2),
        };
        let ids = |state: &CausalState| match CausalCandidateGenerator::generate(state, &view, &policy, &gates).unwrap()
        {
            CandidateSet::Legal {
                candidates,
                exact_mentions,
                ..
            } => (
                candidates
                    .iter()
                    .map(|candidate| candidate.candidate_id().clone())
                    .collect::<Vec<_>>(),
                exact_mentions,
            ),
            CandidateSet::NoLegalPath { .. } => panic!("expected legal candidates"),
        };
        let baseline = ids(&state);
        assert!(
            counting_cost.entries.load(Ordering::Relaxed) > 0,
            "ordinary candidate generation must request exact entry estimates"
        );
        let search_entry = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "prx_search")
            .expect("second searchable fixture");
        let search_ref = CapabilityRef {
            id: search_entry.id.clone(),
            fingerprint: search_entry.fingerprint,
            generation: search_entry.generation,
        };
        let only_search = DiscoveredRefs::from_refs_for_test(BTreeSet::from([search_ref]));
        let prior_policy = CandidatePolicy {
            discovered_refs: &only_search,
            ..policy
        };
        let generic_tool = |policy: &CandidatePolicy<'_>| {
            let CandidateSet::Legal { candidates, .. } =
                CausalCandidateGenerator::generate(&state, &view, policy, &gates).expect("legal candidates")
            else {
                panic!("expected a legal generic tool");
            };
            candidates
                .into_iter()
                .find(|candidate| {
                    candidate.action() == CausalActionKind::UseTool
                        && candidate.target() == CapabilityTarget::NativeTool
                })
                .expect("one generic native tool")
        };
        let prior_generic = generic_tool(&prior_policy);
        let discovered_generic = generic_tool(&policy);
        assert_eq!(prior_generic.candidate_id(), discovered_generic.candidate_id());
        assert_ne!(
            prior_generic.required_capabilities(),
            discovered_generic.required_capabilities(),
            "same-family discovery can change the exact bound ref without changing the candidate ID"
        );
        let mut prior_state = state.clone();
        prior_state.derived =
            crate::causal_tree::state::DerivedState::new(vec![crate::causal_tree::state::DerivedCandidate {
                candidate: prior_generic,
                summary: Arc::from("fixture"),
                exact_mention: false,
                available: true,
            }])
            .expect("one prior candidate");
        let mut discovered_state = state.clone();
        discovered_state.derived =
            crate::causal_tree::state::DerivedState::new(vec![crate::causal_tree::state::DerivedCandidate {
                candidate: discovered_generic,
                summary: Arc::from("fixture"),
                exact_mention: false,
                available: true,
            }])
            .expect("one discovered candidate");
        assert_eq!(
            prior_state.evidence_fingerprint(&key),
            discovered_state.evidence_fingerprint(&key)
        );
        assert_ne!(prior_state.derived.candidates(), discovered_state.derived.candidates());
        let mut merged = only_search.clone();
        assert!(
            !merged.extend_selected(DiscoveredRefs::empty()),
            "empty search makes no progress"
        );
        assert!(
            !merged.extend_selected(only_search.clone()),
            "repeat search makes no progress"
        );
        assert!(
            merged.extend_selected(discovered_all.clone()),
            "new exact ref makes progress"
        );
        let before_discovery = CandidatePolicy {
            discovered_refs: &EMPTY_DISCOVERED_REFS,
            ..policy
        };
        let CandidateSet::Legal { candidates, .. } =
            CausalCandidateGenerator::generate(&state, &view, &before_discovery, &gates).unwrap()
        else {
            panic!("expected legal candidates before discovery");
        };
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.action() != CausalActionKind::UseTool),
            "searchable tools require exact mention or turn-scoped discovery"
        );
        let discovered_entry = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "prx_read")
            .unwrap();
        // This exercises candidate identity matching only. Real source/grant
        // issuance is covered by the recorded Chat collection/resume fixture.
        let resumed_ref = CapabilityRef {
            id: discovered_entry.id.clone(),
            fingerprint: discovered_entry.fingerprint,
            generation: discovered_entry.generation,
        };
        let CandidateSet::Legal { candidates, .. } = CausalCandidateGenerator::generate_with_resume_target(
            &state,
            &view,
            &before_discovery,
            &gates,
            Some(&resumed_ref),
        )
        .unwrap() else {
            panic!("expected legal candidates for exact resume target");
        };
        let work: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.action() == CausalActionKind::UseTool)
            .collect();
        assert_eq!(work.len(), 1);
        assert_eq!(
            work.first().unwrap().required_capabilities(),
            std::slice::from_ref(&resumed_ref)
        );
        for stale_schema in [true, false] {
            let mut stale_resume = resumed_ref.clone();
            if stale_schema {
                *stale_resume.fingerprint.0.first_mut().unwrap() ^= 1;
            } else {
                *stale_resume.generation.0.first_mut().unwrap() ^= 1;
            }
            let CandidateSet::Legal { candidates, .. } = CausalCandidateGenerator::generate_with_resume_target(
                &state,
                &view,
                &before_discovery,
                &gates,
                Some(&stale_resume),
            )
            .unwrap() else {
                panic!("expected terminal candidates for stale resume target");
            };
            assert!(
                candidates
                    .iter()
                    .all(|candidate| candidate.action() != CausalActionKind::UseTool)
            );
        }
        let stale = DiscoveredRefs::from_refs_for_test(BTreeSet::from([CapabilityRef {
            id: discovered_entry.id.clone(),
            fingerprint: crate::causal_tree::branch::SchemaFingerprint([0; 32]),
            generation: discovered_entry.generation,
        }]));
        let stale_policy = CandidatePolicy {
            discovered_refs: &stale,
            ..policy
        };
        let CandidateSet::Legal { candidates, .. } =
            CausalCandidateGenerator::generate(&state, &view, &stale_policy, &gates).unwrap()
        else {
            panic!("expected legal candidates with stale reference");
        };
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.action() != CausalActionKind::UseTool)
        );
        let discovered = DiscoveredRefs::from_refs_for_test(BTreeSet::from([CapabilityRef {
            id: discovered_entry.id.clone(),
            fingerprint: discovered_entry.fingerprint,
            generation: discovered_entry.generation,
        }]));
        let after_discovery = CandidatePolicy {
            discovered_refs: &discovered,
            ..policy
        };
        let CandidateSet::Legal { candidates, .. } =
            CausalCandidateGenerator::generate(&state, &view, &after_discovery, &gates).unwrap()
        else {
            panic!("expected legal candidates after discovery");
        };
        assert!(candidates.iter().any(|candidate| {
            candidate.action() == CausalActionKind::UseTool
                && candidate
                    .required_capabilities()
                    .iter()
                    .any(|reference| reference.id == discovered_entry.id)
        }));
        state.routing_input = RawRoutingInput::new("different unrelated request".into());
        assert_eq!(ids(&state), baseline);
        state.routing_input = RawRoutingInput::new("use prx_read".into());
        let exact = ids(&state);
        assert_eq!(exact.1.len(), 1);
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).expect("exact mention candidates")
        else {
            panic!("expected admitted exact mention");
        };
        let exact_candidate = candidates
            .iter()
            .find(|candidate| exact_mentions.contains(candidate.candidate_id()))
            .expect("exact mention retained");
        let exact_ref = exact_candidate
            .required_capabilities()
            .first()
            .expect("exact entry reference");
        assert!(
            candidates.iter().all(|candidate| {
                candidate.candidate_id() == exact_candidate.candidate_id()
                    || !candidate
                        .required_capabilities()
                        .iter()
                        .any(|reference| reference.id == exact_ref.id)
                    || candidate.action() != CausalActionKind::UseTool
            }),
            "no family duplicate of the exact entry"
        );
        assert!(
            baseline.0.iter().any(|id| id.as_str() == "use_tool-native_tool"),
            "unmatched input retains its family fallback"
        );
        let mut trusted = state.trusted.parts().clone();
        trusted.exclusions.push("prx_read".into());
        state.trusted = TrustedState::from_runtime(trusted);
        state.routing_input = RawRoutingInput::new("unrelated request".into());
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        let CandidateSet::Legal { candidates, .. } = generated else {
            panic!("expected legal fallback");
        };
        let selected = candidates
            .iter()
            .find(|candidate| {
                candidate.action() == CausalActionKind::UseTool && candidate.target() == CapabilityTarget::NativeTool
            })
            .unwrap();
        assert_eq!(
            selected.required_capabilities().first().unwrap().id.as_str(),
            view.entries()
                .iter()
                .find(|entry| entry.public_name == "prx_search")
                .unwrap()
                .id
                .as_str()
        );
        state.routing_input = RawRoutingInput::new("use prx_read and prx_search".into());
        let generated = CausalCandidateGenerator::generate(&state, &view, &policy, &gates).unwrap();
        let CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } = generated
        else {
            panic!("expected legal exact mention");
        };
        assert_eq!(exact_mentions.len(), 1);
        let exact = candidates
            .iter()
            .find(|candidate| exact_mentions.contains(candidate.candidate_id()))
            .unwrap();
        assert_eq!(
            exact.required_capabilities().first().unwrap().id.as_str(),
            selected.required_capabilities().first().unwrap().id.as_str()
        );

        for cap in 0..3 {
            let invalid = CandidatePolicy {
                max_candidates: cap,
                ..policy
            };
            assert!(matches!(
                CausalCandidateGenerator::generate(&state, &view, &invalid, &gates),
                Err(GateError::InvalidCandidateLimit)
            ));
        }
        let small = CandidatePolicy {
            max_candidates: 3,
            ..policy
        };
        let generated = CausalCandidateGenerator::generate(&state, &view, &small, &gates).unwrap();
        assert!(matches!(generated, CandidateSet::Legal { candidates, .. } if candidates.len() <= 3));
    }
    // Preserve the original transient probe algorithm as the differential oracle.
    fn unfiltered_family_candidate<'a>(
        action: CausalActionKind,
        family: CapabilityTarget,
        entries: impl IntoIterator<Item = &'a CapabilityEntry>,
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        gates: &GateInputs<'_>,
    ) -> Result<Option<CausalCandidate>, GateError> {
        let mut approval_only = None;
        for entry in entries {
            // G9 will reject a write effect under a read-only turn. This
            // transient probe cannot become Passed or ApprovalOnly, so avoid
            // constructing and pricing it during family fallback selection.
            if state.trusted.parts().side_effect_mode == SideEffectMode::ReadOnly
                && entry.effect > SideEffectClass::ReadOnly
            {
                continue;
            }
            let candidate = from_entry(action, family, entry, policy, None);
            let outcome = HardGates::check_one(GatePass::PrePrediction, state, catalog, &candidate, gates);
            match outcome {
                GateOutcome::Passed => return Ok(Some(candidate)),
                GateOutcome::ApprovalOnly { op_ids } if !op_ids.is_empty() && approval_only.is_none() => {
                    approval_only = Some(candidate);
                }
                _ => {}
            }
        }
        Ok(approval_only)
    }

    #[test]
    fn borrowed_exclusions_preserve_family_selection_reports_and_stale_layout() {
        let key = fixtures::key();
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(ProbeTool("prx_first")),
            Box::new(ProbeTool("prx_second")),
            Box::new(ProbeTool("prx_third")),
        ];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let cost = fixture_cost_for_catalog(
            &catalog,
            CapabilityCostModel {
                per_capability: BTreeMap::new(),
                estimates: BTreeMap::from([(
                    CapabilityTarget::NativeTool,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                )]),
            },
        );
        let counting_cost = CountingCost {
            inner: &cost,
            entries: AtomicUsize::new(0),
        };
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 3,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &counting_cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Full;
        let calls = BTreeMap::new();
        let history = BTreeSet::new();
        let gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(2),
        };
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        trusted.side_effect_mode = SideEffectMode::GuardedWrite;
        state.trusted = TrustedState::from_runtime(trusted.clone());

        let entries = view.entries();
        let first = *entries.first().expect("first capability");
        let second = *entries.get(1).expect("second capability");
        let third = *entries.get(2).expect("third capability");
        let other_registry: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool("prx_different_layout"))];
        let other_catalog =
            CapabilityCatalog::build(&other_registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        for scenario in 0..6 {
            trusted.exclusions.clear();
            trusted.model_admission = None;
            trusted.channel = None;
            match scenario {
                0 => trusted.exclusions.push(Arc::from(first.public_name.as_str())),
                1 => trusted
                    .exclusions
                    .extend(entries.iter().map(|entry| Arc::from(entry.root.as_str()))),
                2 => {
                    trusted.model_admission = Some(crate::tools::capability::model_policy::ModelAdmission::fresh(
                        &registry,
                        &view,
                        "prx-model",
                        &[],
                        "prx-policy",
                        &key,
                    ))
                }
                3 => {
                    trusted.model_admission = Some(crate::tools::capability::model_policy::ModelAdmission::fresh(
                        &other_registry,
                        &other_catalog.view(&CatalogScope::default()),
                        "prx-model",
                        &[],
                        "prx-policy",
                        &key,
                    ))
                }
                4 => {}
                _ => trusted.channel = Some(Arc::from("terminal")),
            }
            let surface = SurfacePolicy {
                excluded_names: if scenario == 4 {
                    BTreeSet::from([first.public_name.clone()])
                } else {
                    BTreeSet::new()
                },
                ..SurfacePolicy::default()
            };
            let view = catalog.view(&CatalogScope::default()).with_surface(&surface);
            state.trusted = TrustedState::from_runtime(trusted.clone());
            counting_cost.entries.store(0, Ordering::Relaxed);
            let expected = unfiltered_family_candidate(
                CausalActionKind::Discover,
                first.family,
                [first, second, third],
                &state,
                &view,
                &policy,
                &gates,
            )
            .unwrap();
            let reference_pricing = counting_cost.entries.load(Ordering::Relaxed);
            counting_cost.entries.store(0, Ordering::Relaxed);
            let actual = select_family_candidate(
                CausalActionKind::Discover,
                first.family,
                [first, second, third],
                &state,
                &view,
                &policy,
                &gates,
            )
            .unwrap();
            let actual_pricing = counting_cost.entries.load(Ordering::Relaxed);
            assert_eq!(
                actual.as_ref().map(CausalCandidate::candidate_id),
                expected.as_ref().map(CausalCandidate::candidate_id)
            );
            assert_eq!(
                actual.as_ref().map(CausalCandidate::required_capabilities),
                expected.as_ref().map(CausalCandidate::required_capabilities)
            );
            let mut expected_proposed = expected.into_iter().collect::<Vec<_>>();
            expected_proposed.push(terminal(CausalActionKind::Refuse, &policy));
            let mut actual_proposed = actual.into_iter().collect::<Vec<_>>();
            actual_proposed.push(terminal(CausalActionKind::Refuse, &policy));
            for pass in [GatePass::PrePrediction, GatePass::PostRehearsal, GatePass::PreCommit] {
                let expected_report = HardGates::apply(pass, &state, &view, &expected_proposed, &gates).unwrap();
                let actual_report = HardGates::apply(pass, &state, &view, &actual_proposed, &gates).unwrap();
                assert_eq!(actual_report.outcomes, expected_report.outcomes);
                assert_eq!(actual_report.gate_set_hash, expected_report.gate_set_hash);
            }
            if scenario == 3 {
                assert_eq!(
                    actual_pricing, reference_pricing,
                    "stale bitmap must not skip construction"
                );
                assert_eq!(
                    HardGates::check_one(
                        GatePass::PrePrediction,
                        &state,
                        &view,
                        &from_entry(CausalActionKind::Discover, first.family, first, &policy, None),
                        &gates,
                    ),
                    GateOutcome::Blocked {
                        gate: 3,
                        reason_code: "unavailable_or_stale"
                    }
                );
            } else {
                assert!(actual_pricing < reference_pricing, "excluded probes must avoid pricing");
            }
        }
    }
    #[test]
    fn borrowed_prefilter_preserves_first_approval_and_later_passed() {
        let key = fixtures::key();
        let mut sources = CatalogSources::default();
        for name in ["prx_first", "prx_second"] {
            sources.mcp.push(McpCapabilitySpec {
                server: "prx_service".into(),
                tool: name.into(),
                state_digest: [2; 32],
                spec: ToolSpec {
                    name: name.into(),
                    description: "Fixture capability".into(),
                    parameters: json!({"type":"object","properties":{}}),
                },
            });
        }
        let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let cost = fixture_cost_for_catalog(
            &catalog,
            CapabilityCostModel {
                per_capability: BTreeMap::new(),
                estimates: BTreeMap::from([(
                    CapabilityTarget::Mcp,
                    (
                        CostEstimate {
                            tokens: 1,
                            micro_usd: 0,
                        },
                        1,
                    ),
                )]),
            },
        );
        let counting_cost = CountingCost {
            inner: &cost,
            entries: AtomicUsize::new(0),
        };
        let policy = CandidatePolicy {
            key: &key,
            max_candidates: 3,
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &counting_cost,
            discovered_refs: &EMPTY_DISCOVERED_REFS,
        };
        let mut security = SecurityPolicy::default();
        security.autonomy = crate::security::AutonomyLevel::Full;
        let mut calls = BTreeMap::new();
        calls.insert(
            CandidateId::new(CausalActionKind::UseTool, CapabilityTarget::Mcp, None),
            [7; 32],
        );
        let history = BTreeSet::new();
        let mut gates = GateInputs {
            policy: &security,
            owner_id: "owner-1",
            sender: "owner-1",
            policy_channel: "terminal",
            chat_type: "dm",
            model_ref: "anthropic:claude",
            model_can_chat: true,
            model_can_call_tools: true,
            decision_endpoint_ready: true,
            child_scope: None,
            verified_approval_calls: BTreeMap::new(),
            call_fingerprints: &calls,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &history,
            retry_limit: Some(2),
        };
        let mut state = fixtures::state(&key, "neutral request");
        let mut trusted = fixtures::trusted();
        trusted.catalog_generation = *catalog.generation();
        trusted.channel = None;
        trusted.exclusions.clear();
        trusted.side_effect_mode = SideEffectMode::ApprovalRequired;
        state.trusted = TrustedState::from_runtime(trusted.clone());

        let entries = view.entries();
        let first = *entries.first().expect("first MCP capability");
        let second = *entries.get(1).expect("second MCP capability");
        for allow_second in [false, true] {
            gates.verified_approval_calls.clear();
            if allow_second {
                gates.verified_approval_calls.insert(
                    [7; 32],
                    BTreeSet::from([CapabilityRef {
                        id: second.id.clone(),
                        fingerprint: second.fingerprint,
                        generation: second.generation,
                    }]),
                );
            }
            let first_probe = from_entry(CausalActionKind::UseTool, CapabilityTarget::Mcp, first, &policy, None);
            assert!(
                matches!(HardGates::check_one(GatePass::PrePrediction, &state, &view, &first_probe, &gates), GateOutcome::ApprovalOnly { op_ids } if !op_ids.is_empty())
            );
            let second_probe = from_entry(CausalActionKind::UseTool, CapabilityTarget::Mcp, second, &policy, None);
            let second_outcome = HardGates::check_one(GatePass::PrePrediction, &state, &view, &second_probe, &gates);
            if allow_second {
                assert_eq!(second_outcome, GateOutcome::Passed);
            } else {
                assert!(matches!(second_outcome, GateOutcome::ApprovalOnly { op_ids } if !op_ids.is_empty()));
            }
            let expected = unfiltered_family_candidate(
                CausalActionKind::UseTool,
                CapabilityTarget::Mcp,
                [first, second],
                &state,
                &view,
                &policy,
                &gates,
            )
            .unwrap()
            .expect("reference selection");
            let actual = select_family_candidate(
                CausalActionKind::UseTool,
                CapabilityTarget::Mcp,
                [first, second],
                &state,
                &view,
                &policy,
                &gates,
            )
            .unwrap()
            .expect("optimized selection");
            assert_eq!(actual.required_capabilities(), expected.required_capabilities());
            assert_eq!(
                actual.required_capabilities().first().expect("selected ref").id,
                if allow_second { &second.id } else { &first.id }.clone()
            );
            let expected_proposed = [expected, terminal(CausalActionKind::Refuse, &policy)];
            let actual_proposed = [actual, terminal(CausalActionKind::Refuse, &policy)];
            for pass in [GatePass::PrePrediction, GatePass::PostRehearsal, GatePass::PreCommit] {
                let expected_report = HardGates::apply(pass, &state, &view, &expected_proposed, &gates).unwrap();
                let actual_report = HardGates::apply(pass, &state, &view, &actual_proposed, &gates).unwrap();
                assert_eq!(actual_report.outcomes, expected_report.outcomes);
                assert_eq!(actual_report.gate_set_hash, expected_report.gate_set_hash);
            }
        }
    }
}
