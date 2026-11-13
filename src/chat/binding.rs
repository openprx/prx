//! Chat turn binding: chat-owned provenance for an actual recorded inbound turn. This receipt
//! establishes source lineage only; it does not authorize a model or tool.

use crate::config::ConfigGeneration;
use crate::memory::{Memory, MessageEvent, MessageEventSource};
use crate::runtime::envelope::RuntimeEnvelope;
use anyhow::Context;
use std::sync::Arc;

/// Resume admission stays opaque: its sources may contain private identifiers.
/// An inbound persistence failure is different. The operator needs the cause,
/// so the user-facing line keeps a public cause and recovery identifiers while
/// full store paths stay in the redacted log chain.
pub(super) fn chat_input_persistence_failure(error: &anyhow::Error) -> anyhow::Error {
    let cause = safe_persistence_cause(error);
    let diagnostic = error.downcast_ref::<crate::runtime::ingress_recovery::IngressRecoveryDiagnostic>();
    let decision_store = diagnostic
        .and_then(|context| context.decision_store.file_name())
        .and_then(std::ffi::OsStr::to_str);
    let run_id = diagnostic.map(|context| {
        let run = context.run_id.as_str();
        if persistence_cause_is_public(run) {
            run
        } else {
            "[redacted]"
        }
    });
    let detail = crate::security::redaction::redact_provider_text(&format!("{error:#}"));
    tracing::error!(store = "brain.db", decision_store, run_id, error = %detail, cause = %cause,
        "chat input persistence failed; restart to retry ingress recovery");
    let identifiers = match (decision_store, run_id) {
        (Some(store), Some(run)) => format!("; decision_store={store}; run_id={run}"),
        _ => String::new(),
    };
    anyhow::anyhow!(
        "chat input persistence failed (brain.db{identifiers}): {cause}. Restart to retry ingress recovery; if it persists, inspect the store error in chat.log."
    )
}

fn safe_persistence_cause(error: &anyhow::Error) -> String {
    let mut selected = None;
    for cause in error.chain() {
        if let Some(rusqlite::Error::SqliteFailure(_, Some(detail))) = cause.downcast_ref::<rusqlite::Error>()
            && persistence_cause_is_public(detail)
        {
            return detail.clone();
        }
        let text = cause.to_string();
        if persistence_cause_is_public(&text) {
            selected = Some(text);
        }
    }
    selected.unwrap_or_else(|| "inbound write was rejected".to_owned())
}

fn persistence_cause_is_public(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    !text.is_empty()
        && text.len() <= 180
        && !text.contains('/')
        && !text.contains('\\')
        && !text.contains('@')
        && !lower.contains("token")
        && !lower.contains("secret")
        && !lower.contains("password")
        && !lower.contains("authorization")
        && !lower.contains("api_key")
        && text
            .chars()
            .all(|character| character.is_ascii() && !character.is_control())
}

/// Only typed infrastructure failures receive retry guidance. Opaque approval
/// failures include owner/scope/consumption checks and retain the same refusal.
/// Never render an error's text: its sources may contain private identifiers.
pub(super) fn resume_admission_failure_reply(error: &anyhow::Error) -> &'static str {
    use crate::runtime::turn_decision::AgentDecisionError;
    let infrastructure = error.chain().any(|cause| {
        cause.is::<std::io::Error>()
            || cause.is::<rusqlite::Error>()
            || cause.is::<reqwest::Error>()
            || matches!(
                cause.downcast_ref::<AgentDecisionError>(),
                Some(
                    AgentDecisionError::RecordWriterUnavailable
                        | AgentDecisionError::DelegationStorage(_)
                        | AgentDecisionError::IngressRecovery(_)
                )
            )
    });
    if infrastructure {
        "Decision resume failed before execution. You can retry this request."
    } else {
        "Decision resume could not be admitted for this chat. No operation was run by this request."
    }
}

/// Selection identity paired with the concrete provider instance. Only Chat's
/// construction/switch path publishes it, before updating either UI slot.
///
/// Generation lifetime: an interactive chat process pins the configuration
/// generation it started with (`ctx.config_generation`) for every turn and
/// every `/model` switch; it is not re-pinned per turn. A configuration change
/// made while the chat is open therefore applies to the next chat process (or
/// to children/gateway work admitted under a newer generation), never
/// half-way through a conversation, so a turn's provider, price and credential
/// origin stay one coherent snapshot. `verify` refuses a binding whose
/// generation differs from the one in use.
#[derive(Clone)]
pub(super) struct ChatProviderBinding {
    provider: Arc<dyn crate::providers::Provider>,
    generation: Arc<ConfigGeneration>,
    choice: crate::agent::entry::AgentModelChoice,
}

