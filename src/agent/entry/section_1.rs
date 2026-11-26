
use crate::agent::context_recall::{current_input_memory_key, increment_recalled_useful_counts};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::runtime_prompt::build_runtime_system_prompt;
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_engine::run_turn;
use crate::agent::turn_request::ToolLoopMemory;
use crate::approval::ApprovalManager;
use crate::config::Config;
use crate::hooks::HookEvent;
use crate::memory::{Memory, MemoryCategory, MemoryFabric, MessageEventScope};
use crate::observability::ObserverEvent;
use crate::providers;
use crate::providers::Provider;
use crate::runtime::envelope::{RuntimeEnvelope, RuntimeSource};
use crate::tools::Tool;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ── Internal single-shot engine ──────────────────────────────────────────
// Wires up all subsystems (observer, runtime, security, memory, tools,
// provider, hardware RAG) and runs one turn for cron, xin, skill evaluation
// and controlled cases.

tokio::task_local! {
    static QUIET_SINGLE_SHOT: bool;
}

/// Internal evaluations return their answer to the caller without mixing it
/// into the operator command's machine-readable stdout.
pub(crate) async fn run_quiet(
    config: Config,
    message: String,
    temperature: f64,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<String> {
    QUIET_SINGLE_SHOT
        .scope(
            true,
            Box::pin(run_with_runtime_envelope(
                config,
                message,
                None,
                None,
                temperature,
                shutdown,
                None,
                None,
            )),
        )
        .await
}

/// A controlled case carries its native filesystem capability explicitly
/// through bootstrap, never in Config or model input.
pub(crate) async fn run_quiet_controlled(
    config: Config,
    message: String,
    temperature: f64,
    shutdown: CancellationToken,
    scope: Arc<crate::runtime::h2_entry::ControlledCaseScope>,
) -> anyhow::Result<String> {
    QUIET_SINGLE_SHOT
        .scope(
            true,
            Box::pin(run_with_runtime_envelope(
                crate::runtime::bootstrap::AgentRuntimeConfig::ControlledCase {
                    config: Box::new(config),
                    scope,
                },
                message,
                None,
                None,
                temperature,
                shutdown,
                None,
                None,
            )),
        )
        .await
}

/// Persist a live CLI or chat input under its per-run semantic identity.
pub(crate) async fn save_current_input(
    fabric: &crate::memory::MemoryFabric,
    run: &str,
    session_key: &str,
    input: &str,
    event: Option<&str>,
    partition: Option<crate::memory::AgentPartition>,
) -> anyhow::Result<()> {
    let key = current_input_memory_key(run);
    fabric
        .record_semantic_memory_from_event(
            &key,
            input,
            MemoryCategory::Conversation,
            Some(session_key),
            event,
            None,
            None,
            partition,
        )
        .await
}

/// Build one CLI turn's memory handles from the turn's own, user-authored text.
///
/// Every single-shot turn needs these handles (cron jobs, xin tasks, skill
/// evaluation and controlled cases all enter through
/// `run_with_runtime_envelope`). One builder keeps the routing pin from going
/// missing in a hand-copied chain.
///
/// Routing is supplied separately by the source-bound decision context.
fn cli_turn_memory_runtime(
    mem: &Arc<dyn Memory>,
    workspace_dir: &Path,
    runtime_envelope: &RuntimeEnvelope,
    memory_fabric: &MemoryFabric,
    fabric_scope: &MessageEventScope,
    user_message_event_id: Option<&str>,
) -> ToolLoopMemory {
    ToolLoopMemory::new(
        mem,
        workspace_dir,
        Some(
            DocumentIngestRuntime::from_envelope(mem.clone(), runtime_envelope)
                .with_source_message_event_id(user_message_event_id.map(ToString::to_string)),
        ),
    )
    .with_event_fabric(memory_fabric.clone())
    .with_request_event_scope({
        let mut scope = fabric_scope.clone();
        scope.causation_event_id = user_message_event_id.map(ToString::to_string);
        scope
    })
}

/// Preserve the CLI actor and chat scope while applying the bootstrap's
/// configured security policy and workspace. Scheduled and externally scoped turns leave
/// this adapter absent so their creator-owned scope retains priority.
fn configured_cli_tool_adapter(
    security: &Arc<crate::security::SecurityPolicy>,
    registry: &Arc<Vec<Box<dyn Tool>>>,
    approval: &Arc<ApprovalManager>,
    memory_runtime: &ToolLoopMemory,
) -> crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
    crate::agent::execution_policy::build_agent_tool_runtime_adapter(
        crate::agent::execution_policy::AgentToolRuntimeInputs {
            scope: None,
            channel_name: "cli",
            tools_registry: Arc::clone(registry),
            approval: Some(Arc::clone(approval)),
            approval_resolver: None,
            cancellation: None,
            ledger: memory_runtime.ledger.clone(),
            unscoped_policy: Some(Arc::clone(security)),
        },
    )
}

fn terminal_binding_snapshot(
    binding: &parking_lot::Mutex<Option<crate::agent::terminal::TerminalBinding>>,
) -> Option<crate::agent::terminal::TerminalBinding> {
    binding.lock().clone()
}

/// A CLI Agent turn whose origin was checked by this entrypoint after inbound
/// event recording was attempted. Fields stay private; the decision service
/// reads them but cannot manufacture an authenticated source from a public
/// `RuntimeEnvelope`.
pub(crate) struct AgentTurnProof {
    workspace_id: Arc<str>,
    owner_id: Arc<str>,
    session_key: Arc<str>,
    run_id: Arc<str>,
    source_event_id: Option<Arc<str>>,
    routing_input: Arc<str>,
    memory_scope: crate::memory::principal::MemoryWriteContext,
    retrieval_principal: crate::memory::MemoryPrincipal,
    provider_name: Arc<str>,
    model_name: Arc<str>,
    model_binding: AgentModelBinding,
    config_generation_id: u64,
    config_source_revision: Arc<str>,
    scheduled: Option<Box<super::scheduled_source::ScheduledInput>>,
}

