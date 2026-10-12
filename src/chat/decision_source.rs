//! Chat-owned provenance for an actual recorded inbound turn. This receipt
//! establishes source lineage only; it does not authorize a model or tool.

use crate::config::ConfigGeneration;
use crate::memory::{Memory, MessageEvent, MessageEventSource};
use crate::runtime::envelope::RuntimeEnvelope;
use anyhow::Context;
use std::sync::Arc;

/// Selection identity paired with the concrete provider instance. Only Chat's
/// construction/switch path publishes it, before updating either UI slot.
#[derive(Clone)]
pub(super) struct ChatProviderBinding {
    provider: Arc<dyn crate::providers::Provider>,
    generation: Arc<ConfigGeneration>,
    choice: crate::agent::entry::AgentModelChoice,
}

impl ChatProviderBinding {
    pub(super) async fn record_guarded(
        &self,
        fabric: crate::memory::MemoryFabric,
        source: (crate::memory::MessageEventScope, &str, ChatAdmissionMode),
        input: &str,
        key: String,
        ticket: crate::runtime::turn_cleanup::CleanupTicket,
    ) -> anyhow::Result<(Option<MessageEvent>, Arc<ChatTurnBinding>, ChatAdmissionGuard)> {
        let fabric = fabric
            .with_ingress_recovery(&self.generation.effective, &ticket)
            .await?;
        let (scope, session, admission_mode) = source;
        let run = scope.run_id.clone().context("chat input run is missing")?;
        let session = session.to_owned();
        let binding = self.clone();
        let input = input.to_owned();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let issue = |event: Option<MessageEvent>| {
                let fabric = &fabric;
                let binding = &binding;
                let input = &input;
                let run = &run;
                let session = &session;
                let key = &key;
                let scope = &scope;
                async move {
                    if let Some(event) = event.as_ref().filter(|event| event.id > 0) {
                        let mut recipient = scope.recipient.clone();
                        if let Some(label) = &mut recipient
                            && crate::providers::byo_dialect_of(label).is_some()
                        {
                            *label = crate::providers::redact_endpoint_userinfo(label).into_owned();
                        }
                        anyhow::ensure!(
                            event.idempotency_key.as_deref() == Some(key.as_str())
                                && event.recipient == recipient
                                && event.subject == scope.subject
                                && event.goal_id == scope.goal_id
                                && event.correlation_id == scope.correlation_id
                                && event.causation_event_id == scope.causation_event_id,
                            "chat input idempotency identity or attribution changed"
                        );
                    }
                    let receipt = ChatSourceReceipt::issue_for_turn(
                        fabric.memory().as_ref(),
                        &binding.generation,
                        session,
                        run,
                        input,
                        event.as_ref(),
                    )
                    .await?;
                    Ok::<_, anyhow::Error>((event, receipt.bind_provider(binding, admission_mode)?))
                }
            };
            let operation = async {
                let event = fabric
                    .record_inbound_user_message(
                        scope.clone(),
                        super::sanitize::sanitize_for_persistence(&input),
                        Some(key.clone()),
                        None,
                    )
                    .await
                    .map_or_else(
                        |_| {
                            tracing::warn!("chat input persistence is unconfirmed");
                            None
                        },
                        Some,
                    );
                issue(event).await
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), operation)
                .await
                .map_err(|_| anyhow::anyhow!("chat input persistence timed out"))
                .and_then(std::convert::identity);
            let result = match result {
                Ok((event, source)) => {
                    let guard = ChatAdmissionGuard {
                        binding: Some(Arc::clone(&source)),
                        fabric,
                        ticket: Some(ticket),
                        pending: None,
                    };
                    Ok((event, source, guard))
                }
                Err(_error) if fabric.ingress_already_accepted() => {
                    ticket.finish(true);
                    Err(crate::runtime::ingress_recovery::AlreadyAccepted.into())
                }
                Err(error) => {
                    let recovery = async {
                        let event = fabric
                            .memory()
                            .find_owned_message_event_by_idempotency_key(
                                fabric.workspace_id(),
                                &scope.owner_id.clone().context("chat input owner missing")?,
                                &key,
                            )
                            .await?
                            .context("chat input commit is unconfirmed")?;
                        issue(Some(event)).await
                    };
                    if let Ok(Ok((_, source))) =
                        tokio::time::timeout(std::time::Duration::from_secs(15), recovery).await
                    {
                        let mut guard = ChatAdmissionGuard {
                            binding: Some(source),
                            fabric,
                            ticket: Some(ticket),
                            pending: None,
                        };
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            guard.finish_failure(sender.is_closed(), "chat input admission failed"),
                        )
                        .await;
                    } else {
                        ticket.finish(false);
                    }
                    Err(error)
                }
            };
            let _ = sender.send(result);
        });
        receiver.await.context("chat input owner ended without a receipt")?
    }

    pub(super) fn capture(
        generation: &Arc<ConfigGeneration>,
        provider: &Arc<dyn crate::providers::Provider>,
        provider_name: &str,
        model_name: &str,
    ) -> anyhow::Result<Option<Self>> {
        let choice = crate::agent::entry::resolve_agent_model_choice(
            &generation.effective,
            Some(provider_name),
            Some(model_name),
        )?;
        anyhow::ensure!(
            choice.factory_credential_source_matches(&generation.effective),
            "chat provider credential origin cannot be bound to its selected model"
        );
        Ok(Some(Self {
            provider: Arc::clone(provider),
            generation: Arc::clone(generation),
            choice,
        }))
    }

    pub(super) fn verify(
        &self,
        generation: &Arc<ConfigGeneration>,
        provider: &Arc<dyn crate::providers::Provider>,
        provider_name: &str,
        model_name: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.provider, provider)
                && Arc::ptr_eq(&self.generation, generation)
                && self.choice.provider_name() == provider_name
                && self.choice.model_name() == model_name,
            "chat provider instance or selected model changed after binding"
        );
        Ok(())
    }
}

pub(super) struct ChatAdmissionGuard {
    binding: Option<Arc<ChatTurnBinding>>,
    fabric: crate::memory::MemoryFabric,
    ticket: Option<crate::runtime::turn_cleanup::CleanupTicket>,
    pending: Option<(crate::agent::terminal::TurnTerminalStatus, String)>,
}
impl ChatAdmissionGuard {
    pub(super) fn handoff(mut self) {
        self.binding = None;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(true);
        }
    }

    pub(super) fn handoff_terminal(mut self) -> anyhow::Result<()> {
        let binding = self.binding.as_ref().context("chat admission source missing")?;
        anyhow::ensure!(self.pending.is_none(), "chat admission failure is already pending");
        let ticket = self.ticket.take().context("chat admission cleanup ticket missing")?;
        binding.install_terminal(ticket, self.fabric.clone())?;
        self.binding = None;
        Ok(())
    }

    pub(super) async fn finish_failure(&mut self, cancelled: bool, summary: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.pending.is_none(), "chat admission failure is already pending");
        let status = if cancelled {
            crate::agent::terminal::TurnTerminalStatus::Cancelled
        } else {
            crate::agent::terminal::TurnTerminalStatus::Failed
        };
        self.pending = Some((status, super::sanitize::sanitize_for_persistence(summary)));
        let (status, summary) = self.pending.as_ref().context("chat admission failure missing")?;
        let binding = self.binding.as_ref().context("chat admission source missing")?;
        let run = binding
            .envelope
            .run_id
            .as_deref()
            .context("chat admission run missing")?;
        let result = binding
            .record_predecision_failure(&self.fabric, run, *status, summary)
            .await;
        self.binding = None;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(result.is_ok());
        }
        result
    }
}
impl Drop for ChatAdmissionGuard {
    fn drop(&mut self) {
        let Some(binding) = self.binding.take() else {
            return;
        };
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let (status, summary) = self.pending.take().unwrap_or_else(|| {
            (
                crate::agent::terminal::TurnTerminalStatus::Cancelled,
                "chat input admission task was dropped".to_owned(),
            )
        });
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            ticket.finish(false);
            return;
        };
        let fabric = self.fabric.clone();
        runtime.spawn(async move {
            let result = async {
                let run = binding
                    .envelope
                    .run_id
                    .as_deref()
                    .context("chat admission run missing")?;
                binding.record_predecision_failure(&fabric, run, status, &summary).await
            };
            ticket.finish(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(15), result).await,
                Ok(Ok(()))
            ));
        });
    }
}

/// Local command provenance only; this is not a model decision or child grant.
/// Private fields keep mailbox content and decoded tool arguments from minting
/// an operator command or replacing the model after verification.
pub(super) struct ChatOperatorSpawn {
    command: String,
    binding: ChatProviderBinding,
    task: String,
    provider: String,
    model: String,
}

