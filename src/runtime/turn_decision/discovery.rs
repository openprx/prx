//! Source-bound discovery offers. An offer does not grant dispatch authority.
mod append;
use super::{
    Arc, BTreeMap, BTreeSet, CapabilityAvailabilityLevel, CapabilityRef, CatalogScope, Exposure, GateInputs, Ordering,
    TurnDecisionContext, TurnDecisionService, TurnExecutionDirective, TurnSourceProof,
};
use crate::causal_tree::branch::{CapabilityId, CapabilityTarget, CausalActionKind};
use crate::tools::ToolSpec;
use crate::tools::capability::search::CatalogSearch;
use crate::tools::capability::search_tool::CapabilitySearchPort;
pub(super) use append::DiscoveryAdmission;
#[cfg(test)]
pub(super) use append::MAX_DISCOVERED_TOOLS_PER_TURN;

pub(super) use crate::tools::capability::search_tool::DiscoveryOffers;
use crate::tools::capability::search_tool::{DiscoveryEntry, DiscoveryScope};

#[derive(Debug, thiserror::Error)]
pub(crate) enum SearchControlUnavailable {
    #[error(
        "capability discovery unavailable: the published search control has no live backend; rebuild the runtime registry"
    )]
    MissingBackend,
}

impl TurnDecisionService {
    pub(crate) fn capability_search_port(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
    ) -> anyhow::Result<CapabilitySearchPort> {
        anyhow::ensure!(
            self.agent_capability_search_spec(proof, context)?.is_some(),
            "turn does not expose capability search"
        );
        let entries = self.discovery_entries(proof, context)?;
        let offers = context.discovery_offers.get_or_init(Default::default);
        Ok(CapabilitySearchPort::new(Arc::new(DiscoveryScope::published(
            Arc::clone(&self.catalog),
            self.publication_policy.clone(),
            entries,
            Arc::downgrade(offers),
            Arc::downgrade(&context.execution_closed),
            Arc::downgrade(&self.execution),
        ))))
    }

    pub(super) fn discovery_entries(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<BTreeMap<CapabilityId, DiscoveryEntry>> {
        let families = self.discovery_families(proof, context)?;
        let view = self
            .catalog
            .view(&CatalogScope::default())
            .with_surface(&self.publication_policy);
        let trusted = context.engine.state.trusted.parts();
        let offered_direct = self.direct_discovery_ids(context);
        let mut entries = BTreeMap::new();
        for entry in view.iter_entries() {
            let core = context.execution_refs.iter().any(|reference| {
                reference.id == entry.id
                    && reference.fingerprint == entry.fingerprint
                    && reference.generation == entry.generation
            });
            let eligible = !matches!(
                entry.public_name.as_str(),
                "capability_search" | "stay_silent" | "transcript_history_lookup" | "delegate"
            ) && (entry.supports_metadata_discovery()
                || matches!(entry.exposure, Exposure::Direct) && (offered_direct.contains(&entry.id) || core))
                && (!trusted.owner_isolated()
                    || crate::causal_tree::capability_policy::admits_owner_isolated_identity(
                        &entry.public_name,
                        &entry.root,
                    )
                    || (trusted
                        .constraints
                        .iter()
                        .any(|code| code.as_str() == "owner_private_filesystem_v1")
                        && entry.public_name == entry.root
                        && matches!(entry.public_name.as_str(), "file_read" | "file_write" | "file_edit")))
                && entry.availability.level >= CapabilityAvailabilityLevel::Configured
                && !trusted.excludes_entry(view.model_admission_entry(entry))
                && !(trusted
                    .constraints
                    .iter()
                    .any(|code| code.as_str() == "intercepts_writes")
                    && entry.plan_intercepted())
                && !(trusted.side_effect_mode == crate::causal_tree::state::SideEffectMode::ReadOnly
                    && entry.effect > crate::causal_tree::branch::SideEffectClass::ReadOnly)
                && (trusted.channel.is_none() || entry.channel_eligible)
                && proof.child_scope().is_none_or(|scope| {
                    scope.gate_inputs().0.allowed.iter().any(|reference| {
                        reference.id == entry.id
                            && reference.fingerprint == entry.fingerprint
                            && reference.generation == entry.generation
                    })
                })
                && crate::tools::execution::decide_tool_execution(
                    &self.security,
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
                    reference: CapabilityRef {
                        id: entry.id.clone(),
                        fingerprint: entry.fingerprint,
                        generation: entry.generation,
                    },
                    core,
                    searchable: entry.supports_metadata_discovery() || offered_direct.contains(&entry.id),
                },
            );
        }
        // An explicit engine-selected family may narrow discovery. A Direct
        // turn has no family prerequisite; an empty narrowing must not strand
        // otherwise eligible capabilities.
        if let Some(family) = families.first().copied() {
            if entries
                .iter()
                .any(|(id, entry)| entry.searchable && view.entry_any(id).is_some_and(|item| item.family == family))
            {
                entries.retain(|id, entry| entry.core || view.entry_any(id).is_some_and(|item| item.family == family));
            }
        }
        Ok(entries)
    }

