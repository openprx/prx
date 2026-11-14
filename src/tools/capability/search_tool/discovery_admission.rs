//! Source-bound discovery offers and their admission (R25 discovery row,
//! R28 execution rules). An offer does not grant dispatch authority; every
//! offered reference is rechecked against the turn's publication rules.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context as _;

use super::{CapabilitySearchPort, DiscoveryEntry, DiscoveryOffers, DiscoveryScope, MAX_SEARCH_HITS};
use crate::capability::CapabilityAvailabilityLevel;
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::ToolSpec;
use crate::tools::capability::catalog::{ExecutionTarget, Exposure};
use crate::tools::capability::contract::{CapabilityId, CapabilityRef};
use crate::tools::capability::execution_binding::specs::{ConstrainedToolSelection, execution_specs};
use crate::tools::capability::search::CatalogSearch;
use crate::tools::surface_owner::publication::{PublicationAdmission, entry_ref, is_delegator};
use crate::tools::surface_owner::turn_port::{TurnSurfacePort, rules};

/// Most discovered tools one turn may declare, counted across every search.
pub(crate) const MAX_DISCOVERED_TOOLS_PER_TURN: usize = 10;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SearchControlUnavailable {
    #[error(
        "capability discovery unavailable: the published search control has no live backend; rebuild the runtime registry"
    )]
    MissingBackend,
}

/// A checked proposal, not an execution binding. Approval outcomes must be
/// handled before a caller publishes or dispatches the definition.
pub(crate) struct DiscoveredToolProposal {
    pub(crate) reference: CapabilityRef,
    pub(crate) root: String,
    pub(crate) spec: ToolSpec,
    pub(crate) admission: PublicationAdmission,
}

/// Created only from a freshly checked real offer. Pending entries may be
/// bound to the approval recorder but must never dispatch before durable
/// approval.
#[derive(Clone)]
pub(crate) struct DiscoveryAdmission {
    pub(crate) reference: CapabilityRef,
    pub(crate) requires_approval: bool,
}

/// The discovery offers and admissions of one live turn.
#[derive(Default)]
pub(crate) struct DiscoveryState {
    offers: std::sync::OnceLock<Arc<parking_lot::Mutex<DiscoveryOffers>>>,
    admissions: std::sync::OnceLock<parking_lot::Mutex<BTreeMap<String, DiscoveryAdmission>>>,
}

impl DiscoveryState {
    pub(crate) fn admission(&self, name: &str) -> Option<DiscoveryAdmission> {
        self.admissions.get()?.lock().get(name).cloned()
    }

    /// Terminal evidence only; callers must not treat these retained refs as
    /// authority after the turn closed. Live consumers recheck every offer.
    pub(crate) fn executable_refs(&self) -> Vec<CapabilityRef> {
        self.admissions.get().map_or_else(Vec::new, |admissions| {
            admissions
                .lock()
                .values()
                .filter(|admission| !admission.requires_approval)
                .map(|admission| admission.reference.clone())
                .collect()
        })
    }

    /// Ids of the admissions that can run without an approval. One that
    /// still waits for approval may never have been attempted, so it says
    /// nothing about whether a run could have called it.
    pub(crate) fn unprompted_ids(&self) -> Vec<String> {
        self.executable_refs()
            .into_iter()
            .map(|reference| reference.id.as_str().to_owned())
            .collect()
    }

    /// Clear the offers and wait for an admission that entered before the
    /// turn closed, so terminal evidence sees its final metadata. Later
    /// admissions check the closed flag.
    pub(crate) fn close(&self) {
        if let Some(offers) = self.offers.get() {
            offers.lock().clear();
        }
        if let Some(admissions) = self.admissions.get() {
            drop(admissions.lock());
        }
    }

    /// A state holding exactly `admissions`, for unit tests of the readers.
    #[cfg(test)]
    pub(crate) fn with_admissions(admissions: BTreeMap<String, DiscoveryAdmission>) -> Self {
        let state = Self::default();
        state.admissions.get_or_init(|| parking_lot::Mutex::new(admissions));
        state
    }
}

