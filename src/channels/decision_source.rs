//! Channel-owned input provenance after transport admission. Model admission
//! remains separate and follows smart-group suppression.
use crate::config::ConfigGeneration;
use crate::memory::{Memory, MessageEvent};
use crate::runtime::envelope::RuntimeEnvelope;
use anyhow::Context;
use std::sync::Arc;

pub(super) struct ChannelSourceReceipt {
    envelope: RuntimeEnvelope,
    input: Arc<str>,
    generation: Arc<ConfigGeneration>,
    chat_type: String,
}

pub(super) struct ChannelAdmissionGuard {
    source: Option<Arc<ChannelSourceReceipt>>,
    fabric: crate::memory::MemoryFabric,
    ticket: Option<crate::runtime::turn_cleanup::CleanupTicket>,
    pending: Option<bool>,
}
impl ChannelAdmissionGuard {
    pub(super) fn finish_without_decision(mut self) {
        self.source = None;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(true);
        }
    }

    pub(super) fn take_terminal_ticket(&mut self) -> anyhow::Result<crate::runtime::turn_cleanup::CleanupTicket> {
        anyhow::ensure!(self.pending.is_none(), "channel admission failure is already pending");
        let ticket = self.ticket.take().context("channel admission cleanup ticket missing")?;
        self.source = None;
        Ok(ticket)
    }

    pub(super) async fn finish_failure(&mut self, cancelled: bool) -> anyhow::Result<()> {
        anyhow::ensure!(self.pending.is_none(), "channel admission failure is already pending");
        self.pending = Some(cancelled);
        let result = self
            .source
            .as_ref()
            .context("channel admission source missing")?
            .record_admission_failure(&self.fabric, cancelled)
            .await;
        self.source = None;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(result.is_ok());
        }
        result
    }
}
impl Drop for ChannelAdmissionGuard {
    fn drop(&mut self) {
        let Some(source) = self.source.take() else {
            return;
        };
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let cancelled = self.pending.unwrap_or(true);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            ticket.finish(false);
            return;
        };
        let fabric = self.fabric.clone();
        runtime.spawn(async move {
            ticket.finish(matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    source.record_admission_failure(&fabric, cancelled)
                )
                .await,
                Ok(Ok(()))
            ));
        });
    }
}

