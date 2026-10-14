//! Gateway-owned durable input lineage. This receipt does not authenticate a
//! caller or grant execution authority; transport admission precedes it.
use crate::config::ConfigGeneration;
use crate::memory::{Memory, MemoryVisibility, MessageEvent, MessageEventSource, SessionContextQuery};
use crate::runtime::envelope::RuntimeEnvelope;
use anyhow::Context;

mod admission;
pub(super) use admission::{GatewayAdmissionGuard, GatewayPendingInput};

#[derive(Clone)]
pub(super) struct GatewaySourceReceipt {
    input: std::sync::Arc<str>,
    envelope: RuntimeEnvelope,
    has_conversation_history: bool,
}

impl GatewaySourceReceipt {
    /// Bind already admitted transport input without claiming durability when
    /// recording is unavailable in disabled mode. Enabled mode remains strict.
    pub(super) async fn issue_for_turn(
        memory: &dyn Memory,
        generation: &ConfigGeneration,
        envelope: &RuntimeEnvelope,
        raw_input: &str,
        recorded: Option<&MessageEvent>,
    ) -> anyhow::Result<Self> {
        if let Some(recorded) = recorded.filter(|event| event.id > 0 && !event.event_id.is_empty()) {
            return Self::issue(memory, generation, envelope, raw_input, recorded).await;
        }
        anyhow::ensure!(
            !generation.effective.causal_tree.enabled,
            "enabled gateway decision source was not recorded"
        );
        anyhow::ensure!(
            envelope.source == crate::runtime::envelope::RuntimeSource::Gateway
                && envelope.workspace_id == generation.effective.workspace_dir.to_string_lossy()
                && envelope.config_generation_id == Some(generation.id.0)
                && envelope.config_source_revision.as_deref()
                    == Some(generation.source_revision.fingerprint_sha256.as_str())
                && envelope.run_id.as_deref().is_some_and(|id| !id.is_empty())
                && !envelope.session_key.is_empty()
                && envelope.visibility == MemoryVisibility::Session,
            "gateway input differs from its admitted generation"
        );
        let mut envelope = envelope.clone();
        envelope.source_message_event_id = None;
        let has_conversation_history = memory
            .load_recent_session_context(SessionContextQuery {
                principal: envelope.memory_principal(),
                since_event_id: None,
                limit: 24,
                include_roles: vec!["user".to_string(), "assistant".to_string()],
            })
            .await?
            .into_iter()
            .next()
            .is_some();
        Ok(Self {
            input: std::sync::Arc::from(raw_input),
            envelope,
            has_conversation_history,
        })
    }

    pub(super) async fn issue(
        memory: &dyn Memory,
        generation: &ConfigGeneration,
        envelope: &RuntimeEnvelope,
        raw_input: &str,
        recorded: &MessageEvent,
    ) -> anyhow::Result<Self> {
        Self::issue_for_source(
            memory,
            generation,
            envelope,
            raw_input,
            recorded,
            MessageEventSource::Gateway,
        )
        .await
    }

