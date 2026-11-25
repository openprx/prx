//! Export ordinary provenance only from a still-live selected spawn call.
use super::OrdinaryChildPublication;
use crate::runtime::child_source::{ChildInputReceipt, ChildProviderBinding};
use crate::runtime::turn_source::TurnSourceProof;
use crate::session_worker::protocol::{
    WorkerCallContext, WorkerCallInputs, WorkerManifest, WorkerOrdinarySource, WorkerProviderSelection,
    WorkerRecordedInput,
};
use std::sync::Arc;

/// Captured while the selected parent call is live; consumed by its sole
/// process owner. It carries no transport key and cannot admit execution.
pub(crate) struct OrdinaryProcessLaunch {
    publication: OrdinaryChildPublication,
    proof: Arc<crate::runtime::turn_source::OwnedTurnSourceProof>,
    execution: crate::tools::ToolExecutionContext,
    receipt: Option<Arc<ChildInputReceipt>>,
    selection: WorkerProviderSelection,
    config_generation: String,
}
impl std::fmt::Debug for OrdinaryProcessLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrdinaryProcessLaunch").finish_non_exhaustive()
    }
}

impl OrdinaryChildPublication {
    pub(crate) fn capture_process_launch(
        &self,
        caller: &crate::tools::execution::ChildCallContext,
        binding: &ChildProviderBinding,
        input_scope: &crate::memory::MessageEventScope,
        input: &str,
        receipt: Option<Arc<ChildInputReceipt>>,
        selection: WorkerProviderSelection,
        config_generation: String,
    ) -> anyhow::Result<OrdinaryProcessLaunch> {
        caller.verify_tool("sessions_spawn")?;
        self.verify_caller(caller.context())?;
        anyhow::ensure!(
            caller.context().ordinary_child_publication.as_ref() == Some(self)
                && input_scope.source == crate::memory::MessageEventSource::SessionsSpawn
                && !config_generation.is_empty(),
            "ordinary process launch source is incomplete"
        );
        let (proof, execution) =
            binding.ordinary_execution_source(&binding.provider(), caller, input_scope, input, receipt.as_deref())?;
        self.verify_caller(caller.context())?;
        Ok(OrdinaryProcessLaunch {
            publication: self.clone(),
            proof,
            execution,
            receipt,
            selection,
            config_generation,
        })
    }
}

impl OrdinaryProcessLaunch {
    pub(crate) fn persona_id(&self) -> Option<&str> {
        self.execution.envelope.persona_id.as_deref()
    }
    pub(crate) fn generation(&self) -> &Arc<crate::config::ConfigGeneration> {
        self.publication.generation()
    }
    pub(crate) fn source_fabric(&self) -> crate::memory::MemoryFabric {
        self.publication.source_fabric()
    }