/// The capability search port of the turn, bound to its eligible entries.
pub(crate) fn capability_search_port(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
) -> anyhow::Result<CapabilitySearchPort> {
    anyhow::ensure!(
        capability_search_spec(port, proof)?.is_some(),
        "turn does not expose capability search"
    );
    let entries = discovery_entries(port, proof)?;
    let offers = port.discovery().offers.get_or_init(Default::default);
    Ok(CapabilitySearchPort::new(Arc::new(DiscoveryScope::published(
        Arc::clone(port.catalog()),
        port.publication_policy().clone(),
        entries,
        Arc::downgrade(offers),
        Arc::downgrade(port.execution_closed()),
        Arc::downgrade(port.execution()),
    ))))
}

/// Discovery eligibility: the entries this turn's search may index.
pub(crate) fn discovery_entries(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
) -> anyhow::Result<BTreeMap<CapabilityId, DiscoveryEntry>> {
    execution_specs(port, proof)?;
    let view = port.catalog_view();
    let rules = rules(port, &view);
    let trusted = port.trusted();
    let offered_direct = direct_discovery_ids(port);
    let mut entries = BTreeMap::new();
    for entry in view.iter_entries() {
        let reference = entry_ref(entry);
        let core = port.execution_refs().contains(&reference);
        let eligible = !matches!(
            entry.public_name.as_str(),
            "capability_search" | "stay_silent" | "transcript_history_lookup" | "delegate"
        ) && (entry.supports_metadata_discovery()
            || matches!(entry.exposure, Exposure::Direct) && (offered_direct.contains(&entry.id) || core))
            && rules.owner_isolation_admits(entry)
            && entry.availability.level >= CapabilityAvailabilityLevel::Configured
            && !trusted.excludes_entry(view.model_admission_entry(entry))
            && !rules.restricts_discovery(entry)
            && (trusted.channel.is_none() || entry.channel_eligible)
            && proof
                .child_scope()
                .is_none_or(|_| rules.child_membership_admits(&reference))
            && crate::tools::execution::decide_tool_execution(
                &port.facts().policy,
                entry.policy_tool_name(),
                proof.policy_sender(),
                proof.policy_channel(),
                proof.policy_chat_type(),
            ) != crate::tools::execution::ToolExecutionDecision::Deny;
        if !eligible {
            continue;
        }
        entries.insert(
            entry.id.clone(),
            DiscoveryEntry {
                reference,
                core,
                searchable: entry.supports_metadata_discovery() || offered_direct.contains(&entry.id),
            },
        );
    }
    Ok(entries)
}

/// The stable discovery control definition, separate from the published
/// business refs and conditional runtime dependencies. Explicit exclusions
/// still apply.
pub(crate) fn capability_search_spec(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
) -> anyhow::Result<Option<ToolSpec>> {
    if port.execution_closed().load(Ordering::Acquire)
        || !proof.tool_calls_allowed()
        || !port.model_can_call_tools(proof)
    {
        return Ok(None);
    }
    if !discovery_entries(port, proof)?.values().any(|entry| entry.searchable) {
        return Ok(None);
    }
    let Some(entry) = port.catalog().entry_for_public_name("capability_search") else {
        // Custom runtimes may intentionally publish a fixed registry with
        // no discovery control. Its absence must not invalidate an answer
        // or a tool already admitted through that registry.
        tracing::warn!("deferred discovery unavailable: this catalog does not publish capability_search");
        return Ok(None);
    };
    let trusted = port.trusted();
    let view = port.catalog_view();
    if view.surface_excludes(entry)
        || trusted.excludes_entry(view.model_admission_entry(entry))
        || (trusted.channel.is_some() && !entry.channel_eligible)
        || matches!(entry.exposure, Exposure::Unavailable { .. })
        || entry.availability.level < CapabilityAvailabilityLevel::Configured
        || crate::tools::execution::decide_tool_execution(
            &port.facts().policy,
            entry.policy_tool_name(),
            proof.policy_sender(),
            proof.policy_channel(),
            proof.policy_chat_type(),
        ) == crate::tools::execution::ToolExecutionDecision::Deny
    {
        return Ok(None);
    }
    let Some(descriptor) = port.execution().current_descriptor("capability_search") else {
        // A child runs on its own task registry, which owns no discovery
        // control unless its parent passed one down. Its manifest no longer
        // hides the parent's catalog in Full (R51), so the absence is the
        // child's structure, not a stale root registry.
        if proof.child_scope().is_some() {
            return Ok(None);
        }
        return Err(SearchControlUnavailable::MissingBackend.into());
    };
    let spec = ToolSpec {
        name: descriptor.public_name,
        description: descriptor.description,
        parameters: descriptor.parameters,
    };
    anyhow::ensure!(
        entry.generation == port.catalog_generation()
            && crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &spec.name,
                &spec.description,
                &spec.parameters,
            ) == entry.fingerprint,
        "capability search definition changed"
    );
    Ok(Some(spec))
}

