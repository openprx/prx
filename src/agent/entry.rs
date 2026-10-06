#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::agent::context_recall::{autosave_memory_key, increment_recalled_useful_counts};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::runtime_prompt::build_runtime_system_prompt;
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_engine::run_turn;
use crate::agent::turn_request::ToolLoopMemory;
use crate::approval::ApprovalManager;
use crate::config::Config;
use crate::hooks::{HookEvent, payload_error};
use crate::memory::{Memory, MemoryCategory, MemoryFabric, MessageEventScope};
use crate::observability::ObserverEvent;
use crate::providers;
use crate::providers::{ChatMessage, Provider};
use crate::runtime::envelope::{RuntimeEnvelope, RuntimeSource};
use crate::tools::Tool;
use anyhow::Result;
use std::borrow::Cow;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ── CLI Entrypoint ───────────────────────────────────────────────────────
// Wires up all subsystems (observer, runtime, security, memory, tools,
// provider, hardware RAG) and enters either single-shot or
// interactive REPL mode. The interactive loop manages history compaction
// and hard trimming to keep the context window bounded.

#[allow(clippy::too_many_lines)]
pub async fn run(
    config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    shutdown: CancellationToken,
) -> Result<String> {
    run_with_runtime_envelope(
        config,
        message,
        provider_override,
        model_override,
        temperature,
        shutdown,
        None,
        None,
    )
    .await
}

/// Build one CLI turn's memory handles from the turn's own, user-authored text.
///
/// Both CLI branches need the identical handles: the single-shot branch (which
/// is also how every cron job and xin task enters, through
/// `run_with_runtime_envelope`) and the interactive REPL. They are built here
/// once instead of twice because two hand-copied builder chains are exactly how
/// the routing pin came to be missing from both.
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

fn decision_settlement_snapshot(
    settlement: &parking_lot::Mutex<Option<crate::agent::terminal::DecisionSettlementRef>>,
) -> Option<crate::agent::terminal::DecisionSettlementRef> {
    settlement.lock().clone()
}

struct CliDecisionClose<'a> {
    terminal_guard: Option<&'a crate::runtime::turn_terminal::TurnTerminalGuard>,
    service: &'a crate::runtime::turn_decision::TurnDecisionService,
    fabric: &'a MemoryFabric,
    config: &'a Config,
    scope: MessageEventScope,
    terminal_id: String,
    history_commit_len: usize,
    started_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait::async_trait]
impl crate::runtime::turn_decision::CloseTurnPorts for CliDecisionClose<'_> {
    type Receipt = crate::agent::terminal::TurnTerminalReceipt;

    async fn finalize_closed_turn(
        &self,
        context: Arc<crate::runtime::turn_decision::TurnDecisionContext>,
        reply: &str,
    ) -> anyhow::Result<Self::Receipt> {
        crate::runtime::turn_terminal::commit_terminal(
            self.terminal_guard,
            self.fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: Some(self.service.terminal_settlement_ref(context)?),
                terminal_id: self.terminal_id.clone(),
                scope: self.scope.clone(),
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history: Some(crate::agent::terminal::TurnHistoryProjection {
                    assistant_content: reply.to_owned(),
                    history_commit_len: self.history_commit_len,
                }),
                history_scope: Some(
                    self.scope
                        .clone()
                        .with_sender("prx_runtime")
                        .with_recipient("local-user"),
                ),
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "agent decision closed".to_owned(),
                    started_at: self.started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            },
            self.config,
        )
        .await
    }
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
        let proof = issue_cli_agent_turn_proof(
            None,
            &self.envelope,
            &self.generation,
            run,
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
        tokio::spawn(async move {
            let operation = async {
                let event = self
                    .fabric
                    .record_inbound_user_message(self.scope.clone(), self.input.clone(), Some(self.key.clone()), None)
                    .await
                    .map_or_else(
                        |_| {
                            tracing::warn!("CLI input persistence is unconfirmed");
                            None
                        },
                        Some,
                    );
                let proof = self.issue(event.as_ref()).await?;
                Ok::<_, anyhow::Error>((event, proof))
            };
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
        let mut scope = RuntimeEnvelope::agent(self.workspace_id.to_string(), self.run_id.to_string())
            .with_source_message_event_id(event_id)
            .message_scope();
        scope.config_generation_id = Some(self.config_generation_id);
        scope.config_source_revision = Some(self.config_source_revision.to_string());
        let summary = crate::security::redaction::redact_secrets(summary);
        let payload = serde_json::json!({
            "run_id": self.run_id.as_ref(), "source_event_id": event_id,
            "status": if cancelled { "cancelled" } else { "failed" }, "summary": summary,
        });
        let expected_content = format!("turn.admission.failed {summary}");
        let event = fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "turn.admission.failed",
                summary,
                Some(serde_json::to_string(&payload)?),
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
    pub(crate) fn surface(&self) -> crate::causal_tree::state::TurnSurfaceKind {
        use crate::causal_tree::state::TurnSurfaceKind;
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
#[error("the CLI Agent turn does not match its authenticated runtime origin")]
enum AgentTurnOriginError {
    Mismatch,
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
    let run_id = envelope.run_id.as_deref().ok_or(AgentTurnOriginError::Mismatch)?;
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
        || envelope.session_key != format!("agent:{run_id}")
        || envelope.config_generation_id != Some(generation.id.0)
        || envelope.config_source_revision.as_deref() != Some(&generation.source_revision.fingerprint_sha256)
        || !choice.factory_credential_source_matches(&generation.effective)
    {
        return Err(AgentTurnOriginError::Mismatch);
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
                return Err(AgentTurnOriginError::Mismatch);
            }
            Some(event.event_id.as_str())
        }
        Some(_) | None if generation.effective.causal_tree.enabled => return Err(AgentTurnOriginError::Mismatch),
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
        &resolve_agent_model_choice(&generation.effective, Some(provider_name), Some(model_name))
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
        &resolve_agent_model_choice(&generation.effective, Some(selection.0), Some(selection.1))
            .expect("test selected model"),
    )
    .expect("recorded CLI Agent proof")
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
            &resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"))
                .expect("test selected model"),
        )
        .expect("local CLI origin");
        assert_eq!(proof.workspace_id(), workspace.path().to_string_lossy());
        assert_eq!(proof.session_key(), "agent:prx_run");
        assert_eq!(proof.run_id(), "prx_run");
        assert!(!event.event_id.is_empty());
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
        crate::runtime::turn_decision::validate_agent_turn_origin(
            &generation,
            crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
            &proof,
        )
        .expect("service accepts the pinned Agent origin");
        assert!(
            crate::runtime::turn_decision::validate_agent_turn_origin(
                &generation,
                crate::causal_tree::state::TurnSurfaceKind::Interactive,
                &proof,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn enabled_agent_cannot_issue_proof_without_inbound_receipt() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = true;
        config.memory.events.record_user_messages = false;
        let generation = crate::config::new_shared(config).pin();
        let envelope =
            RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run").with_config_generation(&generation);
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"))
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
        assert!(!synthetic.event_id.is_empty());
        let failing_fabric = MemoryFabric::new(
            Arc::new(crate::memory::NoneMemory::new()),
            workspace.path().to_string_lossy(),
        );
        assert!(
            failing_fabric
                .record_inbound_user_message(envelope.message_scope(), "raw", None, None)
                .await
                .is_err(),
            "an enabled record path must report a failed append"
        );
        for receipt in [None, Some(&synthetic)] {
            assert!(
                issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "raw", receipt, &choice).is_err(),
                "missing inbound receipt cannot confer Agent authority"
            );
        }
        let persisted = inbound_event(&envelope, &generation, "raw");
        assert!(
            issue_cli_agent_turn_proof(
                None,
                &envelope,
                &generation,
                "prx_run",
                "raw",
                Some(&persisted),
                &choice,
            )
            .is_ok()
        );
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
        config.causal_tree.enabled = false;
        config.memory.events.record_user_messages = false;
        let generation = crate::config::new_shared(config).pin();
        let envelope =
            RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run").with_config_generation(&generation);
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"))
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
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"))
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
        let mut trusted = crate::causal_tree::state::fixtures::trusted();
        trusted.surface = crate::causal_tree::state::TurnSurfaceKind::AgentLoop;
        trusted.channel = None;
        trusted.workspace_id = Arc::from(proof_a.workspace_id());
        trusted.owner_id = Some(Arc::from(proof_a.owner_id()));
        trusted.session_key = Some(Arc::from(proof_a.session_key()));
        trusted.run_id = Some(Arc::from(proof_a.run_id()));
        trusted.source_event_id = proof_a.source_event_id().map(Arc::from);
        trusted.resolved_model = Arc::from("prx:model");
        trusted.policy_version = Arc::from("policy-v1");
        trusted.exclusions.clear();
        let verify = crate::runtime::turn_decision::validate_agent_retrieval_provenance;
        assert!(verify(&proof_a, &trusted, "prx:model", "policy-v1", &[], None).is_ok());
        assert!(verify(&proof_b, &trusted, "prx:model", "policy-v1", &[], None).is_err());
        trusted.owner_id = Some(Arc::from("prx_foreign_owner"));
        assert!(verify(&proof_a, &trusted, "prx:model", "policy-v1", &[], None).is_err());
        trusted.owner_id = Some(Arc::from(proof_a.owner_id()));
        trusted.source_event_id = Some(Arc::from("foreign-event"));
        assert!(verify(&proof_a, &trusted, "prx:model", "policy-v1", &[], None).is_err());
        trusted.source_event_id = proof_a.source_event_id().map(Arc::from);
        assert!(verify(&proof_a, &trusted, "foreign:model", "policy-v1", &[], None).is_err());
        assert!(verify(&proof_a, &trusted, "prx:model", "foreign-policy", &[], None).is_err());
        assert!(
            verify(
                &proof_a,
                &trusted,
                "prx:model",
                "policy-v1",
                &[Arc::from("blocked")],
                None
            )
            .is_err()
        );
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
        let choice = resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"))
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
                    &resolve_agent_model_choice(&generation.effective, Some("ollama"), Some("prx-local-model"))
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

    #[test]
    fn raw_model_spelling_must_match_the_pmm_model_on_direct_and_hint_routes() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.default_provider = Some("ollama".to_string());
        config.default_model = Some("prx-local-model ".to_string());
        let direct = resolve_agent_model_choice(&config, None, None).expect("PMM trims direct model");
        let direct_generation = crate::config::new_shared(config.clone()).pin();
        let direct_envelope = RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run")
            .with_config_generation(&direct_generation);
        assert!(
            issue_cli_agent_turn_proof(
                None,
                &direct_envelope,
                &direct_generation,
                "prx_run",
                "ping",
                None,
                &direct,
            )
            .is_err()
        );

        config.default_model = Some("prx-local-model".to_string());
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "fast".to_string(),
            provider: "ollama".to_string(),
            model: "prx-route-model ".to_string(),
            api_key: None,
            credential_ref: None,
        });
        let hint = resolve_agent_model_choice(&config, None, Some("hint:fast")).expect("PMM trims route model");
        let hint_generation = crate::config::new_shared(config).pin();
        let hint_envelope = RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run")
            .with_config_generation(&hint_generation);
        assert!(
            issue_cli_agent_turn_proof(None, &hint_envelope, &hint_generation, "prx_run", "ping", None, &hint).is_err()
        );
    }

    #[test]
    fn oauth_placeholder_cannot_attest_to_root_wire_material() {
        for (provider, value) in [
            ("qwen-code", "qwen-oauth"),
            ("minimax", "minimax-oauth"),
            ("minimax-cn", "minimax-oauth-cn"),
            ("claude-code", "claude-code"),
            ("anthropic", "claude-cli"),
        ] {
            assert!(
                providers::credential_override_is_runtime_indirection(provider, value),
                "factory indirection must be visible to the proof guard"
            );
        }
        assert!(!providers::credential_override_is_runtime_indirection(
            "openai",
            "prx-literal-key"
        ));
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.default_provider = Some("qwen-code".to_string());
        config.default_model = Some("qwen3-coder-plus".to_string());
        config.api_key = Some("qwen-oauth".to_string());
        let choice = resolve_agent_model_choice(&config, None, None).expect("PMM root placeholder selection");
        let generation = crate::config::new_shared(config).pin();
        let envelope =
            RuntimeEnvelope::agent(workspace.path().to_string_lossy(), "prx_run").with_config_generation(&generation);
        assert!(issue_cli_agent_turn_proof(None, &envelope, &generation, "prx_run", "ping", None, &choice).is_err());
    }
}

