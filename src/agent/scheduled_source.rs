//! Creator provenance supplied by the scheduler from its stored task row.
//! This admission does not approve a tool call or manufacture a scheduler lease.
use crate::runtime::envelope::{RuntimeEnvelope, RuntimeSource};
use crate::tools::execution::{JobOrigin, ScheduledToolScope};
use anyhow::{Context, Result};
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

tokio::task_local! {
    static SCHEDULED_ORIGIN: ScheduledAgentOrigin;
}

pub(crate) struct ScheduledAgentOrigin {
    source: RuntimeSource,
    task_id: String,
    owner_id: Option<String>,
    topic_id: Option<String>,
    source_event_id: Option<String>,
    origin: JobOrigin,
    claimed_attempt: Option<String>,
    retry_boundary: ScheduledRetryBoundary,
}

/// A read-only observation held by the scheduler, never a permission to replay
/// an admitted decision or any of its tool effects.
#[derive(Clone, Default)]
pub(crate) struct ScheduledRetryBoundary(Arc<AtomicBool>);

impl ScheduledRetryBoundary {
    pub(crate) fn may_retry_startup(&self) -> bool {
        !self.0.load(Ordering::Acquire)
    }
}

pub(super) fn begin_decision() -> Result<()> {
    SCHEDULED_ORIGIN
        .try_with(|origin| {
            anyhow::ensure!(
                !origin.retry_boundary.0.swap(true, Ordering::AcqRel),
                "scheduled invocation already entered its decision"
            );
            Ok(())
        })
        .context("scheduled decision has no owning invocation")?
}

impl ScheduledAgentOrigin {
    pub(crate) fn from_cron(job: &crate::cron::CronJob, claim: Option<&crate::cron::CronClaim>) -> Self {
        Self {
            source: RuntimeSource::Cron,
            task_id: job.id.clone(),
            owner_id: job.owner_id.clone(),
            topic_id: job.topic_id.clone(),
            source_event_id: job.source_message_event_id.clone(),
            origin: job.origin.clone(),
            claimed_attempt: claim.map(|claim| claim.attempt_id.clone()),
            retry_boundary: ScheduledRetryBoundary::default(),
        }
    }

    pub(crate) fn from_xin(task: &crate::xin::types::XinTask) -> Self {
        Self {
            source: RuntimeSource::Xin,
            task_id: task.id.clone(),
            owner_id: task.owner_id.clone(),
            topic_id: task.topic_id.clone(),
            source_event_id: task.source_message_event_id.clone(),
            origin: task.origin.clone(),
            claimed_attempt: None,
            retry_boundary: ScheduledRetryBoundary::default(),
        }
    }

    pub(crate) fn retry_boundary(&self) -> ScheduledRetryBoundary {
        self.retry_boundary.clone()
    }

    pub(crate) fn scope<T, F: Future<Output = Result<T>>>(self, work: F) -> impl Future<Output = Result<T>> {
        // Box before constructing the scheduled and cleanup wrapper futures.
        // The full agent entry state must not be copied into each owner frame.
        let work = Box::pin(work);
        async move {
            let boundary = self.retry_boundary.clone();
            let parent = crate::runtime::turn_cleanup::current_tracker();
            let cleanup = match crate::runtime::turn_cleanup::CleanupScope::new(
                parent.as_ref(),
                tokio_util::sync::CancellationToken::new(),
            ) {
                Ok(cleanup) => cleanup,
                Err(error) => {
                    boundary.0.store(true, Ordering::Release);
                    return Err(error);
                }
            };
            cleanup
                .run(SCHEDULED_ORIGIN.scope(self, work))
                .await
                .inspect_err(|error| {
                    if error
                        .downcast_ref::<crate::runtime::turn_cleanup::CleanupIncomplete>()
                        .is_some()
                    {
                        boundary.0.store(true, Ordering::Release);
                    }
                })
        }
    }

