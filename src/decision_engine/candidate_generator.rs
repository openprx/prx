//! Deterministic capability-backed candidate enumeration for the decision plane.
//!
//! Kept separate from the `llm-router`-only legacy expander so the new
//! decision domain also compiles in bare builds.

use std::collections::{BTreeMap, BTreeSet};

use crate::decision_engine::admission_probe::{Admission, AdmissionProbe, GenerationError, ProbeReport};
use crate::decision_engine::candidate::{CandidateId, CandidateSpec, CausalCandidate, Ns8};
use crate::decision_engine::cost_model::CostEstimateSource;
use crate::decision_engine::state::{CausalState, ObservedStepKind, ObservedStepStatus};
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::catalog::{CapabilityEntry, CatalogView, Exposure};
use crate::tools::capability::contract::{
    ApprovalRequirement, CapabilityRef, CapabilityTarget, CausalActionKind, CostEstimate, OpId, SideEffectClass,
};
use crate::tools::capability::turn_state::SideEffectMode;

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

    #[cfg(test)]
    pub(crate) const fn from_refs_for_test(refs: BTreeSet<CapabilityRef>) -> Self {
        Self { refs }
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

/// Every family that metadata discovery may search, in fixed priority order.
const DISCOVERY_FAMILIES: [CapabilityTarget; 12] = [
    CapabilityTarget::Memory,
    CapabilityTarget::Skill,
    CapabilityTarget::Mcp,
    CapabilityTarget::NativeTool,
    CapabilityTarget::Wasm,
    CapabilityTarget::Hook,
    CapabilityTarget::Filesystem,
    CapabilityTarget::Shell,
    CapabilityTarget::Document,
    CapabilityTarget::Web,
    CapabilityTarget::Im,
    CapabilityTarget::Session,
];

#[derive(Debug)]
pub(crate) enum CandidateSet {
    Legal {
        candidates: Vec<CausalCandidate>,
        exact_mentions: BTreeSet<CandidateId>,
        approval_scopes: BTreeMap<CandidateId, Vec<PendingApproval>>,
        report: ProbeReport,
    },
    NoLegalPath {
        /// Admitted terminal bodies retained for fresh post/pre-commit gates.
        terminals: Vec<CausalCandidate>,
        report: ProbeReport,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct PendingApproval {
    pub candidate: CausalCandidate,
    pub op_ids: Vec<OpId>,
}

pub(crate) struct CausalCandidateGenerator;

impl CausalCandidateGenerator {
    pub(crate) fn generate(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        probe: &dyn AdmissionProbe,
    ) -> Result<CandidateSet, GenerationError> {
        Self::generate_mode(state, catalog, policy, probe, false, None)
    }

    fn generate_mode(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        probe: &dyn AdmissionProbe,
        terminal_only: bool,
        resume_target: Option<&CapabilityRef>,
    ) -> Result<CandidateSet, GenerationError> {
        if policy.max_candidates < 3 {
            return Err(GenerationError::InvalidCandidateLimit);
        }
        let mut generation_rejections = BTreeMap::new();
        let mut exact_mentions = BTreeSet::new();
        // Approval-only exact mentions with no bound call that must not turn
        // into a plain Answer (full-name mentions of collectable entries).
        let mut unbound_approval_mentions = BTreeSet::new();
        let mut exact_capability_requires_close = false;
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
                false,
                state,
                catalog,
                policy,
                probe,
                &mut generation_rejections,
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
                    let mut approval_only_unbound = false;
                    let mut selected = None;
                    let mut named_files = Vec::new();
                    for entry in catalog.iter_entries().filter(|entry| {
                        entry.family == family
                            && !matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn")
                            && (exact_operation_mention(&folded_input, entry, catalog)
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
                        let admission = probe.check_one(&candidate).admission;
                        trace_rejected(&candidate, &admission, &mut generation_rejections);
                        // Advisory: every structural exact match takes the
                        // `Passed` arm; the outcome stays an annotation.
                        let admission = if probe.filters() { admission } else { Admission::Passed };
                        match admission {
                            Admission::Passed if resume_target.is_none() && is_native_file_entry(entry) => {
                                named_files.push(candidate);
                            }
                            Admission::Passed => {
                                if named_files.is_empty() {
                                    selected = Some(candidate);
                                }
                                break;
                            }
                            Admission::ApprovalOnly { op_ids } if !op_ids.is_empty() => {
                                // A full-named collectable entry outranks an
                                // earlier bare-alias match in the same family.
                                let unbound = collectable_approval_entry(entry) && names_entry(&folded_input, entry);
                                if approval_only.is_none() || (unbound && !approval_only_unbound) {
                                    approval_only = Some(candidate);
                                    approval_only_unbound = unbound;
                                }
                            }
                            Admission::ApprovalDeferred | Admission::Blocked { gate: 4 | 7 | 9, .. } => {
                                // An exact action that lacks approval scope,
                                // model authority or structural permission is
                                // a denied action request. Do not reinterpret
                                // it as an ordinary answer or substitute a
                                // same-family capability.
                                exact_capability_requires_close = true;
                            }
                            _ => {}
                        }
                    }
                    if let Some(first) = named_files.first() {
                        // One named file plan can require several adapters. Do
                        // not replace write/read intent with its first read.
                        let group = native_file_group(&named_files, first.candidate_id().namespace());
                        let outcome = probe.check_one(&group).admission;
                        trace_rejected(&group, &outcome, &mut generation_rejections);
                        // Advisory: always the structural group.
                        selected = Some(if !probe.filters() || outcome == Admission::Passed {
                            group
                        } else {
                            first.clone()
                        });
                    }
                    // Advisory: `approval_only` is never populated, so the
                    // unbound-approval mentions are never fed from admission.
                    if probe.filters()
                        && selected.is_none()
                        && approval_only_unbound
                        && let Some(candidate) = &approval_only
                    {
                        unbound_approval_mentions.insert(candidate.candidate_id().clone());
                    }
                    if let Some(candidate) = selected.or(approval_only) {
                        exact_mentions.insert(candidate.candidate_id().clone());
                        proposed.push(candidate);
                    }
                }

                if !exact_capability_requires_close {
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
                            false,
                            state,
                            catalog,
                            policy,
                            probe,
                            &mut generation_rejections,
                        )? {
                            // A complete named native plan is narrower than its
                            // generic family fallback. Other operations remain
                            // available through selected filesystem discovery.
                            let exact_entry_present = proposed.iter().any(|prior: &CausalCandidate| {
                                exact_mentions.contains(prior.candidate_id())
                                    && if candidate.required_capabilities().len() == 1 {
                                        prior
                                            .required_capabilities()
                                            .iter()
                                            .any(|exact| candidate.required_capabilities().contains(exact))
                                    } else {
                                        candidate.target() == CapabilityTarget::Filesystem
                                            && prior.target() == CapabilityTarget::Filesystem
                                            && prior.required_capabilities().len() > 1
                                            && prior
                                                .required_capabilities()
                                                .iter()
                                                .all(|exact| candidate.required_capabilities().contains(exact))
                                    }
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
                }
                if !exact_capability_requires_close && state.routing_input.supports_selected_read_query() {
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
                            false,
                            state,
                            catalog,
                            policy,
                            probe,
                            &mut generation_rejections,
                        )? {
                            proposed.push(candidate);
                        }
                    }
                }
                // A later capability_search call supplies its own bounded query.
                // Only initial Discover hops require the raw source to fit that
                // query contract; both paths use the same capability gates.
                let discovery_completed =
                    state.observed.completed_steps().iter().any(|step| {
                        step.kind() == ObservedStepKind::Discover && step.status() == ObservedStepStatus::Ok
                    });
                if !exact_capability_requires_close && !discovery_completed {
                    let mut discovery_anchor = None;
                    for family in DISCOVERY_FAMILIES {
                        let searchable = catalog.iter_entries().filter(|entry| {
                            entry.family == family
                                && entry.supports_metadata_discovery()
                                && !policy.discovered_refs.contains(&CapabilityRef {
                                    id: entry.id.clone(),
                                    fingerprint: entry.fingerprint,
                                    generation: entry.generation,
                                })
                        });
                        if let Some(candidate) = select_family_candidate(
                            CausalActionKind::Discover,
                            family,
                            searchable,
                            false,
                            state,
                            catalog,
                            policy,
                            probe,
                            &mut generation_rejections,
                        )? {
                            // Advisory: every structurally returned family
                            // candidate is eligible and no diagnostic is pushed.
                            let outcome = probe.filters().then(|| probe.check_one(&candidate).admission);
                            let eligible = outcome.as_ref().is_none_or(|outcome| *outcome == Admission::Passed);
                            if eligible && discovery_anchor.is_none() {
                                discovery_anchor = Some(candidate);
                            } else if !eligible
                                && matches!(outcome, Some(Admission::ApprovalOnly { .. }))
                                && state.routing_input.supports_selected_read_query()
                            {
                                // Preserve the exact family candidate only as an
                                // approval diagnostic. It cannot become the
                                // generic local-search path without a verified
                                // action scope.
                                proposed.push(candidate);
                            }
                        }
                    }
                    if policy.can_deliver_text
                        && state.routing_input.supports_selected_read_query()
                        && let Some(anchor) = discovery_anchor
                    {
                        // Discovery is one local catalog action. The ranked search
                        // resolves the family from the query after every eligible
                        // family has passed its own capability gates; separate
                        // per-family candidates create an artificial zero-margin
                        // tie for a language-neutral decision backend.
                        proposed.push(anchor);
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
                    policy.delegation_target.is_some() || exact_delegator.is_some(),
                    state,
                    catalog,
                    policy,
                    probe,
                    &mut generation_rejections,
                )? {
                    if policy.delegation_target.is_some() || exact_delegator.is_some() {
                        exact_mentions.insert(candidate.candidate_id().clone());
                    }
                    proposed.push(candidate);
                }
            }
        }
        if !routing_history
            && policy.can_deliver_text
            && policy.delegation_target.is_none()
            && !exact_capability_requires_close
        {
            proposed.push(terminal(CausalActionKind::Answer, policy));
        }
        if policy.can_deliver_text {
            proposed.push(terminal(CausalActionKind::Clarify, policy));
        }
        proposed.push(terminal(CausalActionKind::Refuse, policy));

        // A duplicate id is a generator bug and is rejected by the probe.
        let initial = probe.check_all(&proposed)?;
        let mut approval_only = Vec::new();
        // Advisory: the initial apply only annotates; no `RequestApproval` is
        // synthesised and `proposed` is unchanged.
        for candidate in proposed.iter().filter(|_| !terminal_only && probe.filters()) {
            if let Some(Admission::ApprovalOnly { op_ids }) = initial.outcomes.get(candidate.candidate_id()) {
                if state.trusted.parts().delivery.approval_channel && !op_ids.is_empty() {
                    // No current producer binds tool arguments, a Discover
                    // query or a Delegate manifest without an exact scope.
                    if probe.check_one(candidate).exact_call_scope {
                        approval_only.push(PendingApproval {
                            candidate: candidate.clone(),
                            op_ids: op_ids.clone(),
                        });
                    }
                }
            }
        }
        let mut approval_scopes = BTreeMap::<CandidateId, Vec<PendingApproval>>::new();
        if state.trusted.parts().delivery.approval_channel && !approval_only.is_empty() {
            let Some(refusal) = proposed.pop() else {
                return Err(GenerationError::MissingRefusal);
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
        let mut report = probe.check_all(&proposed)?;
        // Preserve refusals that removed a probe before final candidate
        // construction. Never override the final admission of a retained ID.
        for (id, outcome) in generation_rejections {
            report.outcomes.entry(id).or_insert(outcome);
        }
        // Advisory: every proposed candidate is retained with its annotation.
        let legal = if probe.filters() {
            proposed
                .into_iter()
                .filter(|candidate| matches!(report.outcomes.get(candidate.candidate_id()), Some(Admission::Passed)))
                .collect::<Vec<_>>()
        } else {
            proposed
        };
        let nonterminal = legal.iter().any(|candidate| {
            !matches!(
                candidate.action(),
                CausalActionKind::Refuse | CausalActionKind::Clarify | CausalActionKind::RequestApproval
            )
        });
        if !nonterminal {
            // Generic supervised calls need arguments before exact OpIds can
            // be derived. CDPB-08 owns the non-executable proposal path.
            // Advisory (`legal == proposed`): reached only for structural
            // reasons, when nothing but terminals was proposed.
            return Ok(CandidateSet::NoLegalPath {
                terminals: legal,
                report,
            });
        }
        let cap = policy.max_candidates;
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
            }
        }
        // An exactly named capability that is approval-only stays recorded as an
        // exact mention: the selector uses it to prefer the approval request,
        // or, with no bound call yet, to refuse a plain answer in its place.
        exact_mentions.retain(|id| {
            kept.iter().any(|candidate| candidate.candidate_id() == id)
                || approval_only
                    .iter()
                    .any(|pending| pending.candidate.candidate_id() == id)
                || unbound_approval_mentions.contains(id)
        });
        approval_scopes.retain(|id, _| kept.iter().any(|candidate| candidate.candidate_id() == id));
        Ok(CandidateSet::Legal {
            candidates: kept,
            exact_mentions,
            approval_scopes,
            report,
        })
    }
}

pub(crate) fn select_family_candidate<'a>(
    action: CausalActionKind,
    family: CapabilityTarget,
    entries: impl IntoIterator<Item = &'a CapabilityEntry>,
    bind_namespace: bool,
    state: &CausalState,
    catalog: &CatalogView<'_>,
    policy: &CandidatePolicy<'_>,
    probe: &dyn AdmissionProbe,
    generation_rejections: &mut BTreeMap<CandidateId, Admission>,
) -> Result<Option<CausalCandidate>, GenerationError> {
    // A selected metadata result must become the next represented operation,
    // rather than being hidden again by an earlier Direct same-family tool.
    // Retain the first passed fallback if the selected ref fails any gate.
    let prefer_discovered = action == CausalActionKind::UseTool
        && policy.discovered_refs.iter().any(|reference| {
            catalog.entry_any(&reference.id).is_some_and(|entry| {
                entry.family == family
                    && entry.fingerprint == reference.fingerprint
                    && entry.generation == reference.generation
            })
        });
    let mut passed_fallback = None;
    let native_files = action == CausalActionKind::UseTool
        && family == CapabilityTarget::Filesystem
        && !bind_namespace
        && !state
            .trusted
            .parts()
            .constraints
            .iter()
            .any(|code| code.as_str() == "resume_exact_call");
    let mut file_members = Vec::new();
    let mut approval_only = None;
    for entry in entries {
        // G9 will reject a write effect under a read-only turn. This
        // transient probe cannot become Passed or ApprovalOnly, so avoid
        // constructing and pricing it during family fallback selection.
        // Advisory: no skip; the candidate is constructed and annotated
        // with the real G9 outcome, keeping its structural priority.
        if probe.filters()
            && state.trusted.parts().side_effect_mode == SideEffectMode::ReadOnly
            && entry.effect > SideEffectClass::ReadOnly
        {
            continue;
        }
        // These borrowed facts can only reject a transient family probe.
        // Keep the full gates authoritative for every proposed candidate and
        // never interpret a model bitmap from a different catalog layout.
        // Advisory: no skip; the real G2 outcome is kept as annotation.
        let trusted = state.trusted.parts();
        if probe.filters()
            && (catalog.surface_excludes(entry)
                || trusted
                    .exclusions
                    .iter()
                    .any(|name| name.as_ref() == entry.public_name || name.as_ref() == entry.root)
                || trusted.model_admission.as_ref().is_some_and(|policy| {
                    policy.matches_layout(catalog.generation().0, catalog.model_admission_layout())
                        && policy.excludes(catalog.model_admission_entry(entry))
                })
                || trusted.channel.is_some() && !entry.channel_eligible)
        {
            continue;
        }
        let selected = prefer_discovered
            && policy.discovered_refs.iter().any(|reference| {
                reference.id == entry.id
                    && reference.fingerprint == entry.fingerprint
                    && reference.generation == entry.generation
            });
        let native_file = native_files && is_native_file_entry(entry);
        if prefer_discovered && passed_fallback.is_some() && !selected && !native_file {
            continue;
        }
        let namespace = bind_namespace.then(|| Ns8::of(policy.key, &entry.namespace));
        let candidate = from_entry(action, family, entry, policy, namespace);
        let outcome = probe.check_one(&candidate).admission;
        trace_rejected(&candidate, &outcome, generation_rejections);
        // Advisory: the first structurally eligible candidate is selected
        // under the structural rules below; `approval_only` stays unused.
        let outcome = if probe.filters() { outcome } else { Admission::Passed };
        match outcome {
            Admission::Passed if native_file => file_members.push(candidate),
            Admission::Passed if !prefer_discovered || selected => return Ok(Some(candidate)),
            Admission::Passed => passed_fallback = Some(candidate),
            Admission::ApprovalOnly { op_ids } if !op_ids.is_empty() && approval_only.is_none() => {
                approval_only = Some(candidate);
            }
            _ => {}
        }
    }
    if let Some(first) = file_members.first() {
        if file_members.len() == 1 {
            return Ok(Some(first.clone()));
        }
        let group = native_file_group(&file_members, None);
        let outcome = probe.check_one(&group).admission;
        trace_rejected(&group, &outcome, generation_rejections);
        // Advisory: the group is always used.
        return Ok(Some(if !probe.filters() || outcome == Admission::Passed {
            group
        } else {
            first.clone()
        }));
    }
    Ok(passed_fallback.or(approval_only))
}

/// One filesystem plan can require reading and editing, or writing and then
/// reading. Combine only individually gated native adapters (at most three),
/// retaining exact refs, maximum effect, all approval ids and checked estimates.
fn is_native_file_entry(entry: &CapabilityEntry) -> bool {
    entry.family == CapabilityTarget::Filesystem
        && entry.namespace == "native"
        && entry.root == entry.public_name
        && matches!(entry.public_name.as_str(), "file_read" | "file_write" | "file_edit")
}

fn native_file_group(members: &[CausalCandidate], namespace: Option<Ns8>) -> CausalCandidate {
    let estimated_cost = members.iter().try_fold(
        CostEstimate {
            tokens: 0,
            micro_usd: 0,
        },
        |sum, member| {
            let cost = member.estimated_cost()?;
            Some(CostEstimate {
                tokens: sum.tokens.checked_add(cost.tokens)?,
                micro_usd: sum.micro_usd.checked_add(cost.micro_usd)?,
            })
        },
    );
    let latency = members
        .iter()
        .try_fold(0u32, |sum, member| sum.checked_add(member.estimated_latency_ms()?));
    let overhead = members
        .iter()
        .try_fold(0u32, |sum, member| sum.checked_add(member.decision_overhead_ms()?));
    let mut approval_required = false;
    let mut op_ids = BTreeSet::new();
    for member in members {
        if let ApprovalRequirement::Required { op_ids: ids } = member.approval() {
            approval_required = true;
            op_ids.extend(ids.iter().cloned());
        }
    }
    CausalCandidate::new(
        CausalActionKind::UseTool,
        CapabilityTarget::Filesystem,
        CandidateSpec {
            namespace,
            required_capabilities: members
                .iter()
                .flat_map(|member| member.required_capabilities().iter().cloned())
                .collect(),
            side_effect: members
                .iter()
                .map(CausalCandidate::side_effect)
                .max()
                .unwrap_or(SideEffectClass::ReadOnly),
            approval: if approval_required {
                ApprovalRequirement::Required {
                    op_ids: op_ids.into_iter().collect(),
                }
            } else {
                ApprovalRequirement::None
            },
            estimated_cost,
            estimated_latency_ms: latency,
        },
    )
    .with_decision_overhead_ms(overhead)
}

pub(crate) fn from_entry(
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
    // Discover runs one bounded in-memory catalog search. The referenced
    // entry supplies the target's effect, approval and availability gates,
    // but its provider-token and execution-latency estimate belongs to a
    // later tool call. Charging that estimate to this control hop can make a
    // perfectly available search expire before selection and turn an
    // ordinary request into Clarify.
    let estimate = (action != CausalActionKind::Discover)
        .then(|| policy.cost_model.estimate_entry(entry))
        .flatten();
    let discovery_estimate = (action == CausalActionKind::Discover).then_some((
        CostEstimate {
            tokens: 0,
            micro_usd: 0,
        },
        0,
    ));
    let estimate = discovery_estimate.or(estimate);
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
    // Charge only work performed during this control hop. A later tool's
    // whole-turn latency remains a scoring estimate, not decision overhead.
    .with_decision_overhead_ms(estimate.map(|_| policy.cost_model.decision_overhead_ms(action, entry)))
}

/// Record why a generator probe left the candidate set. Probes that fail a
/// gate are retained as diagnostic outcomes when the final candidate set
/// does not contain their ID. They never supply executable candidate bodies.
fn trace_rejected(candidate: &CausalCandidate, outcome: &Admission, rejections: &mut BTreeMap<CandidateId, Admission>) {
    if matches!(outcome, Admission::Blocked { .. }) {
        rejections
            .entry(candidate.candidate_id().clone())
            .or_insert_with(|| outcome.clone());
    }
    match outcome {
        Admission::Passed | Admission::ApprovalOnly { .. } => {}
        Admission::Blocked { gate, reason_code } => tracing::debug!(
            target: "causal_tree::candidates",
            candidate = candidate.candidate_id().as_str(),
            gate = *gate,
            reason = *reason_code,
            "candidate rejected by a hard gate"
        ),
        Admission::ApprovalDeferred => tracing::debug!(
            target: "causal_tree::candidates",
            candidate = candidate.candidate_id().as_str(),
            "candidate deferred: approval scope unavailable"
        ),
    }
}

pub(crate) fn terminal(action: CausalActionKind, policy: &CandidatePolicy<'_>) -> CausalCandidate {
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
            // Answer is the baseline serving path, so its normal model-response
            // latency is not extra decision latency. Keep the estimate
            // presence coupled to the pinned model price so an unpriced model
            // still fails G6; the priced baseline contributes zero extra
            // milliseconds. Tool and retrieval candidates retain their own
            // execution-latency estimates.
            estimated_latency_ms: if action != CausalActionKind::Answer {
                Some(0)
            } else {
                estimate.map(|_| 0)
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

/// Approval-required capabilities whose fresh-turn call arguments can be
/// collected, never executed, before an owner reviews them: the file mutations
/// and an MCP tool with one fixed approval operation. Under Supervised
/// autonomy every discovered MCP tool qualifies; multi-operation entries such
/// as nodes do not.
pub(crate) fn collectable_approval_entry(entry: &CapabilityEntry) -> bool {
    match entry.family {
        CapabilityTarget::Filesystem => {
            matches!(entry.public_name.as_str(), "file_write" | "file_edit") || entry.collects_exact_call()
        }
        CapabilityTarget::Mcp => entry.effect > SideEffectClass::ReadOnly && entry.op_ids.len() == 1,
        // A generic tool whose lifecycle actions need a grant bound to their
        // exact target derives that operation from the arguments, like a file
        // mutation. Collection asks the model for them before any review.
        _ => entry.effect > SideEffectClass::ReadOnly && entry.op_ids.is_empty() && entry.collects_exact_call(),
    }
}

/// The text names the capability itself (public name or id), not one of its
/// bare aliases, which are ordinary words such as `write` or `search`.
pub(crate) fn names_entry(folded_text: &str, entry: &CapabilityEntry) -> bool {
    contains_bounded(folded_text, &entry.id.as_str().to_lowercase())
        || contains_bounded(folded_text, &entry.public_name.to_lowercase())
}

fn available_for_fallback(entry: &CapabilityEntry, policy: &CandidatePolicy<'_>) -> bool {
    !matches!(entry.exposure, Exposure::Searchable)
        || policy.discovered_refs.contains(&CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        })
}

pub(crate) fn exact_mention(folded_text: &str, entry: &CapabilityEntry) -> bool {
    entry
        .exact_mention_terms
        .iter()
        .any(|name| contains_bounded(folded_text, name))
}

/// Naming a Skill and its registered read-only loader names one operation.
/// Keep the concrete Skill as the exact candidate so generic target-choice
/// ties cannot disagree with the cheaper, more specific candidate's score.
/// This changes intent ranking only; every Skill reference still passes gates.
pub(crate) fn exact_operation_mention(folded_text: &str, entry: &CapabilityEntry, catalog: &CatalogView<'_>) -> bool {
    // A concrete read alias supersedes its mixed-effect management root.
    // Its schema admits only that read; naming a write still keeps the root.
    if entry.public_name == "skills_manage"
        && entry.root == "skills_manage"
        && !entry.names_collectable_action(folded_text)
        && catalog.iter_entries().any(|read| {
            read.root == "skills_manage"
                && read.effect == SideEffectClass::ReadOnly
                && matches!(
                    read.public_name.as_str(),
                    "lifecycle_status" | "lifecycle_list" | "lifecycle_inspect"
                )
                && exact_mention(folded_text, read)
        })
    {
        return false;
    }
    exact_mention(folded_text, entry)
        && !catalog
            .iter_entries()
            .any(|skill| exact_mention(folded_text, skill) && entry.loads_read_only_skill(skill))
}

pub(crate) fn contains_bounded(text: &str, name: &str) -> bool {
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
