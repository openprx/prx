//! Read back ordinary process input without inventing durable provenance.
pub(crate) mod journal;
mod segments;
pub(crate) use segments::OrdinaryProcessSegments;

use super::{
    Arc, ChildInputReceipt, ChildProviderBinding, Context, MemoryFabric, MemoryVisibility, MessageEventScope,
    MessageEventSource, OrdinaryChildModelSelection, parent,
};

pub(crate) async fn restore_input(
    fabric: &MemoryFabric,
    worker: &crate::session_worker::protocol::WorkerManifest,
    scope: &crate::tools::capability::child::VerifiedChildScope,
    parent_source: &parent::ParentSource,
) -> anyhow::Result<Option<Arc<ChildInputReceipt>>> {
    let source = worker
        .ordinary_source
        .as_ref()
        .context("ordinary worker source missing")?;
    let (child, parent) = scope.gate_inputs();
    anyhow::ensure!(
        worker.child_source.is_none()
            && worker.capability_manifest.is_none()
            && crate::tools::capability::child::ChildCapabilityManifest::from_worker_manifest(
                &source.capability_manifest,
                parent,
            )? == *child
            && fabric.workspace_id() == child.authority.workspace
            && parent_source.workspace_id() == fabric.workspace_id()
            && parent_source.owner_id() == child.authority.owner
            && worker.owner_id.as_deref() == Some(parent_source.owner_id())
            && worker.parent_run_id.as_deref() == Some(parent_source.run_id()),
        "ordinary worker input lookup differs from verified source"
    );
    if let Some(id) = parent_source.source_event_id() {
        let event = fabric
            .memory()
            .find_owned_message_event(fabric.workspace_id(), parent_source.owner_id(), id)
            .await?
            .context("ordinary worker parent event is unavailable")?;
        anyhow::ensure!(
            event.id > 0
                && event.event_id == id
                && event.workspace_id == parent_source.workspace_id()
                && event.owner_id.as_deref() == Some(parent_source.owner_id())
                && event.run_id.as_deref() == Some(parent_source.run_id())
                && event.session_key.as_deref() == Some(parent_source.session_key())
                && event.role == "user"
                && event.event_type == "message.created"
                && crate::runtime::turn_source::recorded_input_matches(
                    parent_source.surface(),
                    &event,
                    parent_source.routing_input(),
                ),
            "ordinary worker parent event differs from signed provenance"
        );
    }
    let recording = &worker.memory_event_recording;
    let Some(input) = &source.input else {
        anyhow::ensure!(
            !recording.enabled || !recording.record_user_messages,
            "ordinary worker recorded input is missing"
        );
        return Ok(None);
    };
    anyhow::ensure!(!input.event_id.is_empty(), "ordinary worker input identity is empty");
    let event = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), parent_source.owner_id(), &input.event_id)
        .await?
        .context("ordinary worker input is unavailable")?;
    anyhow::ensure!(
        parent.key.verify_hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"worker-child-input-v1",
                child.parent_decision_id.to_string().as_bytes(),
                &serde_json::to_vec(&event)?
            ],
            &input.event_mac,
        ) && event.id > 0
            && event.event_id == input.event_id
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id.as_deref() == Some(parent_source.owner_id())
            && event.source == MessageEventSource::SessionsSpawn
            && event.visibility == MemoryVisibility::Workspace
            && event.role == "user"
            && event.event_type == "message.created"
            && event.content == worker.task
            && event.run_id.as_deref() == Some(worker.run_id.as_str())
            && event.session_key.as_deref() == Some(worker.session_scope_key.as_str())
            && event.parent_run_id.as_deref() == Some(parent_source.run_id())
            && event.parent_session_key.as_deref() == Some(parent_source.session_key())
            && event.causation_event_id.as_deref() == parent_source.source_event_id()
            && event.run_id != event.parent_run_id
            && event.config_generation_id == worker.runtime_config_generation_id
            && event.config_source_revision == worker.runtime_config_source_revision
            && event.channel == worker.scope_channel
            && event.sender == worker.scope_sender
            && event.attempt_id == source.attempt_id
            && event.lease_epoch == source.lease_epoch
            && event.recipient == worker.scope_chat_id
            && event.agent_id == worker.agent_id
            && event.persona_id == worker.persona_id,
        "ordinary worker input differs from authenticated lineage"
    );
    Ok(Some(Arc::new(ChildInputReceipt {
        previous_segment_input: None,
        segment_issued: false,
        memory: fabric.memory(),
        input: Arc::from(worker.task.as_str()),
        event,
        authority_guard: None,
    })))
}