impl ChatOperatorSpawn {
    pub(super) fn capture(
        message: &crate::channels::traits::ChannelMessage,
        task: &str,
        binding: &ChatProviderBinding,
        generation: &Arc<ConfigGeneration>,
        provider: &Arc<dyn crate::providers::Provider>,
        provider_name: &str,
        model_name: &str,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            super::is_local_operator_input(message) && !message.sender_is_bot,
            "background command requires local operator input"
        );
        let parsed = super::sessions::command::parse_session_command(&message.content);
        anyhow::ensure!(
            matches!(parsed, Some(super::sessions::SessionCommand::Bg { task: ref parsed }) if parsed == task),
            "background command task differs from original input"
        );
        binding.verify(generation, provider, provider_name, model_name)?;
        Ok(Self {
            command: message.content.clone(),
            binding: ChatProviderBinding {
                choice: binding.choice.clone(),
                generation: Arc::clone(&binding.generation),
                provider: Arc::clone(&binding.provider),
            },
            task: task.to_owned(),
            provider: binding.choice.provider_name().to_owned(),
            model: binding.choice.model_name().to_owned(),
        })
    }

    pub(super) async fn admit(
        self,
        memory: Arc<dyn Memory>,
        session: &str,
        security: Arc<crate::security::SecurityPolicy>,
        publication: Arc<crate::runtime::turn_decision::TurnDecisionService>,
    ) -> anyhow::Result<(serde_json::Value, crate::runtime::operator_spawn::OperatorSpawnSource)> {
        let generation = &self.binding.generation;
        let run = uuid::Uuid::new_v4().to_string();
        let workspace = generation.effective.workspace_dir.to_string_lossy();
        let envelope = super::chat_runtime_envelope(&workspace, session)
            .with_config_generation(generation)
            .with_run_id(run.clone());
        let recording = generation.effective.memory.event_recording_config();
        let fabric =
            crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.as_ref()).with_event_recording(recording);
        let (binding, admission_guard) = if recording.enabled && recording.record_user_messages {
            let owner =
                crate::runtime::turn_cleanup::current_tracker().context("chat operator input cleanup owner missing")?;
            let (_, binding, guard) = self
                .binding
                .record_guarded(
                    fabric.clone(),
                    (envelope.message_scope(), session, ChatAdmissionMode::OperatorCommand),
                    &self.command,
                    format!("operator:{run}:input"),
                    owner.register(tokio_util::sync::CancellationToken::new())?,
                )
                .await?;
            (binding, Some(guard))
        } else {
            // Local operator authority remains valid when input recording is
            // disabled; this branch neither writes nor invents an input event.
            let receipt = ChatSourceReceipt {
                envelope,
                routing_input: Arc::from(self.command.as_str()),
            };
            (
                receipt.bind_provider(&self.binding, ChatAdmissionMode::OperatorCommand)?,
                None,
            )
        };
        let source = crate::runtime::operator_spawn::OperatorSpawnSource::new(
            binding.as_ref(),
            Arc::clone(generation),
            fabric,
            Arc::clone(&self.binding.provider),
            self.task.clone(),
            security,
            publication,
        )?;
        if let Some(guard) = admission_guard {
            guard.handoff();
        }
        Ok((
            serde_json::json!({ "task": self.task, "provider": self.provider, "model": self.model }),
            source,
        ))
    }
}

pub(super) struct ChatSourceReceipt {
    envelope: RuntimeEnvelope,
    routing_input: std::sync::Arc<str>,
}

/// Distinguish model mode from the existing explicit operator-command path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChatAdmissionMode {
    Model(crate::agent::turn_contracts::ChatMode),
    // Explicit /bg retains its operator-command contract, not a model-mode proof.
    OperatorCommand,
}

/// Immutable input to a queued Chat task; no UI action can synthesize authority.
pub struct ChatTurnBinding {
    admission_mode: ChatAdmissionMode,
    terminal: std::sync::OnceLock<(
        crate::runtime::turn_terminal::TurnTerminalGuard,
        Arc<crate::agent::turn_contracts::RetainedProviderTrace>,
    )>,
    returned_trace: std::sync::OnceLock<CapturedChatTrace>,
    decision: tokio::sync::OnceCell<ChatDecision>,
    // Serializes decision issuance with pre-decision closure. Once closure
    // starts, even a failed write must not reopen this input for execution.
    admission_closed: tokio::sync::Mutex<bool>,
    choice: crate::agent::entry::AgentModelChoice,
    owner_id: String,
    memory_scope: crate::memory::principal::MemoryWriteContext,
    generation: Arc<ConfigGeneration>,
    provider: Arc<dyn crate::providers::Provider>,
    model: String,
    envelope: RuntimeEnvelope,
    routing_input: Arc<str>,
}

#[cfg(not(feature = "terminal-tui"))]
type CapturedChatTrace = ();

#[cfg(feature = "terminal-tui")]
struct CapturedChatTrace {
    trace: crate::agent::turn_contracts::ToolLoopTrace,
    finished_at: chrono::DateTime<chrono::Utc>,
}

struct ChatDecision {
    resume: bool,
    service: Arc<crate::runtime::turn_decision::TurnDecisionService>,
    context: Arc<crate::runtime::turn_decision::TurnDecisionContext>,
}

impl std::fmt::Debug for ChatTurnBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatTurnBinding").finish_non_exhaustive()
    }
}

impl ChatTurnBinding {
    fn install_terminal(
        &self,
        ticket: crate::runtime::turn_cleanup::CleanupTicket,
        fabric: crate::memory::MemoryFabric,
    ) -> anyhow::Result<()> {
        let decision = self
            .decision
            .get()
            .context("chat terminal requires an admitted decision")?;
        anyhow::ensure!(self.terminal.get().is_none(), "chat terminal owner already exists");
        let route = crate::llm::route_decision::RouteDecision::from_model_routes_for_context(
            self.choice.provider_name(),
            self.choice.model_name(),
            &self.generation.effective.model_routes,
            self.owner_id.clone(),
            self.envelope
                .legacy_session_key
                .clone()
                .context("chat terminal legacy session identity missing")?,
            self.envelope.source_message_event_id.clone(),
            "chat",
            u32::try_from(self.routing_input.chars().count() / 4 + 1).unwrap_or(u32::MAX),
            true,
            false,
        );
        let retained = Arc::new(crate::agent::turn_contracts::RetainedProviderTrace::default());
        let guard = crate::runtime::turn_terminal::TurnTerminalGuard::new(
            ticket,
            Arc::clone(&decision.service),
            Arc::clone(&decision.context),
            fabric,
            Arc::clone(&self.generation.effective),
            self.envelope.clone(),
            None,
        )?
        .with_provider_trace(Arc::clone(&retained), route);
        self.terminal
            .set((guard, retained))
            .map_err(|_| anyhow::anyhow!("chat terminal owner already exists"))
    }

    pub(super) fn retained_provider_trace(&self) -> Option<Arc<crate::agent::turn_contracts::RetainedProviderTrace>> {
        self.terminal.get().map(|(_, trace)| Arc::clone(trace))
    }