/// The provider and model an agent entry runs on, resolved and owned.
///
/// `prx agent` (with or without `--provider`), `prx go --message`, and cron
/// and xin agent jobs all resolve their selection with
/// [`resolve_agent_model_choice`] before anything is constructed: before the
/// runtime bootstrap (memory hygiene, snapshots, the database, plugins,
/// tools), so a refused selection leaves no trace (D-065 M-1).
#[derive(Clone)]
pub(crate) struct AgentModelChoice {
    provider_name: String,
    model_name: String,
    model_binding: AgentModelBinding,
}

impl AgentModelChoice {
    pub(crate) const fn binding(&self) -> &AgentModelBinding {
        &self.model_binding
    }

    pub(crate) fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub(crate) fn model_name(&self) -> &str {
        &self.model_name
    }

    /// An injected provider may switch model IDs only within the connection
    /// identity already owned by that instance and pinned configuration.
    pub(crate) fn same_provider_connection(&self, other: &Self) -> bool {
        self.model_binding.provider == other.model_binding.provider
            && self.model_binding.provider_label == other.model_binding.provider_label
            && self.model_binding.credential_ref == other.model_binding.credential_ref
            && self.model_binding.credential_has_key == other.model_binding.credential_has_key
    }

    pub(crate) fn resolved_selection(&self) -> (&str, &str) {
        (&self.model_binding.provider_label, &self.model_binding.model)
    }

    /// The routed factory builds one provider per exact name and takes the
    /// first route for that provider, whether keyed or not. Deny a decision proof whenever
    /// that source could differ from the PMM selection's credential slot.
    pub(crate) fn factory_credential_source_matches(&self, config: &Config) -> bool {
        // A URL or rotation list makes PMM label the root slot as present,
        // yet a missing primary key lets the provider factory look up a key
        // from another source. That is not a Root wire-key attestation.
        if matches!(
            self.model_binding.credential_ref,
            crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
        ) && config.api_key.as_deref().is_none_or(|key| key.trim().is_empty())
        {
            return false;
        }
        if matches!(
            self.model_binding.credential_ref,
            crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
        ) && config
            .api_key
            .as_deref()
            .is_some_and(|key| providers::credential_override_is_runtime_indirection(&self.provider_name, key))
        {
            return false;
        }
        // PMM trims labels, while the provider factory uses exact raw names.
        // Refuse spelling differences before comparing its route list.
        if self.provider_name.trim() != self.provider_name {
            return false;
        }
        let factory_name = if let Some(selected) = self.model_binding.route.as_ref() {
            let Some(route) = config
                .model_routes
                .iter()
                .find(|route| route.hint.trim() == selected.hint)
            else {
                return false;
            };
            if route.model != self.model_binding.model.as_ref() {
                return false;
            }
            route.provider.as_str()
        } else {
            if self.model_name != self.model_binding.model.as_ref() {
                return false;
            }
            self.provider_name.as_str()
        };
        if factory_name.trim() != factory_name || factory_name != self.model_binding.provider_label.as_ref() {
            return false;
        }
        let same_provider_routes: Vec<_> = config
            .model_routes
            .iter()
            .filter(|route| route.provider == factory_name)
            .collect();
        let is_keyed = |route: &crate::config::ModelRouteConfig| {
            route.api_key.as_deref().is_some_and(|key| !key.trim().is_empty())
                || route.credential_ref.as_deref().is_some_and(|id| !id.trim().is_empty())
        };
        match self.model_binding.route.as_ref() {
            Some(selected) if selected.has_own_key || selected.credential_ref.is_some() => {
                same_provider_routes.first().is_some_and(|first| {
                    first.hint.trim() == selected.hint
                        && !first.api_key.as_deref().is_some_and(|key| {
                            providers::credential_override_is_runtime_indirection(factory_name, key)
                        })
                })
                    && same_provider_routes.iter().filter(|route| is_keyed(route)).count() == 1
                    && self.model_binding.credential_has_key
                    // Auth profiles are reread when the provider is built.
                    // Their material can disappear or change in between.
                    && selected.credential_ref.is_none()
                    // The primary routed connection also carries the root URL
                    // and rotation keys; PMM checked only this route's key.
                    && (factory_name != self.provider_name
                        || (config.api_url.as_deref().is_none_or(|url| url.trim().is_empty())
                            && config.reliability.api_keys.is_empty()))
            }
            _ => !same_provider_routes.iter().any(|route| is_keyed(route)),
        }
    }
}