    async fn issue_for_source(
        memory: &dyn Memory,
        generation: &ConfigGeneration,
        envelope: &RuntimeEnvelope,
        raw_input: &str,
        recorded: &MessageEvent,
        source: MessageEventSource,
    ) -> anyhow::Result<Self> {
        let scope = envelope.message_scope();
        anyhow::ensure!(
            recorded.id > 0
                && !recorded.event_id.is_empty()
                && recorded.workspace_id == generation.effective.workspace_dir.to_string_lossy()
                && recorded.workspace_id == envelope.workspace_id
                && recorded.owner_id.as_deref() == Some(envelope.resolved_owner_id().as_str())
                && recorded.source == source
                && scope.source == source
                && recorded.channel == scope.channel
                && recorded.sender == scope.sender
                && recorded.recipient == scope.recipient
                && recorded.session_key.as_deref() == Some(envelope.session_key.as_str())
                && recorded.run_id == envelope.run_id
                && recorded.run_id.as_deref().is_some_and(|id| !id.is_empty())
                && recorded.parent_run_id == envelope.parent_run_id
                && recorded.parent_session_key.is_none()
                && recorded.agent_id == envelope.agent_id
                && recorded.persona_id == envelope.persona_id
                && recorded.attempt_id == envelope.attempt_id
                && recorded.lease_epoch == envelope.lease_epoch
                && recorded.subject == scope.subject
                && recorded.role == "user"
                && recorded.event_type == "message.created"
                && recorded.config_generation_id == Some(generation.id.0)
                && recorded.config_source_revision.as_deref()
                    == Some(generation.source_revision.fingerprint_sha256.as_str())
                && envelope.config_generation_id == recorded.config_generation_id
                && envelope.config_source_revision == recorded.config_source_revision
                && recorded.visibility == MemoryVisibility::Session
                && recorded.visibility == envelope.visibility
                && recorded.content == raw_input,
            "gateway source event differs from its admitted ingress"
        );
        let durable = memory
            .find_owned_message_event(
                &envelope.workspace_id,
                &envelope.resolved_owner_id(),
                &recorded.event_id,
            )
            .await?
            .context("gateway source event is not durable")?;
        anyhow::ensure!(
            serde_json::to_value(&durable)? == serde_json::to_value(recorded)?,
            "gateway source receipt differs from durable input"
        );
        let has_conversation_history = memory
            .load_recent_session_context(SessionContextQuery {
                principal: envelope.memory_principal(),
                since_event_id: None,
                limit: 24,
                include_roles: vec!["user".to_string(), "assistant".to_string()],
            })
            .await?
            .into_iter()
            .any(|event| event.id != recorded.id);
        Ok(Self {
            input: std::sync::Arc::from(raw_input),
            envelope: envelope.clone().with_source_message_event_id(recorded.event_id.clone()),
            has_conversation_history,
        })
    }

    /// Consumes the ingress receipt when admission fails before model execution.
    /// It cannot produce a model terminal or settle provider usage.
    pub(super) async fn record_admission_failure(
        self,
        fabric: &crate::memory::MemoryFabric,
        generation: &ConfigGeneration,
        cancelled: bool,
        summary: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            fabric.workspace_id() == self.envelope.workspace_id
                && self.envelope.config_generation_id == Some(generation.id.0)
                && self.envelope.config_source_revision.as_deref()
                    == Some(generation.source_revision.fingerprint_sha256.as_str()),
            "gateway admission failure differs from its source generation"
        );
        if self.envelope.source_message_event_id.is_none()
            && !generation.effective.memory.should_record_user_message_event()
        {
            return Ok(());
        }
        let event_id = self
            .envelope
            .source_message_event_id
            .as_deref()
            .context("gateway admission failure has no durable source")?;
        let memory = fabric.memory();
        let input = memory
            .find_owned_message_event(fabric.workspace_id(), &self.envelope.resolved_owner_id(), event_id)
            .await?
            .context("gateway admission input is missing")?;
        let scope = self.envelope.message_scope();
        Self::issue_for_source(
            memory.as_ref(),
            generation,
            &self.envelope,
            &self.input,
            &input,
            scope.source.clone(),
        )
        .await?;
        let run_id = self
            .envelope
            .run_id
            .as_deref()
            .context("gateway admission run is missing")?;
        let summary = crate::security::redaction::redact_secrets(summary);
        let payload = serde_json::json!({
            "run_id": run_id,
            "source_event_id": event_id,
            "status": if cancelled { "cancelled" } else { "failed" },
            "summary": summary,
        });
        let expected_content = format!("turn.admission.failed {summary}");
        let key = format!("turn:{run_id}:admission-failed");
        let event = fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "turn.admission.failed",
                summary,
                Some(serde_json::to_string(&payload)?),
                key.clone(),
            )
            .await?;
        anyhow::ensure!(
            event.id > 0
                && !event.event_id.is_empty()
                && event.workspace_id == fabric.workspace_id()
                && event.owner_id == scope.owner_id
                && event.source == scope.source
                && event.channel == scope.channel
                && event.sender == scope.sender
                && event.recipient == scope.recipient
                && event.session_key == scope.session_key
                && event.run_id == scope.run_id
                && event.parent_run_id == scope.parent_run_id
                && event.parent_session_key == scope.parent_session_key
                && event.agent_id == scope.agent_id
                && event.persona_id == scope.persona_id
                && event.subject == scope.subject
                && event.attempt_id == scope.attempt_id
                && event.lease_epoch == scope.lease_epoch
                && event.visibility == scope.visibility
                && event.config_generation_id == scope.config_generation_id
                && event.config_source_revision == scope.config_source_revision
                && event.causation_event_id == scope.causation_event_id
                && event.role == "event"
                && event.event_type == "turn.admission.failed"
                && event.content == expected_content
                && event.idempotency_key.as_deref() == Some(key.as_str())
                && event.goal_id == scope.goal_id
                && event.correlation_id == scope.correlation_id
                && event
                    .raw_payload_json
                    .as_deref()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .transpose()?
                    .as_ref()
                    == Some(&payload),
            "gateway admission failure retry conflicts with its source or status"
        );
        let durable = memory
            .find_owned_message_event(
                fabric.workspace_id(),
                &self.envelope.resolved_owner_id(),
                &event.event_id,
            )
            .await?
            .context("gateway admission failure receipt is not durable")?;
        anyhow::ensure!(
            serde_json::to_value(&durable)? == serde_json::to_value(&event)?,
            "gateway admission failure receipt differs from durable event"
        );
        Ok(())
    }

    pub(super) const fn envelope(&self) -> &RuntimeEnvelope {
        &self.envelope
    }
}