    pub(super) async fn commit_terminal(
        &self,
        fabric: &crate::memory::MemoryFabric,
        proposal: crate::agent::terminal::TurnTerminalDraft,
        cost: &crate::config::schema::CostConfig,
        workspace: &std::path::Path,
    ) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
        anyhow::ensure!(
            fabric.workspace_id() == self.envelope.workspace_id && workspace == self.generation.effective.workspace_dir,
            "chat terminal workspace changed"
        );
        if let Some((guard, _)) = self.terminal.get() {
            guard.commit(proposal).await
        } else {
            // Read-only fixtures and already-owned legacy recovery calls use
            // the same original terminal writer; no source is manufactured.
            crate::agent::terminal::finalize_turn(fabric, proposal, cost, workspace).await
        }
    }

    pub(super) fn capture_trace(&self, trace: &crate::agent::turn_contracts::ToolLoopTrace) -> anyhow::Result<()> {
        anyhow::ensure!(
            trace.final_provider.is_some() && trace.final_model.is_some() && !trace.attempts.is_empty(),
            "chat returned no provider attribution trace"
        );
        if let Some(decided) = self.decision.get() {
            anyhow::ensure!(
                trace.execution_trace.as_ref() == Some(&decided.context.terminal_execution_trace()),
                "chat execution trace differs from its owning decision"
            );
        }
        #[cfg(feature = "terminal-tui")]
        let captured = CapturedChatTrace {
            trace: trace.clone(),
            finished_at: chrono::Utc::now(),
        };
        #[cfg(not(feature = "terminal-tui"))]
        let captured = ();
        self.returned_trace
            .set(captured)
            .map_err(|_| anyhow::anyhow!("chat provider trace was already captured"))
    }

    #[cfg(feature = "terminal-tui")]
    pub(super) fn provider_outcome(
        &self,
        route: &crate::llm::route_decision::RouteDecision,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<crate::llm::route_decision::ProviderExecutionOutcome> {
        let captured = self.returned_trace.get().context("chat provider trace is missing")?;
        anyhow::ensure!(
            captured.finished_at >= started_at,
            "chat provider trace predates the turn"
        );
        let mut outcome =
            crate::agent::terminal::provider_outcome_from_trace(route, started_at, captured.trace.clone());
        outcome.finished_at = captured.finished_at;
        Ok(outcome)
    }
    pub(super) fn pending_decision_id(&self) -> Option<crate::causal_tree::ids::DecisionId> {
        let decided = self.decision.get()?;
        matches!(
            decided.context.directive(),
            crate::causal_tree::directive::TurnExecutionDirective::RequestApproval { .. }
        )
        .then(|| decided.context.decision_id())
    }

    pub(super) fn is_decided(&self) -> bool {
        self.decision.initialized()
    }

    /// Close an admitted input that failed before a model decision existed.
    /// This event grants no execution rights and carries no provider settlement.
    pub(super) async fn record_predecision_failure(
        &self,
        fabric: &crate::memory::MemoryFabric,
        run_id: &str,
        status: crate::agent::terminal::TurnTerminalStatus,
        summary: &str,
    ) -> anyhow::Result<()> {
        use crate::agent::terminal::TurnTerminalStatus;
        let mut closed = self.admission_closed.lock().await;
        anyhow::ensure!(
            !self.is_decided()
                && matches!(status, TurnTerminalStatus::Failed | TurnTerminalStatus::Cancelled)
                && self.envelope.run_id.as_deref() == Some(run_id)
                && fabric.workspace_id() == self.envelope.workspace_id,
            "chat pre-decision failure differs from its source"
        );
        *closed = true;
        // Input recording may be disabled independently of runtime events.
        // Do not invent a durable source or override that privacy choice.
        if self.envelope.source_message_event_id.is_none()
            && !self.generation.effective.memory.should_record_user_message_event()
        {
            return Ok(());
        }
        let event_id = self
            .envelope
            .source_message_event_id
            .as_deref()
            .context("chat pre-decision failure has no durable input")?;
        let memory = fabric.memory();
        let input = memory
            .find_owned_message_event(fabric.workspace_id(), &self.owner_id, event_id)
            .await?
            .context("chat pre-decision input is missing")?;
        // Revalidate the original ingress against this actual backend before
        // writing; labels from the caller cannot substitute for the receipt.
        ChatSourceReceipt::issue(
            memory.as_ref(),
            &self.generation,
            self.envelope
                .legacy_session_key
                .as_deref()
                .context("chat source legacy session identity missing")?,
            run_id,
            &self.routing_input,
            &input,
        )
        .await?;
        let summary = super::sanitize::sanitize_for_persistence(summary);
        let payload = serde_json::json!({
            "run_id": run_id,
            "source_event_id": event_id,
            "status": status,
            "summary": summary,
        });
        let expected_content = format!("turn.admission.failed {summary}");
        let event = fabric
            .record_runtime_event_idempotent(
                self.envelope.message_scope(),
                "turn.admission.failed",
                summary,
                Some(serde_json::to_string(&payload)?),
                format!("turn:{run_id}:admission-failed"),
            )
            .await?;
        anyhow::ensure!(
            event.content == expected_content
                && event.idempotency_key.as_deref() == Some(format!("turn:{run_id}:admission-failed").as_str())
                && event.id > 0
                && !event.event_id.is_empty()
                && event.workspace_id == fabric.workspace_id()
                && event.owner_id.as_deref() == Some(self.owner_id.as_str())
                && event.run_id.as_deref() == Some(run_id)
                && event.session_key.as_deref() == Some(self.envelope.session_key.as_str())
                && event.source == MessageEventSource::Chat
                && event.channel == self.envelope.channel
                && event.sender == self.envelope.sender
                && event.parent_run_id == self.envelope.parent_run_id
                && event.agent_id == self.envelope.agent_id
                && event.persona_id == self.envelope.persona_id
                && event.attempt_id == self.envelope.attempt_id
                && event.lease_epoch == self.envelope.lease_epoch
                && event.config_generation_id == self.envelope.config_generation_id
                && event.config_source_revision == self.envelope.config_source_revision
                && event.causation_event_id.as_deref() == Some(event_id)
                && event.visibility == self.envelope.visibility
                && event.role == "event"
                && event.event_type == "turn.admission.failed"
                && event
                    .raw_payload_json
                    .as_deref()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .transpose()?
                    .as_ref()
                    == Some(&payload),
            "chat admission failure retry conflicts with its source or status"
        );
        let durable = fabric
            .memory()
            .find_owned_message_event(fabric.workspace_id(), self.owner_id.as_str(), &event.event_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("admission failure receipt is not durable"))?;
        anyhow::ensure!(
            serde_json::to_value(durable)? == serde_json::to_value(&event)?,
            "admission failure receipt differs from durable event"
        );
        Ok(())
    }

    pub(super) fn settlement_ref(
        &self,
        terminal_id: &str,
    ) -> anyhow::Result<Option<crate::agent::terminal::DecisionSettlementRef>> {
        let Some(decided) = self.decision.get() else {
            return Ok(None);
        };
        anyhow::ensure!(
            self.envelope.run_id.as_deref() == Some(terminal_id),
            "chat terminal id differs from its source"
        );
        Ok(Some(
            decided.service.terminal_settlement_ref(Arc::clone(&decided.context))?,
        ))
    }

    pub(super) async fn rejoin_terminal(&self, terminal_id: &str) -> anyhow::Result<()> {
        if let Some(decided) = self.decision.get() {
            anyhow::ensure!(
                self.envelope.run_id.as_deref() == Some(terminal_id),
                "chat terminal id differs from its source"
            );
            if self.settlement_ref(terminal_id)?.is_some_and(|link| link.is_recorded()) {
                decided.service.rejoin_terminal(terminal_id, &self.owner_id).await?;
            }
        }
        Ok(())
    }

    /// Queue consumption may not replace the immutable admission mode.
    pub(super) fn verify_execution_mode(&self, mode: crate::agent::turn_contracts::ChatMode) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.admission_mode == ChatAdmissionMode::Model(mode),
            "chat mode changed after admission"
        );
        Ok(())
    }

    pub(super) fn admitted_model_mode(&self) -> anyhow::Result<crate::agent::turn_contracts::ChatMode> {
        match self.admission_mode {
            ChatAdmissionMode::Model(mode) => Ok(mode),
            ChatAdmissionMode::OperatorCommand => {
                anyhow::bail!("operator command has no model-turn mode")
            }
        }
    }

    pub(super) async fn decide(
        &self,
        service: &Arc<crate::runtime::turn_decision::TurnDecisionService>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<&Arc<crate::runtime::turn_decision::TurnDecisionContext>> {
        self.decide_mode(service, cancellation, false).await
    }

    pub(super) async fn decide_resume(
        &self,
        service: &Arc<crate::runtime::turn_decision::TurnDecisionService>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<&Arc<crate::runtime::turn_decision::TurnDecisionContext>> {
        self.decide_mode(service, cancellation, true).await
    }

    async fn decide_mode(
        &self,
        service: &Arc<crate::runtime::turn_decision::TurnDecisionService>,
        cancellation: &tokio_util::sync::CancellationToken,
        resume: bool,
    ) -> anyhow::Result<&Arc<crate::runtime::turn_decision::TurnDecisionContext>> {
        anyhow::ensure!(
            matches!(self.admission_mode, ChatAdmissionMode::Model(_)),
            "operator command cannot issue a model-turn decision"
        );
        let closed = self.admission_closed.lock().await;
        anyhow::ensure!(!*closed, "chat input was closed before decision issuance");
        let decided = self
            .decision
            .get_or_try_init(|| async {
                let kind = crate::runtime::turn_decision::AgentTurnKind::Repl;
                let context = if resume {
                    service.decide_agent_resume(self, kind, cancellation).await?
                } else {
                    service.decide_agent_initial(self, kind, cancellation).await?
                };
                Ok::<_, anyhow::Error>(ChatDecision {
                    resume,
                    service: Arc::clone(service),
                    context,
                })
            })
            .await?;
        anyhow::ensure!(Arc::ptr_eq(&decided.service, service), "chat decision service changed");
        anyhow::ensure!(decided.resume == resume, "chat decision mode changed");
        Ok(&decided.context)
    }

    /// Read only the context selected by this turn's admitted decision.
    pub(super) fn recalled_context(&self) -> anyhow::Result<crate::agent::context_recall::RecalledMemoryContext> {
        let decided = self
            .decision
            .get()
            .context("chat recall requires an admitted decision")?;
        if self.fixed_reply()?.is_some() {
            return Ok(crate::agent::context_recall::RecalledMemoryContext {
                preamble: String::new(),
                ids: Vec::new(),
            });
        }
        Ok(crate::agent::context_recall::RecalledMemoryContext {
            preamble: crate::agent::memory_loader::format_selected_context(None, decided.context.retrieved()?)?,
            ids: Vec::new(),
        })
    }

    pub(super) fn is_approval_collection(&self) -> anyhow::Result<bool> {
        self.decision.get().map_or(Ok(false), |decided| {
            Ok(matches!(
                decided.context.disposition()?,
                crate::runtime::turn_decision::TurnDisposition::CollectApprovalArguments
            ))
        })
    }

    pub(super) fn fixed_reply(&self) -> anyhow::Result<Option<String>> {
        let Some(decided) = self.decision.get() else {
            return Ok(None);
        };
        match decided.context.disposition()? {
            crate::runtime::turn_decision::TurnDisposition::Close { reply } => Ok(Some(reply.into_owned())),
            crate::runtime::turn_decision::TurnDisposition::Run
            | crate::runtime::turn_decision::TurnDisposition::CollectApprovalArguments => Ok(None),
        }
    }

    pub(super) fn main_compaction(&self) -> Option<&crate::config::AgentCompactionConfig> {
        self.decision.get().map(|decided| decided.context.main_compaction())
    }

    /// Persist a fixed directive without inventing provider execution. The
    /// caller must separately commit ordered session history and deliver it.
    pub(super) async fn close(
        &self,
        fabric: &crate::memory::MemoryFabric,
        history_commit_len: usize,
        started_at: chrono::DateTime<chrono::Utc>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt> {
        let decided = self.decision.get().context("chat close has no decision")?;
        let ports = ChatDecisionClose {
            binding: self,
            fabric,
            history_commit_len,
            started_at,
        };
        decided
            .service
            .close_agent_directive_turn(self, Arc::clone(&decided.context), &ports, cancellation)
            .await
    }

    /// Direct approved-call execution has its own sealed tool trace. It must
    /// never manufacture a provider outcome or weaken capture_trace checks.
    pub(super) async fn execute_resume_and_finalize(
        &self,
        fabric: &crate::memory::MemoryFabric,
        history_commit_len: usize,
        started_at: chrono::DateTime<chrono::Utc>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<(String, bool)> {
        use crate::agent::terminal::{TurnTerminalDraft, TurnTerminalStatus};
        let decided = self.decision.get().context("chat resume has no decision")?;
        anyhow::ensure!(decided.resume, "chat turn is not an explicit resume");
        let outcome = decided
            .service
            .execute_agent_resume(
                self,
                &decided.context,
                &crate::tools::ToolExecutionContext::new(self.envelope.clone(), "private")
                    .with_chat_id(&self.envelope.session_key),
                cancellation,
            )
            .await?;
        let (status, reply) = match outcome.status {
            crate::tools::ToolExecutionStatus::Succeeded => (
                TurnTerminalStatus::Completed,
                crate::security::redaction::redact_secrets(&outcome.model_content),
            ),
            crate::tools::ToolExecutionStatus::Cancelled => (
                TurnTerminalStatus::Cancelled,
                "Approved operation was cancelled.".to_owned(),
            ),
            _ => (
                TurnTerminalStatus::Failed,
                "Approved operation could not be completed.".to_owned(),
            ),
        };
        let terminal_id = self.envelope.run_id.as_deref().context("chat resume has no run")?;
        let scope = self.envelope.message_scope();
        self.commit_terminal(
            fabric,
            TurnTerminalDraft {
                decision: self.settlement_ref(terminal_id)?,
                terminal_id: terminal_id.to_owned(),
                history_scope: Some(scope.clone().with_sender("prx_runtime").with_recipient("local-user")),
                scope,
                status,
                history: Some(crate::agent::terminal::TurnHistoryProjection {
                    assistant_content: reply.clone(),
                    history_commit_len,
                }),
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "chat approved operation finished".to_owned(),
                    started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                    target: "terminal:local-user".to_owned(),
                },
            },
            &self.generation.effective.cost,
            &self.generation.effective.workspace_dir,
        )
        .await?;
        Ok((reply, status == TurnTerminalStatus::Completed))
    }

    pub(super) fn prompt_skills(&self) -> anyhow::Result<Vec<crate::skills::Skill>> {
        let decided = self
            .decision
            .get()
            .context("chat skill view requires an admitted decision")?;
        decided.service.agent_prompt_skills(self, &decided.context)
    }

    pub(super) fn decision_routing(&self) -> Option<crate::agent::turn_request::TurnDecision<'_>> {
        self.decision
            .get()
            .map(|decided| crate::agent::turn_request::TurnDecision {
                service: &decided.service,
                context: &decided.context,
                proof: self,
            })
    }

    pub(super) fn provider_name(&self) -> &str {
        self.choice.provider_name()
    }

    pub(super) fn provider(&self) -> Arc<dyn crate::providers::Provider> {
        Arc::clone(&self.provider)
    }
    pub(super) fn model(&self) -> &str {
        &self.model
    }
    pub(super) const fn envelope(&self) -> &RuntimeEnvelope {
        &self.envelope
    }
    #[cfg(feature = "terminal-tui")]
    pub(super) fn routing_input(&self) -> &str {
        &self.routing_input
    }
}

impl ChatSourceReceipt {
    pub(super) fn bind_provider(
        &self,
        binding: &ChatProviderBinding,
        admission_mode: ChatAdmissionMode,
    ) -> anyhow::Result<Arc<ChatTurnBinding>> {
        anyhow::ensure!(
            self.envelope.config_generation_id == Some(binding.generation.id.0)
                && self.envelope.config_source_revision.as_deref()
                    == Some(binding.generation.source_revision.fingerprint_sha256.as_str()),
            "chat source and provider generations differ"
        );
        Ok(Arc::new(ChatTurnBinding {
            admission_mode,
            terminal: std::sync::OnceLock::new(),
            returned_trace: std::sync::OnceLock::new(),
            decision: tokio::sync::OnceCell::new(),
            admission_closed: tokio::sync::Mutex::new(false),
            choice: binding.choice.clone(),
            owner_id: self.envelope.resolved_owner_id(),
            memory_scope: self.envelope.memory_write_context("private"),
            generation: Arc::clone(&binding.generation),
            provider: Arc::clone(&binding.provider),
            model: binding.choice.model_name().to_owned(),
            envelope: self.envelope.clone(),
            routing_input: Arc::clone(&self.routing_input),
        }))
    }
    /// Disabled mode can bind local input without claiming it was persisted.
    /// A durable event, when supplied, keeps the same exact verification path.
    pub(super) async fn issue_for_turn(
        memory: &dyn Memory,
        generation: &ConfigGeneration,
        session_key: &str,
        run_id: &str,
        raw_input: &str,
        recorded: Option<&MessageEvent>,
    ) -> anyhow::Result<Self> {
        if let Some(recorded) = recorded.filter(|event| event.id > 0 && !event.event_id.is_empty()) {
            return Self::issue(memory, generation, session_key, run_id, raw_input, recorded).await;
        }
        anyhow::ensure!(
            !generation.effective.causal_tree.enabled,
            "enabled chat decision source was not recorded"
        );
        anyhow::ensure!(
            !session_key.is_empty() && !run_id.is_empty(),
            "chat source identity is missing"
        );
        let workspace = generation.effective.workspace_dir.to_string_lossy();
        let envelope = super::chat_runtime_envelope(&workspace, session_key)
            .with_config_generation(generation)
            .with_run_id(run_id.to_owned());
        Ok(Self {
            envelope,
            routing_input: Arc::from(raw_input),
        })
    }

    pub(super) async fn issue(
        memory: &dyn Memory,
        generation: &ConfigGeneration,
        session_key: &str,
        run_id: &str,
        raw_input: &str,
        recorded: &MessageEvent,
    ) -> anyhow::Result<Self> {
        let workspace = generation.effective.workspace_dir.to_string_lossy();
        let envelope = super::chat_runtime_envelope(&workspace, session_key)
            .with_config_generation(generation)
            .with_run_id(run_id.to_owned())
            .with_source_message_event_id(recorded.event_id.clone());
        anyhow::ensure!(
            !run_id.is_empty()
                && recorded.id > 0
                && !recorded.event_id.is_empty()
                && recorded.workspace_id == workspace
                && recorded.owner_id.as_deref() == Some(envelope.resolved_owner_id().as_str())
                && recorded.source == MessageEventSource::Chat
                && recorded.channel.as_deref() == Some("terminal")
                && recorded.sender.as_deref() == Some("local-user")
                && recorded.session_key.as_deref() == Some(envelope.session_key.as_str())
                && recorded.run_id.as_deref() == Some(run_id)
                && recorded.parent_session_key.is_none()
                && recorded.parent_run_id.is_none()
                && recorded.agent_id.is_none()
                && recorded.persona_id.is_none()
                && recorded.attempt_id.is_none()
                && recorded.lease_epoch.is_none()
                && recorded.role == "user"
                && recorded.event_type == "message.created"
                && recorded.config_generation_id == Some(generation.id.0)
                && recorded.config_source_revision.as_deref()
                    == Some(generation.source_revision.fingerprint_sha256.as_str())
                && recorded.visibility == envelope.visibility
                && recorded.content == super::sanitize::sanitize_for_persistence(raw_input),
            "chat source event differs from its local ingress"
        );
        let durable = memory
            .find_owned_message_event(&workspace, &envelope.resolved_owner_id(), &recorded.event_id)
            .await?
            .context("chat source event is not durable")?;
        anyhow::ensure!(
            serde_json::to_value(&durable)? == serde_json::to_value(recorded)?,
            "chat source receipt differs from the durable event"
        );
        Ok(Self {
            envelope,
            routing_input: std::sync::Arc::from(raw_input),
        })
    }
}

impl crate::runtime::turn_source::TurnSourceProof for ChatTurnBinding {
    fn owner_isolated(&self) -> bool {
        self.envelope.owner_isolated
    }
    fn approval_review_adapter(&self) -> Option<crate::causal_tree::approval_collection::ApprovalReviewAdapter> {
        self.envelope
            .source_message_event_id
            .as_ref()
            .map(|_| crate::causal_tree::approval_collection::ApprovalReviewAdapter::ChatOwnerReview)
    }
    fn intercepts_writes(&self) -> bool {
        matches!(self.admission_mode, ChatAdmissionMode::Model(mode) if mode.intercepts_writes())
    }
    fn source_memory_principal(&self) -> crate::memory::MemoryPrincipal {
        self.envelope.memory_principal()
    }

    fn workspace_id(&self) -> &str {
        &self.envelope.workspace_id
    }
    fn owner_id(&self) -> &str {
        &self.owner_id
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
        &self.routing_input
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
        &self.memory_scope
    }
    fn model_binding(&self) -> &crate::agent::entry::AgentModelBinding {
        self.choice.binding()
    }
    fn config_generation_id(&self) -> u64 {
        self.generation.id.0
    }
    fn surface(&self) -> crate::causal_tree::state::TurnSurfaceKind {
        crate::causal_tree::state::TurnSurfaceKind::Interactive
    }
    fn policy_channel(&self) -> &str {
        "terminal"
    }
}

/// Only the owning binding supplies source scope and settlement authority.
struct ChatDecisionClose<'a> {
    binding: &'a ChatTurnBinding,
    fabric: &'a crate::memory::MemoryFabric,
    history_commit_len: usize,
    started_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait::async_trait]
