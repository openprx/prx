//! Export ordinary provenance only from a still-live selected spawn call.
use super::{
    Arc, CatalogScope, CausalDecisionStore, Context, DecisionBackendFactory, FactoryDeps, OrdinaryChildPublication,
    TurnDecisionService, TurnLoopRuntime, TurnServiceBuild, TurnSourceProof, TurnSurfaceKind, pinned_policy_version,
};
use crate::runtime::child_source::{ChildInputReceipt, ChildProviderBinding};
use crate::session_worker::protocol::{
    WorkerManifest, WorkerOrdinarySource, WorkerProviderSelection, WorkerRecordedInput,
};

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
                && worker.child_source.is_none()
                && worker.capability_manifest.is_none()
                && worker.ordinary_source.is_none()
                && worker.operator_source.is_none()
                && worker.task == proof.routing_input()
                && worker.config_generation == self.config_generation,
            "ordinary worker manifest differs from captured launch"
        );
        anyhow::ensure!(
            worker.run_id == proof.run_id()
                && worker.session_scope_key == proof.session_key()
                && publication.generation().effective.workspace_dir.to_string_lossy() == proof.workspace_id()
                && worker.owner_id.as_deref() == Some(proof.owner_id())
                && worker.parent_run_id == envelope.parent_run_id
                && worker.agent_id == envelope.agent_id
                && worker.persona_id == envelope.persona_id
                && worker.scope_chat_id == envelope.recipient
                && worker.scope_channel.as_deref() == Some(proof.policy_channel())
                && worker.scope_sender.as_deref() == Some(proof.policy_sender())
                && worker.scope_chat_type.as_deref() == Some(proof.policy_chat_type())
                && worker.runtime_config_generation_id == Some(proof.config_generation_id())
                && worker.runtime_config_source_revision.as_deref() == Some(proof.config_source_revision()),
            "ordinary worker manifest differs from its issued input"
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
        anyhow::ensure!(
            crate::providers::ProviderId::of(resolved.provider_label()) == source_provider
                && crate::providers::ProviderId::of(&worker.provider_name) == source_provider
                && resolved.model() == proof.model_name()
                && worker.model == proof.model_name()
                && resolved.credential().reference() == proof.model_binding().credential_reference(),
            "ordinary worker provider selection differs from its source"
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
        let key = crate::decision::key::DecisionKey::from_worker_channel(
            transport_key,
            proof.workspace_id(),
            &worker.run_id,
            &worker.config_generation,
        )?;
        let scope = publication.scope().rebind_process_key(key)?;
        let (child, parent) = scope.gate_inputs();
        let input = receipt
            .map(|receipt| -> anyhow::Result<_> {
                Ok(WorkerRecordedInput {
                    event_id: receipt.event().event_id.clone(),
                    event_mac: parent.key.hmac(
                        crate::decision::key::KeyDomain::StateFingerprint,
                        &[
                            b"worker-child-input-v1",
                            child.parent_decision_id.to_string().as_bytes(),
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
            parent_source: crate::runtime::child_source::parent::ParentSource::capture(&publication.0.source)
                .bind_process(child.parent_decision_id, &parent.key)?,
            provider_catalog: publication
                .provider_catalog()
                .bind_process(&parent.key, child.parent_decision_id)?,
            parent_constraints: parent.to_worker_constraints()?,
            capability_manifest: child.to_worker_manifest(),
            config_generation: publication.generation().bind_process(&parent.key)?,
            catalog: publication.0.service.catalog.bind_process(&parent.key)?,
        };
        // Do not partially mutate the manifest if snapshot or backend revalidation fails.
        publication.verify_task_scope()?;
        let root = matches!(
            resolved.credential().reference(),
            crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
        );
        worker.api_key = resolved.credential().api_key().map(str::to_owned);
        worker.direct_uses_root_connection = route.is_none() && root;
        worker.route_uses_root_connection = route.is_some() && root;
        worker.route_source_provider = route.map(|_| selection.provider.clone());
        worker.route_credential = route.map(|_| resolved.credential().reference().clone());
        worker.ordinary_source = Some(source);
        Ok(())
    }
}

impl TurnDecisionService {
    /// Bind the restored source to this process's actual executor and runtime
    /// extensions. The signed catalog alone cannot authorize execution.
    pub(crate) fn ordinary_process_service(
        authority: Arc<crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority>,
        build: TurnServiceBuild<'_>,
        runtime: TurnLoopRuntime,
    ) -> anyhow::Result<Arc<Self>> {
        authority.verify()?;
        let snapshots = authority.snapshots();
        anyhow::ensure!(
            build.parent_grant.is_none()
                && Arc::ptr_eq(&build.generation, &snapshots.generation)
                && Arc::ptr_eq(&build.memory, &authority.source_fabric().memory())
                && build.catalog_sources.config_generation_id == snapshots.generation.id.0.to_string(),
            "ordinary worker service differs from restored source"
        );
        let (child, parent) = snapshots.scope.gate_inputs();
        let view = snapshots
            .catalog
            .view(&CatalogScope::default())
            .with_surface(&build.surface_policy);
        for reference in &child.allowed {
            let entry = view
                .entry_any(&reference.id)
                .context("ordinary worker capability missing")?;
            anyhow::ensure!(
                !view.surface_excludes(entry),
                "ordinary worker capability exceeds executor policy"
            );
        }
        snapshots
            .catalog
            .check_child_parity(&build.execution, &snapshots.scope)?;
        let config = &snapshots.generation.effective;
        let policy_version = pinned_policy_version(config, &build.security, &build.surface_policy, &parent.key)?;
        let factory = DecisionBackendFactory::from_config(
            config,
            FactoryDeps {
                catalog: &snapshots.provider_catalog,
                key: None,
                transports: build.transports,
                config_generation: snapshots.generation.id.0,
                events: Arc::clone(&build.events),
            },
        )?
        .with_unproven_budget_gate();
        let store = CausalDecisionStore::open_unshared_read_only(&config.workspace_dir);
        let service = Arc::new(Self {
            child_recovery: Default::default(),
            memory_loader: None,
            loop_runtime: Some(runtime),
            publication_policy: build.surface_policy,
            ordinary_parent: None,
            operator_task: None,
            operator_process: None,
            ordinary_task: None,
            ordinary_process: Some(Arc::clone(&authority)),
            parent_grant: None,
            service_id: uuid::Uuid::now_v7(),
            surface: TurnSurfaceKind::Worker,
            generation: Arc::clone(&snapshots.generation),
            catalog: Arc::clone(&snapshots.catalog),
            provider_catalog: snapshots.provider_catalog.as_ref().clone(),
            factory,
            events: build.events,
            state_key: Arc::clone(&parent.key),
            policy_version,
            durable_key: false,
            recordable: false,
            registry: build.registry,
            execution: build.execution,
            memory: build.memory,
            security: build.security,
            store,
            durable_events: None,
        });
        service.check_live_parity()?;
        Ok(service)
    }
}