impl ChatProviderBinding {
    pub(super) fn effective_compaction(&self) -> crate::providers::context_window::EffectiveCompactionConfig {
        let config = &self.generation.effective;
        crate::providers::context_window::resolve_effective_compaction_config_with_catalog(
            &config.agent.compaction,
            self.choice.provider_name(),
            self.choice.model_name(),
            crate::providers::context_window::ContextWindowSources::from_config(config),
            &config.model_routes,
            self.choice.catalog(),
        )
    }

    pub(super) async fn record_guarded(
        &self,
        fabric: crate::memory::MemoryFabric,
        source: (crate::memory::MessageEventScope, &str, ChatAdmissionMode, bool),
        input: &str,
        key: String,
        ticket: crate::runtime::turn_cleanup::CleanupTicket,
    ) -> anyhow::Result<(Option<MessageEvent>, Arc<ChatTurnBinding>, ChatAdmissionGuard)> {
        self.record_guarded_with_origin(fabric, source, input, key, ticket, None)
            .await
    }

    pub(super) async fn record_guarded_with_origin(
        &self,
        fabric: crate::memory::MemoryFabric,
        source: (crate::memory::MessageEventScope, &str, ChatAdmissionMode, bool),
        input: &str,
        key: String,
        ticket: crate::runtime::turn_cleanup::CleanupTicket,
        origin: Option<crate::runtime::h2_entry::TerminalInputProof>,
    ) -> anyhow::Result<(Option<MessageEvent>, Arc<ChatTurnBinding>, ChatAdmissionGuard)> {
        let fabric = fabric
            .with_ingress_recovery(&self.generation.effective, &ticket)
            .await
            .map_err(|error| chat_input_persistence_failure(&error))?;
        let (scope, session, admission_mode, has_conversation_history) = source;
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
                let origin = &origin;
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
                    let source =
                        receipt.bind_provider_with_history(binding, admission_mode, has_conversation_history)?;
                    if let Some(origin) = origin {
                        source
                            .h2_origin
                            .set(origin.bind(source.as_ref())?)
                            .map_err(|_| anyhow::anyhow!("chat input origin was already bound"))?;
                    }
                    Ok::<_, anyhow::Error>((event, source))
                }
            };
            let operation = async {
                let event = match fabric
                    .record_inbound_user_message(
                        scope.clone(),
                        super::sanitize::sanitize_for_persistence(&input),
                        Some(key.clone()),
                        None,
                    )
                    .await
                {
                    Ok(event) => Some(event),
                    Err(error) => return Err(chat_input_persistence_failure(&error)),
                };
                issue(event).await
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), operation)
                .await
                .map_err(|_| chat_input_persistence_failure(&anyhow::anyhow!("inbound write timed out")))
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
                    (
                        envelope.message_scope(),
                        session,
                        ChatAdmissionMode::OperatorCommand,
                        false,
                    ),
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
    h2_origin: std::sync::OnceLock<Arc<crate::runtime::h2_entry::VerifiedEntryOrigin>>,
    pub(super) admission_mode: ChatAdmissionMode,
    terminal: std::sync::OnceLock<(
        crate::runtime::turn_terminal::TurnTerminalGuard,
        Arc<crate::agent::turn_contracts::RetainedProviderTrace>,
    )>,
    returned_trace: std::sync::OnceLock<CapturedChatTrace>,
    pub(super) decision: tokio::sync::OnceCell<ChatDecision>,
    // Serializes decision issuance with pre-decision closure. Once closure
    // starts, even a failed write must not reopen this input for execution.
    pub(super) admission_closed: tokio::sync::Mutex<bool>,
    choice: crate::agent::entry::AgentModelChoice,
    owner_id: String,
    memory_scope: crate::memory::principal::MemoryWriteContext,
    generation: Arc<ConfigGeneration>,
    provider: Arc<dyn crate::providers::Provider>,
    model: String,
    envelope: RuntimeEnvelope,
    routing_input: Arc<str>,
    has_conversation_history: bool,
}

#[cfg(not(feature = "terminal-tui"))]
type CapturedChatTrace = ();

#[cfg(feature = "terminal-tui")]
struct CapturedChatTrace {
    trace: crate::agent::turn_contracts::ToolLoopTrace,
    finished_at: chrono::DateTime<chrono::Utc>,
}