/// Identity selected before provider construction. It deliberately excludes
/// credential material: the provider factory must receive its own pinned
/// material, while the decision service can compare route and owner facts.
#[derive(Clone)]
pub(crate) struct AgentModelBinding {
    provider: crate::providers::ProviderId,
    provider_label: Arc<str>,
    model: Arc<str>,
    credential_ref: crate::providers::credentials::CredentialRef,
    route: Option<crate::providers::catalog::RouteTarget>,
    verification: crate::providers::catalog::Verification,
    source: crate::providers::catalog::DescriptorSource,
    dialects: crate::providers::catalog::Dialects,
    credential_has_key: bool,
}

impl AgentModelBinding {
    pub(crate) const fn credential_reference(&self) -> &crate::providers::credentials::CredentialRef {
        &self.credential_ref
    }

    pub(crate) fn from_resolved(
        resolved: &crate::providers::catalog::ResolvedModel,
        route: Option<crate::providers::catalog::RouteTarget>,
    ) -> Self {
        Self {
            provider: resolved.provider().clone(),
            provider_label: Arc::from(resolved.provider_label()),
            model: Arc::from(resolved.model()),
            credential_ref: resolved.credential().reference().clone(),
            route,
            verification: resolved.verification().clone(),
            source: resolved.source(),
            dialects: resolved.dialects(),
            credential_has_key: resolved.credential().api_key().is_some(),
        }
    }

    pub(crate) fn matches_identity(&self, resolved: &crate::providers::catalog::ResolvedModel) -> bool {
        self.provider == *resolved.provider()
            && self.provider_label.as_ref() == resolved.provider_label()
            && self.model.as_ref() == resolved.model()
            && self.credential_ref == *resolved.credential().reference()
    }

    pub(crate) fn matches(&self, resolved: &crate::providers::catalog::ResolvedModel) -> bool {
        self.matches_identity(resolved)
            && self.verification == *resolved.verification()
            && self.source == resolved.source()
            && self.dialects == resolved.dialects()
    }

    pub(crate) const fn route(&self) -> Option<&crate::providers::catalog::RouteTarget> {
        self.route.as_ref()
    }
}

/// Resolve the selection for an agent entry, check-only.
///
/// This is the one resolution of the entry. An operator's invocation runs an
/// id-only model with a warning and refuses one the evidence rules out
/// (D-034). The chain the entry builds is routed, so every `[[model_routes]]`
/// destination is resolved too, on every run, and a `hint:` model is checked
/// as the destination it names (D-035).
pub(crate) fn resolve_agent_model_choice(
    config: &Config,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<AgentModelChoice> {
    let provider_name = provider_override.unwrap_or_else(|| providers::selection::default_provider(config));
    let model_name = model_override.unwrap_or_else(|| providers::selection::default_model(config));
    let catalog = providers::selection::catalog_for(config);
    let resolved = providers::selection::check_with(
        config,
        &catalog,
        provider_name,
        model_name,
        providers::selection::credential_for(config, provider_name),
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    providers::selection::check_routes(config, &catalog, providers::catalog::UnknownPolicy::Invocation)?;
    let route = providers::selection::route_target(&catalog, provider_name, model_name)?.cloned();
    Ok(AgentModelChoice {
        provider_name: provider_name.to_string(),
        model_name: model_name.to_string(),
        model_binding: AgentModelBinding::from_resolved(&resolved, route),
    })
}

/// The provider an agent entry runs on, and the only connection it may use.
///
/// The root credential, base URL and rotation keys are released only when
/// the resolved selection is the provider identity they were configured for.
/// Anything else resolves its own credential (auth profile, or none for a
/// keyless `custom:` endpoint) or is refused at construction, before any
/// transport exists.
pub(crate) struct AgentProviderSelection<'a> {
    pub(crate) provider_name: &'a str,
    pub(crate) model_name: &'a str,
    pub(crate) connection: providers::ProviderConnection<'a>,
    pub(crate) reliability: Cow<'a, crate::config::ReliabilityConfig>,
}

/// Scope the connection for a resolved selection. Resolution already ran in
/// [`resolve_agent_model_choice`]; this does not resolve again.
pub(crate) fn agent_provider_selection<'a>(
    config: &'a Config,
    choice: &'a AgentModelChoice,
) -> AgentProviderSelection<'a> {
    let connection = providers::configured_connection_for(
        &choice.provider_name,
        providers::selection::default_provider(config),
        config.api_key.as_deref(),
        config.api_url.as_deref(),
    );
    let reliability = providers::scoped_reliability(&config.reliability, &connection);
    AgentProviderSelection {
        provider_name: &choice.provider_name,
        model_name: &choice.model_name,
        connection,
        reliability,
    }
}

impl AgentProviderSelection<'_> {
    /// Build the routed provider chain for this selection.
    pub(crate) fn build_provider(
        &self,
        config: &Config,
        options: &providers::ProviderRuntimeOptions,
    ) -> Result<Box<dyn Provider>> {
        providers::create_routed_provider_with_options(
            self.provider_name,
            self.connection.api_key,
            self.connection.api_url,
            &self.reliability,
            &config.model_routes_with_references(),
            self.model_name,
            options,
        )
    }
}

/// P0-B: `prx agent --provider <other>` must give the override only its own
/// credential. Driven through the production seam, the real factory and a
/// loopback endpoint; every credential is a sentinel and the config directory
/// is a fresh temporary one with no auth profiles.
#[path = "turn_tests/entry_provider_selection.rs"]
mod agent_provider_selection_tests;