/// A published Skill needs a way to discover its execution tools after its
/// loader narrows the serving slice. Otherwise only source-named direct
/// peers are offered. These ids are search eligibility, never grants;
/// exclusions, read-only mode, child scope and publication rules still apply.
fn direct_discovery_ids(port: &dyn TurnSurfacePort) -> BTreeSet<CapabilityId> {
    let view = port.catalog_view();
    let published = port.execution_refs();
    let selected_skill = published.iter().any(|reference| {
        view.entry_any(&reference.id)
            .is_some_and(|entry| matches!(&entry.target, ExecutionTarget::Skill { .. }))
    });
    let mut entries = CatalogSearch::exact_mentions(&view, port.routing_input(), 64).unwrap_or_default();
    if selected_skill {
        entries.extend(
            view.iter_entries()
                .filter(|entry| matches!(entry.exposure, Exposure::Direct)),
        );
    }
    entries
        .into_iter()
        .filter(|entry| {
            matches!(entry.exposure, Exposure::Direct) && !published.iter().any(|reference| reference.id == entry.id)
        })
        .map(|entry| entry.id.clone())
        .collect()
}

/// Recheck one actually returned offer against the turn's publication rules.
pub(crate) fn prepare_discovered_tool(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
    name_or_id: &str,
) -> anyhow::Result<Option<DiscoveredToolProposal>> {
    let Some(offers) = port.discovery().offers.get() else {
        return Ok(None);
    };
    let reference = offers.lock().find(name_or_id);
    let Some(reference) = reference else {
        return Ok(None);
    };
    anyhow::ensure!(
        capability_search_spec(port, proof)?.is_some(),
        "turn no longer permits discovered capability admission"
    );
    let catalog = port.catalog();
    let entries = catalog.resolve(std::slice::from_ref(&reference))?;
    let entry = entries.first().context("offered capability is absent")?;
    let view = port.catalog_view();
    let rules = rules(port, &view);
    // Publication rules stop before dispatch budgets, but a discovery cannot
    // undo the turn's structural restrictions or exact resume scope.
    anyhow::ensure!(
        !rules.restricts(entry),
        "discovered capability conflicts with turn restrictions"
    );
    let model = port.resolve_model(proof)?;
    let delegation = is_delegator(&entry.public_name);
    anyhow::ensure!(
        !delegation || port.outgoing_child(),
        "offered delegation has no source-issued child manifest"
    );
    let estimate = crate::cost::estimate::estimate_cost(&model, &port.config().cost, entry.schema_bytes);
    let admission = rules.capability(entry, &reference, delegation, estimate);
    let descriptor = port
        .execution()
        .current_descriptor(&entry.public_name)
        .context("offered capability has no executable descriptor")?;
    let spec = ToolSpec {
        name: descriptor.public_name,
        description: descriptor.description,
        parameters: descriptor.parameters,
    };
    anyhow::ensure!(
        crate::tools::capability::fingerprint::tool_spec_fingerprint(&spec.name, &spec.description, &spec.parameters)
            == reference.fingerprint
            && !port.execution_closed().load(Ordering::Acquire),
        "offered capability changed or its turn closed"
    );
    Ok(Some(DiscoveredToolProposal {
        reference,
        root: entry.root.clone(),
        spec,
        admission,
    }))
}