pub(super) struct ChatDecision {
    pub(super) advice: Option<crate::decision_engine::IntentAnalysis>,
    pub(super) resume: bool,
    pub(super) service: Arc<crate::runtime::turn_decision::TurnDecisionService>,
    pub(super) context: Arc<crate::runtime::turn_decision::TurnDecisionContext>,
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
    pub(super) fn pending_decision_id(&self) -> Option<crate::security::decision_id::DecisionId> {
        self.decision.get()?.context.pending_approval_id()
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

    pub(super) async fn rejoin_recovery_terminal(&self, terminal_id: &str) -> anyhow::Result<()> {
        if let Some(decided) = self.decision.get() {
            anyhow::ensure!(
                self.envelope.run_id.as_deref() == Some(terminal_id),
                "chat terminal id differs from its source"
            );
            if self.settlement_ref(terminal_id)?.is_some_and(|link| link.is_recorded()) {
                decided
                    .service
                    .runtime_holder()
                    .recovery_scope()
                    .rejoin_terminal(terminal_id, &self.owner_id)
                    .await?;
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

    /// Recall from the authenticated binding, even before a decision exists.
    pub(super) async fn recalled_context(
        &self,
        memory: &dyn Memory,
    ) -> crate::agent::context_recall::RecalledMemoryContext {
        crate::agent::context_recall::scoped_recall(
            memory,
            &crate::agent::context_recall::RecallScope::from_envelope(
                &self.envelope,
                &self.generation.effective.memory,
                "private",
            ),
            &self.routing_input,
            crate::agent::context_recall::RECALL_CONTEXT_BUDGET,
        )
        .await
    }

    pub(super) fn main_compaction(&self) -> Option<&crate::config::AgentCompactionConfig> {
        self.decision.get().map(|decided| decided.context.main_compaction())
    }

    pub(super) fn effective_compaction(
        &self,
    ) -> anyhow::Result<crate::providers::context_window::EffectiveCompactionConfig> {
        let decided = self
            .decision
            .get()
            .context("chat compaction requires an admitted decision")?;
        Ok(decided
            .service
            .effective_compaction(self.choice.provider_name(), self.choice.model_name()))
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
        use crate::runtime::turn_source::TurnSourceProof;
        let decided = self.decision.get().context("chat resume has no decision")?;
        anyhow::ensure!(decided.resume, "chat turn is not an explicit resume");
        let execution = crate::tools::surface_owner::model_turn_context(
            self.envelope.clone(),
            "private",
            crate::tools::surface_owner::SurfaceParts {
                execution: decided.service.execution(),
                catalog: decided.service.catalog(),
                publication_policy: decided.service.publication_policy(),
                tiering: decided.service.tool_tiering(),
                security: decided.service.security(),
                pinned_model: self.model_binding().pinned_model(),
                channel: decided.service.channel_surface(),
                inherited_exclusions: &[],
                sender: self.policy_sender(),
                channel_name: self.policy_channel(),
                chat_type: self.policy_chat_type(),
                tool_calls_allowed: decided.service.model_can_call_tools(self),
            },
            crate::agent::send_admission::call_token_budget(&decided.service, &decided.context, self)?,
        );
        let outcome = decided
            .service
            .execute_agent_resume(self, &decided.context, &execution, cancellation)
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

    /// Only the verified h2 ingress opts foreground chat into durable review.
    pub(super) fn h2_approval_ledger(
        &self,
    ) -> Option<crate::security::approval_ledger::strategy::ExecutionApprovalLedger> {
        self.h2_origin.get()?;
        self.decision.get()?.service.execution_approval_ledger()
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
        self.bind_provider_with_history(binding, admission_mode, false)
    }

    fn bind_provider_with_history(
        &self,
        binding: &ChatProviderBinding,
        admission_mode: ChatAdmissionMode,
        has_conversation_history: bool,
    ) -> anyhow::Result<Arc<ChatTurnBinding>> {
        anyhow::ensure!(
            self.envelope.config_generation_id == Some(binding.generation.id.0)
                && self.envelope.config_source_revision.as_deref()
                    == Some(binding.generation.source_revision.fingerprint_sha256.as_str()),
            "chat source and provider generations differ"
        );
        Ok(Arc::new(ChatTurnBinding {
            h2_origin: std::sync::OnceLock::new(),
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
            has_conversation_history,
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
    fn h2_entry_origin(&self) -> Option<&Arc<crate::runtime::h2_entry::VerifiedEntryOrigin>> {
        self.h2_origin.get()
    }

    fn owner_isolated(&self) -> bool {
        self.envelope.owner_isolated
    }
    fn approval_review_adapter(&self) -> Option<crate::security::approval_ledger::ApprovalReviewAdapter> {
        self.envelope
            .source_message_event_id
            .as_ref()
            .map(|_| crate::security::approval_ledger::ApprovalReviewAdapter::ChatOwnerReview)
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
    fn has_conversation_history(&self) -> bool {
        self.has_conversation_history
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
    fn surface(&self) -> crate::tools::capability::turn_state::TurnSurfaceKind {
        crate::tools::capability::turn_state::TurnSurfaceKind::Interactive
    }
    fn policy_channel(&self) -> &str {
        "terminal"
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(feature = "test-mock")]
pub(crate) mod mailbox_fixture;