    /// Only an explicit engine-selected family may narrow the index. Empty
    /// families on ordinary Direct turns mean the full eligible index.
    fn discovery_families(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<Vec<CapabilityTarget>> {
        self.agent_execution_specs(proof, context)?;
        Ok(super::discovery_scope(context.audit_hops())?.into_iter().collect())
    }

    /// Stable control definition, separate from the selected business refs and
    /// conditional runtime dependencies. Explicit exclusions still apply.
    pub(crate) fn agent_capability_search_spec(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<Option<ToolSpec>> {
        if context.is_plain_answer_abstention()
            || !matches!(
                context.directive(),
                TurnExecutionDirective::AnswerDirectly | TurnExecutionDirective::Execute { .. }
            )
            || context.execution_closed.load(Ordering::Acquire)
            || !proof.tool_calls_allowed()
            || !self.model_can_call_tools(proof)
        {
            return Ok(None);
        }
        if !self
            .discovery_entries(proof, context)?
            .values()
            .any(|entry| entry.searchable)
        {
            return Ok(None);
        }
        let Some(entry) = self.catalog.entry_for_public_name("capability_search") else {
            // Custom runtimes may intentionally publish a fixed registry with
            // no discovery control. Its absence must not invalidate an answer
            // or a tool already admitted through that registry.
            tracing::warn!("deferred discovery unavailable: this catalog does not publish capability_search");
            return Ok(None);
        };
        let trusted = context.engine.state.trusted.parts();
        if self.catalog_view().surface_excludes(entry)
            || trusted.excludes_entry(self.catalog_view().model_admission_entry(entry))
            || (trusted.channel.is_some() && !entry.channel_eligible)
            || matches!(entry.exposure, Exposure::Unavailable { .. })
            || entry.availability.level < CapabilityAvailabilityLevel::Configured
            || crate::tools::execution::decide_tool_execution(
                &self.security,
                entry.policy_tool_name(),
                proof.policy_sender(),
                proof.policy_channel(),
                proof.policy_chat_type(),
            ) == crate::tools::execution::ToolExecutionDecision::Deny
        {
            return Ok(None);
        }
        let descriptor = self
            .execution
            .current_descriptor("capability_search")
            .ok_or(SearchControlUnavailable::MissingBackend)?;
        let spec = ToolSpec {
            name: descriptor.public_name,
            description: descriptor.description,
            parameters: descriptor.parameters,
        };
        anyhow::ensure!(
            entry.generation == context.catalog_generation()
                && crate::tools::capability::fingerprint::tool_spec_fingerprint(
                    &spec.name,
                    &spec.description,
                    &spec.parameters,
                ) == entry.fingerprint,
            "capability search definition changed"
        );
        Ok(Some(spec))
    }

    /// A selected Skill needs a way to discover its execution tools after its
    /// loader narrows the serving slice. A selected filesystem operation also
    /// needs its read/write peers; process execution is excluded from that
    /// family handoff. Otherwise only source-named direct peers are offered.
    /// These IDs are search eligibility, never grants;
    /// exclusions, read-only mode, child scope and publication gates still apply.
    fn direct_discovery_ids(&self, context: &TurnDecisionContext) -> BTreeSet<CapabilityId> {
        let view = self.catalog_view();
        let selected_skill = context.execution_refs.iter().any(|reference| {
            view.entry_any(&reference.id).is_some_and(|entry| {
                matches!(
                    &entry.target,
                    crate::tools::capability::catalog::ExecutionTarget::Skill { .. }
                )
            })
        });
        let selected_filesystem = super::discovery_scope(context.audit_hops()).ok().flatten()
            == Some(CapabilityTarget::Filesystem)
            || matches!(context.directive(), TurnExecutionDirective::Execute { .. })
                && context.execution_refs.iter().any(|reference| {
                    view.entry_any(&reference.id).is_some_and(|entry| {
                        entry.family == CapabilityTarget::Filesystem
                            && entry.effect <= crate::causal_tree::branch::SideEffectClass::LocalWrite
                    })
                });
        let mut entries =
            CatalogSearch::exact_mentions(&view, context.engine.state.routing_input.as_str(), 64).unwrap_or_default();
        if selected_skill || selected_filesystem {
            entries.extend(view.iter_entries().filter(|entry| {
                matches!(entry.exposure, Exposure::Direct)
                    && (selected_skill
                        || entry.family == CapabilityTarget::Filesystem
                            && entry.effect <= crate::causal_tree::branch::SideEffectClass::LocalWrite)
            }));
        }
        entries
            .into_iter()
            .filter(|entry| {
                matches!(entry.exposure, Exposure::Direct)
                    && !context.execution_refs.iter().any(|reference| reference.id == entry.id)
            })
            .map(|entry| entry.id.clone())
            .collect()
    }
}