impl ChannelSourceReceipt {
    pub(super) async fn record_guarded(
        ctx: Arc<super::ChannelRuntimeContext>,
        generation: Arc<ConfigGeneration>,
        message: super::traits::ChannelMessage,
        run: String,
        ticket: crate::runtime::turn_cleanup::CleanupTicket,
    ) -> anyhow::Result<(Option<MessageEvent>, Arc<Self>, ChannelAdmissionGuard)> {
        let fabric = crate::memory::MemoryFabric::new(
            Arc::clone(&ctx.memory),
            generation.effective.workspace_dir.to_string_lossy(),
        )
        .with_event_recording(generation.effective.memory.event_recording_config())
        .with_ingress_recovery(&generation.effective, &ticket)
        .await?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let key = super::ConversationKey::from_message(&message);
            let envelope = RuntimeEnvelope::channel_with_session(
                fabric.workspace_id(),
                key.legacy.clone(),
                message.channel.clone(),
                message.sender.clone(),
                message.reply_target.clone(),
                super::channel_message_visibility(&message),
            )
            .with_run_id(&run)
            .with_config_generation(&generation);
            let input_key = format!("channel:{}:{}", message.channel, message.id);
            let mut ticket = Some(ticket);
            let mut admission = None;
            let operation = async {
                let event = fabric
                    .record_inbound_user_message(
                        envelope.message_scope(),
                        crate::agent::turn_contracts::inbound_user_message(&message.content).content,
                        Some(input_key.clone()),
                        None,
                    )
                    .await
                    .map_or_else(
                        |_| {
                            tracing::warn!("channel input persistence is unconfirmed");
                            None
                        },
                        Some,
                    );
                if let Some(event) = event.as_ref().filter(|event| event.id > 0) {
                    anyhow::ensure!(
                        event.idempotency_key.as_deref() == Some(input_key.as_str()),
                        "channel input idempotency identity changed"
                    );
                }
                let source =
                    Arc::new(Self::issue(ctx.memory.as_ref(), &generation, &message, &run, event.as_ref()).await?);
                admission = Some(ChannelAdmissionGuard {
                    source: Some(Arc::clone(&source)),
                    fabric: fabric.clone(),
                    ticket: Some(ticket.take().context("channel input cleanup ticket missing")?),
                    pending: None,
                });
                let timestamp = super::to_rfc3339_timestamp(message.timestamp);
                // Write the transcript only after its source has a recoverable
                // event identity. Do not append the message event a second time.
                super::append_sender_turn(
                    ctx.as_ref(),
                    &generation,
                    &key,
                    &message.channel,
                    &message.sender,
                    Some(&message.reply_target),
                    crate::agent::turn_contracts::inbound_user_message(&message.content),
                    super::channel_message_visibility(&message),
                    timestamp.as_deref(),
                    Some(&message.id),
                    &run,
                    false,
                )
                .await;
                Ok::<_, anyhow::Error>((event, source))
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), operation)
                .await
                .map_err(|_| anyhow::anyhow!("channel input persistence timed out"))
                .and_then(std::convert::identity);
            let result = match result {
                Ok((event, source)) => admission
                    .take()
                    .map(|guard| (event, source, guard))
                    .context("channel input owner did not issue a guard"),
                Err(_error) if fabric.ingress_already_accepted() => {
                    if let Some(ticket) = ticket.take() {
                        ticket.finish(true);
                    }
                    Err(crate::runtime::ingress_recovery::AlreadyAccepted.into())
                }
                Err(error) => {
                    if admission.is_none() {
                        let recovery = async {
                            let event = ctx
                                .memory
                                .find_owned_message_event_by_idempotency_key(
                                    fabric.workspace_id(),
                                    &envelope.resolved_owner_id(),
                                    &input_key,
                                )
                                .await?
                                .context("channel input commit is unconfirmed")?;
                            anyhow::ensure!(
                                event.idempotency_key.as_deref() == Some(input_key.as_str()),
                                "channel recovery input identity changed"
                            );
                            Self::issue(ctx.memory.as_ref(), &generation, &message, &run, Some(&event)).await
                        };
                        if let Ok(Ok(source)) = tokio::time::timeout(std::time::Duration::from_secs(15), recovery).await
                        {
                            admission = Some(ChannelAdmissionGuard {
                                source: Some(Arc::new(source)),
                                fabric,
                                ticket: ticket.take(),
                                pending: None,
                            });
                        }
                    }
                    if let Some(mut guard) = admission {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            guard.finish_failure(sender.is_closed()),
                        )
                        .await;
                    } else if let Some(ticket) = ticket {
                        ticket.finish(false);
                    }
                    Err(error)
                }
            };
            let _ = sender.send(result);
        });
        receiver.await.context("channel input owner ended without a receipt")?
    }

    async fn record_admission_failure(
        &self,
        fabric: &crate::memory::MemoryFabric,
        cancelled: bool,
    ) -> anyhow::Result<()> {
        if !self.generation.effective.memory.should_record_user_message_event() {
            return Ok(());
        }
        anyhow::ensure!(
            fabric.workspace_id() == self.envelope.workspace_id,
            "channel cleanup workspace changed"
        );
        let id = self
            .envelope
            .source_message_event_id
            .as_deref()
            .context("channel cleanup source missing")?;
        let run = self.envelope.run_id.as_deref().context("channel cleanup run missing")?;
        let event = fabric
            .memory()
            .find_owned_message_event(fabric.workspace_id(), &self.envelope.resolved_owner_id(), id)
            .await?
            .context("channel cleanup input is not durable")?;
        let mut input_scope = self.envelope.message_scope();
        // The inbound row precedes its own source-id attachment.
        input_scope.causation_event_id = None;
        anyhow::ensure!(
            event.id > 0
                && event.event_id == id
                && event.workspace_id == fabric.workspace_id()
                && event.owner_id == input_scope.owner_id
                && event.source == input_scope.source
                && event.channel == input_scope.channel
                && event.sender == input_scope.sender
                && event.recipient == input_scope.recipient
                && event.session_key == input_scope.session_key
                && event.run_id == input_scope.run_id
                && event.parent_run_id == input_scope.parent_run_id
                && event.parent_session_key == input_scope.parent_session_key
                && event.subject == input_scope.subject
                && event.agent_id == input_scope.agent_id
                && event.persona_id == input_scope.persona_id
                && event.attempt_id == input_scope.attempt_id
                && event.lease_epoch == input_scope.lease_epoch
                && event.goal_id == input_scope.goal_id
                && event.correlation_id == input_scope.correlation_id
                && event.causation_event_id == input_scope.causation_event_id
                && event.config_generation_id == input_scope.config_generation_id
                && event.config_source_revision == input_scope.config_source_revision
                && event.visibility == input_scope.visibility
                && event.role == "user"
                && event.event_type == "message.created"
                && event.content == crate::agent::turn_contracts::inbound_user_message(&self.input).content,
            "channel cleanup input differs from its source"
        );
        let summary = "channel input admission did not complete";
        let payload = serde_json::json!({ "run_id": run, "source_event_id": id,
            "status": if cancelled { "cancelled" } else { "failed" }, "summary": summary });
        let scope = self.envelope.message_scope();
        let result = fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "turn.admission.failed",
                summary,
                Some(serde_json::to_string(&payload)?),
                format!("turn:{run}:admission-failed"),
            )
            .await?;
        anyhow::ensure!(
            result.id > 0
                && !result.event_id.is_empty()
                && result.workspace_id == fabric.workspace_id()
                && result.owner_id == scope.owner_id
                && result.source == scope.source
                && result.channel == scope.channel
                && result.sender == scope.sender
                && result.recipient == scope.recipient
                && result.session_key == scope.session_key
                && result.run_id == scope.run_id
                && result.parent_run_id == scope.parent_run_id
                && result.parent_session_key == scope.parent_session_key
                && result.subject == scope.subject
                && result.goal_id == scope.goal_id
                && result.correlation_id == scope.correlation_id
                && result.agent_id == scope.agent_id
                && result.persona_id == scope.persona_id
                && result.attempt_id == scope.attempt_id
                && result.lease_epoch == scope.lease_epoch
                && result.config_generation_id == scope.config_generation_id
                && result.config_source_revision == scope.config_source_revision
                && result.causation_event_id == scope.causation_event_id
                && result.visibility == scope.visibility
                && result.idempotency_key.as_deref() == Some(format!("turn:{run}:admission-failed").as_str())
                && result.role == "event"
                && result.event_type == "turn.admission.failed"
                && result.content == format!("turn.admission.failed {summary}")
                && result
                    .raw_payload_json
                    .as_deref()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .transpose()?
                    .as_ref()
                    == Some(&payload),
            "channel admission failure retry conflicts with source or status"
        );
        let durable = fabric
            .memory()
            .find_owned_message_event(
                fabric.workspace_id(),
                scope.owner_id.as_deref().context("admission failure owner missing")?,
                &result.event_id,
            )
            .await?
            .context("admission failure receipt is not durable")?;
        anyhow::ensure!(
            serde_json::to_value(durable)? == serde_json::to_value(&result)?,
            "admission failure receipt differs from durable event"
        );
        Ok(())
    }

    pub(super) async fn issue(
        memory: &dyn Memory,
        generation: &Arc<ConfigGeneration>,
        message: &super::traits::ChannelMessage,
        run_id: &str,
        recorded: Option<&MessageEvent>,
    ) -> anyhow::Result<Self> {
        let mut envelope = Self::source_envelope(generation, message, run_id)?;
        if let Some(recorded) = recorded.filter(|event| event.id > 0 && !event.event_id.is_empty()) {
            let scope = envelope.message_scope();
            anyhow::ensure!(
                recorded.workspace_id == envelope.workspace_id
                    && recorded.owner_id.as_deref() == Some(envelope.resolved_owner_id().as_str())
                    && recorded.source == scope.source
                    && recorded.channel == scope.channel
                    && recorded.sender == scope.sender
                    && recorded.recipient == scope.recipient
                    && recorded.session_key.as_deref() == Some(envelope.session_key.as_str())
                    && recorded.run_id == envelope.run_id
                    && recorded.parent_run_id == envelope.parent_run_id
                    && recorded.parent_session_key.is_none()
                    && recorded.agent_id == envelope.agent_id
                    && recorded.persona_id == envelope.persona_id
                    && recorded.attempt_id == envelope.attempt_id
                    && recorded.lease_epoch == envelope.lease_epoch
                    && recorded.subject == scope.subject
                    && recorded.role == "user"
                    && recorded.event_type == "message.created"
                    && recorded.config_generation_id == envelope.config_generation_id
                    && recorded.config_source_revision == envelope.config_source_revision
                    && recorded.visibility == envelope.visibility
                    && recorded.content == crate::agent::turn_contracts::inbound_user_message(&message.content).content,
                "channel source event differs from admitted transport input"
            );
            let durable = memory
                .find_owned_message_event(
                    &envelope.workspace_id,
                    &envelope.resolved_owner_id(),
                    &recorded.event_id,
                )
                .await?
                .context("channel source event is not durable")?;
            anyhow::ensure!(
                serde_json::to_value(&durable)? == serde_json::to_value(recorded)?,
                "channel source receipt differs from durable input"
            );
            envelope = envelope.with_source_message_event_id(recorded.event_id.clone());
        } else {
            anyhow::ensure!(
                !generation.effective.causal_tree.enabled,
                "enabled channel decision source was not recorded"
            );
        }
        Ok(Self {
            envelope,
            input: Arc::from(message.content.as_str()),
            generation: Arc::clone(generation),
            chat_type: super::chat_kind_from_message(message).scope_chat_type().to_owned(),
        })
    }

    fn source_envelope(
        generation: &Arc<ConfigGeneration>,
        message: &super::traits::ChannelMessage,
        run_id: &str,
    ) -> anyhow::Result<RuntimeEnvelope> {
        anyhow::ensure!(
            !message.channel.is_empty() && !message.sender.is_empty() && !run_id.is_empty(),
            "channel source has incomplete transport identity"
        );
        let key = super::ConversationKey::from_message(message);
        let mut envelope = RuntimeEnvelope::channel_with_session(
            generation.effective.workspace_dir.to_string_lossy().to_string(),
            key.legacy,
            message.channel.clone(),
            message.sender.clone(),
            message.reply_target.clone(),
            super::channel_message_visibility(message),
        )
        .with_run_id(run_id)
        .with_config_generation(generation);
        if let Some(topic_id) = message
            .thread_ts
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            envelope = envelope.with_topic_id(topic_id);
        }
        Ok(envelope)
    }

    pub(super) const fn envelope(&self) -> &RuntimeEnvelope {
        &self.envelope
    }

    /// Resolve the immutable model identity before constructing its transport.
    pub(super) fn bind_model(&self, provider: &str, model: &str) -> anyhow::Result<ChannelTurnProof> {
        let choice =
            crate::agent::entry::resolve_agent_model_choice(&self.generation.effective, Some(provider), Some(model))?;
        Ok(ChannelTurnProof {
            envelope: self.envelope.clone(),
            owner: self.envelope.resolved_owner_id(),
            scope: self.envelope.memory_write_context(&self.chat_type),
            input: Arc::clone(&self.input),
            generation: Arc::clone(&self.generation),
            choice,
        })
    }
}