impl crate::runtime::turn_decision::CloseTurnPorts for ChatDecisionClose<'_> {
    type Receipt = crate::agent::terminal::TurnTerminalReceipt;

    async fn finalize_closed_turn(
        &self,
        context: Arc<crate::runtime::turn_decision::TurnDecisionContext>,
        reply: &str,
    ) -> anyhow::Result<Self::Receipt> {
        let decided = self.binding.decision.get().context("chat close has no decision")?;
        anyhow::ensure!(Arc::ptr_eq(&context, &decided.context), "chat close decision changed");
        let terminal_id = self
            .binding
            .envelope
            .run_id
            .as_deref()
            .context("chat close has no run")?;
        let scope = self.binding.envelope.message_scope();
        self.binding
            .commit_terminal(
                self.fabric,
                crate::agent::terminal::TurnTerminalDraft {
                    decision: self.binding.settlement_ref(terminal_id)?,
                    terminal_id: terminal_id.to_owned(),
                    history_scope: Some(scope.clone().with_sender("prx_runtime").with_recipient("local-user")),
                    scope,
                    status: crate::agent::terminal::TurnTerminalStatus::Completed,
                    history: Some(crate::agent::terminal::TurnHistoryProjection {
                        assistant_content: reply.to_owned(),
                        history_commit_len: self.history_commit_len,
                    }),
                    provider_outcome: None,
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "chat decision closed".to_owned(),
                        started_at: self.started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Reply {
                        target: "terminal:local-user".to_owned(),
                    },
                },
                &self.binding.generation.effective.cost,
                &self.binding.generation.effective.workspace_dir,
            )
            .await
    }
}