/// A process owns these verified snapshots independently of the parent's stack.
/// They still require input readback and a lifetime before source issuance.
pub(crate) struct OrdinaryProcessSnapshots {
    pub scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
    pub catalog: Arc<crate::tools::capability::catalog::CapabilityCatalog>,
    pub generation: Arc<crate::config::ConfigGeneration>,
    pub provider_catalog: Arc<crate::providers::catalog::CatalogView>,
    pub parent: parent::ParentSource,
}

pub(crate) struct OrdinaryProcessAuthority {
    id: uuid::Uuid,
    run_id: String,
    session_key: String,
    task: String,
    initial_scope: MessageEventScope,
    sequence_claimed: std::sync::atomic::AtomicBool,
    snapshots: OrdinaryProcessSnapshots,
    binding: Arc<ChildProviderBinding>,
    input: Option<Arc<ChildInputReceipt>>,
    fabric: MemoryFabric,
    cancellation: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for OrdinaryProcessAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrdinaryProcessAuthority").finish_non_exhaustive()
    }
}

/// Kept by the process driver until terminal settlement. Every exit path,
/// including cancellation and unwind, revokes retained source authority.
#[derive(Debug)]
pub(crate) struct OrdinaryProcessLifetime(tokio_util::sync::CancellationToken);
impl Drop for OrdinaryProcessLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl OrdinaryProcessSnapshots {
    pub(crate) async fn issue(
        self,
        worker: &crate::session_worker::protocol::WorkerManifest,
        fabric: MemoryFabric,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<(Arc<OrdinaryProcessAuthority>, OrdinaryProcessLifetime)> {
        let cancellation = cancellation.child_token();
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "ordinary worker was cancelled before issuance"
        );
        let input = restore_input(&fabric, worker, &self.scope, &self.parent).await?;
        let selection = &worker
            .ordinary_source
            .as_ref()
            .context("ordinary worker source missing")?
            .provider_selection;
        let route =
            crate::providers::selection::route_target(&self.provider_catalog, &selection.provider, &selection.model)?
                .cloned();
        let resolved = crate::providers::selection::check_with(
            &self.generation.effective,
            &self.provider_catalog,
            &selection.provider,
            &selection.model,
            selection.credential.clone(),
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        let binding = Arc::new(ChildProviderBinding::from_selection(
            None,
            Arc::clone(&self.generation),
            &self.provider_catalog,
            &selection.provider,
            &selection.model,
            resolved,
            route,
        )?);
        let source = worker
            .ordinary_source
            .as_ref()
            .context("ordinary worker source missing")?;
        let mut initial_scope = MessageEventScope::new("session_worker", MemoryVisibility::Workspace);
        initial_scope.owner_id = Some(self.parent.owner_id().to_owned());
        initial_scope.channel = Some(self.parent.policy_channel().to_owned());
        initial_scope.sender = Some(self.parent.policy_sender().to_owned());
        initial_scope.recipient = worker.scope_chat_id.clone();
        initial_scope.session_key = Some(worker.session_scope_key.clone());
        initial_scope.parent_session_key = Some(self.parent.session_key().to_owned());
        initial_scope.run_id = Some(worker.run_id.clone());
        initial_scope.parent_run_id = Some(self.parent.run_id().to_owned());
        initial_scope.agent_id = worker.agent_id.clone();
        initial_scope.persona_id = worker.persona_id.clone();
        initial_scope.causation_event_id = self.parent.source_event_id().map(str::to_owned);
        initial_scope.attempt_id = source.attempt_id.clone();
        initial_scope.lease_epoch = source.lease_epoch;
        initial_scope.config_generation_id = Some(self.generation.id.0);
        initial_scope.config_source_revision = Some(self.generation.source_revision.fingerprint_sha256.clone());
        if let Some(receipt) = &input {
            initial_scope.goal_id = receipt.event().goal_id.clone();
            initial_scope.correlation_id = receipt
                .event()
                .correlation_id
                .clone()
                .or_else(|| Some(receipt.event().event_id.clone()));
        }
        let authority = Arc::new(OrdinaryProcessAuthority {
            id: uuid::Uuid::now_v7(),
            run_id: worker.run_id.clone(),
            session_key: worker.session_scope_key.clone(),
            task: worker.task.clone(),
            initial_scope,
            sequence_claimed: std::sync::atomic::AtomicBool::new(false),
            snapshots: self,
            binding,
            input,
            fabric,
            cancellation: cancellation.clone(),
        });
        authority.verify()?;
        Ok((authority, OrdinaryProcessLifetime(cancellation)))
    }
}

impl OrdinaryProcessAuthority {
    pub(crate) fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancellation.is_cancelled(), "ordinary worker authority is closed");
        let source = &self.snapshots;
        anyhow::ensure!(
            !source.generation.effective.causal_tree.enabled
                && Arc::ptr_eq(&self.binding.generation, &source.generation)
                && source.parent.workspace_id() == self.fabric.workspace_id()
                && source.parent.config_generation_id() == source.generation.id.0
                && source.parent.config_source_revision() == source.generation.source_revision.fingerprint_sha256,
            "ordinary worker source configuration differs"
        );
        let (_, parent) = source.scope.gate_inputs();
        source.scope.validate_runtime(
            &parent.key,
            source.parent.owner_id(),
            source.parent.workspace_id(),
            *source.catalog.generation(),
        )?;
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "ordinary worker authority closed during verification"
        );
        Ok(())
    }

    pub(crate) const fn id(&self) -> uuid::Uuid {
        self.id
    }
    pub(crate) const fn snapshots(&self) -> &OrdinaryProcessSnapshots {
        &self.snapshots
    }
    pub(crate) const fn binding(&self) -> &Arc<ChildProviderBinding> {
        &self.binding
    }
    pub(crate) fn input(&self) -> Option<&ChildInputReceipt> {
        self.input.as_deref()
    }
    pub(crate) fn logical_scope(&self) -> MessageEventScope {
        let mut scope = self.initial_scope.clone();
        scope.subject = Some(crate::memory::MessageEventSubject::Task(self.run_id.clone()));
        scope
    }

    pub(crate) const fn source_fabric(&self) -> &MemoryFabric {
        &self.fabric
    }
}