    fn validate(&self, envelope: &RuntimeEnvelope, scheduled: Option<&ScheduledToolScope>) -> Result<()> {
        anyhow::ensure!(
            !self.task_id.trim().is_empty()
                && envelope.source == self.source
                && envelope.task_id.as_deref() == Some(self.task_id.as_str())
                && envelope.session_key == format!("{}:{}", self.source.as_str(), self.task_id)
                && envelope.owner_id == self.owner_id
                && envelope.topic_id == self.topic_id
                && envelope.source_message_event_id == self.source_event_id
                && envelope.owner_isolated == self.origin.is_owner_isolated()
                && envelope.channel.as_deref() == Some(self.source.as_str())
                && envelope.sender.as_deref() == Some(self.source.as_str()),
            "scheduled input differs from its stored task origin"
        );
        let attempt = envelope.attempt_id.as_deref().context("scheduled attempt is missing")?;
        anyhow::ensure!(!attempt.is_empty(), "scheduled attempt is empty");
        match self.source {
            RuntimeSource::Cron => {
                anyhow::ensure!(
                    envelope.run_id.as_deref() == Some(format!("cron:{}:{attempt}", self.task_id).as_str())
                        && envelope.lease_epoch.is_none(),
                    "Cron run identity differs from its attempt"
                );
                if let Some(claimed) = &self.claimed_attempt {
                    anyhow::ensure!(
                        attempt == claimed && envelope.authority_guard.is_some(),
                        "Cron claim authority is missing or changed"
                    );
                } else {
                    anyhow::ensure!(
                        envelope.authority_guard.is_none()
                            && attempt.strip_prefix("immediate-").is_some_and(|id| {
                                uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
                            }),
                        "immediate Cron invocation has no valid source-owned identity"
                    );
                }
            }
            RuntimeSource::Xin => {
                let epoch = envelope.lease_epoch.context("Xin lease epoch is missing")?;
                anyhow::ensure!(
                    epoch >= 0
                        && attempt == format!("xin:{}:{epoch}", self.task_id)
                        && envelope.run_id.as_deref() == Some(attempt)
                        && envelope.authority_guard.is_some(),
                    "Xin run identity differs from its lease"
                );
            }
            _ => anyhow::bail!("unsupported scheduled source"),
        }
        anyhow::ensure!(
            envelope.visibility == crate::memory::MemoryVisibility::Workspace,
            "scheduled visibility differs from its admitted source"
        );
        let expected = self.origin.scheduled_tool_context(envelope.clone());
        anyhow::ensure!(
            match (expected.as_ref(), scheduled) {
                (None, None) => self.origin.is_local_operator(),
                (Some(expected), Some(actual)) => expected == &actual.context,
                _ => false,
            },
            "scheduled execution actor differs from its stored creator"
        );
        Ok(())
    }
}

/// Immutable admitted source and its creator-authorized execution actor.
#[derive(Clone)]
pub(super) struct ScheduledAdmission {
    envelope: RuntimeEnvelope,
    execution: crate::tools::ToolExecutionContext,
}

pub(super) struct ScheduledInput {
    pub(super) envelope: RuntimeEnvelope,
    pub(super) execution: crate::tools::ToolExecutionContext,
}

impl ScheduledAdmission {
    pub(super) const fn envelope(&self) -> &RuntimeEnvelope {
        &self.envelope
    }