    /// The process owner must sign the complete manifest with this same key.
    /// Consuming the capture prevents a second export after the parent closes.
    pub(crate) fn bind_worker_manifest(
        self,
        transport_key: &ring::hmac::Key,
        worker: &mut WorkerManifest,
    ) -> anyhow::Result<()> {
        let publication = &self.publication;
        publication.verify_task_scope()?;
        let proof = &self.proof;
        let selection = &self.selection;
        let receipt = self.receipt.as_deref();
        let envelope = &self.execution.envelope;
        anyhow::ensure!(
            worker.parent_capability.is_none()
                && worker.ordinary_source.is_none()
                && worker.operator_source.is_none()
                && worker.direct_tools.is_none()
                && worker.call_context.is_none()
                && worker.task == proof.routing_input()
                && worker.config_generation == self.config_generation,
            "ordinary worker manifest differs from captured launch"
        );
        // Name the differing fields (never their values).
        let differing: Vec<&str> = [
            ("run_id", worker.run_id == proof.run_id()),
            ("session_scope_key", worker.session_scope_key == proof.session_key()),
            (
                "workspace",
                publication.generation().effective.workspace_dir.to_string_lossy() == proof.workspace_id(),
            ),
            ("owner_id", worker.owner_id.as_deref() == Some(proof.owner_id())),
            ("parent_run_id", worker.parent_run_id == envelope.parent_run_id),
            ("agent_id", worker.agent_id == envelope.agent_id),
            ("persona_id", worker.persona_id == envelope.persona_id),
            ("scope_chat_id", worker.scope_chat_id == envelope.recipient),
            (
                "scope_channel",
                worker.scope_channel.as_deref() == Some(proof.policy_channel()),
            ),
            (
                "scope_sender",
                worker.scope_sender.as_deref() == Some(proof.policy_sender()),
            ),
            (
                "scope_chat_type",
                worker.scope_chat_type.as_deref() == Some(proof.policy_chat_type()),
            ),
            (
                "config_generation_id",
                worker.runtime_config_generation_id == Some(proof.config_generation_id()),
            ),
            (
                "config_source_revision",
                worker.runtime_config_source_revision.as_deref() == Some(proof.config_source_revision()),
            ),
        ]
        .into_iter()
        .filter_map(|(field, equal)| (!equal).then_some(field))
        .collect();
        anyhow::ensure!(
            differing.is_empty(),
            "ordinary worker manifest differs from its issued input: {}",
            differing.join(", ")
        );
        let resolved = crate::providers::selection::check_with(
            &publication.generation().effective,
            publication.provider_catalog(),
            &selection.provider,
            &selection.model,
            selection.credential.clone(),
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        let source_provider = crate::providers::ProviderId::of(proof.provider_name());
        // A route spelling (`hint:<name>`) also names its provider: the
        // provider pinned at ingress is the route's, not the selection's.
        let pinned_provider = crate::providers::ProviderId::of(proof.model_binding().pinned_provider_label());
        let worker_provider = crate::providers::ProviderId::of(&worker.provider_name);
        // Name the differing fields (never their values).
        let differing: Vec<&str> = [
            (
                "resolved_provider",
                crate::providers::ProviderId::of(resolved.provider_label()) == pinned_provider,
            ),
            (
                "worker_provider",
                worker_provider == source_provider || worker_provider == pinned_provider,
            ),
            // A route spelling (`hint:<name>`) resolves to the model pinned
            // at ingress; the worker may carry either spelling of it.
            (
                "resolved_model",
                resolved.model() == proof.model_binding().pinned_model(),
            ),
            (
                "worker_model",
                worker.model == proof.model_name() || worker.model == proof.model_binding().pinned_model(),
            ),
            (
                "credential",
                resolved.credential().reference() == proof.model_binding().credential_reference(),
            ),
        ]
        .into_iter()
        .filter_map(|(field, equal)| (!equal).then_some(field))
        .collect();
        anyhow::ensure!(
            differing.is_empty(),
            "ordinary worker provider selection differs from its source: {}",
            differing.join(", ")
        );
        let route = crate::providers::selection::route_target(
            publication.provider_catalog(),
            &selection.provider,
            &selection.model,
        )?;
        anyhow::ensure!(
            route.map(|route| route.hint.as_str()) == worker.route_hint.as_deref(),
            "ordinary worker route differs from its source"
        );
        let key = crate::security::runtime_key::DecisionKey::from_worker_channel(
            transport_key,
            proof.workspace_id(),
            &worker.run_id,
            &worker.config_generation,
        )?;
        // The sealed ceiling is the parent's whole remaining balance (capped by its
        // own ceiling). The worker's spends reserve in the parent workspace's cost
        // journal, which stays the authority for the total.
        let (cents, _) = publication.allocate_child_budget()?;
        let (issued, original_parent) = publication.scope().gate_inputs();
        // Seal only backends a worker can reconstruct. Interactive event-loop
        // tools remain in the parent catalog, but are not worker capabilities.
        let references: Vec<_> = issued.allowed.iter().cloned().collect();
        let allowed = publication
            .inner
            .service
            .catalog
            .resolve(&references)?
            .into_iter()
            .filter(|entry| entry.process_worker_compatible)
            .map(crate::tools::surface_owner::publication::entry_ref)
            .collect();
        let narrowed = issued.clone().with_cost_ceiling(original_parent, cents)?;
        let scope = crate::tools::capability::child::VerifiedChildScope::new(narrowed, original_parent)?
            .rebind_process_key(key, allowed)?;
        let (child, parent) = scope.gate_inputs();
        let input = receipt
            .map(|receipt| -> anyhow::Result<_> {
                Ok(WorkerRecordedInput {
                    event_id: receipt.event().event_id.clone(),
                    event_mac: parent.key.hmac(
                        crate::security::runtime_key::KeyDomain::StateFingerprint,
                        &[
                            b"worker-child-input-v1",
                            child.parent_authority_id.to_string().as_bytes(),
                            &serde_json::to_vec(receipt.event())?,
                        ],
                    ),
                })
            })
            .transpose()?;
        let source = WorkerOrdinarySource {
            attempt_id: envelope.attempt_id.clone(),
            lease_epoch: envelope.lease_epoch,
            input,
            provider_selection: selection.clone(),
            parent_source: crate::runtime::child_source::parent::ParentSource::capture(&publication.inner.source)
                .bind_process(child.parent_authority_id, &parent.key)?,
            provider_catalog: publication
                .provider_catalog()
                .bind_process(&parent.key, child.parent_authority_id)?,
            parent_constraints: parent.to_worker_constraints()?,
            capability_manifest: child.to_worker_manifest(),
            config_generation: publication.generation().bind_process(&parent.key)?,
            catalog: publication.inner.service.catalog.bind_process(&parent.key)?,
        };
        // Do not partially mutate the manifest if snapshot or backend revalidation fails.
        publication.verify_task_scope()?;
        let root = matches!(
            resolved.credential().reference(),
            crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
        );
        worker.direct_uses_root_connection = route.is_none() && root;
        worker.route_uses_root_connection = route.is_some() && root;
        worker.route_source_provider = route.map(|_| selection.provider.clone());
        worker.route_credential = route.map(|_| resolved.credential().reference().clone());
        worker.direct_tools = publication.chat_lineage_direct_tools();
        // The parent decides the worker's level, place, model and published
        // surface once, here; the worker reads them from the sealed manifest.
        worker.call_context = Some(WorkerCallContext::for_worker(&WorkerCallInputs {
            owner_id: proof.owner_id(),
            session_key: proof.session_key(),
            agent_id: worker.agent_id.as_deref(),
            level: child.authority.level,
            workspace_root: &source.parent_constraints.workspace,
            working_root: source.config_generation.working_root.as_deref(),
            provider: resolved.provider_label(),
            model: resolved.model(),
            config: &publication.generation().effective,
            allowed_tools: &worker.allowed_tools,
            channel_excluded_tools: &worker.channel_excluded_tools,
            direct_tools: worker.direct_tools.as_deref(),
        }));
        worker.ordinary_source = Some(source);
        Ok(())
    }
}