#[cfg(test)]
pub(crate) mod admission_failure_tests {
    mod operator_accounting;
    #[cfg(unix)]
    mod worker_stdin_protocol;
    use super::*;
    use crate::agent::terminal::TurnTerminalStatus;
    use crate::memory::{MemoryFabric, SqliteMemory};

    /// Admit only an actual local `/bg` fixture through Chat's private source path.
    pub(crate) async fn recorded_local_operator_spawn(
        publication: crate::runtime::turn_publication::TurnDecisionPublication,
        provider: Arc<dyn crate::providers::Provider>,
        task: &str,
    ) -> anyhow::Result<(serde_json::Value, crate::runtime::operator_spawn::OperatorSpawnSource)> {
        let generation = Arc::clone(&publication.generation);
        let memory = Arc::clone(&publication.memory);
        let security = Arc::clone(&publication.security);
        let config = &generation.effective;
        let provider_name = config
            .default_provider
            .as_deref()
            .context("operator fixture provider missing")?;
        let model_name = config
            .default_model
            .as_deref()
            .context("operator fixture model missing")?;
        let binding = ChatProviderBinding::capture(&generation, &provider, provider_name, model_name)?
            .context("operator fixture provider binding missing")?;
        let service = publication
            .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
            .await?;
        crate::runtime::turn_cleanup::current_tracker()
            .context("operator fixture cleanup owner missing")?
            .retain_service(Arc::clone(&service));
        let message = crate::channels::traits::ChannelMessage {
            id: "prx-operator-command".to_owned(),
            sender: "user".to_owned(),
            channel: "terminal".to_owned(),
            content: format!("/bg {task}"),
            ..Default::default()
        };
        ChatOperatorSpawn::capture(
            &message,
            task,
            &binding,
            &generation,
            &provider,
            provider_name,
            model_name,
        )?
        .admit(memory, "prx_operator_session", security, service)
        .await
    }