/// Run the Agent entry with a caller-owned runtime envelope. This preserves
/// owner/topic/task/source lineage for scheduler-driven single-shot turns while
/// keeping the public CLI entrypoint's existing defaults.
///
/// `scheduled_tool_scope` is the tool context and policy a cron job or xin
/// task runs under when its `JobOrigin` is not the local operator
/// (CRON-ORIGIN); `None` keeps the `prx agent` CLI context and policy. It
/// governs a single-shot turn only.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_with_runtime_envelope(
    config_source: impl Into<crate::runtime::bootstrap::AgentRuntimeConfig>,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    shutdown: CancellationToken,
    runtime_envelope_override: Option<RuntimeEnvelope>,
    scheduled_tool_scope: Option<crate::tools::execution::ScheduledToolScope>,
) -> Result<String> {
    let config_source = config_source.into();
    let config = config_source.effective();
    let scheduled_admission = super::scheduled_source::validate_current(
        runtime_envelope_override.as_ref(),
        scheduled_tool_scope.as_ref(),
        &shutdown,
    )
    .await?;
    anyhow::ensure!(
        runtime_envelope_override.is_none() || scheduled_admission.is_some(),
        "agent runtime override requires an admitted scheduled source"
    );
    if message.is_none() && (scheduled_tool_scope.is_some() || runtime_envelope_override.is_some()) {
        anyhow::bail!("a scoped runtime run must be a single-shot turn");
    }
    // Validate scoped identity before bootstrap can initialize persistence.
    // Scheduled sources retain their attempt/lease run id through terminal
    // recording and child lineage instead of acquiring a second CLI run id.
    let scoped_run_id = runtime_envelope_override
        .as_ref()
        .map(|envelope| {
            anyhow::ensure!(
                envelope.workspace_id == config.workspace_dir.to_string_lossy()
                    && !envelope.session_key.trim().is_empty(),
                "scoped runtime workspace or session does not match this invocation"
            );
            let run_id = envelope
                .run_id
                .as_deref()
                .filter(|run_id| !run_id.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("a scoped runtime run requires its source-owned run identity"))?;
            Ok::<_, anyhow::Error>(run_id.to_owned())
        })
        .transpose()?;
    // Manual scheduled tool calls inherit this owner in ordinary CLI/REPL
    // execution. Let the entry's existing shutdown branch close its own turn;
    // cancellation of this child scope must not race and drop that branch.
    let caller_cleanup = if scheduled_admission.is_none() {
        let parent = crate::runtime::turn_cleanup::current_tracker();
        Some(crate::runtime::turn_cleanup::CleanupScope::new(
            parent.as_ref(),
            CancellationToken::new(),
        )?)
    } else {
        None
    };

    // D-065 M-1: resolve before the bootstrap, so a refused selection runs
    // no memory hygiene, snapshot, database, plugin or tool setup.
    let choice = resolve_agent_model_choice(config, provider_override.as_deref(), model_override.as_deref())?;
    // ── Wire up subsystems via RuntimeBootstrap (D1 step 2) ──────
    // The CLI agent entry is always a full interactive/single-shot run with no
    // memory-only early-exit (unlike chat's `--list-sessions`), so it always
    // needs memory + tools. It uses the dedicated `AgentLoop` profile: identical
    // core to `Interactive` (memory + tools, no router), with the published
    // source-bound turn decision service. Bootstrap constructs subsystems in
    // observer/security/memory/runtime/tools order.
    let ctx = crate::runtime::bootstrap::RuntimeBootstrap::build_agent(
        config_source,
        runtime_envelope_override.as_ref(),
        scheduled_tool_scope.as_ref(),
    )
    .await?;

    // Scheduled cleanup owns the published writer even if this entry future is
    // dropped before a decision or terminal proposal is available.
    if scheduled_admission.is_some() {
        let cleanup = crate::runtime::turn_cleanup::current_tracker()
            .ok_or_else(|| anyhow::anyhow!("scheduled cleanup owner is missing"))?;
        if let Some(service) = &ctx.turn_decisions {
            cleanup.retain_service(Arc::clone(service));
        }
    }

    // Build the CLI work in a separate frame; cleanup owns the same future.
    let work = boxed_cli_work(CliWork {
        ctx: &ctx,
        choice: &choice,
        message,
        temperature,
        shutdown: &shutdown,
        runtime_envelope_override,
        scheduled_tool_scope,
        scoped_run_id,
        scheduled_admission: &scheduled_admission,
    });
    let result = match caller_cleanup {
        Some(cleanup) => cleanup.run(work).await,
        None => work.await,
    };
    if scheduled_admission.is_none() {
        if let Some(service) = &ctx.turn_decisions {
            service
                .shutdown(Instant::now() + std::time::Duration::from_secs(2))
                .await;
        }
    }
    result
}

// This invocation state preserves all captures across the boxed work boundary.
struct CliWork<'a> {
    ctx: &'a crate::runtime::bootstrap::AppContext,
    choice: &'a AgentModelChoice,
    message: Option<String>,
    temperature: f64,
    shutdown: &'a CancellationToken,
    runtime_envelope_override: Option<RuntimeEnvelope>,
    scheduled_tool_scope: Option<crate::tools::execution::ScheduledToolScope>,
    scoped_run_id: Option<String>,
    scheduled_admission: &'a Option<super::scheduled_source::ScheduledAdmission>,
}

// Separate construction of the large work state from the entry poll frame.
fn boxed_cli_work(
    work: CliWork<'_>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
    Box::pin(cli_work(work))
}

// Keep the single-turn poll state out of the enclosing CLI select frame.
// The synchronous boundary also keeps Box construction in its own stack frame.
#[inline(never)]
fn boxed_cli_single_shot<'a>(
    work: impl std::future::Future<Output = Result<()>> + Send + 'a,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(work)
}

// Interactive engine await state must not enlarge the single-shot caller's poll frame.
#[inline(never)]
fn boxed_cli_interactive<'a>(
    work: impl std::future::Future<Output = Result<()>> + Send + 'a,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(work)
}

