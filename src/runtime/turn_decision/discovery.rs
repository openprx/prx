//! Source-bound discovery offers. An offer does not grant dispatch authority.
mod append;
use super::{
    Arc, BTreeMap, BTreeSet, CapabilityAvailabilityLevel, CapabilityRef, CatalogScope, Context, Exposure, GateInputs,
    Ordering, TurnDecisionContext, TurnDecisionService, TurnExecutionDirective, TurnSourceProof,
};
use crate::causal_tree::branch::{CapabilityTarget, CausalActionKind};
use crate::causal_tree::candidate_generator::CandidateSet;
use crate::runtime::turn_source::OwnedTurnSourceProof;
use crate::tools::ToolSpec;
use crate::tools::capability::search::CatalogSearch;
use crate::tools::capability::search_tool::{CapabilitySearchPort, MAX_QUERY_BYTES, MAX_SEARCH_HITS};
pub(super) use append::DiscoveryAdmission;
use std::sync::Weak;

#[derive(serde::Serialize)]
pub(crate) struct CapabilitySearchHit {
    capability: CapabilitySearchReference,
    schema: ToolSpec,
}

/// Public offer metadata carries no execution authority.
#[derive(serde::Serialize)]
struct CapabilitySearchReference {
    id: String,
    fingerprint: String,
    generation: String,
}

/// Weak references prevent retaining this tool from keeping a completed turn
/// alive. The original proof is captured by runtime, never supplied in args.
pub(crate) struct CapabilitySearchAuthority {
    service: Weak<TurnDecisionService>,
    context: Weak<TurnDecisionContext>,
    proof: OwnedTurnSourceProof,
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
        Ok(CapabilitySearchPort::new(Arc::new(CapabilitySearchAuthority {
            service: Arc::downgrade(self),
            context: Arc::downgrade(context),
            proof: OwnedTurnSourceProof::capture(proof),
        })))
    }

    /// Stable control definition, separate from the selected business refs and
    /// the two fixed runtime dependencies. Explicit exclusions still apply.
    pub(crate) fn agent_capability_search_spec(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<Option<ToolSpec>> {
        if !matches!(
            context.directive(),
            TurnExecutionDirective::AnswerDirectly | TurnExecutionDirective::Execute { .. }
        ) || context.execution_closed.load(Ordering::Acquire)
            || !proof.tool_calls_allowed()
            || !self.model_can_call_tools(proof)
        {
            return Ok(None);
        }
        if matches!(context.directive(), TurnExecutionDirective::AnswerDirectly) {
            let signals = crate::decision::builtin::features::text_signals(context.engine.state.routing_input.as_str());
            if !signals.structural()
                && signals.request_weight <= crate::decision::builtin::weights::BRIEF_REQUEST_MAX_WEIGHT
            {
                return Ok(None);
            }
        }
        if self.discovery_families(proof, context)?.is_empty() {
            return Ok(None);
        }
        let entry = self
            .catalog
            .entry_for_public_name("capability_search")
            .context("discoverable turn lacks the capability search backend")?;
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
            .context("capability search descriptor is absent")?;
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

    fn discovery_families(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<Vec<CapabilityTarget>> {
        anyhow::ensure!(
            !context.execution_closed.load(Ordering::Acquire)
                && matches!(
                    context.directive(),
                    TurnExecutionDirective::AnswerDirectly | TurnExecutionDirective::Execute { .. }
                )
                && proof.tool_calls_allowed()
                && self.model_can_call_tools(proof),
            "turn does not permit capability discovery"
        );
        self.agent_execution_specs(proof, context)?;
        let CandidateSet::Legal {
            discoverable_families, ..
        } = &context.engine.candidates
        else {
            anyhow::bail!("turn has no admitted candidate set");
        };
        Ok(discoverable_families.iter().collect())
    }
}

impl CapabilitySearchAuthority {
    pub(crate) fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<CapabilitySearchHit>> {
        anyhow::ensure!(
            !query.trim().is_empty() && query.len() <= MAX_QUERY_BYTES && (1..=MAX_SEARCH_HITS).contains(&limit),
            "capability search query or limit is outside bounds"
        );
        let service = self.service.upgrade().context("capability search service expired")?;
        let context = self.context.upgrade().context("capability search turn expired")?;
        anyhow::ensure!(
            service.agent_capability_search_spec(&self.proof, &context)?.is_some(),
            "capability search is no longer exposed"
        );
        let families = service.discovery_families(&self.proof, &context)?;
        let view = service
            .catalog
            .view(&CatalogScope::default())
            .with_surface(&service.publication_policy);
        let trusted = context.engine.state.trusted.parts();
        let mut results = Vec::new();
        let mut issued = Vec::new();
        let hits = CatalogSearch::find_filtered(&view, query, &families, limit, |entry| {
            !matches!(
                entry.public_name.as_str(),
                "capability_search" | "stay_silent" | "transcript_history_lookup" | "delegate" | "sessions_spawn"
            ) && matches!(entry.exposure, Exposure::Searchable)
                && entry.availability.level >= CapabilityAvailabilityLevel::Configured
                && !trusted.excludes_entry(view.model_admission_entry(entry))
                && (trusted.channel.is_none() || entry.channel_eligible)
                && self.proof.child_scope().is_none_or(|scope| {
                    scope.gate_inputs().0.allowed.iter().any(|reference| {
                        reference.id == entry.id
                            && reference.fingerprint == entry.fingerprint
                            && reference.generation == entry.generation
                    })
                })
                && crate::tools::execution::decide_tool_execution(
                    &service.security,
                    entry.policy_tool_name(),
                    self.proof.policy_sender(),
                    self.proof.policy_channel(),
                    self.proof.policy_chat_type(),
                ) != crate::tools::execution::ToolExecutionDecision::Deny
        });
        for hit in hits {
            let entry = hit.entry;
            let descriptor = service
                .execution
                .current_descriptor(&entry.public_name)
                .context("discovered capability has no executable descriptor")?;
            let schema = ToolSpec {
                name: descriptor.public_name,
                description: descriptor.description,
                parameters: descriptor.parameters,
            };
            anyhow::ensure!(
                crate::tools::capability::fingerprint::tool_spec_fingerprint(
                    &schema.name,
                    &schema.description,
                    &schema.parameters,
                ) == entry.fingerprint,
                "discovered capability schema changed"
            );
            issued.push((
                entry.public_name.clone(),
                CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                },
            ));
            results.push(CapabilitySearchHit {
                capability: CapabilitySearchReference {
                    id: entry.id.as_str().to_owned(),
                    fingerprint: hex::encode(entry.fingerprint.0),
                    generation: entry.generation.to_hex(),
                },
                schema,
            });
        }
        // Recheck lifetime and parity after reading the descriptor snapshot.
        service.discovery_families(&self.proof, &context)?;
        if !issued.is_empty() {
            let mut offers = context.discovery_offers.get_or_init(Default::default).lock();
            // Serialize issuance with terminal clearing. Stored refs are offers,
            // never entries in the executable capability set.
            anyhow::ensure!(
                !context.execution_closed.load(Ordering::Acquire),
                "capability search turn expired"
            );
            offers.extend(issued);
        }
        Ok(results)
    }
}
