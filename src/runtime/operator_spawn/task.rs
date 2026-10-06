//! Detached task lifetime issued only by a live explicit operator command.
mod segments;

use super::{Arc, AtomicBool, CancellationToken, OperatorSpawnSource, Ordering, OwnedTurnSourceProof, TurnSourceProof};
use crate::runtime::child_source::{ChildInputReceipt, ChildProviderBinding};
use anyhow::Context;

pub(crate) struct OperatorTaskAuthority {
    id: uuid::Uuid,
    source_event_id: Option<String>,
    scope: crate::memory::MessageEventScope,
    sequence_claimed: AtomicBool,
    source: Arc<OperatorSpawnSource>,
    binding: Arc<ChildProviderBinding>,
    run_id: String,
    input: String,
    cancellation: CancellationToken,
}

impl std::fmt::Debug for OperatorTaskAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OperatorTaskAuthority")
    }
}

#[derive(Debug)]
pub(crate) struct OperatorTaskLifetime(CancellationToken);
impl Drop for OperatorTaskLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl OperatorSpawnSource {
    pub(crate) fn issue_task(
        self: &Arc<Self>,
        binding: Arc<ChildProviderBinding>,
        scope: &crate::memory::MessageEventScope,
        input: &str,
        receipt: Option<&ChildInputReceipt>,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<(Arc<OperatorTaskAuthority>, OperatorTaskLifetime)> {
        self.verify_live()?;
        binding.verify_operator_source(self)?;
        anyhow::ensure!(
            self.claimed.load(Ordering::Acquire)
                && input == self.task.trim()
                && Arc::ptr_eq(&binding.provider(), &self.provider)
                && binding.provider_name() == self.resolved.provider_label()
                && binding.model() == self.resolved.model(),
            "operator task differs from its consumed command or provider"
        );
        let run_id = scope.run_id.as_deref().context("operator task run missing")?;
        anyhow::ensure!(
            !run_id.is_empty()
                && run_id != self.proof.run_id()
                && scope.source == crate::memory::MessageEventSource::SessionsSpawn
                && scope.owner_id.as_deref() == Some(self.proof.owner_id())
                && scope.session_key.as_deref() == Some(self.proof.session_key())
                && scope.parent_session_key == scope.session_key
                && scope.parent_run_id.as_deref() == Some(self.proof.run_id())
                && scope.channel.as_deref() == Some(self.proof.policy_channel())
                && scope.sender.as_deref() == Some(self.proof.policy_sender())
                && scope.recipient == self.proof.memory_scope().chat_id
                && scope.config_generation_id == Some(self.generation.id.0)
                && scope.config_source_revision.as_deref() == Some(self.proof.config_source_revision()),
            "operator task scope differs from admitted source"
        );
        let recording = self.generation.effective.memory.event_recording_config();
        if let Some(receipt) = receipt {
            receipt.verify_backend(&self.fabric.memory())?;
            let event = receipt.event();
            anyhow::ensure!(
                receipt.input() == input
                    && event.id > 0
                    && event.workspace_id == self.fabric.workspace_id()
                    && event.source == scope.source
                    && event.owner_id == scope.owner_id
                    && event.run_id == scope.run_id
                    && event.session_key == scope.session_key
                    && event.parent_run_id == scope.parent_run_id
                    && event.parent_session_key == scope.parent_session_key
                    && event.channel == scope.channel
                    && event.sender == scope.sender
                    && event.recipient == scope.recipient
                    && event.agent_id == scope.agent_id
                    && event.persona_id == scope.persona_id
                    && event.subject == scope.subject
                    && event.goal_id == scope.goal_id
                    && event.causation_event_id == scope.causation_event_id
                    && event.correlation_id == scope.correlation_id
                    && event.attempt_id == scope.attempt_id
                    && event.lease_epoch == scope.lease_epoch
                    && event.visibility == scope.visibility
                    && event.role == "user"
                    && event.event_type == "message.created"
                    && event.content == input
                    && event.config_generation_id == scope.config_generation_id
                    && event.config_source_revision == scope.config_source_revision,
                "operator task receipt differs from admitted input"
            );
        } else {
            anyhow::ensure!(
                !recording.enabled || !recording.record_user_messages,
                "operator task recording requires an actual input receipt"
            );
        }
        anyhow::ensure!(
            self.child_issued
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "operator command already issued a task authority"
        );
        let cancellation = cancellation.child_token();
        let authority = Arc::new(OperatorTaskAuthority {
            id: uuid::Uuid::now_v7(),
            source_event_id: receipt.map(|receipt| receipt.event().event_id.clone()),
            scope: scope.clone(),
            sequence_claimed: AtomicBool::new(false),
            source: Arc::clone(self),
            binding,
            run_id: run_id.to_owned(),
            input: input.to_owned(),
            cancellation: cancellation.clone(),
        });
        self.verify_live()?;
        authority.verify()?;
        Ok((authority, OperatorTaskLifetime(cancellation)))
    }
}

impl OperatorTaskAuthority {
    pub(crate) fn parent_source(&self) -> crate::runtime::child_source::parent::ParentSource {
        crate::runtime::child_source::parent::ParentSource::capture(&self.source.proof)
    }