/// Retains the exact selection passed to the provider factory. No Debug or
/// serialization implementation: the resolved selection owns credential data.
pub(super) struct GatewayProviderBinding {
    provider: std::sync::Arc<dyn crate::providers::Provider>,
    resolved: crate::providers::catalog::ResolvedModel,
}

impl GatewayProviderBinding {
    pub(super) fn new(
        provider: std::sync::Arc<dyn crate::providers::Provider>,
        resolved: crate::providers::catalog::ResolvedModel,
    ) -> Self {
        Self { provider, resolved }
    }

    pub(super) fn provider_name(&self) -> &str {
        self.resolved.provider_label()
    }

    pub(super) fn verify(&self, runtime: &super::TurnRuntimeGeneration) -> anyhow::Result<()> {
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&self.provider, &runtime.provider) && runtime.model == self.resolved.model(),
            "gateway provider instance or selected model changed after construction"
        );
        Ok(())
    }
}

/// Constructed only after gateway transport admission and durable input
/// validation. Neither arbitrary metadata nor a caller-supplied owner can
/// construct this proof.
pub(crate) struct GatewayTurnProof {
    envelope: RuntimeEnvelope,
    provider_binding: std::sync::Arc<GatewayProviderBinding>,
    owner: String,
    scope: crate::memory::principal::MemoryWriteContext,
    input: std::sync::Arc<str>,
    generation: std::sync::Arc<ConfigGeneration>,
    provider_name: String,
    model_name: String,
    model_binding: crate::agent::entry::AgentModelBinding,
    has_conversation_history: bool,
}

impl GatewayTurnProof {
    pub(super) fn verify_runtime_input(
        &self,
        runtime: &super::TurnRuntimeGeneration,
        input: &str,
    ) -> anyhow::Result<()> {
        let binding = runtime
            .provider_binding
            .as_ref()
            .context("gateway provider binding missing")?;
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&self.generation, &runtime.config_generation)
                && std::sync::Arc::ptr_eq(&self.provider_binding, binding)
                && self.input.as_ref() == input,
            "gateway runtime or input differs from admitted source"
        );
        binding.verify(runtime)
    }

    pub(super) const fn envelope(&self) -> &RuntimeEnvelope {
        &self.envelope
    }
}

impl GatewaySourceReceipt {
    pub(super) fn bind(
        &self,
        runtime: &super::TurnRuntimeGeneration,
        ingress: &super::GatewayFabricContext,
    ) -> anyhow::Result<GatewayTurnProof> {
        let binding = runtime
            .provider_binding
            .as_ref()
            .context("gateway provider binding missing")?;
        binding.verify(runtime)?;
        anyhow::ensure!(
            self.envelope.channel.as_deref() == Some(ingress.channel.as_str())
                && self.envelope.sender.as_deref() == Some(ingress.sender.as_str())
                && self.envelope.recipient.as_deref() == Some(ingress.recipient.as_str()),
            "gateway transport context differs from durable source"
        );
        let owner = self.envelope.resolved_owner_id();
        if let Some(expected) = ingress.webhook_owner_id(&self.envelope.workspace_id) {
            anyhow::ensure!(
                owner == expected && self.envelope.session_key == ingress.session_key,
                "gateway webhook credential scope changed"
            );
        }
        self.bind_model(
            runtime,
            if ingress.channel == "webhook" {
                "webhook"
            } else {
                "private"
            },
        )
    }

