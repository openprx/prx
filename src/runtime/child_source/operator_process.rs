//! Process-owned operator lifetime, issued after actual input readback.
mod segments;
pub(crate) use segments::OperatorProcessSegments;
mod completion;
pub(crate) mod journal;
use super::{
    Arc, ChildInputReceipt, ChildProviderBinding, Context, MemoryFabric, MemoryVisibility, MessageEventScope,
    OrdinaryChildModelSelection,
};
use crate::runtime::operator_spawn::process::OperatorProcessSnapshots;

pub(crate) struct OperatorProcessAuthority {
    id: uuid::Uuid,
    run_id: String,
    session_key: String,
    task: String,
    initial_scope: MessageEventScope,
    sequence_claimed: std::sync::atomic::AtomicBool,
    snapshots: OperatorProcessSnapshots,
    binding: Arc<ChildProviderBinding>,
    input: Option<Arc<ChildInputReceipt>>,
    fabric: MemoryFabric,
    cancellation: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for OperatorProcessAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorProcessAuthority").finish_non_exhaustive()
    }
}

/// Kept by the process driver until terminal settlement. Every exit path,
/// including cancellation and unwind, revokes retained source authority.
#[derive(Debug)]
pub(crate) struct OperatorProcessLifetime(tokio_util::sync::CancellationToken);
impl Drop for OperatorProcessLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl OperatorProcessSnapshots {
    pub(crate) async fn issue(
        self,
        worker: &crate::session_worker::protocol::WorkerManifest,
        fabric: MemoryFabric,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<(Arc<OperatorProcessAuthority>, OperatorProcessLifetime)> {
        let cancellation = cancellation.child_token();
        let lifetime = OperatorProcessLifetime(cancellation.clone());
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "operator worker was cancelled before issuance"
        );
        let input = super::operator_process_input::restore_input(&fabric, worker, &self).await?;
        let selection = &self.selection;
        let route = self.route.clone();
        let resolved = self.resolved.clone();
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
            .operator_source
            .as_ref()
            .context("operator worker source missing")?;
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
        let authority = Arc::new(OperatorProcessAuthority {
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
        Ok((authority, lifetime))
    }
}

impl OperatorProcessAuthority {
    pub(crate) fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancellation.is_cancelled(), "operator worker authority is closed");
        let source = &self.snapshots;
        anyhow::ensure!(
            Arc::ptr_eq(&self.binding.generation, &source.generation)
                && source.parent.workspace_id() == self.fabric.workspace_id()
                && source.parent.config_generation_id() == source.generation.id.0
                && source.parent.config_source_revision() == source.generation.source_revision.fingerprint_sha256,
            "operator worker source configuration differs"
        );
        anyhow::ensure!(
            source.publication.owner() == source.parent.owner_id()
                && source.publication.workspace() == self.fabric.workspace_id()
                && self.binding.grant.is_none()
                && self.binding.requested_provider == source.selection.provider
                && self.binding.requested_model == source.selection.model
                && self.binding.resolved == source.resolved
                && self.binding.route == source.route,
            "operator worker binding differs from publication"
        );
        let security = crate::security::SecurityPolicy::from_config(
            &source.generation.effective.autonomy,
            &source.generation.effective.workspace_dir,
        );
        source
            .publication
            .verify_policy(&security, &source.parent, &source.catalog)?;
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "operator worker authority closed during verification"
        );
        Ok(())
    }

    pub(crate) const fn id(&self) -> uuid::Uuid {
        self.id
    }
    pub(crate) const fn snapshots(&self) -> &OperatorProcessSnapshots {
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

impl OperatorProcessAuthority {
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

impl OperatorProcessAuthority {
    pub(crate) fn source_proof(&self) -> anyhow::Result<crate::runtime::turn_source::OwnedTurnSourceProof> {
        crate::runtime::turn_source::OwnedTurnSourceProof::from_operator_process(self)
    }
    pub(crate) fn verify_source(&self, proof: &dyn crate::runtime::turn_source::TurnSourceProof) -> anyhow::Result<()> {
        self.verify()?;
        let parent = &self.snapshots.parent;
        anyhow::ensure!(
            proof.operator_process_authority_id() == Some(self.id)
                && proof.owner_isolated() == parent.owner_isolated()
                && proof.operator_task_authority_id().is_none()
                && proof.ordinary_task_authority_id().is_none()
                && proof.ordinary_process_authority_id().is_none()
                && proof.child_scope().is_none()
                && proof.surface() == crate::causal_tree::state::TurnSurfaceKind::Worker
                && proof.workspace_id() == parent.workspace_id()
                && proof.owner_id() == parent.owner_id()
                && proof.session_key() == self.session_key
                && proof.task_id() == Some(self.run_id.as_str())
                && proof.provider_name() == self.binding.requested_provider
                && proof.model_name() == self.binding.requested_model
                && proof.model_binding().matches(&self.binding.resolved)
                && proof.model_binding().route() == self.binding.route.as_ref()
                && proof.config_generation_id() == self.snapshots.generation.id.0
                && proof.config_source_revision() == parent.config_source_revision()
                && proof.policy_channel() == parent.policy_channel()
                && proof.policy_sender() == parent.policy_sender()
                && proof.policy_chat_type() == parent.policy_chat_type(),
            "operator worker proof belongs to another authority"
        );
        Ok(())
    }
}