    /// Record and admit a model-origin turn before dispatching its spawn call.
    /// This preserves the configured decision policy, including an abstention.
    pub(crate) async fn dispatch_recorded_model_spawn(
        publication: crate::runtime::turn_publication::TurnDecisionPublication,
        provider: Arc<dyn crate::providers::Provider>,
        arguments: serde_json::Value,
        serving_phase: Arc<std::sync::atomic::AtomicU8>,
    ) -> anyhow::Result<crate::tools::ToolResult> {
        use crate::runtime::turn_source::TurnSourceProof as _;
        let generation = Arc::clone(&publication.generation);
        let memory = Arc::clone(&publication.memory);
        let config = &generation.effective;
        let provider_name = config
            .default_provider
            .as_deref()
            .context("model fixture provider missing")?;
        let model_name = config.default_model.as_deref().context("model fixture model missing")?;
        let selected = ChatProviderBinding::capture(&generation, &provider, provider_name, model_name)?
            .context("model fixture provider binding missing")?;
        let service = publication
            .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
            .await?;
        crate::runtime::turn_cleanup::current_tracker()
            .context("model fixture cleanup owner missing")?
            .retain_service(Arc::clone(&service));
        let input = "spawn model child";
        let session = "chat:session-1";
        let run = "turn-run-xyz";
        let envelope = super::super::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), session)
            .with_config_generation(&generation)
            .with_run_id(run.to_owned());
        let fabric = MemoryFabric::new(Arc::clone(&memory), config.workspace_dir.to_string_lossy())
            .with_event_recording(config.memory.event_recording_config());
        let event = fabric
            .record_inbound_user_message(envelope.message_scope(), input, None, None)
            .await?;
        let receipt = ChatSourceReceipt::issue(memory.as_ref(), &generation, session, run, input, &event).await?;
        let binding = receipt.bind_provider(
            &selected,
            ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
        )?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let context = binding.decide(&service, &cancellation).await?;
        use crate::causal_tree::directive::TurnExecutionDirective;
        let (directive, reason) = match context.directive() {
            TurnExecutionDirective::Clarify { reason_code } => ("Clarify", reason_code.as_str()),
            TurnExecutionDirective::Refuse { policy_code } => ("Refuse", policy_code.as_str()),
            TurnExecutionDirective::AnswerDirectly => ("AnswerDirectly", "not_applicable"),
            TurnExecutionDirective::Execute { .. } => ("Execute", "not_applicable"),
            TurnExecutionDirective::Delegate { .. } => ("Delegate", "not_applicable"),
            TurnExecutionDirective::Retrieve { .. } => ("Retrieve", "not_applicable"),
            TurnExecutionDirective::Discover { .. } => ("Discover", "not_applicable"),
            TurnExecutionDirective::RequestApproval { .. } => ("RequestApproval", "not_applicable"),
            TurnExecutionDirective::CollectApprovalArguments(_) => ("CollectApprovalArguments", "not_applicable"),
        };
        let reason = match reason {
            "insufficient_evidence"
            | "commit_threshold_or_margin"
            | "no_legal_path"
            | "retrieve_evidence"
            | "hop_stopped"
            | "choice_outside_legal_set"
            | "approval_only"
            | "deadline_exceeded"
            | "not_applicable" => reason,
            _ => "unclassified",
        };
        eprintln!("PRX_Q47_PARENT_ADMISSION directive={directive} reason_code={reason}");
        let specs = service.agent_execution_specs(binding.as_ref(), context)?;
        anyhow::ensure!(
            specs.specs.iter().any(|spec| spec.name == "sessions_spawn"),
            "genuine model admission did not select sessions_spawn"
        );
        let execution_context =
            crate::tools::ToolExecutionContext::new(binding.envelope().clone(), binding.policy_chat_type());
        let (execution, execution_context) =
            service.bind_agent_execution(binding.as_ref(), context, &execution_context, &specs.specs)?;
        let messages = [crate::providers::ChatMessage::user(input)];
        let response = provider
            .chat(
                crate::providers::ChatRequest {
                    messages: &messages,
                    tools: Some(&specs.specs),
                },
                model_name,
                0.7,
            )
            .await?;
        anyhow::ensure!(response.tool_calls.len() == 1, "one genuine parent tool call required");
        let call = response.tool_calls.into_iter().next().context("parent call missing")?;
        anyhow::ensure!(call.name == "sessions_spawn", "unexpected parent tool name");
        let mut returned_arguments: serde_json::Value = serde_json::from_str(&call.arguments)?;
        anyhow::ensure!(
            returned_arguments == serde_json::json!({"task":"model child"}),
            "parent arguments changed"
        );
        let grant_key = crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG;
        let grant = arguments.get(grant_key).context("host resource grant missing")?.clone();
        returned_arguments
            .as_object_mut()
            .context("parent object required")?
            .insert(grant_key.to_owned(), grant);
        serving_phase
            .compare_exchange(
                1,
                2,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .map_err(|_| anyhow::anyhow!("parent response phase was not observed"))?;
        let outcome = execution
            .execute(
                crate::tools::ToolExecutionCommand::new(call.name, returned_arguments),
                execution_context,
                Some(cancellation),
            )
            .await;
        outcome
            .result
            .context("genuine model dispatch did not return a tool result")
    }

    /// Record and admit a model-origin turn before dispatching its spawn call.
    /// This preserves the configured decision policy, including an abstention.
    pub(crate) async fn dispatch_unknown_agent_fixture(
        publication: crate::runtime::turn_publication::TurnDecisionPublication,
        provider: Arc<dyn crate::providers::Provider>,
        arguments: serde_json::Value,
    ) -> anyhow::Result<crate::tools::ToolResult> {
        use crate::runtime::turn_source::TurnSourceProof as _;
        let generation = Arc::clone(&publication.generation);
        let memory = Arc::clone(&publication.memory);
        let config = &generation.effective;
        let provider_name = config
            .default_provider
            .as_deref()
            .context("model fixture provider missing")?;
        let model_name = config.default_model.as_deref().context("model fixture model missing")?;
        let selected = ChatProviderBinding::capture(&generation, &provider, provider_name, model_name)?
            .context("model fixture provider binding missing")?;
        let service = publication
            .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
            .await?;
        crate::runtime::turn_cleanup::current_tracker()
            .context("model fixture cleanup owner missing")?
            .retain_service(Arc::clone(&service));
        let input = "sessions_spawn hello";
        let session = "chat:unknown-agent";
        let run = "prx_unknown_agent_parent";
        let envelope = super::super::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), session)
            .with_config_generation(&generation)
            .with_run_id(run.to_owned());
        let fabric = MemoryFabric::new(Arc::clone(&memory), config.workspace_dir.to_string_lossy())
            .with_event_recording(config.memory.event_recording_config());
        let event = fabric
            .record_inbound_user_message(envelope.message_scope(), input, None, None)
            .await?;
        let receipt = ChatSourceReceipt::issue(memory.as_ref(), &generation, session, run, input, &event).await?;
        let binding = receipt.bind_provider(
            &selected,
            ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
        )?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let context = binding.decide(&service, &cancellation).await?;
        use crate::causal_tree::directive::TurnExecutionDirective;
        let (directive, reason) = match context.directive() {
            TurnExecutionDirective::Clarify { reason_code } => ("Clarify", reason_code.as_str()),
            TurnExecutionDirective::Refuse { policy_code } => ("Refuse", policy_code.as_str()),
            TurnExecutionDirective::AnswerDirectly => ("AnswerDirectly", "not_applicable"),
            TurnExecutionDirective::Execute { .. } => ("Execute", "not_applicable"),
            TurnExecutionDirective::Delegate { .. } => ("Delegate", "not_applicable"),
            TurnExecutionDirective::Retrieve { .. } => ("Retrieve", "not_applicable"),
            TurnExecutionDirective::Discover { .. } => ("Discover", "not_applicable"),
            TurnExecutionDirective::RequestApproval { .. } => ("RequestApproval", "not_applicable"),
            TurnExecutionDirective::CollectApprovalArguments(_) => ("CollectApprovalArguments", "not_applicable"),
        };
        let reason = match reason {
            "insufficient_evidence"
            | "commit_threshold_or_margin"
            | "no_legal_path"
            | "retrieve_evidence"
            | "hop_stopped"
            | "choice_outside_legal_set"
            | "approval_only"
            | "deadline_exceeded"
            | "not_applicable" => reason,
            _ => "unclassified",
        };
        let specs = service
            .agent_execution_specs(binding.as_ref(), context)
            .with_context(|| format!("unknown-agent parent admission directive={directive} reason_code={reason}"))?;
        anyhow::ensure!(
            specs.specs.iter().any(|spec| spec.name == "sessions_spawn"),
            "genuine model admission did not select sessions_spawn"
        );
        let execution_context =
            crate::tools::ToolExecutionContext::new(binding.envelope().clone(), binding.policy_chat_type());
        let (execution, execution_context) =
            service.bind_agent_execution(binding.as_ref(), context, &execution_context, &specs.specs)?;
        let outcome = execution
            .execute(
                crate::tools::ToolExecutionCommand::new("sessions_spawn", arguments),
                execution_context,
                Some(cancellation),
            )
            .await;
        outcome
            .result
            .context("genuine model dispatch did not return a tool result")
    }

    async fn fixture(record_input: bool) -> (tempfile::TempDir, MemoryFabric, Arc<ChatTurnBinding>) {
        fixture_with_plane(record_input, false).await
    }

    async fn fixture_with_plane(
        record_input: bool,
        enabled: bool,
    ) -> (tempfile::TempDir, MemoryFabric, Arc<ChatTurnBinding>) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = dir.path().to_path_buf();
        config.causal_tree.enabled = enabled;
        config.memory.events.record_user_messages = record_input;
        let generation = crate::config::new_shared(config).pin();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), dir.path().to_string_lossy())
            .with_event_recording(generation.effective.memory.event_recording_config());
        let session = super::super::session::ChatSession::new("ollama", "prx-local-model");
        let event = super::super::record_chat_user_message_event_in_generation(
            &fabric,
            &session,
            "prx_session",
            "prx_run",
            "ollama",
            "prx-local-model",
            1,
            "prx input",
            Some(&generation),
        )
        .await
        .unwrap();
        let source = ChatSourceReceipt::issue_for_turn(
            memory.as_ref(),
            &generation,
            "prx_session",
            "prx_run",
            "prx input",
            Some(&event),
        )
        .await
        .unwrap();
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::ollama::OllamaProvider::new(None, None));
        let provider_binding = ChatProviderBinding::capture(&generation, &provider, "ollama", "prx-local-model")
            .unwrap()
            .unwrap();
        let binding = source
            .bind_provider(
                &provider_binding,
                ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
            )
            .unwrap();
        (dir, fabric, binding)
    }

    pub(in crate::chat) async fn decided_binding(
        fabric: &MemoryFabric,
        session_key: &str,
        run_id: &str,
    ) -> (
        Arc<ChatTurnBinding>,
        Arc<crate::runtime::turn_decision::TurnDecisionService>,
    ) {
        let (binding, service, _) = decided_binding_for_input(fabric, session_key, run_id, "prx fixture input").await;
        (binding, service)
    }

    pub(in crate::chat) async fn decided_binding_for_input(
        fabric: &MemoryFabric,
        session_key: &str,
        run_id: &str,
        input: &str,
    ) -> (
        Arc<ChatTurnBinding>,
        Arc<crate::runtime::turn_decision::TurnDecisionService>,
        MessageEvent,
    ) {
        decided_binding_for_provider(fabric, session_key, run_id, input, "ollama", "prx-local-model").await
    }

    pub(in crate::chat) async fn decided_binding_for_provider(
        fabric: &MemoryFabric,
        session_key: &str,
        run_id: &str,
        input: &str,
        provider_name: &str,
        model_name: &str,
    ) -> (
        Arc<ChatTurnBinding>,
        Arc<crate::runtime::turn_decision::TurnDecisionService>,
        MessageEvent,
    ) {
        decided_binding_for_provider_retained(fabric, session_key, run_id, input, provider_name, model_name, None).await
    }

    pub(in crate::chat) struct FixtureServiceDrain {
        service: Option<Arc<crate::runtime::turn_decision::TurnDecisionService>>,
        ticket: Option<crate::runtime::turn_cleanup::CleanupTicket>,
    }

    impl Drop for FixtureServiceDrain {
        fn drop(&mut self) {
            let Some(service) = self.service.take() else {
                return;
            };
            let Some(ticket) = self.ticket.take() else {
                return;
            };
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let drained = service
                        .drain_writes(std::time::Instant::now() + std::time::Duration::from_secs(2))
                        .await;
                    ticket.finish(drained);
                });
            }
            // Without a live runtime, dropping the unfinished ticket fails cleanup.
        }
    }

    pub(in crate::chat) async fn decided_binding_for_input_retained(
        fabric: &MemoryFabric,
        session_key: &str,
        run_id: &str,
        input: &str,
        retained: &mut Option<FixtureServiceDrain>,
    ) -> (
        Arc<ChatTurnBinding>,
        Arc<crate::runtime::turn_decision::TurnDecisionService>,
        MessageEvent,
    ) {
        decided_binding_for_provider_retained(
            fabric,
            session_key,
            run_id,
            input,
            "ollama",
            "prx-local-model",
            Some(retained),
        )
        .await
    }

    async fn decided_binding_for_provider_retained(
        fabric: &MemoryFabric,
        session_key: &str,
        run_id: &str,
        input: &str,
        provider_name: &str,
        model_name: &str,
        retained: Option<&mut Option<FixtureServiceDrain>>,
    ) -> (
        Arc<ChatTurnBinding>,
        Arc<crate::runtime::turn_decision::TurnDecisionService>,
        MessageEvent,
    ) {
        let mut config = crate::config::Config::default();
        config.workspace_dir = std::path::PathBuf::from(fabric.workspace_id());
        config.config_path = config.workspace_dir.join("config.toml");
        config.default_provider = Some(provider_name.to_owned());
        config.default_model = Some(model_name.to_owned());
        if provider_name == "anthropic" {
            config.api_key = Some("prx-test-only-credential".to_owned());
        }
        config.causal_tree.enabled = false;
        let generation = crate::config::new_shared(config).pin();
        let session = super::super::session::ChatSession::new(provider_name, model_name);
        let event = super::super::record_chat_user_message_event_in_generation(
            fabric,
            &session,
            session_key,
            run_id,
            provider_name,
            model_name,
            1,
            input,
            Some(&generation),
        )
        .await
        .unwrap();
        let source = ChatSourceReceipt::issue(
            fabric.memory().as_ref(),
            &generation,
            session_key,
            run_id,
            input,
            &event,
        )
        .await
        .unwrap();
        let provider: Arc<dyn crate::providers::Provider> = match provider_name {
            "ollama" => Arc::new(crate::providers::ollama::OllamaProvider::new(None, None)),
            "anthropic" => Arc::new(crate::providers::anthropic::AnthropicProvider::new(
                generation.effective.api_key.as_deref(),
            )),
            _ => panic!("unsupported Chat fixture provider"),
        };
        let provider_binding = ChatProviderBinding::capture(&generation, &provider, provider_name, model_name)
            .unwrap()
            .unwrap();
        let binding = source
            .bind_provider(
                &provider_binding,
                ChatAdmissionMode::Model(crate::agent::turn_contracts::ChatMode::Edit),
            )
            .unwrap();
        let service = service(fabric, &binding).await;
        if let Some(retained) = retained {
            let tracker = crate::runtime::turn_cleanup::current_tracker().expect("owned fixture cleanup tracker");
            let ticket = tracker
                .register(tokio_util::sync::CancellationToken::new())
                .expect("owned fixture service ticket");
            *retained = Some(FixtureServiceDrain {
                service: Some(Arc::clone(&service)),
                ticket: Some(ticket),
            });
        }
        binding
            .decide(&service, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        (binding, service, event)
    }

    async fn service(
        fabric: &MemoryFabric,
        binding: &ChatTurnBinding,
    ) -> Arc<crate::runtime::turn_decision::TurnDecisionService> {
        let config = &binding.generation.effective;
        crate::runtime::turn_publication::TurnDecisionPublication {
            generation: Arc::clone(&binding.generation),
            registry: Arc::new(Vec::new()),
            mcp_tool: None,
            security: Arc::new(crate::security::SecurityPolicy::from_config(
                &config.autonomy,
                &config.workspace_dir,
            )),
            observer: Arc::new(crate::observability::noop::NoopObserver),
            hooks: Arc::new(crate::hooks::HookManager::new(config.workspace_dir.clone())),
            memory: fabric.memory(),
            plugin_generation: 0,
        }
        .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn guarded_plan_admission_preserves_mode_and_owned_capture_with_plane_on_and_off() {
        use crate::agent::turn_contracts::ChatMode;
        use crate::runtime::turn_source::{OwnedTurnSourceProof, TurnSourceProof};
        for enabled in [false, true] {
            let (_dir, fabric, original) = fixture_with_plane(true, enabled).await;
            let provider =
                ChatProviderBinding::capture(&original.generation, &original.provider, "ollama", "prx-local-model")
                    .unwrap()
                    .unwrap();
            let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
            let cancellation = tokio_util::sync::CancellationToken::new();
            let envelope = super::super::chat_runtime_envelope(fabric.workspace_id(), "prx_plan_session")
                .with_config_generation(&original.generation)
                .with_run_id("prx_plan_run");
            let (_, binding, guard) = provider
                .record_guarded(
                    fabric.clone(),
                    (
                        envelope.message_scope(),
                        "prx_plan_session",
                        ChatAdmissionMode::Model(ChatMode::Plan),
                    ),
                    "prx plan input",
                    "prx_plan_input".to_owned(),
                    owner.register(cancellation.clone()).unwrap(),
                )
                .await
                .unwrap();
            guard.handoff();
            assert_eq!(binding.admitted_model_mode().unwrap(), ChatMode::Plan);
            assert!(binding.verify_execution_mode(ChatMode::Plan).is_ok());
            assert!(binding.verify_execution_mode(ChatMode::Edit).is_err());
            assert!(binding.verify_execution_mode(ChatMode::Auto).is_err());
            let queued = Arc::clone(&binding);
            let owned = OwnedTurnSourceProof::capture(queued.as_ref());
            let copied = OwnedTurnSourceProof::capture(&owned);
            assert!(owned.intercepts_writes() && copied.intercepts_writes());
            let service = service(&fabric, &binding).await;
            let context = binding.decide(&service, &cancellation).await.unwrap();
            assert!(
                context
                    .inherited_constraint_codes()
                    .iter()
                    .any(|code| code.as_str() == "intercepts_writes")
            );
            let actual = &context.engine().state;
            let mut without_plan = actual.clone();
            let mut trusted = without_plan.trusted.parts().clone();
            trusted.constraints.retain(|code| code.as_str() != "intercepts_writes");
            without_plan.trusted = crate::causal_tree::state::TrustedState::from_runtime(trusted);
            let key = crate::causal_tree::state::fixtures::key();
            assert_ne!(
                actual.material_fingerprint(&key),
                without_plan.material_fingerprint(&key)
            );
            let source = ChatSourceReceipt {
                envelope: binding.envelope.clone(),
                routing_input: Arc::clone(&binding.routing_input),
            };
            let changed_mode = source
                .bind_provider(&provider, ChatAdmissionMode::Model(ChatMode::Edit))
                .unwrap();
            let trusted = actual.trusted.parts();
            assert!(
                crate::runtime::turn_decision::validate_agent_retrieval_provenance(
                    binding.as_ref(),
                    trusted,
                    &trusted.resolved_model,
                    &trusted.policy_version,
                    &trusted.exclusions,
                    trusted.model_admission.as_deref(),
                )
                .is_ok()
            );
            assert!(
                crate::runtime::turn_decision::validate_agent_retrieval_provenance(
                    changed_mode.as_ref(),
                    trusted,
                    &trusted.resolved_model,
                    &trusted.policy_version,
                    &trusted.exclusions,
                    trusted.model_admission.as_deref(),
                )
                .is_err()
            );
            // Resume cannot reinterpret a cached initial decision or clear its mode.
            assert!(binding.decide_resume(&service, &cancellation).await.is_err());
            assert!(queued.verify_execution_mode(ChatMode::Edit).is_err());
            assert!(binding.intercepts_writes());
            service
                .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                .await;
        }
    }

    #[tokio::test]
    async fn operator_command_binding_cannot_be_reused_as_model_admission() {
        use crate::agent::turn_contracts::ChatMode;
        let (_dir, fabric, original) = fixture(true).await;
        let provider =
            ChatProviderBinding::capture(&original.generation, &original.provider, "ollama", "prx-local-model")
                .unwrap()
                .unwrap();
        let source = ChatSourceReceipt {
            envelope: original.envelope.clone(),
            routing_input: Arc::clone(&original.routing_input),
        };
        let operator = source
            .bind_provider(&provider, ChatAdmissionMode::OperatorCommand)
            .unwrap();
        for mode in [ChatMode::Plan, ChatMode::Edit, ChatMode::Auto] {
            assert!(operator.verify_execution_mode(mode).is_err());
        }
        assert!(operator.admitted_model_mode().is_err());
        let service = service(&fabric, &operator).await;
        assert!(
            operator
                .decide(&service, &tokio_util::sync::CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            operator
                .decide_resume(&service, &tokio_util::sync::CancellationToken::new())
                .await
                .is_err()
        );
        assert!(!operator.is_decided());
        service
            .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await;
    }

    /// Test-only access to the actual Chat issuer; no raw proof constructor.
    pub(crate) async fn recorded_plan_source(
        generation: &Arc<ConfigGeneration>,
        memory: Arc<dyn Memory>,
        input: &str,
    ) -> Arc<ChatTurnBinding> {
        recorded_approval_source(
            generation,
            memory,
            input,
            crate::agent::turn_contracts::ChatMode::Plan,
            "prx_plan_discovery_run",
            1,
        )
        .await
        .0
    }

    /// Actual durable Chat issuance for exact-call approval lifecycle tests.
    /// Mode is captured at ingress; the returned envelope comes from that binding.
    pub(crate) async fn recorded_approval_source(
        generation: &Arc<ConfigGeneration>,
        memory: Arc<dyn Memory>,
        input: &str,
        mode: crate::agent::turn_contracts::ChatMode,
        run: &str,
        turn: u64,
    ) -> (Arc<ChatTurnBinding>, RuntimeEnvelope) {
        let fabric = MemoryFabric::new(memory.clone(), generation.effective.workspace_dir.to_string_lossy())
            .with_event_recording(generation.effective.memory.event_recording_config());
        let session = super::super::session::ChatSession::new("ollama", "prx-local-model");
        let event = super::super::record_chat_user_message_event_in_generation(
            &fabric,
            &session,
            "prx_plan_discovery_session",
            run,
            "ollama",
            "prx-local-model",
            turn,
            input,
            Some(generation),
        )
        .await
        .unwrap();
        let receipt = ChatSourceReceipt::issue(
            memory.as_ref(),
            generation,
            "prx_plan_discovery_session",
            run,
            input,
            &event,
        )
        .await
        .unwrap();
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::ollama::OllamaProvider::new(None, None));
        let provider = ChatProviderBinding::capture(generation, &provider, "ollama", "prx-local-model")
            .unwrap()
            .unwrap();
        let binding = receipt
            .bind_provider(&provider, ChatAdmissionMode::Model(mode))
            .unwrap();
        let envelope = binding.envelope().clone();
        (binding, envelope)
    }

    pub(in crate::chat) fn install_fixture_terminal(
        binding: &ChatTurnBinding,
        owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
        fabric: MemoryFabric,
    ) {
        binding
            .install_terminal(
                owner.register(tokio_util::sync::CancellationToken::new()).unwrap(),
                fabric,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn enabled_fresh_plan_resume_without_approved_grant_is_rejected() {
        use crate::agent::turn_contracts::ChatMode;
        use crate::runtime::turn_source::TurnSourceProof;
        let (_dir, fabric, original) = fixture_with_plane(true, true).await;
        let provider =
            ChatProviderBinding::capture(&original.generation, &original.provider, "ollama", "prx-local-model")
                .unwrap()
                .unwrap();
        let source = ChatSourceReceipt {
            envelope: original.envelope.clone(),
            routing_input: Arc::clone(&original.routing_input),
        };
        let binding = source
            .bind_provider(&provider, ChatAdmissionMode::Model(ChatMode::Plan))
            .unwrap();
        let service = service(&fabric, &binding).await;
        assert!(
            binding
                .decide_resume(&service, &tokio_util::sync::CancellationToken::new())
                .await
                .is_err()
        );
        assert!(!binding.is_decided());
        assert!(binding.intercepts_writes());
        assert!(binding.verify_execution_mode(ChatMode::Edit).is_err());
        service
            .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn closed_input_cannot_issue_a_decision() {
        let (_dir, fabric, binding) = fixture(true).await;
        let service = service(&fabric, &binding).await;
        binding
            .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Failed, "failed")
            .await
            .unwrap();
        let result = binding
            .decide(&service, &tokio_util::sync::CancellationToken::new())
            .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("closed before decision issuance")
        );
        assert!(!binding.is_decided());
        service
            .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn decided_input_rejects_admission_failure_and_commits_redacted_reply() {
        let (dir, fabric, binding) = fixture(true).await;
        let service = service(&fabric, &binding).await;
        binding
            .decide(&service, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        assert!(
            binding
                .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Failed, "failed",)
                .await
                .is_err()
        );
        assert!(
            fabric
                .memory()
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed",)
                .await
                .unwrap()
                .is_none()
        );
        let route = crate::llm::route_decision::RouteDecision::single_candidate("ollama", "prx-local-model");
        let reply = format!("prx reply {}", crate::agent::terminal::persistence_probe::PROBE_SECRET);
        let receipt = super::super::finalize_chat_success_turn(
            Some(binding.as_ref()),
            &fabric,
            "prx_session",
            "prx_run",
            "ollama",
            "prx-local-model",
            binding.envelope().message_scope(),
            crate::llm::route_decision::ProviderExecutionOutcome::success_for_decision(&route, chrono::Utc::now()),
            Some(reply),
            1,
            "completed",
            &binding.generation.effective.cost,
            dir.path(),
        )
        .await
        .unwrap();
        assert!(receipt.assistant_event.unwrap().content.contains("[REDACTED]"));
        assert!(receipt.terminal_event.id > 0);
        crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(dir.path());
        service
            .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn admission_failure_retries_redact_and_reject_conflicting_status() {
        let (_dir, fabric, binding) = fixture(true).await;
        let summary = format!("failed {}", crate::agent::terminal::persistence_probe::PROBE_SECRET);
        for _ in 0..2 {
            binding
                .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Failed, &summary)
                .await
                .unwrap();
        }
        let memory = fabric.memory();
        let event = memory
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
            .await
            .unwrap()
            .unwrap();
        assert!(event.id > 0);
        assert!(
            !event
                .raw_payload_json
                .as_deref()
                .unwrap()
                .contains(crate::agent::terminal::persistence_probe::PROBE_SECRET,)
        );
        assert!(
            binding
                .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Cancelled, &summary,)
                .await
                .is_err()
        );
        assert!(
            memory
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:final",)
                .await
                .unwrap()
                .is_none()
        );
        assert!(*binding.admission_closed.lock().await);
        assert!(!binding.is_decided());
    }

    #[tokio::test]
    async fn admission_failure_rejects_a_backend_without_the_original_input() {
        let (_dir, fabric, binding) = fixture(true).await;
        let foreign = tempfile::tempdir().unwrap();
        let foreign_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(foreign.path()).unwrap());
        let foreign_fabric = MemoryFabric::new(foreign_memory.clone(), fabric.workspace_id());
        assert!(
            binding
                .record_predecision_failure(&foreign_fabric, "prx_run", TurnTerminalStatus::Failed, "failed",)
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
        // A failed persistence attempt closes admission but allows a safe retry.
        binding
            .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Failed, "failed")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn admission_failure_respects_disabled_user_recording() {
        let (_dir, fabric, binding) = fixture(false).await;
        binding
            .record_predecision_failure(&fabric, "prx_run", TurnTerminalStatus::Cancelled, "cancelled")
            .await
            .unwrap();
        assert!(*binding.admission_closed.lock().await);
        assert!(
            fabric
                .memory()
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed",)
                .await
                .unwrap()
                .is_none()
        );
    }
}