    /// Console admission uses the principal installed by API auth middleware.
    /// A durable owner string alone cannot enter this constructor.
    pub(super) async fn console_proof(
        memory: &dyn Memory,
        runtime: &super::TurnRuntimeGeneration,
        principal: &super::api::ApiPrincipal,
        envelope: &RuntimeEnvelope,
        raw_input: &str,
        event_id: &str,
    ) -> anyhow::Result<GatewayTurnProof> {
        anyhow::ensure!(
            envelope.source == crate::runtime::envelope::RuntimeSource::Console
                && envelope.owner_isolated
                && envelope.owner_id.as_deref() == Some(principal.owner_id.as_str())
                && envelope.workspace_id == runtime.config_generation.effective.workspace_dir.to_string_lossy()
                && envelope.run_id.as_deref().is_some_and(|id| !id.is_empty())
                && !envelope.session_key.is_empty(),
            "console input does not belong to its authenticated API principal"
        );
        let recorded = if event_id.is_empty() {
            None
        } else {
            memory
                .find_owned_message_event(&envelope.workspace_id, &principal.owner_id, event_id)
                .await?
        };
        let Some(recorded) = recorded else {
            anyhow::ensure!(
                !runtime.config_generation.effective.causal_tree.enabled,
                "console source event is not durable"
            );
            let mut local_envelope = envelope.clone();
            local_envelope.source_message_event_id = None;
            let has_conversation_history = memory
                .load_recent_session_context(SessionContextQuery {
                    principal: local_envelope.memory_principal(),
                    since_event_id: None,
                    limit: 24,
                    include_roles: vec!["user".to_string(), "assistant".to_string()],
                })
                .await?
                .into_iter()
                .next()
                .is_some();
            return Self {
                input: std::sync::Arc::from(raw_input),
                envelope: local_envelope,
                has_conversation_history,
            }
            .bind_model(runtime, "private");
        };
        let receipt = Self::issue_for_source(
            memory,
            &runtime.config_generation,
            envelope,
            raw_input,
            &recorded,
            MessageEventSource::Console,
        )
        .await?;
        receipt.bind_model(runtime, "private")
    }

    fn bind_model(&self, runtime: &super::TurnRuntimeGeneration, chat_type: &str) -> anyhow::Result<GatewayTurnProof> {
        let binding = runtime
            .provider_binding
            .as_ref()
            .context("gateway provider binding missing")?;
        binding.verify(runtime)?;
        let owner = self.envelope.resolved_owner_id();
        anyhow::ensure!(
            self.envelope.config_generation_id == Some(runtime.config_generation.id.0)
                && self.envelope.config_source_revision.as_deref()
                    == Some(runtime.config_generation.source_revision.fingerprint_sha256.as_str()),
            "gateway source and provider generations differ"
        );
        let config = &runtime.config_generation.effective;
        let provider_name = crate::providers::selection::default_provider(config).to_owned();
        let model_name = crate::providers::selection::default_model(config).to_owned();
        let catalog = crate::providers::selection::catalog_for(config);
        let route = crate::providers::selection::route_target(&catalog, &provider_name, &model_name)?.cloned();
        Ok(GatewayTurnProof {
            provider_binding: std::sync::Arc::clone(binding),
            owner,
            scope: self.envelope.memory_write_context(chat_type),
            envelope: self.envelope.clone(),
            input: std::sync::Arc::clone(&self.input),
            generation: std::sync::Arc::clone(&runtime.config_generation),
            provider_name,
            model_name,
            model_binding: crate::agent::entry::AgentModelBinding::from_resolved(&binding.resolved, route),
            has_conversation_history: self.has_conversation_history,
        })
    }
}

impl crate::runtime::turn_source::TurnSourceProof for GatewayTurnProof {
    fn owner_isolated(&self) -> bool {
        self.envelope.owner_isolated
    }
    fn approval_review_adapter(&self) -> Option<crate::causal_tree::approval_collection::ApprovalReviewAdapter> {
        (self.envelope.source == crate::runtime::envelope::RuntimeSource::Console
            && self.envelope.owner_isolated
            && self.envelope.source_message_event_id.is_some())
        .then_some(crate::causal_tree::approval_collection::ApprovalReviewAdapter::ConsoleOwnerReview)
    }
    fn source_memory_principal(&self) -> crate::memory::MemoryPrincipal {
        self.envelope.memory_principal()
    }