/// This proof authenticates input and selection; provider construction must use
/// the same selection before it can execute an admitted decision.
pub(crate) struct ChannelTurnProof {
    envelope: RuntimeEnvelope,
    owner: String,
    scope: crate::memory::principal::MemoryWriteContext,
    input: Arc<str>,
    generation: Arc<ConfigGeneration>,
    choice: crate::agent::entry::AgentModelChoice,
}

impl crate::runtime::turn_source::TurnSourceProof for ChannelTurnProof {
    fn owner_isolated(&self) -> bool {
        self.envelope.owner_isolated
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
        self.choice.provider_name()
    }
    fn model_name(&self) -> &str {
        self.choice.model_name()
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
        self.choice.binding()
    }
    fn config_generation_id(&self) -> u64 {
        self.generation.id.0
    }
    fn surface(&self) -> crate::causal_tree::state::TurnSurfaceKind {
        crate::causal_tree::state::TurnSurfaceKind::Channel
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
}

/// Assemble a recorded channel source for shared-loop fixtures without exposing
/// constructors for ChannelTurnProof or inventing a durable event identity.
#[cfg(test)]
pub(crate) async fn recorded_fixture_source(
    generation: &Arc<ConfigGeneration>,
    memory: Arc<dyn Memory>,
    message: &super::traits::ChannelMessage,
    run_id: &str,
    selection: (&str, &str),
) -> anyhow::Result<(ChannelTurnProof, RuntimeEnvelope)> {
    anyhow::ensure!(
        !generation.effective.causal_tree.enabled,
        "fixture requires disabled plane"
    );
    recorded_fixture_source_inner(generation, memory, message, run_id, selection).await
}

/// The same recorded channel source for tests of the default enabled builtin
/// path. Keeping a separate constructor preserves the disabled-fixture guard.
#[cfg(test)]
pub(crate) async fn recorded_enabled_fixture_source(
    generation: &Arc<ConfigGeneration>,
    memory: Arc<dyn Memory>,
    message: &super::traits::ChannelMessage,
    run_id: &str,
    selection: (&str, &str),
) -> anyhow::Result<(ChannelTurnProof, RuntimeEnvelope)> {
    anyhow::ensure!(
        generation.effective.causal_tree.enabled,
        "fixture requires enabled plane"
    );
    recorded_fixture_source_inner(generation, memory, message, run_id, selection).await
}

#[cfg(test)]
async fn recorded_fixture_source_inner(
    generation: &Arc<ConfigGeneration>,
    memory: Arc<dyn Memory>,
    message: &super::traits::ChannelMessage,
    run_id: &str,
    selection: (&str, &str),
) -> anyhow::Result<(ChannelTurnProof, RuntimeEnvelope)> {
    let envelope = ChannelSourceReceipt::source_envelope(generation, message, run_id)?;
    let fabric = crate::memory::MemoryFabric::new(
        Arc::clone(&memory),
        generation.effective.workspace_dir.to_string_lossy(),
    )
    .with_event_recording(generation.effective.memory.event_recording_config());
    let event = fabric
        .record_inbound_user_message(envelope.message_scope(), &message.content, None, None)
        .await?;
    let source = ChannelSourceReceipt::issue(memory.as_ref(), generation, message, run_id, Some(&event)).await?;
    Ok((source.bind_model(selection.0, selection.1)?, source.envelope().clone()))
}