    /// Validate persistence attribution only. In particular this does not
    /// renew a lease or manufacture execution authority during cleanup.
    async fn verify_input_record(
        &self,
        memory: &dyn crate::memory::Memory,
        generation: &crate::config::ConfigGeneration,
        envelope: &RuntimeEnvelope,
        input: (&str, &super::entry::AgentModelChoice),
        recorded: Option<&crate::memory::MessageEvent>,
    ) -> Result<Option<String>> {
        let (input, choice) = input;
        anyhow::ensure!(
            envelope == &self.envelope.clone().with_config_generation(generation)
                && envelope.workspace_id == generation.effective.workspace_dir.to_string_lossy(),
            "scheduled persistence source changed after admission"
        );
        let event_id = if let Some(event) = recorded.filter(|event| event.id > 0 && !event.event_id.is_empty()) {
            let scope = envelope.message_scope();
            let recipient = format!("{}/{}", choice.provider_name(), choice.model_name());
            let recipient = if crate::providers::byo_dialect_of(&recipient).is_some() {
                crate::providers::redact_endpoint_userinfo(&recipient)
            } else {
                std::borrow::Cow::Borrowed(recipient.as_str())
            };
            anyhow::ensure!(
                event.workspace_id == envelope.workspace_id
                    && event.owner_id.as_deref() == Some(envelope.resolved_owner_id().as_str())
                    && event.source == scope.source
                    && event.channel == scope.channel
                    && event.recipient.as_deref() == Some(recipient.as_ref())
                    && event.sender == scope.sender
                    && event.session_key == scope.session_key
                    && event.run_id == scope.run_id
                    && event.parent_run_id == scope.parent_run_id
                    && event.parent_session_key == scope.parent_session_key
                    && event.subject == scope.subject
                    && event.attempt_id == scope.attempt_id
                    && event.lease_epoch == scope.lease_epoch
                    && event.agent_id == scope.agent_id
                    && event.persona_id == scope.persona_id
                    && event.causation_event_id == scope.causation_event_id
                    && event.goal_id == scope.goal_id
                    && event.correlation_id == scope.correlation_id
                    && event.config_generation_id == Some(generation.id.0)
                    && event.config_source_revision.as_deref()
                        == Some(generation.source_revision.fingerprint_sha256.as_str())
                    && event.visibility == scope.visibility
                    && event.role == "user"
                    && event.event_type == "message.created"
                    && event.content == input,
                "scheduled inbound event differs from the admitted task"
            );
            let durable = memory
                .find_owned_message_event(&envelope.workspace_id, &envelope.resolved_owner_id(), &event.event_id)
                .await?
                .context("scheduled inbound event is not durable")?;
            anyhow::ensure!(
                serde_json::to_value(&durable)? == serde_json::to_value(event)?,
                "scheduled source receipt differs from durable input"
            );
            Some(event.event_id.clone())
        } else {
            anyhow::ensure!(
                !generation.effective.causal_tree.enabled,
                "enabled scheduled decision requires durable inbound input"
            );
            None
        };
        Ok(event_id)
    }

    pub(super) async fn record_guarded(
        &self,
        fabric: crate::memory::MemoryFabric,
        generation: Arc<crate::config::ConfigGeneration>,
        input: (String, String, super::entry::AgentModelChoice),
        ticket: crate::runtime::turn_cleanup::CleanupTicket,
    ) -> Result<(Option<crate::memory::MessageEvent>, ScheduledAdmissionGuard)> {
        let fabric = fabric.with_ingress_recovery(&generation.effective, &ticket).await?;
        let (input, key, choice) = input;
        let request = ScheduledPendingInput {
            admission: self.clone(),
            fabric,
            generation,
            input,
            key,
            choice,
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), request.record(false))
                .await
                .map_err(|_| anyhow::anyhow!("scheduled input persistence timed out"))
                .and_then(std::convert::identity);
            let result = match result {
                Ok((event, event_id)) => Ok((
                    event,
                    ScheduledAdmissionGuard {
                        request,
                        event_id,
                        ticket: Some(ticket),
                        pending: None,
                    },
                )),
                Err(_error) if request.fabric.ingress_already_accepted() => {
                    ticket.finish(true);
                    Err(crate::runtime::ingress_recovery::AlreadyAccepted.into())
                }
                Err(error) => {
                    match tokio::time::timeout(std::time::Duration::from_secs(15), request.record(true)).await {
                        Ok(Ok((_, event_id))) => {
                            let mut guard = ScheduledAdmissionGuard {
                                request,
                                event_id,
                                ticket: Some(ticket),
                                pending: None,
                            };
                            let _ = tokio::time::timeout(
                                std::time::Duration::from_secs(15),
                                guard.finish_failure(sender.is_closed()),
                            )
                            .await;
                        }
                        _ => ticket.finish(false),
                    }
                    Err(error)
                }
            };
            let _ = sender.send(result);
        });
        receiver
            .await
            .context("scheduled input owner ended without a receipt")?
    }

    pub(super) async fn bind_input(
        &self,
        memory: &dyn crate::memory::Memory,
        generation: &crate::config::ConfigGeneration,
        envelope: &RuntimeEnvelope,
        input: (&str, &super::entry::AgentModelChoice),
        recorded: Option<&crate::memory::MessageEvent>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<ScheduledInput> {
        let (input, choice) = input;
        anyhow::ensure!(
            choice.factory_credential_source_matches(&generation.effective),
            "scheduled provider selection changed"
        );
        let mut expected = self.envelope.clone().with_config_generation(generation);
        anyhow::ensure!(envelope == &expected, "scheduled source changed after admission");
        anyhow::ensure!(
            envelope.workspace_id == generation.effective.workspace_dir.to_string_lossy(),
            "scheduled workspace differs from the pinned configuration"
        );
        if let Some(guard) = envelope.authority_guard.clone() {
            let current = tokio::select! {
                biased;
                () = cancellation.cancelled() => anyhow::bail!("scheduled input binding cancelled"),
                result = crate::runtime::blocking::spawn_blocking(move || guard.validate()) => {
                    result.context("scheduled authority validation task failed")??
                }
            };
            anyhow::ensure!(current, "scheduled authority expired before decision");
        }
        let event_id = self
            .verify_input_record(memory, generation, envelope, (input, choice), recorded)
            .await?;
        anyhow::ensure!(!cancellation.is_cancelled(), "scheduled input binding cancelled");
        expected.source_message_event_id = event_id.clone();
        let mut execution = self.execution.clone();
        execution.envelope = execution.envelope.with_config_generation(generation);
        execution.envelope.source_message_event_id = event_id.clone();
        execution.semantic_turn_id = event_id
            .or_else(|| envelope.run_id.clone())
            .context("scheduled execution has no turn identity")?;
        Ok(ScheduledInput {
            envelope: expected,
            execution,
        })
    }
}

