//! Shared terminal commit for provider/tool turns.
//!
//! Provider execution has one owner in `agent::turn_engine`; this module owns the
//! matching durable terminal boundary. Entry points supply their projection and
//! delivery intent, while this finalizer writes idempotent history/telemetry,
//! computes one usage settlement, and appends one `turn.finalized` commit marker.

pub(crate) mod binding;
pub(crate) mod logical;
pub(crate) mod oversized_reply;
pub(crate) mod recovery_commit;
use crate::config::schema::CostConfig;
use crate::cost::types::CostSettlement;
use crate::llm::route_decision::{MeteredTokenUsageRecord, ProviderExecutionOutcome};
use crate::memory::{MemoryFabric, MessageEvent, MessageEventScope};
use anyhow::Context;
pub use binding::TerminalBinding;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalStatus {
    Completed,
    Silent,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnDeliveryIntent {
    Reply { target: String },
    ReturnToCaller,
    Suppress { reason: String },
    Deferred { route: String },
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnHistoryProjection {
    pub assistant_content: String,
    pub history_commit_len: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnTerminalTelemetry {
    pub summary: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct TurnTerminalDraft {
    /// The execution-owned terminal binding; the owning guard fills it, and
    /// direct finalization rejects a draft without one.
    pub binding: Option<TerminalBinding>,
    pub terminal_id: String,
    pub scope: MessageEventScope,
    pub status: TurnTerminalStatus,
    pub history: Option<TurnHistoryProjection>,
    /// Optional message scope for the assistant history projection. Runtime
    /// telemetry may use a different channel/recipient scope.
    pub history_scope: Option<MessageEventScope>,
    pub provider_outcome: Option<ProviderExecutionOutcome>,
    pub telemetry: TurnTerminalTelemetry,
    pub delivery_intent: TurnDeliveryIntent,
}

/// A finalized model-turn proposal always carries service-issued authority.
#[derive(Debug, Clone)]
pub struct TurnTerminalCommit {
    /// Required execution-owned terminal binding.
    pub binding: TerminalBinding,
    pub terminal_id: String,
    pub scope: MessageEventScope,
    pub status: TurnTerminalStatus,
    pub history: Option<TurnHistoryProjection>,
    /// Optional message scope for the assistant history projection. Runtime
    /// telemetry may use a different channel/recipient scope.
    pub history_scope: Option<MessageEventScope>,
    pub provider_outcome: Option<ProviderExecutionOutcome>,
    pub telemetry: TurnTerminalTelemetry,
    pub delivery_intent: TurnDeliveryIntent,
}

impl TryFrom<TurnTerminalDraft> for TurnTerminalCommit {
    type Error = anyhow::Error;

    fn try_from(draft: TurnTerminalDraft) -> anyhow::Result<Self> {
        let binding = draft.binding.context("terminal draft has no terminal binding")?;
        Ok(Self {
            binding,
            terminal_id: draft.terminal_id,
            scope: draft.scope,
            status: draft.status,
            history: draft.history,
            history_scope: draft.history_scope,
            provider_outcome: draft.provider_outcome,
            telemetry: draft.telemetry,
            delivery_intent: draft.delivery_intent,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TurnTerminalReceipt {
    pub terminal_id: String,
    pub terminal_event: MessageEvent,
    pub assistant_event: Option<MessageEvent>,
    pub usage_settlement: Option<MeteredTokenUsageRecord>,
    pub cost_settlement: Option<CostSettlement>,
    pub delivery_intent: TurnDeliveryIntent,
}

impl TurnTerminalReceipt {
    /// The event id of the assistant `message.created` row this terminal
    /// stored (none when it stored no reply).
    #[must_use]
    pub fn assistant_event_id(&self) -> Option<String> {
        self.assistant_event.as_ref().map(|event| event.event_id.clone())
    }

    /// The turn's usage as a chat session records it, tagged with the class
    /// its cost settled as (CF-R8).
    #[must_use]
    pub fn into_session_usage(self) -> Option<MeteredTokenUsageRecord> {
        let class = self
            .cost_settlement
            .as_ref()
            .and_then(crate::llm::route_decision::UsageSettlementClass::of);
        self.usage_settlement.map(|mut usage| {
            usage.settlement_class = class;
            usage
        })
    }
}

/// Redaction for reply, error or summary text persisted from a turn (D-053,
/// D-054).
///
/// Every surface persists a turn's reply, its provider outcome and its error
/// text through [`finalize_turn`], which applies this policy once; the few
/// writes beside it on a turn path (a fallback projection when the terminal
/// commit fails, a conversation-turn row, a worker or task result record) use
/// these same functions, all backed by the one redactor
/// ([`crate::security::redaction`]). Redaction never truncates: a store that
/// caps what it keeps does so itself.
#[must_use]
pub(crate) fn redact_persisted_text(text: &str) -> String {
    crate::security::redaction::redact_secrets(text)
}

/// Redaction for a JSON payload persisted beside a turn: every string is
/// redacted and values under credential-named keys are replaced.
#[must_use]
pub(crate) fn redact_persisted_json(mut value: serde_json::Value) -> serde_json::Value {
    crate::security::redaction::redact_json_value(&mut value);
    value
}

#[must_use]
pub(crate) fn provider_outcome_from_trace(
    decision: &crate::llm::route_decision::RouteDecision,
    started_at: chrono::DateTime<chrono::Utc>,
    trace: crate::agent::turn_contracts::ToolLoopTrace,
) -> ProviderExecutionOutcome {
    let parameters = trace.prepared_parameters;
    let outcome = if trace.final_model.is_some() && !trace.attempts.is_empty() {
        let final_provider = trace
            .final_provider
            .unwrap_or_else(|| decision.selected.provider.clone());
        let final_model = trace.final_model.unwrap_or_else(|| decision.selected.model.clone());
        ProviderExecutionOutcome::from_trace_with_usage(
            decision,
            trace.attempts,
            final_provider,
            final_model,
            started_at,
            chrono::Utc::now(),
            trace.any_turn_had_fallback,
            trace.tokens_used,
        )
    } else {
        ProviderExecutionOutcome::success_for_decision_with_usage(decision, started_at, trace.tokens_used)
    };
    outcome.with_prepared_parameters(parameters)
}

#[must_use]
pub fn usage_settlement(
    terminal_id: &str,
    outcome: &ProviderExecutionOutcome,
    cost_config: &CostConfig,
) -> Option<MeteredTokenUsageRecord> {
    // The settlement is persisted (usage event, cost ledger, a chat session's
    // usage records when the terminal commit failed), so the serving label
    // loses its URL userinfo first; `finalize_turn` has already done so for its
    // own copy (D-054).
    MeteredTokenUsageRecord::from_parts(
        &crate::providers::redact_endpoint_userinfo(&outcome.final_provider),
        &outcome.final_model,
        &outcome.tokens_used,
        cost_config,
    )
    .map(|mut record| {
        record.settlement_id = Some(terminal_id.to_string());
        record
    })
}

#[derive(Debug, Serialize)]
struct TurnTerminalPayload<'a> {
    terminal_id: &'a str,
    decision_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_catalog_revision: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_catalog_content_sha256: Option<&'a str>,
    status: TurnTerminalStatus,
    history: &'a Option<TurnHistoryProjection>,
    history_scope: Option<serde_json::Value>,
    execution_trace: Option<&'a crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
    assistant_event_id: Option<&'a str>,
    provider_outcome: &'a Option<ProviderExecutionOutcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    prepared_parameters: &'a Vec<crate::providers::catalog::parameters::ResolvedParameters>,
    usage_settlement: &'a Option<MeteredTokenUsageRecord>,
    cost_settlement: &'a Option<CostSettlement>,
    telemetry: &'a TurnTerminalTelemetry,
    delivery_intent: &'a TurnDeliveryIntent,
    attempt_id: Option<&'a str>,
    lease_epoch: Option<i64>,
}

/// Stable persisted metadata covered by a terminal claim. Runtime authority
/// is validated separately and its callback is never serialized.
fn terminal_semantic_scope(scope: &MessageEventScope) -> serde_json::Value {
    serde_json::json!({
        "source": scope.source,
        "owner_id": scope.owner_id,
        "channel": scope.channel,
        "session_key": scope.session_key,
        "parent_session_key": scope.parent_session_key,
        "run_id": scope.run_id,
        "parent_run_id": scope.parent_run_id,
        "agent_id": scope.agent_id,
        "persona_id": scope.persona_id,
        "sender": scope.sender,
        "recipient": scope.recipient,
        "subject": scope.subject,
        "goal_id": scope.goal_id,
        "causation_event_id": scope.causation_event_id,
        "correlation_id": scope.correlation_id,
        "attempt_id": scope.attempt_id,
        "lease_epoch": scope.lease_epoch,
        "config_generation_id": scope.config_generation_id,
        "config_source_revision": scope.config_source_revision,
        "visibility": scope.visibility,
    })
}

/// The scope as every terminal row persists it (sender and recipient through
/// the provider-label redaction, as the memory fabric stores them), for
/// field-exact comparison with a stored row. Scope fields are row
/// identifiers and are never secret-redacted: a secret-shaped `session_key`
/// such as `console-<uuid>` must stay comparable with its row.
fn bound_scope(scope: &MessageEventScope) -> anyhow::Result<serde_json::Value> {
    let mut bound = terminal_semantic_scope(scope);
    let object = bound
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("terminal scope is not an object"))?;
    for (field, value) in [("sender", &scope.sender), ("recipient", &scope.recipient)] {
        let stored = value.clone().map(crate::memory::fabric::redact_provider_label);
        object.insert(field.to_owned(), serde_json::to_value(stored)?);
    }
    Ok(bound)
}

/// Finalize one semantic turn.
///
/// The `turn.finalized` marker is deliberately written last. A retry after a
/// partial failure replays assistant/provider writes through stable idempotency
/// keys, then closes the transaction with exactly one marker. The marker embeds
/// the sole metered usage settlement and the requested delivery disposition.
/// A linked decision retry must reproduce the same terminal facts, including
/// telemetry timestamps. A changed commit is rejected by its durable claim;
/// recovery from an already committed marker uses the decision rejoin reader.
async fn ensure_runtime_authority(scope: &MessageEventScope, phase: &'static str) -> anyhow::Result<()> {
    let Some(authority_guard) = scope.authority_guard.clone() else {
        return Ok(());
    };
    anyhow::ensure!(
        crate::runtime::blocking::spawn_blocking(move || authority_guard.validate())
            .await
            .context("runtime authority validation task failed")??,
        "runtime execution authority was lost during terminal finalization phase {phase}"
    );
    Ok(())
}

pub async fn finalize_turn<C>(
    fabric: &MemoryFabric,
    commit: C,
    cost_config: &CostConfig,
    workspace_dir: &Path,
) -> anyhow::Result<TurnTerminalReceipt>
where
    C: TryInto<TurnTerminalCommit>,
    C::Error: Into<anyhow::Error>,
{
    let commit = commit.try_into().map_err(Into::into)?;
    finalize_turn_impl(fabric, commit, cost_config, workspace_dir).await
}

// Construct terminal work before polling it. A single owned box keeps the
// finalizer state out of the nested entry, cleanup and terminal-guard futures.
// Conversion and rejection of an invalid draft remain in the lazy wrappers.
#[inline(never)]
fn boxed_terminal_work<'a, F, Fut, T>(
    make_work: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 'a>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>> + Send + 'a,
{
    Box::pin(make_work())
}

/// Key of the skill-lifecycle experience a terminal belongs to: the same
/// resolution `skill_read` applies to its trusted scope. A child (a scope with
/// a parent run) is always keyed by its own run, never by a task id it might
/// share with the parent, so its terminal cannot release the parent's pins.
fn skill_task_key(scope: &MessageEventScope) -> Option<String> {
    let task = match &scope.subject {
        Some(crate::memory::MessageEventSubject::Task(id)) => Some(id.as_str()),
        _ => None,
    };
    crate::skills::lifecycle::task_key(task, scope.run_id.as_deref(), scope.parent_run_id.as_deref())
}

/// Releases the run's skill revision pins when finalization ends by any path:
/// success, an early error (lost authority, scope or replay conflicts, I/O) or
/// the future being dropped. Releasing is in-memory and idempotent.
struct SkillPinGuard {
    owner: crate::skills::lifecycle::OwnerScope,
    task: String,
}

impl SkillPinGuard {
    fn new(scope: &MessageEventScope) -> Option<Self> {
        let owner = crate::skills::lifecycle::OwnerScope::new(scope.owner_id.as_deref()?).ok()?;
        Some(Self {
            owner,
            task: skill_task_key(scope)?,
        })
    }
}

impl Drop for SkillPinGuard {
    fn drop(&mut self) {
        crate::skills::lifecycle::release_task_pins(&self.owner, &self.task);
    }
}

/// Whether the turn ended waiting for an approval rather than with an answer.
/// Those turns are reported as `Completed` by their entrypoints, so the signal
/// comes from 02a ledger state: a tool attempt that is awaiting approval.
pub(super) fn skill_awaiting_approval(context: &crate::runtime::turn_context::TurnRuntimeContext) -> bool {
    context.terminal_execution_trace().awaiting_approval_attempts > 0
}

/// The runtime's terminal observations as the skill lifecycle takes them:
/// plain values only, no interpretation (the lifecycle derives infrastructure,
/// permission and adherence facts from them).
fn skill_observations(
    context: &crate::runtime::turn_context::TurnRuntimeContext,
    provider: Option<&ProviderExecutionOutcome>,
) -> crate::skills::lifecycle::TerminalObservations {
    let mut observations = observations_from(&context.terminal_execution_trace(), provider);
    observations.catalog_ids = Some(context.executable_capability_ids());
    observations
}

pub(crate) fn observations_from(
    trace: &crate::tools::capability::execution_trace::ExecutionTraceSnapshot,
    provider: Option<&ProviderExecutionOutcome>,
) -> crate::skills::lifecycle::TerminalObservations {
    use crate::llm::route_decision::ExecutionStatus;
    use crate::skills::lifecycle::{ObservedCallStatus, ObservedToolCall, ProviderEnd, TerminalObservations};
    // "Served" needs the execution trace to agree, whatever else reports a
    // success: every provider call observed and the last attempt a success. A
    // retained trace of an earlier round (`abort_commit` after a failed later
    // round) can read as a success while the run ended in an outage.
    let trace_served = !trace.provider_attempts_incomplete
        && trace.provider_pending == 0
        && trace
            .provider_attempts
            .as_ref()
            .and_then(|attempts| attempts.iter().rev().find(|attempt| attempt.status != "skipped"))
            .is_some_and(|attempt| attempt.status == "success");
    let provider = match provider.map(|outcome| &outcome.status) {
        Some(ExecutionStatus::AllFailed { last_error_class }) => ProviderEnd::from_failure_class(last_error_class),
        Some(ExecutionStatus::Success | ExecutionStatus::FallbackSuccess) | None if trace_served => ProviderEnd::Served,
        Some(ExecutionStatus::Success | ExecutionStatus::FallbackSuccess) | None => ProviderEnd::Unknown,
    };
    TerminalObservations {
        tool_calls: trace
            .calls
            .iter()
            .filter_map(|call| {
                Some(ObservedToolCall {
                    capability_id: call.capability_id.clone(),
                    status: ObservedCallStatus::from_trace_code(&call.status)?,
                })
            })
            .collect(),
        denied_attempts: trace.denied_attempts,
        unavailable_attempts: trace.unavailable_attempts.unwrap_or(0),
        awaiting_approval_attempts: trace.awaiting_approval_attempts,
        complete: !trace.incomplete && trace.pending_attempts == 0 && trace.unbound_attempts == 0,
        provider,
        catalog_ids: None,
    }
}

/// Task-terminal call-in of the skill lifecycle (see
/// `skills::lifecycle::settle_task_terminal`, shared with restart recovery). A
/// silent turn is not credited and a turn waiting for approval is `Blocked`,
/// never `Success`. Failures are logged and never fail finalization.
async fn release_skill_lifecycle(
    workspace_dir: &Path,
    owner: Option<String>,
    task_key: Option<String>,
    terminal_id: &str,
    status: TurnTerminalStatus,
    awaiting_approval: bool,
    root: bool,
    observations: crate::skills::lifecycle::TerminalObservations,
) {
    crate::skills::lifecycle::settle_task_terminal(
        workspace_dir,
        owner,
        task_key,
        terminal_id,
        crate::skills::lifecycle::TerminalObservation {
            root,
            ..status.skill_observation(awaiting_approval, true)
        },
        Some(observations),
    )
    .await;
}

impl TurnTerminalStatus {
    /// The skill lifecycle's view of this status; `verified` is `false` when
    /// the facts behind it were not observed first hand (restart recovery).
    pub(crate) const fn skill_observation(
        self,
        awaiting_approval: bool,
        verified: bool,
    ) -> crate::skills::lifecycle::TerminalObservation {
        use crate::skills::lifecycle::{TerminalKind, TerminalObservation};
        TerminalObservation {
            kind: match self {
                Self::Completed => TerminalKind::Completed,
                Self::Silent => TerminalKind::Silent,
                Self::Failed => TerminalKind::Failed,
                Self::Cancelled => TerminalKind::Cancelled,
            },
            awaiting_approval,
            verified,
            recovered: false,
            root: true,
        }
    }
}

async fn finalize_turn_impl(
    fabric: &MemoryFabric,
    commit: TurnTerminalCommit,
    cost_config: &CostConfig,
    workspace_dir: &Path,
) -> anyhow::Result<TurnTerminalReceipt> {
    // Keep pins across both phases, including errors and cancellation. The
    // preparation frame ends before projection is polled, rather than staying
    // nested beneath recovery persistence and its authenticated replay reads.
    let _skill_pins = SkillPinGuard::new(&commit.scope);
    let projection = boxed_terminal_work(move || async move {
        let mut commit = commit;
        anyhow::ensure!(!commit.terminal_id.trim().is_empty(), "terminal_id must not be empty");
        ensure_runtime_authority(&commit.scope, "start").await?;
        commit.binding.validate_terminal_scope(
            fabric,
            &commit.scope,
            commit.history_scope.as_ref(),
            workspace_dir,
            cost_config,
        )?;
        let prepared_parameters = commit.binding.runtime().prepared_main_parameters();
        if !prepared_parameters.is_empty()
            && let Some(outcome) = commit.provider_outcome.as_mut()
        {
            outcome.prepared_parameters.clone_from(&prepared_parameters);
        }
        binding::validate_silent_terminal(
            commit.binding.runtime().execution_trace.get(),
            commit.status,
            commit.provider_outcome.as_ref(),
        )?;
        // Preserve the gateway's response hash and byte identity before the
        // persistence redactor changes text. Only bounded digests are retained.
        let reply_reference = if fabric.records_recovery_terminal() {
            commit
                .history
                .as_ref()
                .map(|history| {
                    let model = &commit.binding.runtime().trusted.resolved_model_id;
                    oversized_reply::ReplyReference::new(&history.assistant_content, model)
                })
                .transpose()?
        } else {
            None
        };
        // Everything below persists the reply and the outcome (the assistant
        // message event, usage settlement, cost ledger, attempt events, the
        // terminal marker). Every surface finalizes here, so both are redacted
        // once, here, at write: the outcome's provider labels lose their URL
        // userinfo, and secrets in the reply and in every outcome string (provider
        // errors included) are replaced by the one redactor, which never
        // truncates (D-054).
        if let Some(outcome) = commit.provider_outcome.as_mut() {
            outcome.redact_provider_labels();
            *outcome = crate::security::redaction::redact_json_structure(&*outcome)?;
        }
        if let Some(history) = commit.history.as_mut() {
            history.assistant_content = redact_persisted_text(&history.assistant_content);
        }

        let usage_settlement = commit
            .provider_outcome
            .as_ref()
            .and_then(|outcome| usage_settlement(&commit.terminal_id, outcome, cost_config))
            .map(|mut record| {
                record.root_agent = commit
                    .scope
                    .agent_partition
                    .as_ref()
                    .map(crate::cost::types::RootAgent::from_partition);
                record
            });

        if fabric.records_recovery_terminal() {
            // Keep the recovery transaction future out of the enclosing
            // terminal future, including in nested child/cleanup callers.
            return Ok(boxed_terminal_work(move || {
                recovery_commit::finalize_recovery_terminal(
                    fabric,
                    commit,
                    cost_config,
                    workspace_dir,
                    recovery_commit::ReplyAndUsage {
                        reply_reference,
                        usage_settlement,
                    },
                )
            }));
        }

        // Recovery and legacy projection are mutually exclusive. Construct the
        // legacy future separately so its settlement/replay state does not
        // occupy the recovery caller's poll stack during nested child completion.
        Ok(boxed_terminal_work(move || async move {
            // The canonical usage event is written before the JSONL projection. A
            // crash can therefore leave usage awaiting projection, but can no longer
            // leave an untraceable cost-only settlement.
            if let Some(usage) = usage_settlement.as_ref() {
                let usage_json = serde_json::to_string(usage)?;
                fabric
                    .record_runtime_event_idempotent(
                        commit.scope.clone(),
                        "usage.settled",
                        format!("settlement_id={}", commit.terminal_id),
                        Some(usage_json),
                        format!("turn:{}:usage", commit.terminal_id),
                    )
                    .await?;
            }

            let cost_settlement = match usage_settlement.as_ref() {
                Some(usage) => {
                    let cost_config = cost_config.clone();
                    let workspace_dir = workspace_dir.to_path_buf();
                    let usage = usage.clone();
                    Some(
                        match crate::runtime::blocking::spawn_blocking(move || {
                            let tracker =
                                crate::cost::tracker::CostTracker::for_workspace(cost_config, &workspace_dir)?;
                            tracker.settle_metered(&usage)
                        })
                        .await
                        {
                            Ok(Ok(settlement)) => settlement,
                            Ok(Err(error)) => CostSettlement::Failed {
                                error: error.to_string(),
                            },
                            Err(error) => CostSettlement::Failed {
                                error: format!("cost settlement task failed: {error}"),
                            },
                        },
                    )
                }
                None => None,
            };
            ensure_runtime_authority(&commit.scope, "projection").await?;

            let assistant_event = match commit.history.as_ref() {
                Some(history) if commit.status == TurnTerminalStatus::Completed => Some(
                    fabric
                        .record_assistant_message_idempotent(
                            commit.history_scope.clone().unwrap_or_else(|| commit.scope.clone()),
                            history.assistant_content.clone(),
                            format!("turn:{}:assistant", commit.terminal_id),
                        )
                        .await?,
                ),
                _ => None,
            };

            if let Some(outcome) = commit.provider_outcome.as_ref() {
                crate::llm::route_decision::record_provider_outcome_events(fabric, commit.scope.clone(), outcome)
                    .await?;
            }

            let payload = TurnTerminalPayload {
                terminal_id: &commit.terminal_id,
                decision_id: None,
                model_catalog_revision: commit
                    .binding
                    .runtime()
                    .model_catalog()
                    .map(|catalog| catalog.revision.as_str()),
                model_catalog_content_sha256: commit
                    .binding
                    .runtime()
                    .model_catalog()
                    .map(crate::providers::catalog::ReleaseCatalog::content_sha256),
                status: commit.status,
                history: &commit.history,
                history_scope: commit.history_scope.as_ref().map(terminal_semantic_scope),
                execution_trace: None,
                assistant_event_id: assistant_event.as_ref().map(|event| event.event_id.as_str()),
                provider_outcome: &commit.provider_outcome,
                prepared_parameters: &prepared_parameters,
                usage_settlement: &usage_settlement,
                cost_settlement: &cost_settlement,
                telemetry: &commit.telemetry,
                delivery_intent: &commit.delivery_intent,
                attempt_id: commit.scope.attempt_id.as_deref(),
                lease_epoch: commit.scope.lease_epoch,
            };
            // The marker also carries the telemetry summary and the delivery intent,
            // which callers fill with error text (and, for sub-agents, the result
            // text): every string in it passes the same redactor.
            let payload_value = redact_persisted_json(serde_json::to_value(&payload)?);
            let payload_json = serde_json::to_string(&payload_value)?;
            ensure_runtime_authority(&commit.scope, "terminal-marker").await?;
            let skill_owner = commit.scope.owner_id.clone();
            let skill_task = skill_task_key(&commit.scope);
            let skill_root = commit.scope.parent_run_id.is_none();
            let terminal_event = fabric
                .record_runtime_event_idempotent(
                    commit.scope,
                    "turn.finalized",
                    format!("terminal_id={} status={:?}", commit.terminal_id, commit.status),
                    Some(payload_json),
                    format!("turn:{}:final", commit.terminal_id),
                )
                .await?;

            // After the durable marker and before the early return, so a retried
            // finalization joins and releases as well. Restart recovery
            // (`rejoin_terminal`, `rejoin_ingress_terminal`) reaches the same
            // lifecycle call-in from its own owner with the facts it can verify.
            let awaiting_approval = skill_awaiting_approval(commit.binding.runtime());
            let observations = skill_observations(commit.binding.runtime(), commit.provider_outcome.as_ref());
            if !awaiting_approval {
                commit.binding.runtime().retire_recursive_allowance();
            }
            release_skill_lifecycle(
                workspace_dir,
                skill_owner,
                skill_task,
                &commit.terminal_id,
                commit.status,
                awaiting_approval,
                skill_root,
                observations,
            )
            .await;
            let receipt = TurnTerminalReceipt {
                terminal_id: commit.terminal_id,
                terminal_event,
                assistant_event,
                usage_settlement,
                cost_settlement,
                delivery_intent: commit.delivery_intent,
            };
            Ok(receipt)
        }))
    })
    .await?;
    projection.await
}

/// Test support for the D-053 per-surface probes: a secret one surface's turn
/// persisted anywhere under its workspace.
#[cfg(test)]
pub(crate) mod persistence_probe {
    use std::path::Path;

    /// An AWS-key-shaped secret, as a provider may echo it back in a reply or
    /// an error.
    pub(crate) const PROBE_SECRET: &str = "AKIAIOSFODNN7EXAMPLE";

    /// What the redactor writes in place of a secret.
    const REDACTION_MARKER: &str = "[REDACTED]";

    fn walk(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir).expect("test: read a probed directory");
        for entry in entries {
            let path = entry.expect("test: read a probed directory entry").path();
            if path.is_dir() {
                walk(&path, files);
            } else {
                files.push(path);
            }
        }
    }

    /// Every file under `root` (SQLite databases and their WAL, cost ledgers,
    /// JSONL traces, session files) is free of [`PROBE_SECRET`], compared
    /// case-insensitively because full-text indexes store lowercased tokens.
    /// At least one file must hold the redaction marker, so a probe whose turn
    /// persisted nothing cannot pass vacuously.
    pub(crate) fn assert_nothing_under_persists_the_probe_secret(root: &Path) {
        let mut files = Vec::new();
        walk(root, &mut files);
        let needle = PROBE_SECRET.to_ascii_lowercase();
        let mut redacted_somewhere = false;
        for file in &files {
            let bytes = match std::fs::read(file) {
                Ok(bytes) => bytes,
                // SQLite may checkpoint and unlink a WAL or shared-memory
                // sidecar after directory enumeration. A path that no longer
                // exists cannot retain the probe secret; every surviving file
                // and the independent redaction witness remain mandatory.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("test: read probed file {}: {error}", file.display()),
            };
            let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
            assert!(!text.contains(&needle), "{} persisted the probe secret", file.display());
            redacted_somewhere |= String::from_utf8_lossy(&bytes).contains(REDACTION_MARKER);
        }
        assert!(
            redacted_somewhere,
            "no file under {} holds a redacted copy: the probe persisted nothing ({files:?})",
            root.display()
        );
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use tests::{
    bind_terminal_fixture, bind_terminal_fixture_mode, bind_terminal_parameter_fixture, completed_commit,
    priced_cost_config,
};