impl OrdinaryProcessAuthority {
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }
    pub(crate) fn session_key(&self) -> &str {
        &self.session_key
    }
    pub(crate) fn task(&self) -> &str {
        &self.task
    }
    pub(in crate::runtime) fn model_selection(&self) -> OrdinaryChildModelSelection<'_> {
        OrdinaryChildModelSelection {
            generation: &self.snapshots.generation,
            provider_name: &self.binding.requested_provider,
            model_name: &self.binding.requested_model,
            resolved: &self.binding.resolved,
            route: self.binding.route.clone(),
        }
    }
}

impl OrdinaryProcessAuthority {
    pub(crate) fn source_proof(&self) -> anyhow::Result<crate::runtime::turn_source::OwnedTurnSourceProof> {
        crate::runtime::turn_source::OwnedTurnSourceProof::from_ordinary_process(self)
    }

    pub(crate) fn verify_source(&self, proof: &dyn crate::runtime::turn_source::TurnSourceProof) -> anyhow::Result<()> {
        self.verify()?;
        anyhow::ensure!(
            proof.ordinary_process_authority_id() == Some(self.id)
                && proof.owner_isolated() == self.snapshots.parent.owner_isolated()
                && proof.ordinary_task_authority_id().is_none()
                && proof
                    .child_scope()
                    .is_some_and(|scope| Arc::ptr_eq(scope, &self.snapshots.scope)),
            "ordinary worker source belongs to a different process lifetime"
        );
        Ok(())
    }
}