struct ScheduledPendingInput {
    admission: ScheduledAdmission,
    fabric: crate::memory::MemoryFabric,
    generation: Arc<crate::config::ConfigGeneration>,
    input: String,
    key: String,
    choice: super::entry::AgentModelChoice,
}
impl ScheduledPendingInput {
    async fn record(&self, recover: bool) -> Result<(Option<crate::memory::MessageEvent>, Option<String>)> {
        let envelope = self.admission.envelope.clone().with_config_generation(&self.generation);
        let event = if recover {
            Some(
                self.fabric
                    .memory()
                    .find_owned_message_event_by_idempotency_key(
                        self.fabric.workspace_id(),
                        &envelope.resolved_owner_id(),
                        &self.key,
                    )
                    .await?
                    .context("scheduled input commit is unconfirmed")?,
            )
        } else {
            let scope = envelope.message_scope().with_recipient(format!(
                "{}/{}",
                self.choice.provider_name(),
                self.choice.model_name()
            ));
            self.fabric
                .record_inbound_user_message(scope, self.input.clone(), Some(self.key.clone()), None)
                .await
                .map_or_else(
                    |_| {
                        tracing::warn!("scheduled input persistence is unconfirmed");
                        None
                    },
                    Some,
                )
        };
        if let Some(event) = event.as_ref().filter(|event| event.id > 0) {
            anyhow::ensure!(
                event.idempotency_key.as_deref() == Some(self.key.as_str()),
                "scheduled input key changed during persistence"
            );
        }
        let id = self
            .admission
            .verify_input_record(
                self.fabric.memory().as_ref(),
                &self.generation,
                &envelope,
                (&self.input, &self.choice),
                event.as_ref(),
            )
            .await?;
        Ok((event, id))
    }

    async fn record_failure(&self, id: Option<&str>, cancelled: bool) -> Result<()> {
        if !self.generation.effective.memory.should_record_user_message_event() {
            return Ok(());
        }
        let id = id.context("scheduled admission input is missing")?;
        let envelope = self.admission.envelope.clone().with_config_generation(&self.generation);
        let event = self
            .fabric
            .memory()
            .find_owned_message_event(self.fabric.workspace_id(), &envelope.resolved_owner_id(), id)
            .await?
            .context("scheduled admission input is not durable")?;
        anyhow::ensure!(
            event.event_id == id && event.idempotency_key.as_deref() == Some(self.key.as_str()),
            "scheduled admission input identity changed"
        );
        self.admission
            .verify_input_record(
                self.fabric.memory().as_ref(),
                &self.generation,
                &envelope,
                (&self.input, &self.choice),
                Some(&event),
            )
            .await?;
        let run = envelope.run_id.clone().context("scheduled admission run missing")?;
        let scope = envelope.with_source_message_event_id(id).message_scope();
        let summary = "scheduled input admission did not complete";
        let payload = serde_json::json!({ "run_id": run, "source_event_id": id,
            "status": if cancelled { "cancelled" } else { "failed" }, "summary": summary });
        let result = self
            .fabric
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
                && result.workspace_id == self.fabric.workspace_id()
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
            "scheduled admission failure retry conflicts with source or status"
        );
        let durable = self
            .fabric
            .memory()
            .find_owned_message_event(
                self.fabric.workspace_id(),
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
}

pub(super) struct ScheduledAdmissionGuard {
    request: ScheduledPendingInput,
    event_id: Option<String>,
    ticket: Option<crate::runtime::turn_cleanup::CleanupTicket>,
    pending: Option<bool>,
}
impl ScheduledAdmissionGuard {
    pub(super) fn into_terminal_ticket(mut self) -> Result<crate::runtime::turn_cleanup::CleanupTicket> {
        anyhow::ensure!(self.pending.is_none(), "scheduled admission failure is already pending");
        self.ticket.take().context("scheduled admission cleanup ticket missing")
    }