#[allow(clippy::too_many_lines)]
async fn cli_work(work: CliWork<'_>) -> Result<String> {
    let CliWork {
        ctx,
        choice,
        message,
        temperature,
        shutdown,
        runtime_envelope_override,
        scheduled_tool_scope,
        scoped_run_id,
        scheduled_admission,
    } = work;
    // `build` took ownership of `config`; reclaim a shared `Arc<Config>` for the
    // rest of this function. `Arc<Config>` deref-coerces to `&Config`, so all the
    // `config.xxx` accesses below are unchanged.
    let config = Arc::clone(&ctx.config);

    let observer = Arc::clone(&ctx.observer);
    let hooks = Arc::clone(&ctx.hooks);
    // security is consumed only by tool construction, which now happens inside the
    // bootstrap; it is not re-bound here on purpose (no downstream use in `run`).
    // The local runtime adapter is likewise built and consumed inside the
    // bootstrap, so it is no longer constructed here.

    // ── Memory (the brain) ────────────────────────────────────────
    // The Interactive profile always builds memory, so it is Some here; take it
    // explicitly without panicking (iron rules 1/6).
    let mem: Arc<dyn Memory> = ctx
        .memory
        .clone()
        .ok_or_else(|| anyhow::anyhow!("bootstrap did not build a memory backend for the agent CLI"))?;
    let memory_fabric = MemoryFabric::new(mem.clone(), config.workspace_dir.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    // D8-3: run_id is per-turn (generated inside each turn branch as `turn_run_id`),
    // no longer a single per-run id held here.
    let mut fabric_turn_seq: u64 = 0;

    // ── Tools ────────────────────────────────────────────────────
    // The Interactive profile built the tool registry inside the bootstrap
    // (security + runtime + memory all ready) using the identical
    // `all_tools_with_runtime` call with the same arguments; take it explicitly.
    let tools_registry: Arc<Vec<Box<dyn Tool>>> = ctx
        .tools
        .clone()
        .ok_or_else(|| anyhow::anyhow!("bootstrap did not build the tool registry for the agent CLI"))?;

    // ── Provider (resolved before the bootstrap) ─────────────────
    let selection = agent_provider_selection(&config, choice);
    let provider_name = selection.provider_name;
    let model_name = selection.model_name;

    let provider_runtime_options = providers::provider_runtime_options_from_config(&config);

    let provider: Box<dyn Provider> = selection.build_provider(&config, &provider_runtime_options)?;
    let effective_compaction = crate::router::resolve_effective_compaction_config(
        &config.agent.compaction,
        provider_name,
        model_name,
        &config.router,
        &config.model_routes,
    );
    crate::router::context::trace_effective_compaction_resolution(&effective_compaction);

    observer.record_event(&ObserverEvent::AgentStart {
        provider: provider_name.to_string(),
        model: model_name.to_string(),
    });
    hooks
        .emit(
            HookEvent::AgentStart,
            serde_json::json!({
                "provider": provider_name,
                "model": model_name,
            }),
        )
        .await;

    // ── Build system prompt from the canonical prompt context ──
    let native_tools = provider
        .capabilities_for(model_name, crate::providers::traits::ProviderRequestMode::NonStreaming)
        .native_tool_calling;
    let skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);

    // ── Approval manager (supervised mode) ───────────────────────
    let approval_manager = Arc::new(ApprovalManager::new());

    // ── Execute ──────────────────────────────────────────────────
    let start = Instant::now();

    let mut final_output = String::new();

    if let Some(msg) = message {
        // ── Single-shot mode (D5/D9 step 2) ──────────────────────────
        // A single non-interactive turn: race the work against the external
        // shutdown token so a root cancel (e.g. ctrl_c wired by dispatch in A6)
        // aborts the in-flight turn instead of waiting it out. The interactive
        // branch below cannot do this because it blocks on synchronous
        // `stdin().read_line()`, which `tokio::select!` cannot poll.
        fabric_turn_seq += 1;
        // Keep the turn identity outside the raced future so a shutdown that
        // drops it can still close the shared terminal boundary as Cancelled.
        let turn_run_id = scoped_run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut runtime_envelope = scheduled_admission.as_ref().map_or_else(
            || {
                resolve_agent_turn_envelope(
                    runtime_envelope_override.as_ref(),
                    memory_fabric.workspace_id(),
                    &turn_run_id,
                )
            },
            |admission| admission.envelope().clone(),
        );
        if runtime_envelope.config_source_revision.is_none() {
            runtime_envelope = runtime_envelope.with_config_generation(&ctx.config_generation);
        }
        let mut fabric_scope = runtime_envelope
            .message_scope()
            .with_recipient(format!("{provider_name}/{model_name}"));
        let cancel_turn_id = turn_run_id.clone();
        let cancel_scope = parking_lot::Mutex::new(runtime_envelope.message_scope());
        let turn_started_at = chrono::Utc::now();
        let decision_settlement = Arc::new(parking_lot::Mutex::new(
            None::<crate::agent::terminal::DecisionSettlementRef>,
        ));
        let admission_proof = std::sync::OnceLock::<AgentTurnProof>::new();
        let terminal_guard = std::sync::OnceLock::<crate::runtime::turn_terminal::TurnTerminalGuard>::new();
        let mut cli_admission_guard = None;
        let mut scheduled_input_guard = None;
        let retained_provider_trace = Arc::new(crate::agent::turn_contracts::RetainedProviderTrace::default());
        let single_shot = boxed_cli_single_shot(async {
            let agent_user_event = if scheduled_admission.is_none() {
                let owner = crate::runtime::turn_cleanup::current_tracker()
                    .ok_or_else(|| anyhow::anyhow!("CLI input cleanup owner missing"))?;
                let (event, guard) = CliPendingInput {
                    fabric: memory_fabric.clone(),
                    generation: Arc::clone(&ctx.config_generation),
                    envelope: runtime_envelope.clone(),
                    scope: fabric_scope.clone(),
                    choice: choice.clone(),
                    input: msg.clone(),
                    key: format!("agent:{turn_run_id}:{fabric_turn_seq}:user"),
                    ticket: owner.register(shutdown.clone())?,
                }
                .record()
                .await?;
                cli_admission_guard = Some(guard);
                event
            } else {
                let admission = scheduled_admission
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("scheduled admission source missing"))?;
                let owner = crate::runtime::turn_cleanup::current_tracker()
                    .ok_or_else(|| anyhow::anyhow!("scheduled input cleanup owner missing"))?;
                let (event, guard) = admission
                    .record_guarded(
                        memory_fabric.clone(),
                        Arc::clone(&ctx.config_generation),
                        (
                            msg.clone(),
                            format!("agent:{turn_run_id}:{fabric_turn_seq}:user"),
                            choice.clone(),
                        ),
                        owner.register(shutdown.clone())?,
                    )
                    .await?;
                scheduled_input_guard = Some(guard);
                event
            };

            let decision_turn = {
                let service = ctx
                    .turn_decisions
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("agent decision service is missing"))?;
                let proof = if let Some(admission) = &scheduled_admission {
                    let source = admission
                        .bind_input(
                            mem.as_ref(),
                            &ctx.config_generation,
                            &runtime_envelope,
                            (&msg, choice),
                            agent_user_event.as_ref(),
                            shutdown,
                        )
                        .await?;
                    AgentTurnProof {
                        workspace_id: Arc::from(source.envelope.workspace_id.as_str()),
                        owner_id: Arc::from(source.envelope.resolved_owner_id()),
                        session_key: Arc::from(source.envelope.session_key.as_str()),
                        run_id: Arc::from(turn_run_id.as_str()),
                        source_event_id: source.envelope.source_message_event_id.as_deref().map(Arc::from),
                        routing_input: Arc::from(msg.as_str()),
                        memory_scope: source
                            .envelope
                            .memory_write_context(source.execution.chat_type.as_str()),
                        provider_name: Arc::from(choice.provider_name()),
                        model_name: Arc::from(choice.model_name()),
                        model_binding: choice.binding().clone(),
                        config_generation_id: ctx.config_generation.id.0,
                        config_source_revision: Arc::from(
                            ctx.config_generation.source_revision.fingerprint_sha256.as_str(),
                        ),
                        retrieval_principal: source.envelope.memory_principal(),
                        scheduled: Some(Box::new(source)),
                    }
                } else {
                    issue_cli_agent_turn_proof(
                        None,
                        &runtime_envelope,
                        &ctx.config_generation,
                        &turn_run_id,
                        &msg,
                        agent_user_event.as_ref(),
                        choice,
                    )?
                };
                if let Some(source) = proof.scheduled.as_ref() {
                    runtime_envelope = source.envelope.clone();
                    fabric_scope = runtime_envelope
                        .message_scope()
                        .with_recipient(format!("{provider_name}/{model_name}"));
                    *cancel_scope.lock() = runtime_envelope.message_scope();
                }
                let proof = admission_proof.get_or_init(|| proof);
                if scheduled_admission.is_some() {
                    // Mark before polling decision work: a failed or interrupted
                    // admission can already have a durable decision identity.
                    super::scheduled_source::begin_decision()?;
                }
                let decision = service
                    .decide_agent_initial(
                        proof,
                        crate::runtime::turn_decision::AgentTurnKind::SingleShot,
                        shutdown,
                    )
                    .await?;
                *decision_settlement.lock() = Some(service.terminal_settlement_ref(Arc::clone(&decision))?);
                let cleanup_ticket = if scheduled_admission.is_some() {
                    scheduled_input_guard
                        .take()
                        .ok_or_else(|| anyhow::anyhow!("scheduled input cleanup guard missing"))?
                        .into_terminal_ticket()?
                } else {
                    cli_admission_guard
                        .take()
                        .ok_or_else(|| anyhow::anyhow!("CLI input cleanup guard missing"))?
                        .into_terminal_ticket()?
                };
                let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
                    provider_name,
                    model_name.to_string(),
                    runtime_envelope.resolved_owner_id(),
                    runtime_envelope.session_key.clone(),
                    runtime_envelope.source_message_event_id.clone(),
                    None,
                    "agent_cli_single",
                    u32::try_from(msg.chars().count() / 4).unwrap_or(u32::MAX),
                    !tools_registry.is_empty(),
                    false,
                );
                let guard = crate::runtime::turn_terminal::TurnTerminalGuard::new(
                    cleanup_ticket,
                    Arc::clone(service),
                    Arc::clone(&decision),
                    memory_fabric.clone(),
                    Arc::clone(&config),
                    runtime_envelope.clone(),
                    None,
                )?
                .with_provider_trace(Arc::clone(&retained_provider_trace), route_decision.clone());
                terminal_guard
                    .set(guard)
                    .map_err(|_| anyhow::anyhow!("agent terminal owner already exists"))?;
                anyhow::ensure!(
                    !matches!(
                        decision.disposition()?,
                        crate::runtime::turn_decision::TurnDisposition::CollectApprovalArguments
                    ),
                    "ingress has no approval collection adapter"
                );
                if let crate::runtime::turn_decision::TurnDisposition::Close { reply } = decision.disposition()? {
                    let ports = CliDecisionClose {
                        terminal_guard: terminal_guard.get(),
                        service,
                        fabric: &memory_fabric,
                        config: &config,
                        scope: runtime_envelope.message_scope(),
                        terminal_id: turn_run_id.clone(),
                        history_commit_len: 2,
                        started_at: turn_started_at,
                    };
                    service
                        .close_agent_directive_turn(proof, Arc::clone(&decision), &ports, shutdown)
                        .await?;
                    final_output = reply.to_string();
                    println!("{reply}");
                    return Ok::<(), anyhow::Error>(());
                }
                (service, proof, decision, route_decision)
            };

            let (decision_service, decision_proof, decision_context, route_decision) = &decision_turn;
            let decision_proof = *decision_proof;
            let selected_skills = decision_service.agent_prompt_skills(decision_proof, decision_context)?;
            let system_prompt = build_runtime_system_prompt(&config, model_name, &selected_skills, native_tools);

            // Auto-save user message to memory (skip short/trivial messages)
            if config.memory.should_auto_promote_user_message(&msg) {
                let user_key = autosave_memory_key("user_msg");
                let _ = memory_fabric
                    .record_semantic_memory_from_event(
                        &user_key,
                        &msg,
                        MemoryCategory::Conversation,
                        None,
                        agent_user_event.as_ref().map(|event| event.event_id.as_str()),
                        None,
                        None,
                    )
                    .await;
            }

            // Inject memory context into user message. The routing input is
            // pinned from `msg` here, ahead of the enrichment below.
            let memory_runtime = cli_turn_memory_runtime(
                &mem,
                &config.workspace_dir,
                &runtime_envelope,
                &memory_fabric,
                &fabric_scope,
                agent_user_event.as_ref().map(|event| event.event_id.as_str()),
            );
            let mem_context = crate::agent::context_recall::RecalledMemoryContext {
                preamble: crate::agent::memory_loader::format_selected_context(None, decision_context.retrieved()?)?,
                ids: Vec::new(),
            };
            let context = mem_context.preamble.clone();
            let enriched = if context.is_empty() {
                msg.clone()
            } else {
                format!("{context}{msg}")
            };

            let mut history = vec![
                ChatMessage::system(&system_prompt),
                crate::agent::turn_contracts::inbound_user_message(&enriched),
            ];

            // D8-4: seed a turn-root spawn context so a sub-agent spawned from this
            // single-shot agent turn inherits parent_run_id = the per-turn run_id.
            let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
                turn_run_id.clone(),
                runtime_envelope.session_key.clone(),
            );
            let provider_started_at = chrono::Utc::now();
            let runtime_adapter = Some({
                let mut adapter =
                    configured_cli_tool_adapter(&ctx.security, &tools_registry, &approval_manager, &memory_runtime);
                {
                    let proof = decision_proof;
                    let mut envelope = runtime_envelope.clone();
                    if let Some(event_id) = proof.source_event_id() {
                        envelope = envelope.with_source_message_event_id(event_id);
                    }
                    adapter.tool_execution_context = proof.scheduled.as_ref().map_or_else(
                        || crate::tools::ToolExecutionContext::new(envelope, "private"),
                        |source| source.execution.clone(),
                    );
                }
                adapter.provider_trace = Some(Arc::clone(&retained_provider_trace));
                adapter
            });
            let turn_result = crate::tools::execution::with_scheduled_job_tool_scope(
                scheduled_tool_scope,
                crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.scope(turn_spawn_ctx, async {
                    run_turn(
                        &mut history,
                        crate::agent::turn_request::TurnRequest {
                            channel_name: "cli",
                            scope: None,
                            mode: ChatMode::default(),
                            silent: false,
                            expose_stay_silent: false,
                            multimodal: &config.multimodal,
                            schedule: crate::agent::turn_request::TurnSchedule {
                                read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
                                priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
                                low_priority_tool_names: config.agent.low_priority_tools.clone(),
                            },
                            compaction: Some(decision_context.main_compaction()),
                            decision: crate::agent::turn_request::TurnDecision {
                                service: decision_service,
                                context: decision_context,
                                proof: decision_proof,
                            },
                        },
                        crate::agent::turn_request::TurnServices {
                            provider: provider.as_ref(),
                            provider_name,
                            model: model_name,
                            temperature,
                            tools_registry: Arc::clone(&tools_registry),
                            observer: observer.as_ref(),
                            hooks: &hooks,
                            approval: Some(Arc::clone(&approval_manager)),
                            approval_resolver: None,
                            runtime_adapter,
                            memory: memory_runtime,
                        },
                        crate::agent::turn_request::TurnDelivery {
                            cancellation: None,
                            on_delta: None,
                            on_tool_call: None,
                        },
                    )
                    .await
                    .map(|(outcome, trace)| (outcome.into_text(), trace))
                }),
            )
            .await;
            let (response, turn_trace) = match turn_result {
                Ok(result) => result,
                Err(error) => {
                    let provider_outcome = crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                        &route_decision,
                        provider_started_at,
                        &error,
                    );
                    if let Err(finalize_error) = crate::runtime::turn_terminal::commit_terminal(
                        terminal_guard.get(),
                        &memory_fabric,
                        crate::agent::terminal::TurnTerminalDraft {
                            decision: decision_settlement_snapshot(&decision_settlement),
                            terminal_id: turn_run_id.clone(),
                            scope: runtime_envelope.message_scope(),
                            status: crate::agent::terminal::TurnTerminalStatus::Failed,
                            history: None,
                            history_scope: None,
                            provider_outcome: Some(provider_outcome),
                            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                                summary: error.to_string(),
                                started_at: provider_started_at,
                                finished_at: chrono::Utc::now(),
                            },
                            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                        },
                        &config,
                    )
                    .await
                    {
                        tracing::warn!(error = %finalize_error, "Failed to commit failed single-shot agent terminal event");
                    }
                    return Err(error);
                }
            };
            {
                let decision = decision_context;
                anyhow::ensure!(
                    turn_trace.execution_trace.as_ref() == Some(&decision.terminal_execution_trace()),
                    "tool loop trace differs from the owning decision's execution facts"
                );
            }
            let provider_outcome =
                crate::agent::terminal::provider_outcome_from_trace(&route_decision, provider_started_at, turn_trace);
            if let Err(error) = crate::runtime::turn_terminal::commit_terminal(
                terminal_guard.get(),
                &memory_fabric,
                crate::agent::terminal::TurnTerminalDraft {
                    decision: decision_settlement_snapshot(&decision_settlement),
                    terminal_id: turn_run_id.clone(),
                    scope: runtime_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Completed,
                    history: Some(crate::agent::terminal::TurnHistoryProjection {
                        assistant_content: response.clone(),
                        history_commit_len: history.len(),
                    }),
                    history_scope: Some(
                        fabric_scope
                            .clone()
                            .with_sender(format!("{provider_name}/{model_name}"))
                            .with_recipient("local-user"),
                    ),
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "single-shot agent turn completed".to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                },
                &config,
            )
            .await
            {
                return Err(error);
            }
            if terminal_guard.get().is_none() && config.causal_tree.enabled {
                decision_service
                    .rejoin_terminal(&turn_run_id, decision_proof.owner_id())
                    .await?;
            }
            increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;
            final_output = response.clone();
            println!("{response}");
            observer.record_event(&ObserverEvent::TurnComplete);
            hooks
                .emit(
                    HookEvent::TurnComplete,
                    serde_json::json!({
                        "mode": "single",
                        "response_chars": response.chars().count(),
                    }),
                )
                .await;
            Ok::<(), anyhow::Error>(())
        });
        let single_result = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                if terminal_guard.get().is_none()
                    && let Some(settlement) = decision_settlement_snapshot(&decision_settlement)
                {
                let cancelled_scope = cancel_scope.lock().clone();
                if let Err(error) = crate::agent::terminal::finalize_turn(
                    &memory_fabric,
                    crate::agent::terminal::TurnTerminalDraft {
                        decision: Some(settlement),
                        terminal_id: cancel_turn_id,
                        scope: cancelled_scope,
                        status: crate::agent::terminal::TurnTerminalStatus::Cancelled,
                        history: None,
                        history_scope: None,
                        provider_outcome: None,
                        telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                            summary: "single-shot agent cancelled by shutdown signal".to_string(),
                            started_at: turn_started_at,
                            finished_at: chrono::Utc::now(),
                        },
                        delivery_intent: crate::agent::terminal::TurnDeliveryIntent::None,
                    },
                    &config.cost,
                    &config.workspace_dir,
                ).await {
                    tracing::warn!(error = %error, "Failed to commit cancelled single-shot agent terminal event");
                }
                }
                Err(anyhow::anyhow!("agent single-shot run cancelled by shutdown signal"))
            }
            result = single_shot => result,
        };
        if let (Err(error), Some(guard)) = (&single_result, terminal_guard.get()) {
            guard.finish_failure(error, shutdown.is_cancelled()).await?;
        }
        if let Err(error) = &single_result
            && let Some(mut guard) = cli_admission_guard.take()
        {
            guard
                .finish_failure(shutdown.is_cancelled(), &error.to_string())
                .await
                .map_err(|record_error| {
                    anyhow::anyhow!("CLI admission failed: {error}; failure persistence is unconfirmed: {record_error}")
                })?;
        }
        if single_result.is_err()
            && let Some(mut guard) = scheduled_input_guard.take()
        {
            guard
                .finish_failure(shutdown.is_cancelled())
                .await
                .map_err(|_| anyhow::anyhow!("scheduled admission failure persistence is unconfirmed"))?;
        }
        single_result?;
    } else {
        boxed_cli_interactive(async {
        println!("🦀 OpenPRX Interactive Mode");
        println!("Type /help for commands.\n");
        let cli = crate::channels::CliChannel::new();

        // Persistent conversation history across turns
        let mut history = if config.skill_rag.available() {
            Vec::new()
        } else {
            vec![ChatMessage::system(build_runtime_system_prompt(
                &config,
                model_name,
                &skills,
                native_tools,
            ))]
        };

        loop {
            print!("> ");
            let _ = std::io::stdout().flush();

            let mut input = String::new();
            match std::io::stdin().read_line(&mut input) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("\nError reading input: {e}\n");
                    break;
                }
            }

            let user_input = input.trim().to_string();
            if user_input.is_empty() {
                continue;
            }
            match user_input.as_str() {
                "/quit" | "/exit" => break,
                "/help" => {
                    println!("Available commands:");
                    println!("  /help        Show this help message");
                    println!("  /clear /new  Clear conversation history");
                    println!("  /quit /exit  Exit interactive mode\n");
                    continue;
                }
                "/clear" | "/new" => {
                    println!("This will clear the current conversation and delete all session memory.");
                    println!("Core memories (long-term facts/preferences) will be preserved.");
                    print!("Continue? [y/N] ");
                    let _ = std::io::stdout().flush();

                    let mut confirm = String::new();
                    if std::io::stdin().read_line(&mut confirm).is_err() {
                        continue;
                    }
                    if !matches!(confirm.trim().to_lowercase().as_str(), "y" | "yes") {
                        println!("Cancelled.\n");
                        continue;
                    }

                    history.clear();
                    if !config.skill_rag.available() {
                        history.push(ChatMessage::system(build_runtime_system_prompt(
                            &config,
                            model_name,
                            &skills,
                            native_tools,
                        )));
                    }
                    // Clear conversation and daily memory
                    let mut cleared = 0;
                    for category in [MemoryCategory::Conversation, MemoryCategory::Daily] {
                        let entries = mem.list(Some(&category), None).await.unwrap_or_default();
                        for entry in entries {
                            if mem.forget(&entry.key).await.unwrap_or(false) {
                                cleared += 1;
                            }
                        }
                    }
                    if cleared > 0 {
                        println!("Conversation cleared ({cleared} memory entries removed).\n");
                    } else {
                        println!("Conversation cleared.\n");
                    }
                    continue;
                }
                _ => {}
            }

            fabric_turn_seq += 1;
            // D8-3: one run_id per turn (was per-run). Generated at the turn entry
            // and used for both the message-event envelope and the user op-id.
            let turn_run_id = Uuid::new_v4().to_string();
            let runtime_envelope =
                RuntimeEnvelope::agent(memory_fabric.workspace_id().to_string(), turn_run_id.clone())
                    .with_recipient("cli:local-user")
                    .with_config_generation(&ctx.config_generation);
            let fabric_scope = runtime_envelope
                .message_scope()
                .with_recipient(format!("{provider_name}/{model_name}"));
            let owner = crate::runtime::turn_cleanup::current_tracker()
                .ok_or_else(|| anyhow::anyhow!("CLI input cleanup owner missing"))?;
            let (agent_user_event, mut admission_guard) = CliPendingInput {
                fabric: memory_fabric.clone(),
                generation: Arc::clone(&ctx.config_generation),
                envelope: runtime_envelope.clone(),
                scope: fabric_scope.clone(),
                choice: choice.clone(),
                input: user_input.clone(),
                key: format!("agent:{turn_run_id}:{fabric_turn_seq}:user"),
                ticket: owner.register(shutdown.clone())?,
            }
            .record()
            .await?;

            let decision_turn = {
                let service = ctx
                    .turn_decisions
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("interactive agent decision service is missing"))?;
                let proof = issue_cli_agent_turn_proof(
                    None,
                    &runtime_envelope,
                    &ctx.config_generation,
                    &turn_run_id,
                    &user_input,
                    agent_user_event.as_ref(),
                    choice,
                )?;
                let decision =
                    match service
                        .decide_agent_initial(&proof, crate::runtime::turn_decision::AgentTurnKind::Repl, shutdown)
                        .await
                    {
                        Ok(decision) => decision,
                        Err(error) => {
                            admission_guard.finish_failure(
                            shutdown.is_cancelled(), &error.to_string(),
                        ).await.map_err(|record_error| anyhow::anyhow!(
                            "CLI admission failed: {error}; failure persistence is unconfirmed: {record_error}"
                        ))?;
                            return Err(anyhow::Error::from(error));
                        }
                    };
                let retained_provider_trace = Arc::new(crate::agent::turn_contracts::RetainedProviderTrace::default());
                let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
                    provider_name,
                    model_name.to_string(),
                    runtime_envelope.resolved_owner_id(),
                    runtime_envelope.session_key.clone(),
                    runtime_envelope.source_message_event_id.clone(),
                    None,
                    "agent_cli_interactive",
                    u32::try_from(user_input.chars().count() / 4).unwrap_or(u32::MAX),
                    !tools_registry.is_empty(),
                    false,
                );
                let terminal_guard = crate::runtime::turn_terminal::TurnTerminalGuard::new(
                    admission_guard.into_terminal_ticket()?,
                    Arc::clone(service),
                    Arc::clone(&decision),
                    memory_fabric.clone(),
                    Arc::clone(&config),
                    runtime_envelope.clone(),
                    None,
                )?
                .with_provider_trace(Arc::clone(&retained_provider_trace), route_decision.clone());
                anyhow::ensure!(
                    !matches!(
                        decision.disposition()?,
                        crate::runtime::turn_decision::TurnDisposition::CollectApprovalArguments
                    ),
                    "ingress has no approval collection adapter"
                );
                if let crate::runtime::turn_decision::TurnDisposition::Close { reply } = decision.disposition()? {
                    if history.is_empty() {
                        history.push(ChatMessage::system(""));
                    }
                    let ports = CliDecisionClose {
                        terminal_guard: Some(&terminal_guard),
                        service,
                        fabric: &memory_fabric,
                        config: &config,
                        scope: runtime_envelope.message_scope(),
                        terminal_id: turn_run_id.clone(),
                        history_commit_len: history.len() + 2,
                        started_at: chrono::Utc::now(),
                    };
                    service
                        .close_agent_directive_turn(&proof, Arc::clone(&decision), &ports, shutdown)
                        .await?;
                    history.push(crate::agent::turn_contracts::inbound_user_message(&user_input));
                    history.push(ChatMessage::assistant(reply.as_ref()));
                    final_output = reply.to_string();
                    println!("{reply}\n");
                    observer.record_event(&ObserverEvent::TurnComplete);
                    hooks
                        .emit(
                            HookEvent::TurnComplete,
                            serde_json::json!({"mode":"interactive","response_chars":reply.chars().count()}),
                        )
                        .await;
                    continue;
                }
                (
                    service,
                    proof,
                    decision,
                    route_decision,
                    terminal_guard,
                    retained_provider_trace,
                )
            };
            let (
                decision_service,
                decision_proof,
                decision_context,
                route_decision,
                terminal_guard,
                retained_provider_trace,
            ) = &decision_turn;
            let decision_settlement = Some(decision_service.terminal_settlement_ref(Arc::clone(decision_context))?);

            // Auto-save conversation turns (skip short/trivial messages)
            if config.memory.should_auto_promote_user_message(&user_input) {
                let user_key = autosave_memory_key("user_msg");
                let _ = memory_fabric
                    .record_semantic_memory_from_event(
                        &user_key,
                        &user_input,
                        MemoryCategory::Conversation,
                        None,
                        agent_user_event.as_ref().map(|event| event.event_id.as_str()),
                        None,
                        None,
                    )
                    .await;
            }

            // Inject memory context into user message. The routing input is
            // pinned from `user_input` here, ahead of the enrichment below.
            let memory_runtime = cli_turn_memory_runtime(
                &mem,
                &config.workspace_dir,
                &runtime_envelope,
                &memory_fabric,
                &fabric_scope,
                agent_user_event.as_ref().map(|event| event.event_id.as_str()),
            );
            let mem_context = crate::agent::context_recall::RecalledMemoryContext {
                preamble: crate::agent::memory_loader::format_selected_context(None, decision_context.retrieved()?)?,
                ids: Vec::new(),
            };
            let context = mem_context.preamble.clone();
            let enriched = if context.is_empty() {
                user_input.clone()
            } else {
                format!("{context}{user_input}")
            };

            let selected_skills = decision_service.agent_prompt_skills(decision_proof, decision_context)?;
            let system_prompt = build_runtime_system_prompt(&config, model_name, &selected_skills, native_tools);
            if history.is_empty() {
                history.push(ChatMessage::system(system_prompt));
            } else if let Some(first) = history.first_mut() {
                *first = ChatMessage::system(system_prompt);
            }
            history.push(crate::agent::turn_contracts::inbound_user_message(&enriched));

            // D8-4: seed a turn-root spawn context so a sub-agent spawned from this
            // interactive agent turn inherits parent_run_id = the per-turn run_id.
            let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
                turn_run_id.clone(),
                format!("agent:{turn_run_id}"),
            );
            let provider_started_at = chrono::Utc::now();
            let mut runtime_adapter =
                configured_cli_tool_adapter(&ctx.security, &tools_registry, &approval_manager, &memory_runtime);
            {
                let proof = decision_proof;
                let mut envelope = runtime_envelope.clone();
                if let Some(event_id) = proof.source_event_id() {
                    envelope = envelope.with_source_message_event_id(event_id);
                }
                runtime_adapter.tool_execution_context = crate::tools::ToolExecutionContext::new(envelope, "private");
            }
            runtime_adapter.provider_trace = Some(Arc::clone(retained_provider_trace));
            let response = match crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                .scope(turn_spawn_ctx, async {
                    run_turn(
                        &mut history,
                        crate::agent::turn_request::TurnRequest {
                            channel_name: "cli",
                            scope: None,
                            mode: ChatMode::default(),
                            silent: false,
                            expose_stay_silent: false,
                            multimodal: &config.multimodal,
                            schedule: crate::agent::turn_request::TurnSchedule {
                                read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
                                priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
                                low_priority_tool_names: config.agent.low_priority_tools.clone(),
                            },
                            compaction: Some(decision_context.main_compaction()),
                            decision: crate::agent::turn_request::TurnDecision {
                                service: decision_service,
                                context: decision_context,
                                proof: decision_proof,
                            },
                        },
                        crate::agent::turn_request::TurnServices {
                            provider: provider.as_ref(),
                            provider_name,
                            model: model_name,
                            temperature,
                            tools_registry: Arc::clone(&tools_registry),
                            observer: observer.as_ref(),
                            hooks: &hooks,
                            approval: Some(Arc::clone(&approval_manager)),
                            approval_resolver: None,
                            runtime_adapter: Some(runtime_adapter),
                            memory: memory_runtime,
                        },
                        crate::agent::turn_request::TurnDelivery {
                            cancellation: Some(shutdown.child_token()),
                            on_delta: None,
                            on_tool_call: None,
                        },
                    )
                    .await
                    .map(|(outcome, trace)| (outcome.into_text(), trace))
                })
                .await
            {
                Ok((response, turn_trace)) => {
                    {
                        let decision = decision_context;
                        anyhow::ensure!(
                            turn_trace.execution_trace.as_ref() == Some(&decision.terminal_execution_trace()),
                            "interactive tool loop trace differs from its owning decision"
                        );
                    }
                    let provider_outcome = crate::agent::terminal::provider_outcome_from_trace(
                        &route_decision,
                        provider_started_at,
                        turn_trace,
                    );
                    if let Err(error) = crate::runtime::turn_terminal::commit_terminal(
                        Some(terminal_guard),
                        &memory_fabric,
                        crate::agent::terminal::TurnTerminalDraft {
                            decision: decision_settlement.clone(),
                            terminal_id: turn_run_id.clone(),
                            scope: runtime_envelope.message_scope(),
                            status: crate::agent::terminal::TurnTerminalStatus::Completed,
                            history: Some(crate::agent::terminal::TurnHistoryProjection {
                                assistant_content: response.clone(),
                                history_commit_len: history.len(),
                            }),
                            history_scope: Some(
                                fabric_scope
                                    .clone()
                                    .with_sender(format!("{provider_name}/{model_name}"))
                                    .with_recipient("local-user"),
                            ),
                            provider_outcome: Some(provider_outcome),
                            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                                summary: "interactive agent turn completed".to_string(),
                                started_at: provider_started_at,
                                finished_at: chrono::Utc::now(),
                            },
                            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                        },
                        &config,
                    )
                    .await
                    {
                        return Err(error);
                    }
                    response
                }
                Err(e) => {
                    let cancelled = crate::agent::turn_contracts::is_tool_loop_cancelled(&e);
                    let provider_outcome = crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                        &route_decision,
                        provider_started_at,
                        &e,
                    );
                    if let Err(finalize_error) = crate::runtime::turn_terminal::commit_terminal(
                        Some(terminal_guard),
                        &memory_fabric,
                        crate::agent::terminal::TurnTerminalDraft {
                            decision: decision_settlement.clone(),
                            terminal_id: turn_run_id.clone(),
                            scope: runtime_envelope.message_scope(),
                            status: if cancelled {
                                crate::agent::terminal::TurnTerminalStatus::Cancelled
                            } else {
                                crate::agent::terminal::TurnTerminalStatus::Failed
                            },
                            history: None,
                            history_scope: None,
                            provider_outcome: (!cancelled).then_some(provider_outcome),
                            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                                summary: e.to_string(),
                                started_at: provider_started_at,
                                finished_at: chrono::Utc::now(),
                            },
                            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                        },
                        &config,
                    )
                    .await
                    {
                        tracing::warn!(error = %finalize_error, "Failed to commit failed interactive agent terminal event");
                    }
                    if cancelled {
                        return Err(e);
                    }
                    eprintln!("\nError: {e}\n");
                    hooks
                        .emit(HookEvent::Error, payload_error("agent-turn", &e.to_string()))
                        .await;
                    continue;
                }
            };
            increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;
            final_output = response.clone();
            if let Err(e) = crate::channels::Channel::send(
                &cli,
                &crate::channels::traits::SendMessage::new(format!("\n{response}\n"), "user"),
            )
            .await
            {
                eprintln!("\nError sending CLI response: {e}\n");
            }
            observer.record_event(&ObserverEvent::TurnComplete);
            hooks
                .emit(
                    HookEvent::TurnComplete,
                    serde_json::json!({
                        "mode": "interactive",
                        "response_chars": response.chars().count(),
                    }),
                )
                .await;
        }
        Ok(())
        }).await?;
    }

    let duration = start.elapsed();
    observer.record_event(&ObserverEvent::AgentEnd {
        provider: provider_name.to_string(),
        model: model_name.to_string(),
        duration,
        tokens_used: None,
        cost_usd: None,
    });
    hooks
        .emit(
            HookEvent::AgentEnd,
            serde_json::json!({
                "duration_ms": duration.as_millis(),
                "tokens_used": serde_json::Value::Null,
            }),
        )
        .await;

    Ok::<String, anyhow::Error>(final_output)
}

