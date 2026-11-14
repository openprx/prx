//! Discovery and execution-spec methods of one opened turn (R25). Their
//! bodies live in `tools::capability::search_tool` and
//! `tools::capability::execution_binding::specs`, which read the turn through
//! the [`TurnSurfacePort`] implemented here.
use std::collections::BTreeSet;
use std::sync::Arc;

use super::OpenedTurn;
use crate::providers::catalog::ResolvedModel;
use crate::runtime::turn_publication::TurnRuntimeService;
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::ToolSpec;
use crate::tools::capability::catalog::{CapabilityCatalog, SurfacePolicy};
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::execution_binding::specs::{self, ConstrainedToolSelection};
use crate::tools::capability::search_tool::{
    self, CapabilitySearchPort, DiscoveredToolProposal, DiscoveryState, MAX_DISCOVERED_TOOLS_PER_TURN,
};
use crate::tools::capability::turn_state::TrustedParts;
use crate::tools::execution::ToolExecutionService;
use crate::tools::surface_owner::facts::PublicationFacts;
use crate::tools::surface_owner::turn_port::TurnSurfacePort;

/// One opened turn of this service, read through the surface port.
pub(crate) struct TurnSurfaceBinding<'a> {
    service: &'a TurnRuntimeService,
    context: &'a OpenedTurn,
}

impl TurnSurfacePort for TurnSurfaceBinding<'_> {
    fn validate_turn(&self, proof: &dyn TurnSourceProof) -> anyhow::Result<ResolvedModel> {
        let (service, context) = (self.service, self.context);
        anyhow::ensure!(
            context.service_id == service.service_id,
            "decision belongs to another service"
        );
        anyhow::ensure!(
            context.source_actor_matches(Some(proof.policy_channel()), Some(proof.policy_sender())),
            "execution decision belongs to another ingress actor"
        );
        let model = service.resolve_agent_model(proof)?;
        service.check_live_parity()?;
        let catalog = service.catalog_view();
        let model_ref = format!("{}:{}", model.provider_label(), model.model());
        let exclusions = super::opening::effective_agent_exclusions(
            &service.generation.effective,
            &service.registry,
            &catalog,
            model.model(),
            &service.policy_version,
            &service.state_key,
        );
        super::validate_agent_retrieval_provenance(
            proof,
            &context.runtime().trusted,
            &model_ref,
            &service.policy_version,
            &exclusions.names,
            exclusions.model_admission.as_deref(),
        )?;
        anyhow::ensure!(
            context.runtime().routing_input.as_str()
                == crate::multimodal::project_image_markers_for_routing(proof.routing_input()).as_ref(),
            "decision input changed"
        );
        anyhow::ensure!(
            context.catalog_generation() == catalog.generation(),
            "decision catalog changed"
        );
        Ok(model)
    }

    fn resolve_model(&self, proof: &dyn TurnSourceProof) -> anyhow::Result<ResolvedModel> {
        Ok(self.service.resolve_agent_model(proof)?)
    }

    fn model_can_call_tools(&self, proof: &dyn TurnSourceProof) -> bool {
        self.service.model_can_call_tools(proof)
    }

    fn catalog(&self) -> &Arc<CapabilityCatalog> {
        &self.service.catalog
    }

    fn publication_policy(&self) -> &SurfacePolicy {
        &self.service.publication_policy
    }

    fn execution(&self) -> &Arc<ToolExecutionService> {
        &self.service.execution
    }

    fn config(&self) -> &crate::config::Config {
        &self.service.generation.effective
    }

    fn trusted(&self) -> &TrustedParts {
        &self.context.runtime().trusted
    }

    fn facts(&self) -> &PublicationFacts {
        &self.context.runtime().admission
    }

    fn routing_input(&self) -> &str {
        self.context.runtime().routing_input.as_str()
    }

    fn execution_refs(&self) -> &BTreeSet<CapabilityRef> {
        &self.context.runtime().published
    }

    fn outgoing_child(&self) -> bool {
        self.context.runtime().outgoing_delegation.is_some()
    }

    fn execution_closed(&self) -> &Arc<std::sync::atomic::AtomicBool> {
        &self.context.runtime().execution_closed
    }

    fn discovery(&self) -> &DiscoveryState {
        &self.context.runtime().discovery
    }
}

impl TurnRuntimeService {
    pub(crate) const fn surface_port<'a>(&'a self, context: &'a OpenedTurn) -> TurnSurfaceBinding<'a> {
        TurnSurfaceBinding { service: self, context }
    }

    pub(crate) fn capability_search_port(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<OpenedTurn>,
    ) -> anyhow::Result<CapabilitySearchPort> {
        search_tool::capability_search_port(&self.surface_port(context), proof)
    }

    pub(crate) fn agent_capability_search_spec(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
    ) -> anyhow::Result<Option<ToolSpec>> {
        search_tool::capability_search_spec(&self.surface_port(context), proof)
    }

    pub(crate) fn admit_discovered_tool(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
        name_or_id: &str,
    ) -> anyhow::Result<Option<DiscoveredToolProposal>> {
        search_tool::admit_discovered_tool(&self.surface_port(context), proof, name_or_id)
    }

    pub(crate) fn prepare_discovered_tool(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
        name_or_id: &str,
    ) -> anyhow::Result<Option<DiscoveredToolProposal>> {
        search_tool::prepare_discovered_tool(&self.surface_port(context), proof, name_or_id)
    }

    pub(crate) fn admit_offered_discoveries(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
        excluded: impl Fn(&str, &str) -> bool,
    ) -> usize {
        self.admit_offered_discoveries_up_to(proof, context, MAX_DISCOVERED_TOOLS_PER_TURN, excluded)
    }

    pub(crate) fn admit_offered_discoveries_up_to(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
        turn_cap: usize,
        excluded: impl Fn(&str, &str) -> bool,
    ) -> usize {
        search_tool::admit_offered_discoveries(&self.surface_port(context), proof, turn_cap, &excluded)
    }

    pub(crate) fn agent_appended_execution_specs(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
    ) -> anyhow::Result<ConstrainedToolSelection> {
        search_tool::appended_execution_specs(&self.surface_port(context), proof)
    }

    pub(crate) fn agent_execution_specs(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
    ) -> anyhow::Result<ConstrainedToolSelection> {
        specs::execution_specs(&self.surface_port(context), proof)
    }

    pub(crate) fn agent_runtime_dependency_spec(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
        name: &str,
    ) -> anyhow::Result<ToolSpec> {
        specs::runtime_dependency_spec(&self.surface_port(context), proof, name)
    }

    pub(crate) fn agent_prompt_skills(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
    ) -> anyhow::Result<Vec<crate::skills::Skill>> {
        specs::prompt_skills(&self.surface_port(context), proof)
    }
}