/// Cleanup ownership starts before CLI durable input persistence. This is
/// separate from the source proof: neither a pending write nor its key grants
/// permission to execute a model or tool.
struct CliPendingInput {
    fabric: MemoryFabric,
    generation: Arc<crate::config::ConfigGeneration>,
    envelope: RuntimeEnvelope,
    expected_session_key: String,
    scope: crate::memory::MessageEventScope,
    choice: AgentModelChoice,
    input: String,
    key: String,
    ticket: crate::runtime::turn_cleanup::CleanupTicket,
}

struct CliAdmissionGuard {
    proof: Option<AgentTurnProof>,
    fabric: MemoryFabric,
    config: Arc<Config>,
    ticket: Option<crate::runtime::turn_cleanup::CleanupTicket>,
    pending: Option<(bool, String)>,
}

impl CliPendingInput {
    async fn issue(&self, event: Option<&crate::memory::MessageEvent>) -> Result<AgentTurnProof> {
        let run = self
            .envelope
            .run_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("CLI input run is missing"))?;
        let proof = issue_cli_agent_turn_proof_for_session(
            None,
            &self.envelope,
            &self.generation,
            (run, &self.expected_session_key),
            &self.input,
            event,
            &self.choice,
        )?;
        if let Some(event) = event.filter(|event| event.id > 0) {
            let mut expected = self.scope.clone();
            for label in [&mut expected.sender, &mut expected.recipient].into_iter().flatten() {
                if crate::providers::byo_dialect_of(label).is_some() {
                    *label = crate::providers::redact_endpoint_userinfo(label).into_owned();
                }
            }
            anyhow::ensure!(
                event.idempotency_key.as_deref() == Some(self.key.as_str())
                    && event.source == expected.source
                    && event.channel == expected.channel
                    && event.sender == expected.sender
                    && event.recipient == expected.recipient
                    && event.owner_id == expected.owner_id
                    && event.visibility == expected.visibility
                    && event.goal_id == expected.goal_id
                    && event.correlation_id == expected.correlation_id
                    && event.subject == expected.subject
                    && event.causation_event_id == expected.causation_event_id,
                "CLI input receipt attribution differs from its issued scope"
            );
            let stored = self
                .fabric
                .memory()
                .find_owned_message_event(
                    &self.envelope.workspace_id,
                    &self.envelope.resolved_owner_id(),
                    &event.event_id,
                )
                .await?
                .ok_or_else(|| anyhow::anyhow!("CLI input receipt is not durable"))?;
            anyhow::ensure!(
                serde_json::to_value(stored)? == serde_json::to_value(event)?,
                "CLI input receipt differs from its durable source"
            );
        }
        Ok(proof)
    }

    async fn record(mut self) -> Result<(Option<crate::memory::MessageEvent>, CliAdmissionGuard)> {
        self.fabric = self
            .fabric
            .with_ingress_recovery(&self.generation.effective, &self.ticket)
            .await?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let experiment_stage = crate::runtime::experiment_stage::current();
        tokio::spawn(async move {
            let operation = crate::runtime::experiment_stage::retain(experiment_stage, async {
                let event = self
                    .fabric
                    .record_inbound_user_message_best_effort(
                        self.scope.clone(),
                        self.input.clone(),
                        Some(self.key.clone()),
                        "CLI",
                    )
                    .await?;
                let proof = self.issue(event.as_ref()).await?;
                Ok::<_, anyhow::Error>((event, proof))
            });
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), operation)
                .await
                .map_err(|_| anyhow::anyhow!("CLI input persistence timed out"))
                .and_then(std::convert::identity);
            let result = match result {
                Ok((event, proof)) => Ok((
                    event,
                    CliAdmissionGuard {
                        proof: Some(proof),
                        fabric: self.fabric,
                        config: Arc::clone(&self.generation.effective),
                        ticket: Some(self.ticket),
                        pending: None,
                    },
                )),
                Err(_error) if self.fabric.ingress_already_accepted() => {
                    self.ticket.finish(true);
                    Err(crate::runtime::ingress_recovery::AlreadyAccepted.into())
                }
                Err(error) => {
                    let recovery = async {
                        let event = self
                            .fabric
                            .memory()
                            .find_owned_message_event_by_idempotency_key(
                                &self.envelope.workspace_id,
                                &self.envelope.resolved_owner_id(),
                                &self.key,
                            )
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("CLI input commit is unconfirmed"))?;
                        self.issue(Some(&event)).await
                    };
                    match tokio::time::timeout(std::time::Duration::from_secs(15), recovery).await {
                        Ok(Ok(proof)) => {
                            let mut guard = CliAdmissionGuard {
                                proof: Some(proof),
                                fabric: self.fabric,
                                config: Arc::clone(&self.generation.effective),
                                ticket: Some(self.ticket),
                                pending: None,
                            };
                            let _ = tokio::time::timeout(
                                std::time::Duration::from_secs(15),
                                guard.finish_failure(sender.is_closed(), "CLI input admission failed"),
                            )
                            .await;
                        }
                        _ => self.ticket.finish(false),
                    }
                    Err(error)
                }
            };
            let _ = sender.send(result);
        });
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("CLI input owner ended without a receipt"))?
    }
}

impl CliAdmissionGuard {
    /// Transfer the same lifecycle ticket to a real terminal owner without
    /// an await or an interval where cancellation has no cleanup owner.
    fn into_terminal_ticket(mut self) -> Result<crate::runtime::turn_cleanup::CleanupTicket> {
        anyhow::ensure!(self.pending.is_none(), "CLI admission failure is already pending");
        let ticket = self
            .ticket
            .take()
            .ok_or_else(|| anyhow::anyhow!("CLI admission cleanup ticket missing"))?;
        self.proof = None;
        Ok(ticket)
    }

    async fn finish_failure(&mut self, cancelled: bool, summary: &str) -> Result<()> {
        anyhow::ensure!(self.pending.is_none(), "CLI admission failure is already pending");
        self.pending = Some((cancelled, crate::security::redaction::redact_secrets(summary)));
        let (cancelled, summary) = self
            .pending
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("CLI failure missing"))?;
        let result = self
            .proof
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("CLI source proof missing"))?
            .record_cli_admission_failure(&self.fabric, &self.config, *cancelled, summary)
            .await;
        self.proof = None;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(result.is_ok());
        }
        result
    }
}