fn resolve_agent_turn_envelope(
    override_envelope: Option<&RuntimeEnvelope>,
    workspace_id: &str,
    turn_run_id: &str,
) -> RuntimeEnvelope {
    let mut envelope = override_envelope
        .cloned()
        .unwrap_or_else(|| RuntimeEnvelope::agent(workspace_id.to_string(), turn_run_id.to_string()));
    if envelope.recipient.is_none() {
        envelope = envelope.with_recipient("cli:local-user");
    }
    envelope
}

#[path = "turn_tests/entry.rs"]
mod tests;

#[cfg(test)]
mod cli_admission_failure_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_admission_retries_without_model_terminal_or_secret() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.workspace_dir = dir.path().to_path_buf();
        config.config_path = dir.path().join("config.toml");
        let generation = crate::config::new_shared(config.clone()).pin();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(dir.path()).unwrap());
        let proof =
            test_recorded_cli_agent_turn_proof_with_input(&generation, memory.clone(), "prx input", "prx_run").await;
        let fabric = MemoryFabric::new(memory.clone(), dir.path().to_string_lossy());
        let summary = format!("cancelled {}", crate::agent::terminal::persistence_probe::PROBE_SECRET);
        for _ in 0..2 {
            proof
                .record_cli_admission_failure(&fabric, &config, true, &summary)
                .await
                .unwrap();
        }
        assert!(
            proof
                .record_cli_admission_failure(&fabric, &config, false, &summary)
                .await
                .is_err()
        );
        let event = memory
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
            .await
            .unwrap()
            .unwrap();
        assert!(event.id > 0);
        assert_eq!(event.causation_event_id.as_deref(), proof.source_event_id.as_deref());
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