    pub(super) async fn finish_failure(&mut self, cancelled: bool) -> Result<()> {
        anyhow::ensure!(self.pending.is_none(), "scheduled admission failure is already pending");
        self.pending = Some(cancelled);
        let result = self.request.record_failure(self.event_id.as_deref(), cancelled).await;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(result.is_ok());
        }
        result
    }
}
impl Drop for ScheduledAdmissionGuard {
    fn drop(&mut self) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            ticket.finish(false);
            return;
        };
        let request = ScheduledPendingInput {
            admission: self.request.admission.clone(),
            fabric: self.request.fabric.clone(),
            generation: Arc::clone(&self.request.generation),
            input: self.request.input.clone(),
            key: self.request.key.clone(),
            choice: self.request.choice.clone(),
        };
        let id = self.event_id.clone();
        let cancelled = self.pending.unwrap_or(true);
        runtime.spawn(async move {
            ticket.finish(matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    request.record_failure(id.as_deref(), cancelled)
                )
                .await,
                Ok(Ok(()))
            ));
        });
    }
}

/// Called before Agent bootstrap. A missing extra tool scope is meaningful
/// only when an ingress-owned stored LocalOperator origin confirms it.
pub(super) async fn validate_current(
    envelope: Option<&RuntimeEnvelope>,
    scheduled: Option<&ScheduledToolScope>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Option<ScheduledAdmission>> {
    match envelope {
        Some(envelope) if matches!(envelope.source, RuntimeSource::Cron | RuntimeSource::Xin) => {
            SCHEDULED_ORIGIN
                .try_with(|origin| origin.validate(envelope, scheduled))
                .map_err(|_| anyhow::anyhow!("scheduled agent invocation has no stored creator admission"))??;
            if let Some(guard) = envelope.authority_guard.clone() {
                let current = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => anyhow::bail!("scheduled admission cancelled"),
                    result = crate::runtime::blocking::spawn_blocking(move || guard.validate()) => {
                        result.context("scheduled authority validation task failed")??
                    }
                };
                anyhow::ensure!(current, "scheduled claim or lease is no longer current");
            }
            anyhow::ensure!(!cancellation.is_cancelled(), "scheduled admission cancelled");
            let execution = scheduled.map_or_else(
                || {
                    // LocalOperator was verified against the stored origin above.
                    // Preserve its existing CLI execution tuple and mirror rights,
                    // while retaining the scheduled run/task/lease lineage.
                    let mut actor = envelope.clone();
                    actor.owner_id = Some(envelope.resolved_owner_id());
                    actor.source = RuntimeSource::Chat;
                    actor.channel = Some("cli".to_owned());
                    actor.sender = Some("agent".to_owned());
                    crate::tools::ToolExecutionContext::new(actor, "private").with_chat_id("agent:local")
                },
                |scheduled| scheduled.context.clone(),
            );
            let mut source = execution.envelope.clone();
            source.source = envelope.source;
            source.owner_id = Some(execution.envelope.resolved_owner_id());
            Ok(Some(ScheduledAdmission {
                envelope: source,
                execution,
            }))
        }
        _ => {
            anyhow::ensure!(
                scheduled.is_none() && SCHEDULED_ORIGIN.try_with(|_| ()).is_err(),
                "scheduled creator admission requires its task runtime envelope"
            );
            Ok(None)
        }
    }
}