    pub(crate) fn generation(&self) -> &Arc<crate::config::ConfigGeneration> {
        &self.source.generation
    }
    pub(crate) fn fabric(&self) -> &crate::memory::MemoryFabric {
        &self.source.fabric
    }

    pub(crate) fn publication(&self) -> &crate::runtime::turn_decision::OperatorChildPublication {
        &self.source.publication
    }

    pub(crate) const fn logical_scope(&self) -> &crate::memory::MessageEventScope {
        &self.scope
    }

    pub(crate) fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancellation.is_cancelled(), "operator task authority is closed");
        // The command may have returned. The independently owned task lifetime
        // remains subject to the original frozen publication and actual policy.
        self.binding.verify_operator_source(&self.source)?;
        self.source.publication.verify(
            &self.source.proof,
            &self.source.generation,
            &self.source.fabric.memory(),
            &self.source.security,
        )?;
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "operator task closed during verification"
        );
        Ok(())
    }

    pub(crate) fn verify_source(&self, proof: &dyn TurnSourceProof) -> anyhow::Result<()> {
        self.verify()?;
        anyhow::ensure!(
            proof.operator_task_authority_id() == Some(self.id)
                && proof.owner_isolated() == self.source.proof.owner_isolated()
                && proof.ordinary_task_authority_id().is_none()
                && proof.ordinary_process_authority_id().is_none()
                && proof.child_scope().is_none()
                && proof.surface() == crate::causal_tree::state::TurnSurfaceKind::Worker
                && proof.workspace_id() == self.source.proof.workspace_id()
                && proof.owner_id() == self.source.proof.owner_id()
                && proof.session_key() == self.source.proof.session_key()
                && proof.task_id() == Some(self.run_id.as_str())
                && proof.provider_name() == self.source.proof.provider_name()
                && proof.model_name() == self.source.proof.model_name()
                && proof.model_binding().matches(self.source.resolved())
                && proof.model_binding().route() == self.source.route()
                && proof.policy_channel() == self.source.proof.policy_channel()
                && proof.policy_sender() == self.source.proof.policy_sender()
                && proof.policy_chat_type() == self.source.proof.policy_chat_type()
                && proof.config_generation_id() == self.source.generation.id.0
                && proof.config_source_revision() == self.source.proof.config_source_revision(),
            "operator task proof belongs to another authority"
        );
        Ok(())
    }

    pub(crate) fn verify_execution(
        &self,
        run_id: Option<&str>,
        input: &str,
        provider: &Arc<dyn crate::providers::Provider>,
        provider_name: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        self.verify_source(self)?;
        anyhow::ensure!(
            run_id == Some(self.run_id.as_str())
                && input == self.input
                && Arc::ptr_eq(provider, &self.binding.provider())
                && provider_name == self.binding.provider_name()
                && model == self.binding.model(),
            "operator task execution differs from issued source"
        );
        Ok(())
    }
}

impl TurnSourceProof for OperatorTaskAuthority {
    fn owner_isolated(&self) -> bool {
        self.source.proof.owner_isolated()
    }
    fn intercepts_writes(&self) -> bool {
        self.source.proof.intercepts_writes()
    }
    fn source_memory_principal(&self) -> crate::memory::MemoryPrincipal {
        self.source.proof.source_memory_principal()
    }

    fn operator_task_authority_id(&self) -> Option<uuid::Uuid> {
        Some(self.id)
    }
    fn workspace_id(&self) -> &str {
        self.source.proof.workspace_id()
    }
    fn owner_id(&self) -> &str {
        self.source.proof.owner_id()
    }
    fn session_key(&self) -> &str {
        self.source.proof.session_key()
    }
    fn run_id(&self) -> &str {
        &self.run_id
    }
    fn task_id(&self) -> Option<&str> {
        Some(&self.run_id)
    }
    fn routing_input(&self) -> &str {
        &self.input
    }
    fn provider_name(&self) -> &str {
        self.source.proof.provider_name()
    }
    fn model_name(&self) -> &str {
        self.source.proof.model_name()
    }
    fn config_source_revision(&self) -> &str {
        self.source.proof.config_source_revision()
    }
    fn source_event_id(&self) -> Option<&str> {
        self.source_event_id.as_deref()
    }
    fn memory_scope(&self) -> &crate::memory::principal::MemoryWriteContext {
        self.source.proof.memory_scope()
    }
    fn model_binding(&self) -> &crate::agent::entry::AgentModelBinding {
        self.source.proof.model_binding()
    }
    fn config_generation_id(&self) -> u64 {
        self.source.generation.id.0
    }
    fn surface(&self) -> crate::causal_tree::state::TurnSurfaceKind {
        crate::causal_tree::state::TurnSurfaceKind::Worker
    }
    fn policy_channel(&self) -> &str {
        self.source.proof.policy_channel()
    }
    fn policy_sender(&self) -> &str {
        self.source.proof.policy_sender()
    }
    fn policy_chat_type(&self) -> &str {
        self.source.proof.policy_chat_type()
    }
}