    fn workspace_id(&self) -> &str {
        &self.envelope.workspace_id
    }
    fn owner_id(&self) -> &str {
        &self.owner
    }
    fn session_key(&self) -> &str {
        &self.envelope.session_key
    }
    fn run_id(&self) -> &str {
        self.envelope.run_id.as_deref().unwrap_or_default()
    }
    fn task_id(&self) -> Option<&str> {
        self.envelope.task_id.as_deref()
    }
    fn routing_input(&self) -> &str {
        &self.input
    }
    fn provider_name(&self) -> &str {
        &self.provider_name
    }
    fn model_name(&self) -> &str {
        &self.model_name
    }
    fn config_source_revision(&self) -> &str {
        &self.generation.source_revision.fingerprint_sha256
    }
    fn source_event_id(&self) -> Option<&str> {
        self.envelope.source_message_event_id.as_deref()
    }
    fn memory_scope(&self) -> &crate::memory::principal::MemoryWriteContext {
        &self.scope
    }
    fn model_binding(&self) -> &crate::agent::entry::AgentModelBinding {
        &self.model_binding
    }
    fn config_generation_id(&self) -> u64 {
        self.generation.id.0
    }
    fn surface(&self) -> crate::causal_tree::state::TurnSurfaceKind {
        crate::causal_tree::state::TurnSurfaceKind::Server
    }
    fn policy_channel(&self) -> &str {
        self.envelope.channel.as_deref().unwrap_or_default()
    }
    fn policy_sender(&self) -> &str {
        self.envelope.sender.as_deref().unwrap_or_default()
    }
    fn policy_chat_type(&self) -> &str {
        self.scope.chat_type.as_deref().unwrap_or("private")
    }
    fn has_conversation_history(&self) -> bool {
        self.has_conversation_history
    }
}

#[cfg(test)]
mod admission_failure_tests {
    use super::*;
    use crate::memory::{MemoryFabric, SqliteMemory};
    use std::sync::Arc;

    #[tokio::test]
    async fn admission_failure_preserves_source_retries_and_redaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = dir.path().to_path_buf();
        config.config_path = dir.path().join("config.toml");
        let generation = crate::config::new_shared(config).pin();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), dir.path().to_string_lossy());
        let envelope = RuntimeEnvelope::gateway(
            fabric.workspace_id(),
            "prx_session",
            "webhook",
            "prx_user",
            "prx_service",
            MemoryVisibility::Session,
        )
        .with_run_id("prx_run")
        .with_config_generation(&generation);
        let input = fabric
            .record_inbound_user_message(envelope.message_scope(), "prx input", None, None)
            .await
            .unwrap();
        let secret = crate::agent::terminal::persistence_probe::PROBE_SECRET;
        let summary = format!("admission failed {secret}");
        let foreign_dir = tempfile::tempdir().unwrap();
        let foreign_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(foreign_dir.path()).unwrap());
        let foreign = MemoryFabric::new(foreign_memory.clone(), fabric.workspace_id());
        let receipt = GatewaySourceReceipt::issue(memory.as_ref(), &generation, &envelope, "prx input", &input)
            .await
            .unwrap();
        assert!(
            receipt
                .record_admission_failure(&foreign, &generation, false, &summary)
                .await
                .is_err()
        );
        assert!(
            foreign_memory
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed",)
                .await
                .unwrap()
                .is_none()
        );
        for cancelled in [false, false, true] {
            let receipt = GatewaySourceReceipt::issue(memory.as_ref(), &generation, &envelope, "prx input", &input)
                .await
                .unwrap();
            let result = receipt
                .record_admission_failure(&fabric, &generation, cancelled, &summary)
                .await;
            assert_eq!(result.is_ok(), !cancelled, "a conflicting retry must fail");
        }
        let event = memory
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.causation_event_id.as_deref(), Some(input.event_id.as_str()));
        assert!(!event.raw_payload_json.as_deref().unwrap().contains(secret));
        assert!(
            memory
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:final",)
                .await
                .unwrap()
                .is_none()
        );
        crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(dir.path());
    }
}