/// Select an actually returned hit. Pending approval is retained as a
/// separate state, never silently promoted to executable publication.
pub(crate) fn admit_discovered_tool(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
    name_or_id: &str,
) -> anyhow::Result<Option<DiscoveredToolProposal>> {
    let Some(proposal) = prepare_discovered_tool(port, proof, name_or_id)? else {
        return Ok(None);
    };
    if port.execution_refs().contains(&proposal.reference) {
        // An ID-form call may name an already published capability. Keep
        // it in the immutable core instead of publishing it twice.
        return Ok(Some(proposal));
    }
    let requires_approval = match proposal.admission {
        PublicationAdmission::Passed => false,
        PublicationAdmission::ApprovalRequired => true,
        PublicationAdmission::Blocked { .. } => return Ok(Some(proposal)),
    };
    let mut admissions = port.discovery().admissions.get_or_init(Default::default).lock();
    anyhow::ensure!(
        !port.execution_closed().load(Ordering::Acquire),
        "discovery admission turn is closed"
    );
    let admitted = DiscoveryAdmission {
        reference: proposal.reference.clone(),
        requires_approval,
    };
    if let Some(previous) = admissions.get(&proposal.spec.name) {
        anyhow::ensure!(
            previous.reference == admitted.reference && previous.requires_approval == admitted.requires_approval,
            "discovery admission changed within the turn"
        );
    } else {
        admissions.insert(proposal.spec.name.clone(), admitted);
    }
    Ok(Some(proposal))
}

/// Offer the hits of a completed `capability_search` to the NEXT model
/// request. Every returned hit goes through the same rules as a call by name
/// ([`admit_discovered_tool`]), so the search result itself never grants
/// anything; a hit that is blocked, pending approval or hidden by `excluded`
/// is not published. The request already sent is not rewritten: call this
/// before the next request's tool surface is built. Hits are taken in
/// search-rank order, at most `MAX_SEARCH_HITS` newly admitted per call and
/// `turn_cap` admitted over the whole turn. Returns the count newly admitted.
pub(crate) fn admit_offered_discoveries(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
    turn_cap: usize,
    excluded: &dyn Fn(&str, &str) -> bool,
) -> usize {
    let Some(offers) = port.discovery().offers.get() else {
        return 0;
    };
    let names = offers.lock().names();
    let mut newly_admitted = 0;
    for name in names {
        let admitted_this_turn = port
            .discovery()
            .admissions
            .get()
            .map_or(0, |admissions| admissions.lock().len());
        if newly_admitted >= MAX_SEARCH_HITS || admitted_this_turn >= turn_cap {
            break;
        }
        if port.discovery().admission(&name).is_some() {
            continue;
        }
        let proposal = match prepare_discovered_tool(port, proof, &name) {
            Ok(Some(proposal)) => proposal,
            Ok(None) => continue,
            Err(error) => {
                tracing::debug!(tool = %name, %error, "discovered capability is not offered to the next request");
                continue;
            }
        };
        // A core capability is already declared; it is never "newly admitted".
        if port.execution_refs().contains(&proposal.reference) {
            continue;
        }
        if proposal.admission != PublicationAdmission::Passed || excluded(&proposal.spec.name, &proposal.root) {
            continue;
        }
        match admit_discovered_tool(port, proof, &name) {
            Ok(Some(admitted)) if admitted.admission == PublicationAdmission::Passed => newly_admitted += 1,
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(tool = %name, %error, "discovered capability admission was refused");
            }
        }
    }
    newly_admitted
}

/// Resolve only separately admitted definitions, with fresh source and
/// schema checks. This never changes the turn's published refs.
pub(crate) fn appended_execution_specs(
    port: &dyn TurnSurfacePort,
    proof: &dyn TurnSourceProof,
) -> anyhow::Result<ConstrainedToolSelection> {
    let names = port.discovery().admissions.get().map_or_else(Vec::new, |admissions| {
        admissions
            .lock()
            .iter()
            .filter(|(_, admission)| !admission.requires_approval)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    });
    let mut roots = BTreeSet::new();
    let mut specs = Vec::with_capacity(names.len());
    for name in names {
        let proposal = prepare_discovered_tool(port, proof, &name)?.context("admitted discovery offer is missing")?;
        let admitted = port
            .discovery()
            .admission(&name)
            .context("discovery admission missing")?;
        anyhow::ensure!(
            !admitted.requires_approval
                && admitted.reference == proposal.reference
                && proposal.admission == PublicationAdmission::Passed,
            "appended capability no longer passes its publication rules"
        );
        roots.insert(proposal.root);
        specs.push(proposal.spec);
    }
    Ok(ConstrainedToolSelection { roots, specs })
}
