//! Deterministic capability-backed candidate enumeration for the decision plane.
//!
//! Kept separate from the `llm-router`-only legacy expander so the new
//! decision domain also compiles in bare builds.

use std::collections::BTreeSet;

use crate::decision_engine::candidate::{CandidateId, CandidateSpec, CausalCandidate, Ns8};
use crate::decision_engine::cost_model::CostEstimateSource;
use crate::decision_engine::state::{CausalState, ObservedStepKind, ObservedStepStatus};
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::catalog::{CapabilityEntry, CatalogView, Exposure};
use crate::tools::capability::contract::{
    ApprovalRequirement, CapabilityRef, CapabilityTarget, CausalActionKind, CostEstimate, SideEffectClass,
};

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

/// Why a candidate set could not be generated.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GenerationError {
    #[error("duplicate candidate id in gate input")]
    DuplicateCandidate,
    #[error("candidate set has no refusal path")]
    MissingRefusal,
    #[error("candidate limit must reserve at least one working and two terminal paths")]
    InvalidCandidateLimit,
}

#[derive(Debug)]
pub(crate) enum CandidateSet {
    Legal {
        candidates: Vec<CausalCandidate>,
        exact_mentions: BTreeSet<CandidateId>,
    },
    /// Only terminal bodies were proposed.
    NoLegalPath,
}

pub(crate) struct CausalCandidateGenerator;

impl CausalCandidateGenerator {
    pub(crate) fn generate(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
    ) -> Result<CandidateSet, GenerationError> {
        Self::generate_mode(state, catalog, policy, false, None)
    }

    fn generate_mode(
        state: &CausalState,
        catalog: &CatalogView<'_>,
        policy: &CandidatePolicy<'_>,
        terminal_only: bool,
        resume_target: Option<&CapabilityRef>,
    ) -> Result<CandidateSet, GenerationError> {
        if policy.max_candidates < 3 {
            return Err(GenerationError::InvalidCandidateLimit);
        }
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
                false,
                catalog,
                policy,
            ) {
                proposed.push(candidate);
            }
        }
        if !terminal_only && !routing_history {
            if policy.delegation_target.is_none() {
                // Raw text affects candidates only through exact mentions.
                // Authenticated resume supplies its separately verified exact
                // target; neither path itself grants execution authority.
                for family in CapabilityTarget::ALL {
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
                        if resume_target.is_none() && is_native_file_entry(entry) {
                            named_files.push(candidate);
                        } else {
                            if named_files.is_empty() {
                                selected = Some(candidate);
                            }
                            break;
                        }
                    }
                    if let Some(first) = named_files.first() {
                        // One named file plan can require several adapters. Do
                        // not replace write/read intent with its first read.
                        selected = Some(native_file_group(&named_files, first.candidate_id().namespace()));
                    }
                    if let Some(candidate) = selected {
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
                    if let Some(candidate) =
                        select_family_candidate(CausalActionKind::UseTool, family, executable, false, catalog, policy)
                    {
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
                            false,
                            catalog,
                            policy,
                        ) {
                            proposed.push(candidate);
                        }
                    }
                }
                // A later capability_search call supplies its own bounded query.
                // Only initial Discover hops require the raw source to fit that
                // query contract.
                let discovery_completed =
                    state.observed.completed_steps().iter().any(|step| {
                        step.kind() == ObservedStepKind::Discover && step.status() == ObservedStepStatus::Ok
                    });
                if !discovery_completed {
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
                            catalog,
                            policy,
                        ) && discovery_anchor.is_none()
                        {
                            discovery_anchor = Some(candidate);
                        }
                    }
                    if policy.can_deliver_text
                        && state.routing_input.supports_selected_read_query()
                        && let Some(anchor) = discovery_anchor
                    {
                        // Discovery is one local catalog action. The ranked search
                        // resolves the family from the query; separate per-family
                        // candidates create an artificial zero-margin tie for a
                        // language-neutral decision backend.
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
                    catalog,
                    policy,
                ) {
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
        if policy.can_deliver_text {
            proposed.push(terminal(CausalActionKind::Clarify, policy));
        }
        proposed.push(terminal(CausalActionKind::Refuse, policy));

        // A duplicate id or a missing refusal is a generator bug.
        validate_structure(&proposed)?;
        let nonterminal = proposed
            .iter()
            .any(|candidate| !matches!(candidate.action(), CausalActionKind::Refuse | CausalActionKind::Clarify));
        if !nonterminal {
            // Reached only for structural reasons, when nothing but
            // terminals was proposed.
            return Ok(CandidateSet::NoLegalPath);
        }
        let cap = policy.max_candidates;
        let mut kept = Vec::new();
        // Refuse and legal Clarify are guaranteed space. Other candidates
        // retain the fixed enumeration priority above.
        let reserved = proposed
            .iter()
            .filter(|candidate| matches!(candidate.action(), CausalActionKind::Refuse | CausalActionKind::Clarify))
            .count();
        for candidate in proposed {
            if matches!(candidate.action(), CausalActionKind::Refuse | CausalActionKind::Clarify)
                || kept.len() < cap.saturating_sub(reserved)
            {
                kept.push(candidate);
            }
        }
        exact_mentions.retain(|id| kept.iter().any(|candidate| candidate.candidate_id() == id));
        Ok(CandidateSet::Legal {
            candidates: kept,
            exact_mentions,
        })
    }
}

/// The structural invariants of a proposed set: unique candidate ids and at
/// least one refusal path.
fn validate_structure(candidates: &[CausalCandidate]) -> Result<(), GenerationError> {
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        if !seen.insert(candidate.candidate_id()) {
            return Err(GenerationError::DuplicateCandidate);
        }
    }
    if candidates
        .iter()
        .any(|candidate| candidate.action() == CausalActionKind::Refuse)
    {
        Ok(())
    } else {
        Err(GenerationError::MissingRefusal)
    }
}

pub(crate) fn select_family_candidate<'a>(
    action: CausalActionKind,
    family: CapabilityTarget,
    entries: impl IntoIterator<Item = &'a CapabilityEntry>,
    bind_namespace: bool,
    catalog: &CatalogView<'_>,
    policy: &CandidatePolicy<'_>,
) -> Option<CausalCandidate> {
    // A selected metadata result must become the next represented operation,
    // rather than being hidden again by an earlier Direct same-family tool.
    let prefer_discovered = action == CausalActionKind::UseTool
        && policy.discovered_refs.iter().any(|reference| {
            catalog.entry_any(&reference.id).is_some_and(|entry| {
                entry.family == family
                    && entry.fingerprint == reference.fingerprint
                    && entry.generation == reference.generation
            })
        });
    let mut passed_fallback = None;
    let native_files = action == CausalActionKind::UseTool && family == CapabilityTarget::Filesystem && !bind_namespace;
    let mut file_members = Vec::new();
    for entry in entries {
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
        if native_file {
            file_members.push(candidate);
        } else if !prefer_discovered || selected {
            return Some(candidate);
        } else {
            passed_fallback = Some(candidate);
        }
    }
    if let Some(first) = file_members.first() {
        if file_members.len() == 1 {
            return Some(first.clone());
        }
        return Some(native_file_group(&file_members, None));
    }
    passed_fallback
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

#[cfg(test)]
mod tests;