impl Drop for CliAdmissionGuard {
    fn drop(&mut self) {
        let Some(proof) = self.proof.take() else {
            return;
        };
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let (cancelled, summary) = self
            .pending
            .take()
            .unwrap_or_else(|| (true, "CLI input admission task was dropped".to_owned()));
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            ticket.finish(false);
            return;
        };
        let fabric = self.fabric.clone();
        let config = Arc::clone(&self.config);
        runtime.spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                proof.record_cli_admission_failure(&fabric, &config, cancelled, &summary),
            )
            .await;
            ticket.finish(matches!(result, Ok(Ok(()))));
        });
    }
}

impl AgentTurnProof {
    async fn record_cli_admission_failure(
        &self,
        fabric: &MemoryFabric,
        config: &Config,
        cancelled: bool,
        summary: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            self.scheduled.is_none(),
            "scheduled admission retains its own failure owner"
        );
        anyhow::ensure!(
            fabric.workspace_id() == self.workspace_id.as_ref()
                && config.workspace_dir.to_string_lossy() == self.workspace_id.as_ref(),
            "CLI admission failure workspace changed"
        );
        let Some(event_id) = self.source_event_id.as_deref() else {
            anyhow::ensure!(
                !config.memory.should_record_user_message_event(),
                "CLI admission has no durable input"
            );
            return Ok(());
        };
        let input = fabric
            .memory()
            .find_owned_message_event(&self.workspace_id, &self.owner_id, event_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("CLI admission input is missing"))?;
        anyhow::ensure!(
            input.id > 0
                && input.event_id == event_id
                && input.workspace_id == self.workspace_id.as_ref()
                && input.owner_id.as_deref() == Some(self.owner_id.as_ref())
                && input.session_key.as_deref() == Some(self.session_key.as_ref())
                && input.run_id.as_deref() == Some(self.run_id.as_ref())
                && input.config_generation_id == Some(self.config_generation_id)
                && input.config_source_revision.as_deref() == Some(self.config_source_revision.as_ref())
                && input.role == "user"
                && input.event_type == "message.created"
                && input.parent_run_id.is_none()
                && input.parent_session_key.is_none()
                && input.agent_id.is_none()
                && input.persona_id.is_none()
                && input.subject.is_none()
                && input.attempt_id.is_none()
                && input.lease_epoch.is_none()
                && crate::runtime::turn_source::recorded_input_matches(self.surface(), &input, &self.routing_input),
            "CLI admission input differs from its source proof"
        );
        let mut envelope = RuntimeEnvelope::agent(self.workspace_id.to_string(), self.run_id.to_string())
            .with_source_message_event_id(event_id);
        envelope.session_key = self.session_key.to_string();
        let mut scope = envelope.message_scope();
        scope.config_generation_id = Some(self.config_generation_id);
        scope.config_source_revision = Some(self.config_source_revision.to_string());
        let summary = crate::security::redaction::redact_secrets(summary);
        let payload = serde_json::json!({
            "run_id": self.run_id.as_ref(), "source_event_id": event_id,
            "status": if cancelled { "cancelled" } else { "failed" }, "summary": summary,
        });
        let expected_content = format!("turn.admission.failed {summary}");
        let event = crate::runtime::ingress_recovery::commit_admission_failure(
            fabric,
            scope.clone(),
            &summary,
            serde_json::to_string(&payload)?,
            format!("turn:{}:admission-failed", self.run_id),
        )
        .await?;
        anyhow::ensure!(
            event.content == expected_content
                && event.idempotency_key.as_deref() == Some(format!("turn:{}:admission-failed", self.run_id).as_str())
                && event.id > 0
                && !event.event_id.is_empty()
                && event.workspace_id == fabric.workspace_id()
                && event.owner_id == scope.owner_id
                && event.source == scope.source
                && event.channel == scope.channel
                && event.sender == scope.sender
                && event.session_key == scope.session_key
                && event.run_id == scope.run_id
                && event.causation_event_id == scope.causation_event_id
                && event.recipient == scope.recipient
                && event.parent_run_id == scope.parent_run_id
                && event.parent_session_key == scope.parent_session_key
                && event.subject == scope.subject
                && event.agent_id == scope.agent_id
                && event.persona_id == scope.persona_id
                && event.attempt_id == scope.attempt_id
                && event.lease_epoch == scope.lease_epoch
                && event.config_generation_id == scope.config_generation_id
                && event.config_source_revision == scope.config_source_revision
                && event.visibility == scope.visibility
                && event.role == "event"
                && event.event_type == "turn.admission.failed"
                && event
                    .raw_payload_json
                    .as_deref()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .transpose()?
                    .as_ref()
                    == Some(&payload),
            "CLI admission failure retry conflicts with source or status"
        );
        let durable = fabric
            .memory()
            .find_owned_message_event(fabric.workspace_id(), self.owner_id.as_ref(), &event.event_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("admission failure receipt is not durable"))?;
        anyhow::ensure!(
            serde_json::to_value(durable)? == serde_json::to_value(&event)?,
            "admission failure receipt differs from durable event"
        );
        Ok(())
    }

    pub(crate) fn owner_isolated(&self) -> bool {
        self.scheduled
            .as_ref()
            .map_or(self.retrieval_principal.require_owner_match, |source| {
                source.envelope.owner_isolated
            })
    }

    pub(crate) fn retrieval_principal(&self) -> crate::memory::MemoryPrincipal {
        self.scheduled.as_ref().map_or_else(
            || self.retrieval_principal.clone(),
            |source| source.envelope.memory_principal(),
        )
    }

    pub(crate) fn task_id(&self) -> Option<&str> {
        self.scheduled
            .as_ref()
            .and_then(|source| source.envelope.task_id.as_deref())
    }
    pub(crate) fn surface(&self) -> crate::tools::capability::turn_state::TurnSurfaceKind {
        use crate::tools::capability::turn_state::TurnSurfaceKind;
        match self.scheduled.as_ref().map(|source| source.envelope.source) {
            Some(RuntimeSource::Cron) => TurnSurfaceKind::Cron,
            Some(RuntimeSource::Xin) => TurnSurfaceKind::Xin,
            _ => TurnSurfaceKind::AgentLoop,
        }
    }
    pub(crate) fn policy_channel(&self) -> &str {
        self.scheduled
            .as_ref()
            .and_then(|source| source.execution.envelope.channel.as_deref())
            .unwrap_or("cli")
    }
    pub(crate) fn policy_sender(&self) -> &str {
        self.scheduled
            .as_ref()
            .and_then(|source| source.execution.envelope.sender.as_deref())
            .unwrap_or("local-user")
    }
    pub(crate) fn policy_chat_type(&self) -> &str {
        self.scheduled
            .as_ref()
            .map_or("private", |source| source.execution.chat_type.as_str())
    }
    pub(crate) fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub(crate) fn owner_id(&self) -> &str {
        &self.owner_id
    }

    pub(crate) fn session_key(&self) -> &str {
        &self.session_key
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) fn source_event_id(&self) -> Option<&str> {
        self.source_event_id.as_deref()
    }

    pub(crate) fn routing_input(&self) -> &str {
        &self.routing_input
    }

    pub(crate) const fn memory_scope(&self) -> &crate::memory::principal::MemoryWriteContext {
        &self.memory_scope
    }

    pub(crate) fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub(crate) fn model_name(&self) -> &str {
        &self.model_name
    }

    pub(crate) const fn model_binding(&self) -> &AgentModelBinding {
        &self.model_binding
    }

    pub(crate) const fn config_generation_id(&self) -> u64 {
        self.config_generation_id
    }

    pub(crate) fn config_source_revision(&self) -> &str {
        &self.config_source_revision
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum AgentTurnOriginError {
    #[error("the CLI Agent turn does not match its authenticated runtime origin ({0})")]
    Mismatch(&'static str),
}

/// Only this entrypoint may issue a CLI Agent origin. Scheduled overrides are
/// separate surfaces and cannot inherit the local operator's authority.
fn issue_cli_agent_turn_proof(
    runtime_envelope_override: Option<&RuntimeEnvelope>,
    envelope: &RuntimeEnvelope,
    generation: &crate::config::ConfigGeneration,
    expected_run_id: &str,
    routing_input: &str,
    recorded_event: Option<&crate::memory::MessageEvent>,
    choice: &AgentModelChoice,
) -> Result<AgentTurnProof, AgentTurnOriginError> {
    issue_cli_agent_turn_proof_for_session(
        runtime_envelope_override,
        envelope,
        generation,
        (expected_run_id, &format!("agent:{expected_run_id}")),
        routing_input,
        recorded_event,
        choice,
    )
}

// The expected session is minted at the CLI entry, independently of the envelope
// being validated. Run IDs remain per-turn.
fn issue_cli_agent_turn_proof_for_session(
    runtime_envelope_override: Option<&RuntimeEnvelope>,
    envelope: &RuntimeEnvelope,
    generation: &crate::config::ConfigGeneration,
    (expected_run_id, expected_session_key): (&str, &str),
    routing_input: &str,
    recorded_event: Option<&crate::memory::MessageEvent>,
    choice: &AgentModelChoice,
) -> Result<AgentTurnProof, AgentTurnOriginError> {
    let run_id = envelope
        .run_id
        .as_deref()
        .ok_or(AgentTurnOriginError::Mismatch("missing_run_id"))?;
    let workspace_id = generation.effective.workspace_dir.to_string_lossy();
    let owner_id = crate::memory::principal::OwnerPrincipal::new(
        workspace_id.as_ref(),
        "cli",
        "local-user",
        &envelope.session_key,
        vec![crate::memory::principal::Role::Anonymous],
    )
    .owner_id;
    if runtime_envelope_override.is_some()
        || envelope.source != RuntimeSource::Agent
        || envelope.workspace_id != workspace_id
        || envelope.channel.as_deref() != Some("cli")
        || envelope.sender.as_deref() != Some("local-user")
        || envelope.owner_id.is_some()
        || envelope.owner_isolated
        || envelope.legacy_session_key.is_some()
        || envelope.topic_id.is_some()
        || envelope.task_id.is_some()
        || envelope.source_message_event_id.is_some()
        || envelope.parent_run_id.is_some()
        || envelope.attempt_id.is_some()
        || envelope.lease_epoch.is_some()
        || envelope.authority_guard.is_some()
        || envelope.agent_id.is_some()
        || envelope.persona_id.is_some()
        || envelope
            .recipient
            .as_deref()
            .is_some_and(|recipient| recipient != "cli:local-user")
        || envelope.visibility != crate::memory::MemoryVisibility::Session
        || run_id != expected_run_id
        || envelope.session_key != expected_session_key
        || envelope.config_generation_id != Some(generation.id.0)
        || envelope.config_source_revision.as_deref() != Some(&generation.source_revision.fingerprint_sha256)
    {
        return Err(AgentTurnOriginError::Mismatch("envelope"));
    }
    if !choice.factory_credential_source_matches(&generation.effective) {
        return Err(AgentTurnOriginError::Mismatch("credential_source"));
    }
    let recorded_event_id = match recorded_event {
        Some(event) if event.id > 0 => {
            if event.event_id.trim().is_empty()
                || event.workspace_id != workspace_id
                || event.owner_id.as_deref() != Some(owner_id.as_str())
                || event.source != crate::memory::MessageEventSource::Agent
                || event.channel.as_deref() != Some("cli")
                || event.sender.as_deref() != Some("local-user")
                || event.session_key.as_deref() != Some(envelope.session_key.as_str())
                || event.parent_session_key.is_some()
                || event.run_id.as_deref() != Some(run_id)
                || event.parent_run_id.is_some()
                || event.agent_id.is_some()
                || event.persona_id.is_some()
                || event.attempt_id.is_some()
                || event.lease_epoch.is_some()
                || event.role != "user"
                || event.event_type != "message.created"
                || event.config_generation_id != Some(generation.id.0)
                || event.config_source_revision.as_deref() != Some(&generation.source_revision.fingerprint_sha256)
                || event.content != routing_input
                || event.visibility != crate::memory::MemoryVisibility::Session
            {
                return Err(AgentTurnOriginError::Mismatch("recorded_event"));
            }
            Some(event.event_id.as_str())
        }
        Some(_) | None => None,
    };
    Ok(AgentTurnProof {
        workspace_id: Arc::from(workspace_id.as_ref()),
        owner_id: Arc::from(owner_id),
        session_key: Arc::from(envelope.session_key.as_str()),
        run_id: Arc::from(run_id),
        source_event_id: recorded_event_id.map(Arc::from),
        routing_input: Arc::from(routing_input),
        memory_scope: envelope.memory_write_context("private"),
        retrieval_principal: envelope.memory_principal(),
        provider_name: Arc::from(choice.provider_name.as_str()),
        model_name: Arc::from(choice.model_name.as_str()),
        model_binding: choice.model_binding.clone(),
        config_generation_id: generation.id.0,
        config_source_revision: Arc::from(generation.source_revision.fingerprint_sha256.as_str()),
        scheduled: None,
    })
}

#[cfg(test)]
pub(crate) fn test_cli_agent_turn_proof(
    generation: &crate::config::ConfigGeneration,
    provider_name: &str,
    model_name: &str,
) -> AgentTurnProof {
    test_cli_agent_turn_proof_with_input(generation, provider_name, model_name, "prx test routing input")
}

#[cfg(test)]
pub(crate) fn test_cli_agent_turn_proof_with_input(
    generation: &crate::config::ConfigGeneration,
    provider_name: &str,
    model_name: &str,
    routing_input: &str,
) -> AgentTurnProof {
    test_cli_agent_turn_proof_with_run(generation, provider_name, model_name, routing_input, "prx_test_run")
}

#[cfg(test)]
pub(crate) fn test_cli_agent_turn_proof_with_run(
    generation: &crate::config::ConfigGeneration,
    provider_name: &str,
    model_name: &str,
    routing_input: &str,
    run_id: &str,
) -> AgentTurnProof {
    let envelope = RuntimeEnvelope::agent(generation.effective.workspace_dir.to_string_lossy(), run_id)
        .with_config_generation(generation);
    issue_cli_agent_turn_proof(
        None,
        &envelope,
        generation,
        run_id,
        routing_input,
        None,
        &resolve_agent_model_choice(&generation.effective, Some(provider_name), Some(model_name), None)
            .expect("test selected model"),
    )
    .expect("test CLI Agent proof")
}

#[cfg(test)]
pub(crate) async fn test_recorded_cli_agent_turn_proof(
    generation: &crate::config::ConfigGeneration,
    memory: Arc<dyn crate::memory::Memory>,
) -> AgentTurnProof {
    test_recorded_cli_agent_turn_proof_with_input(generation, memory, "prx test routing input", "prx_test_run").await
}

#[cfg(test)]
pub(crate) async fn test_recorded_cli_agent_turn_proof_with_input(
    generation: &crate::config::ConfigGeneration,
    memory: Arc<dyn crate::memory::Memory>,
    input: &str,
    run_id: &str,
) -> AgentTurnProof {
    test_recorded_cli_agent_turn_proof_with_selection(generation, memory, input, run_id, ("ollama", "prx-local-model"))
        .await
}

#[cfg(test)]
pub(crate) async fn test_recorded_cli_agent_turn_proof_with_selection(
    generation: &crate::config::ConfigGeneration,
    memory: Arc<dyn crate::memory::Memory>,
    input: &str,
    run_id: &str,
    selection: (&str, &str),
) -> AgentTurnProof {
    let envelope = RuntimeEnvelope::agent(generation.effective.workspace_dir.to_string_lossy(), run_id)
        .with_config_generation(generation);
    let fabric = crate::memory::MemoryFabric::new(memory, generation.effective.workspace_dir.to_string_lossy())
        .with_event_recording(generation.effective.memory.event_recording_config());
    let event = fabric
        .record_inbound_user_message(envelope.message_scope(), input, None, None)
        .await
        .expect("persisted inbound event");
    issue_cli_agent_turn_proof(
        None,
        &envelope,
        generation,
        run_id,
        input,
        Some(&event),
        &resolve_agent_model_choice(&generation.effective, Some(selection.0), Some(selection.1), None)
            .expect("test selected model"),
    )
    .expect("recorded CLI Agent proof")
}

#[cfg(test)]
pub(crate) async fn test_recovery_cli_agent_turn_proof_with_selection(
    generation: &crate::config::ConfigGeneration,
    memory: Arc<dyn crate::memory::Memory>,
    input: &str,
    run_id: &str,
    selection: (&str, &str),
) -> AgentTurnProof {
    let envelope = RuntimeEnvelope::agent(generation.effective.workspace_dir.to_string_lossy(), run_id)
        .with_config_generation(generation);
    let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let ticket = owner
        .register(tokio_util::sync::CancellationToken::new())
        .expect("test: ingress owner");
    let fabric = crate::memory::MemoryFabric::new(memory, generation.effective.workspace_dir.to_string_lossy())
        .with_event_recording(generation.effective.memory.event_recording_config())
        .with_ingress_recovery(&generation.effective, &ticket)
        .await
        .expect("test: recovery ingress fabric");
    let event = fabric
        .record_inbound_user_message(
            envelope.message_scope(),
            input,
            Some(format!("prx:terminal-fixture:{run_id}:input")),
            None,
        )
        .await
        .expect("test: recovery input");
    let proof = issue_cli_agent_turn_proof(
        None,
        &envelope,
        generation,
        run_id,
        input,
        Some(&event),
        &resolve_agent_model_choice(&generation.effective, Some(selection.0), Some(selection.1), None)
            .expect("test: selected recovery model"),
    )
    .expect("test: recovery CLI proof");
    drop(fabric);
    ticket.finish(true);
    owner
        .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
        .await
        .expect("test: ingress owner drained");
    proof
}

#[cfg(test)]
mod agent_turn_origin_tests {
    use super::*;

    fn inbound_event(
        envelope: &RuntimeEnvelope,
        generation: &crate::config::ConfigGeneration,
        content: &str,
    ) -> crate::memory::MessageEvent {
        crate::memory::MessageEvent {
            id: 1,
            event_id: "recorded_event".to_string(),
            idempotency_key: None,
            workspace_id: envelope.workspace_id.clone(),
            owner_id: Some(envelope.resolved_owner_id()),
            source: crate::memory::MessageEventSource::Agent,
            channel: Some("cli".to_string()),
            session_key: Some(envelope.session_key.clone()),
            parent_session_key: None,
            run_id: envelope.run_id.clone(),
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: Some("local-user".to_string()),
            recipient: None,
            role: "user".to_string(),
            event_type: "message.created".to_string(),
            subject: None,
            goal_id: None,
            causation_event_id: None,
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: Some(generation.id.0),
            config_source_revision: Some(generation.source_revision.fingerprint_sha256.clone()),
            content: content.to_string(),
            content_hash: None,
            raw_payload_json: None,
            visibility: envelope.visibility.clone(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn cli_proof_binds_pinned_generation_and_recorded_inbound_event() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config).pin();
        let envelope =
            RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run").with_config_generation(&generation);
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test message-event backend"));
        let fabric = MemoryFabric::new(memory, workspace.path().to_string_lossy())
            .with_event_recording(generation.effective.memory.event_recording_config());
        let event = fabric
            .record_inbound_user_message(envelope.message_scope(), "raw request", None, None)
            .await
            .expect("persisted inbound receipt");
        assert!(event.id > 0);
        let proof = issue_cli_agent_turn_proof(
            None,
            &envelope,
            &generation,
            "prx_run",
            "raw request",
            Some(&event),
            &resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"), None)
                .expect("test selected model"),
        )
        .expect("local CLI origin");
        assert_eq!(proof.workspace_id(), workspace.path().to_string_lossy());
        assert_eq!(proof.session_key(), "agent:prx_run");
        assert_eq!(proof.run_id(), "prx_run");
        assert!(!event.event_id.is_empty(), "expected a non-empty value");
        assert_eq!(proof.source_event_id(), Some(event.event_id.as_str()));
        assert_eq!(proof.routing_input(), "raw request");
        assert_eq!(proof.provider_name(), "ollama");
        assert_eq!(proof.model_name(), "prx-local-model");
        assert_eq!(proof.config_generation_id(), generation.id.0);
        let expected_scope = envelope.memory_write_context("private");
        assert_eq!(proof.memory_scope().workspace_id, expected_scope.workspace_id);
        assert_eq!(proof.memory_scope().channel, expected_scope.channel);
        assert_eq!(proof.memory_scope().chat_id, expected_scope.chat_id);
        assert_eq!(proof.memory_scope().sender_id, expected_scope.sender_id);
        assert_eq!(proof.memory_scope().raw_sender, expected_scope.raw_sender);
        assert_eq!(proof.memory_scope().owner_id, expected_scope.owner_id);
        assert_eq!(
            proof.config_source_revision(),
            generation.source_revision.fingerprint_sha256
        );
        assert_eq!(
            proof.owner_id(),
            crate::memory::principal::OwnerPrincipal::new(
                workspace.path().to_string_lossy(),
                "cli",
                "local-user",
                "agent:prx_run",
                vec![crate::memory::principal::Role::Anonymous],
            )
            .owner_id
        );
        crate::runtime::turn_context::validate_agent_turn_origin(
            &generation,
            crate::tools::capability::turn_state::TurnSurfaceKind::AgentLoop,
            &proof,
        )
        .expect("service accepts the pinned Agent origin");
        assert!(
            crate::runtime::turn_context::validate_agent_turn_origin(
                &generation,
                crate::tools::capability::turn_state::TurnSurfaceKind::Interactive,
                &proof,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn repl_proof_and_admission_failure_bind_session_independently_of_run() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config).pin();
        let envelope = RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_session")
            .with_run_id("prx_turn")
            .with_config_generation(&generation);
        let fabric = MemoryFabric::new(
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("actual source memory")),
            workspace.path().to_string_lossy(),
        );
        let event = fabric
            .record_inbound_user_message(envelope.message_scope(), "raw", None, None)
            .await
            .expect("persist inbound receipt");
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"), None)
            .expect("test selected model");
        let issue = |identity| {
            issue_cli_agent_turn_proof_for_session(None, &envelope, &generation, identity, "raw", Some(&event), &choice)
        };
        assert!(issue(("prx_turn", "agent:other_session")).is_err());
        assert!(issue(("other_turn", "agent:prx_session")).is_err());
        let proof = issue(("prx_turn", "agent:prx_session")).expect("REPL proof");
        assert_eq!(proof.session_key(), "agent:prx_session");
        proof
            .record_cli_admission_failure(&fabric, &generation.effective, false, "test refusal")
            .await
            .expect("persist admission failure");
        let failure = fabric
            .memory()
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_turn:admission-failed")
            .await
            .expect("failure lookup")
            .expect("durable failure event");
        assert_eq!(failure.session_key.as_deref(), Some("agent:prx_session"));
        assert_eq!(failure.owner_id, Some(envelope.resolved_owner_id()));
        assert_eq!(failure.run_id.as_deref(), Some("prx_turn"));
    }

    #[tokio::test]
    async fn missing_or_synthetic_inbound_receipt_confers_no_source_event() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.memory.events.record_user_messages = false;
        let generation = crate::config::new_shared(config).pin();
        let envelope =
            RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run").with_config_generation(&generation);
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"), None)
            .expect("test selected model");
        let fabric = MemoryFabric::new(
            Arc::new(crate::memory::NoneMemory::new()),
            workspace.path().to_string_lossy(),
        )
        .with_event_recording(generation.effective.memory.event_recording_config());
        let synthetic = fabric
            .record_inbound_user_message(envelope.message_scope(), "raw", None, None)
            .await
            .expect("disabled user-event recording yields a synthetic receipt");
        assert_eq!(synthetic.id, 0);
        assert!(!synthetic.event_id.is_empty(), "expected a non-empty value");
        let failing_fabric = MemoryFabric::new(
            Arc::new(crate::memory::MarkdownMemory::new(workspace.path())),
            workspace.path().to_string_lossy(),
        );
        assert!(
            failing_fabric
                .record_inbound_user_message(envelope.message_scope(), "raw", None, None)
                .await
                .is_err(),
            "an enabled record path must report a failed append"
        );
        // P2-R22: an unrecorded turn is never refused, but a missing or
        // synthetic receipt never attests a source event for it.
        for receipt in [None, Some(&synthetic)] {
            let proof = issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "raw", receipt, &choice)
                .expect("an unrecorded CLI turn is not refused");
            assert_eq!(
                proof.source_event_id(),
                None,
                "a missing or synthetic receipt confers no source event"
            );
        }
        let persisted = inbound_event(&envelope, &generation, "raw");
        let proof = issue_cli_agent_turn_proof(
            None,
            &envelope,
            &generation,
            "prx_run",
            "raw",
            Some(&persisted),
            &choice,
        )
        .expect("a persisted receipt issues the proof");
        assert_eq!(proof.source_event_id(), Some(persisted.event_id.as_str()));
        let mut foreign = persisted;
        foreign.run_id = Some("other_run".to_string());
        assert!(
            issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "raw", Some(&foreign), &choice)
                .is_err()
        );
        foreign.run_id = Some("prx_run".to_string());
        foreign.parent_run_id = Some("nested_run".to_string());
        assert!(
            issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "raw", Some(&foreign), &choice)
                .is_err()
        );
    }

    #[tokio::test]
    async fn disabled_agent_does_not_claim_synthetic_inbound_lineage() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.memory.events.record_user_messages = false;
        let generation = crate::config::new_shared(config).pin();
        let envelope =
            RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run").with_config_generation(&generation);
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"), None)
            .expect("test selected model");
        let fabric = MemoryFabric::new(
            Arc::new(crate::memory::NoneMemory::new()),
            workspace.path().to_string_lossy(),
        )
        .with_event_recording(generation.effective.memory.event_recording_config());
        let synthetic = fabric
            .record_inbound_user_message(envelope.message_scope(), "raw", None, None)
            .await
            .expect("disabled user-event recording yields a synthetic receipt");
        assert_eq!(synthetic.id, 0);
        for receipt in [None, Some(&synthetic)] {
            let proof = issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "raw", receipt, &choice)
                .expect("disabled plane may run builtin without a persisted inbound event");
            assert_eq!(proof.source_event_id(), None);
        }
    }

    #[tokio::test]
    async fn selected_memory_read_rejects_a_different_cli_turn_with_the_same_query() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config).pin();
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"), None)
            .expect("test selected model");
        let workspace_id = workspace.path().to_string_lossy();
        let envelope_a = RuntimeEnvelope::agent(workspace_id.as_ref(), "prx_run_a").with_config_generation(&generation);
        let envelope_b = RuntimeEnvelope::agent(workspace_id.as_ref(), "prx_run_b").with_config_generation(&generation);
        let fabric = MemoryFabric::new(
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("actual source memory")),
            workspace_id.as_ref(),
        );
        let inbound_a = fabric
            .record_inbound_user_message(envelope_a.message_scope(), "same query", None, None)
            .await
            .expect("persist first ingress");
        let inbound_b = fabric
            .record_inbound_user_message(envelope_b.message_scope(), "same query", None, None)
            .await
            .expect("persist second ingress");
        let proof_a = issue_cli_agent_turn_proof(
            None,
            &envelope_a,
            &generation,
            "prx_run_a",
            "same query",
            Some(&inbound_a),
            &choice,
        )
        .expect("first source proof");
        let proof_b = issue_cli_agent_turn_proof(
            None,
            &envelope_b,
            &generation,
            "prx_run_b",
            "same query",
            Some(&inbound_b),
            &choice,
        )
        .expect("second source proof");
        assert_eq!(proof_a.routing_input(), proof_b.routing_input());
        assert_eq!(proof_a.owner_id(), proof_b.owner_id());
        assert_ne!(proof_a.session_key(), proof_b.session_key());
        let mut trusted = crate::decision_engine::state::fixtures::trusted();
        trusted.surface = crate::tools::capability::turn_state::TurnSurfaceKind::AgentLoop;
        trusted.channel = None;
        trusted.workspace_id = Arc::from(proof_a.workspace_id());
        trusted.owner_id = Some(Arc::from(proof_a.owner_id()));
        trusted.session_key = Some(Arc::from(proof_a.session_key()));
        trusted.run_id = Some(Arc::from(proof_a.run_id()));
        trusted.source_event_id = proof_a.source_event_id().map(Arc::from);
        trusted.resolved_model = Arc::from("prx:model");
        trusted.policy_version = Arc::from("policy-v1");
        trusted.exclusions.clear();
        let verify = crate::runtime::turn_context::validate_agent_retrieval_provenance;
        assert!(verify(&proof_a, &trusted, "prx:model", "policy-v1", &[]).is_ok());
        assert!(verify(&proof_b, &trusted, "prx:model", "policy-v1", &[]).is_err());
        trusted.owner_id = Some(Arc::from("prx_foreign_owner"));
        assert!(verify(&proof_a, &trusted, "prx:model", "policy-v1", &[]).is_err());
        trusted.owner_id = Some(Arc::from(proof_a.owner_id()));
        trusted.source_event_id = Some(Arc::from("foreign-event"));
        assert!(verify(&proof_a, &trusted, "prx:model", "policy-v1", &[]).is_err());
        trusted.source_event_id = proof_a.source_event_id().map(Arc::from);
        assert!(verify(&proof_a, &trusted, "foreign:model", "policy-v1", &[]).is_err());
        assert!(verify(&proof_a, &trusted, "prx:model", "foreign-policy", &[]).is_err());
        assert!(verify(&proof_a, &trusted, "prx:model", "policy-v1", &[Arc::from("blocked")]).is_err());
    }

    #[tokio::test]
    async fn override_and_mismatched_generation_cannot_issue_cli_proof() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config).pin();
        let good = resolve_agent_turn_envelope(None, &workspace.path().to_string_lossy(), "prx_run")
            .with_config_generation(&generation);
        let interactive = RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run")
            .with_recipient("cli:local-user")
            .with_config_generation(&generation);
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"), None)
            .expect("test selected model");
        let fabric = MemoryFabric::new(
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("actual source memory")),
            workspace.path().to_string_lossy(),
        );
        let inbound = fabric
            .record_inbound_user_message(good.message_scope(), "raw", None, None)
            .await
            .expect("persist actual ingress");
        for envelope in [&good, &interactive] {
            assert!(
                issue_cli_agent_turn_proof(None, envelope, &generation, "prx_run", "raw", Some(&inbound), &choice)
                    .is_ok(),
                "ordinary CLI envelope must retain Agent proof eligibility"
            );
        }
        let rejected = |envelope: &RuntimeEnvelope, override_envelope: Option<&RuntimeEnvelope>| {
            assert!(
                issue_cli_agent_turn_proof(
                    override_envelope,
                    envelope,
                    &generation,
                    "prx_run",
                    "raw",
                    Some(&inbound),
                    &resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"), None)
                        .expect("test selected model"),
                )
                .is_err()
            );
        };
        rejected(&good, Some(&good));
        let mut wrong_source = good.clone();
        wrong_source.source = RuntimeSource::Cron;
        rejected(&wrong_source, None);
        let mut wrong_id = good.clone();
        wrong_id.config_generation_id = Some(generation.id.0.saturating_add(1));
        rejected(&wrong_id, None);
        let mut wrong_revision = good.clone();
        wrong_revision.config_source_revision = Some("stale".to_string());
        rejected(&wrong_revision, None);
        let mut wrong_workspace = good.clone();
        wrong_workspace.workspace_id = "other_workspace".to_string();
        rejected(&wrong_workspace, None);
        let mut wrong_run = good.clone();
        wrong_run.run_id = Some("other_run".to_string());
        wrong_run.session_key = "agent:other_run".to_string();
        rejected(&wrong_run, None);
        let mut forged_event = good.clone();
        forged_event.source_message_event_id = Some("untrusted_event".to_string());
        rejected(&forged_event, None);
        let mut forged_owner = good;
        forged_owner.owner_id = Some("untrusted_owner".to_string());
        rejected(&forged_owner, None);
        let mut nested = forged_owner;
        nested.owner_id = None;
        nested.parent_run_id = Some("parent_run".to_string());
        rejected(&nested, None);
        nested.parent_run_id = None;
        nested.task_id = Some("child_task".to_string());
        rejected(&nested, None);
        nested.task_id = None;
        nested.attempt_id = Some("child_attempt".to_string());
        rejected(&nested, None);
        nested.attempt_id = None;
        nested.recipient = Some("foreign:recipient".to_string());
        rejected(&nested, None);
    }

    /// P2-R22 and controller ruling CR-CI-1: no spelling check exists at
    /// proof issuance. A receipt-less CLI proof issues and binds the
    /// PMM-trimmed model on the direct and the hint route.
    #[test]
    fn raw_model_spelling_issues_a_proof_bound_to_the_pmm_model_on_direct_and_hint_routes() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.default_provider = Some("ollama".to_string());
        config.default_model = Some("prx-local-model ".to_string());
        let direct = resolve_agent_model_choice(&config, None, None, None).expect("PMM trims direct model");
        let direct_generation = crate::config::new_shared(config.clone()).pin();
        let direct_envelope = RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run")
            .with_config_generation(&direct_generation);
        let proof = issue_cli_agent_turn_proof(
            None,
            &direct_envelope,
            &direct_generation,
            "prx_run",
            "ping",
            None,
            &direct,
        )
        .expect("the direct route issues a proof");
        assert_eq!(proof.model_binding().pinned_model(), "prx-local-model");
        assert_eq!(proof.source_event_id(), None);

        config.default_model = Some("prx-local-model".to_string());
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".to_string(),
            provider: "ollama".to_string(),
            model: "prx-route-model ".to_string(),
            api_key: None,
            credential_ref: None,
        });
        let hint = resolve_agent_model_choice(&config, None, Some("hint:fast"), None).expect("PMM trims route model");
        let hint_generation = crate::config::new_shared(config).pin();
        let hint_envelope = RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run")
            .with_config_generation(&hint_generation);
        let proof = issue_cli_agent_turn_proof(None, &hint_envelope, &hint_generation, "prx_run", "ping", None, &hint)
            .expect("the hint route issues a proof");
        assert_eq!(proof.model_binding().pinned_model(), "prx-route-model");
    }

    /// P2-R22 and controller ruling CR-CI-1: the OAuth placeholder root
    /// issues a proof on the resolved provider and model. Whether the
    /// placeholder becomes key material is decided at provider construction
    /// (the OAuth context), not at the proof boundary.
    #[test]
    fn oauth_placeholder_root_issues_a_proof_on_the_resolved_provider_and_model() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.default_provider = Some("qwen-code".to_string());
        config.default_model = Some("qwen3-coder-plus".to_string());
        config.api_key = Some("qwen-oauth".to_string());
        let choice = resolve_agent_model_choice(&config, None, None, None).expect("PMM root placeholder selection");
        let generation = crate::config::new_shared(config).pin();
        let envelope =
            RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run").with_config_generation(&generation);
        let proof = issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice)
            .expect("the placeholder root issues a proof");
        assert_eq!(proof.model_binding().pinned_model(), "qwen3-coder-plus");
        assert_eq!(proof.model_binding().pinned_model(), choice.binding().pinned_model());
        assert_eq!(proof.provider_name(), choice.provider_name());
    }
}

