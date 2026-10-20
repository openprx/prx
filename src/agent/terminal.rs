//! Shared terminal commit for provider/tool turns.
//!
//! Provider execution has one owner in `agent::turn_engine`; this module owns the
//! matching durable terminal boundary. Entry points supply their projection and
//! delivery intent, while this finalizer writes idempotent history/telemetry,
//! computes one usage settlement, and appends one `turn.finalized` commit marker.

pub(crate) mod logical;
mod replay;
pub(crate) mod served_auxiliary;
pub(crate) use replay::{VerifiedTerminalReplay, verify_terminal_replay, verify_terminal_replay_until};

use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::outcome::{OutcomeCorrelator, TerminalFacts};
use crate::causal_tree::store::{DecisionScope, DecisionStoreReader, DecisionStoreWriter, Enqueue};
use crate::config::schema::CostConfig;
use crate::cost::types::CostSettlement;
use crate::decision::{DecisionKey, KeyDomain};
use crate::llm::route_decision::{MeteredTokenUsageRecord, ProviderExecutionOutcome};
use crate::memory::{Memory, MemoryFabric, MessageEvent, MessageEventScope};
use crate::runtime::turn_decision::{SettlementIssueToken, TurnDecisionContext};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// The owning guard fills omitted authority before sealing this draft.
    /// Direct finalization rejects an omitted link before any writes.
    pub decision: Option<DecisionSettlementRef>,
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
    /// Required service-issued source binding, including disabled turns.
    pub decision: DecisionSettlementRef,
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
        Ok(Self {
            decision: draft
                .decision
                .context("terminal draft has no service-issued decision")?,
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

#[derive(Clone)]
pub struct DecisionSettlementRef {
    parent_grant: Option<Arc<crate::runtime::child_source::IssuedChildGrant>>,
    operator_parent: Option<Arc<crate::runtime::child_source::parent::ParentSource>>,
    decision_id: DecisionId,
    context: Arc<TurnDecisionContext>,
    recorded: Option<RecordedDecisionSettlement>,
    workspace_dir: PathBuf,
    cost_config: CostConfig,
    record_redacted_projection: bool,
    memory: Arc<dyn Memory>,
}

#[derive(Clone)]
struct RecordedDecisionSettlement {
    writer: DecisionStoreWriter,
    reader: DecisionStoreReader,
    store_workspace_id: Arc<str>,
    key: Arc<DecisionKey>,
}

impl std::fmt::Debug for DecisionSettlementRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionSettlementRef")
            .field("decision_id", &self.decision_id)
            .finish_non_exhaustive()
    }
}

impl DecisionSettlementRef {
    /// Whether this link can participate in durable decision replay/outcomes.
    /// Disabled source binding alone never grants that authority.
    pub(crate) const fn is_recorded(&self) -> bool {
        self.recorded.is_some()
    }

    /// A terminal guard may reuse only its own issued context, never another
    /// decision with a caller-selected identifier or matching scope strings.
    pub(crate) fn same_context(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.context, &other.context)
    }

    pub(crate) fn issued(
        context: Arc<TurnDecisionContext>,
        writer: DecisionStoreWriter,
        reader: DecisionStoreReader,
        store_workspace_id: Arc<str>,
        key: Arc<DecisionKey>,
        workspace_dir: PathBuf,
        cost_config: CostConfig,
        record_redacted_projection: bool,
        memory: Arc<dyn Memory>,
        _token: SettlementIssueToken,
    ) -> Self {
        Self {
            parent_grant: None,
            operator_parent: None,
            decision_id: context.decision_id(),
            context,
            recorded: Some(RecordedDecisionSettlement {
                writer,
                reader,
                store_workspace_id,
                key,
            }),
            workspace_dir,
            cost_config,
            record_redacted_projection,
            memory,
        }
    }

    /// A disabled turn retains its source binding without any store writer,
    /// reader or replay key. It cannot claim a durable decision outcome.
    pub(crate) fn issued_disabled(
        context: Arc<TurnDecisionContext>,
        workspace_dir: PathBuf,
        cost_config: CostConfig,
        memory: Arc<dyn Memory>,
        _token: SettlementIssueToken,
    ) -> Self {
        Self {
            parent_grant: None,
            operator_parent: None,
            decision_id: context.decision_id(),
            context,
            recorded: None,
            workspace_dir,
            cost_config,
            record_redacted_projection: false,
            memory,
        }
    }

    pub(crate) fn with_parent_grant(
        mut self,
        grant: Option<Arc<crate::runtime::child_source::IssuedChildGrant>>,
    ) -> Self {
        self.parent_grant = grant;
        self
    }

    pub(crate) fn with_operator_parent(
        mut self,
        parent: Option<Arc<crate::runtime::child_source::parent::ParentSource>>,
    ) -> Self {
        self.operator_parent = parent;
        self
    }

    fn validate_terminal_scope(
        &self,
        fabric: &MemoryFabric,
        scope: &MessageEventScope,
        history_scope: Option<&MessageEventScope>,
        workspace_dir: &Path,
        cost_config: &CostConfig,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.memory, &fabric.memory()),
            "decision terminal memory does not match the pinned service"
        );
        let trusted = self
            .context
            .audit_hops()
            .last()
            .context("decision has no final hop")?
            .state
            .trusted
            .parts();
        anyhow::ensure!(self.decision_id == self.context.decision_id(), "decision link changed");
        anyhow::ensure!(
            workspace_dir == self.workspace_dir && cost_config == &self.cost_config,
            "decision settlement workspace or cost configuration does not match the pinned turn"
        );
        anyhow::ensure!(
            trusted.workspace_id.as_ref() == fabric.workspace_id(),
            "decision terminal workspace does not match the pinned turn"
        );
        // Both writes retain the recorded ingress identity and visibility. Sender
        // and recipient are presentation labels and may differ for history.
        for write_scope in [scope, history_scope.unwrap_or(scope)] {
            anyhow::ensure!(
                crate::runtime::turn_source::ingress_event_surface(
                    &write_scope.source,
                    write_scope.channel.as_deref(),
                    &write_scope.visibility
                ) == Some(trusted.surface)
                    && write_scope.channel.as_deref() == Some(self.context.source_channel())
                    && write_scope.owner_id.as_deref() == trusted.owner_id.as_deref()
                    && write_scope.session_key.as_deref() == trusted.session_key.as_deref()
                    && write_scope.run_id.as_deref() == trusted.run_id.as_deref()
                    && crate::runtime::turn_source::subject_task_id(write_scope.subject.as_ref())
                        == trusted.task_id.as_deref(),
                "decision terminal or history scope does not match the pinned turn"
            );
        }
        Ok(())
    }

    pub(crate) async fn validate_durable_chain(&self) -> anyhow::Result<()> {
        let recorded = self
            .recorded
            .as_ref()
            .context("disabled decision has no durable settlement")?;
        anyhow::ensure!(
            recorded.writer.drain(Instant::now() + Duration::from_secs(5)).await,
            "decision chain did not reach durable storage before terminal claim"
        );
        let trusted = self
            .context
            .audit_hops()
            .last()
            .context("decision has no final hop")?
            .state
            .trusted
            .parts();
        let scope = DecisionScope::Owner {
            workspace_id: Arc::clone(&recorded.store_workspace_id),
            owner_id: trusted.owner_id.clone().context("decision has no owner")?,
        };
        for (position, id) in self.context.hop_ids().iter().enumerate() {
            let view = recorded
                .reader
                .get(&scope, id)?
                .context("queued decision hop was not durably stored")?;
            anyhow::ensure!(
                view.enabled_snapshot
                    && !view.legacy_v1
                    && view.prior_decision_id
                        == position
                            .checked_sub(1)
                            .and_then(|n| self.context.hop_ids().get(n))
                            .copied()
                    && view.session_key.as_deref() == trusted.session_key.as_deref()
                    && view.run_id.as_deref() == trusted.run_id.as_deref()
                    && view.channel.is_none()
                    && view.policy_version == trusted.policy_version.as_ref()
                    && view.decision_id == *id,
                "durable decision chain does not match the pinned turn"
            );
        }
        Ok(())
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
    if trace.final_model.is_some() && !trace.attempts.is_empty() {
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
    }
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
    status: TurnTerminalStatus,
    history: &'a Option<TurnHistoryProjection>,
    history_scope: Option<serde_json::Value>,
    execution_trace: Option<&'a crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
    assistant_event_id: Option<&'a str>,
    provider_outcome: &'a Option<ProviderExecutionOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    served_auxiliary: Option<&'a served_auxiliary::ServedAuxiliaryProjection>,
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
    finalize_turn_impl(fabric, commit, cost_config, workspace_dir, false).await
}

#[cfg(test)]
pub(crate) async fn finalize_turn_stop_after_marker<C>(
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
    finalize_turn_impl(fabric, commit, cost_config, workspace_dir, true).await
}

// Construct terminal work before polling it. A single owned box keeps the
// finalizer state out of the nested entry, cleanup and terminal-guard futures.
// Conversion and rejection of an invalid draft remain in the lazy wrappers.
#[inline(never)]
fn boxed_terminal_work<'a, F, Fut>(
    make_work: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<TurnTerminalReceipt>> + Send + 'a>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<TurnTerminalReceipt>> + Send + 'a,
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

/// How long finalization waits for the skill lifecycle call-in (the capture
/// locks are bounded to about a second on their own).
const SKILL_TERMINAL_BUDGET: Duration = Duration::from_millis(1500);

/// Whether the turn ended waiting for an approval rather than with an answer.
/// Those turns are reported as `Completed` by their entrypoints, so the signal
/// comes from the decision: an approval directive, an approval collection, or a
/// tool attempt that is awaiting approval.
fn skill_awaiting_approval(
    decision: &DecisionSettlementRef,
    trace: Option<&crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
) -> bool {
    use crate::causal_tree::directive::TurnExecutionDirective;
    let context = &decision.context;
    matches!(context.directive(), TurnExecutionDirective::RequestApproval { .. })
        || context.approval_collection_proof().is_some()
        || trace.map_or_else(
            || context.terminal_execution_trace().awaiting_approval_attempts,
            |trace| trace.awaiting_approval_attempts,
        ) > 0
}

/// The runtime's terminal observations as the skill lifecycle takes them:
/// plain values only, no interpretation (the lifecycle derives infrastructure,
/// permission and adherence facts from them).
fn skill_observations(
    decision: &DecisionSettlementRef,
    trace: Option<&crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
    provider: Option<&ProviderExecutionOutcome>,
) -> crate::skills::lifecycle::TerminalObservations {
    trace.map_or_else(
        || observations_from(&decision.context.terminal_execution_trace(), provider),
        |trace| observations_from(trace, provider),
    )
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
    }
}

/// Task-terminal call-in of the skill lifecycle: joins the terminal to the
/// task's skill experience and releases the task's revision pins. A silent turn
/// is not credited (pins are released, nothing is joined) and a turn waiting
/// for approval is `Blocked`, never `Success`. Failures and a missed time
/// budget are logged and never fail finalization.
async fn release_skill_lifecycle(
    workspace_dir: &Path,
    owner: Option<String>,
    task_key: Option<String>,
    terminal_id: &str,
    status: TurnTerminalStatus,
    awaiting_approval: bool,
    observations: crate::skills::lifecycle::TerminalObservations,
) {
    use crate::skills::lifecycle::{ExperienceOutcome, OwnerScope};
    let (Some(owner), Some(task)) = (owner, task_key) else {
        return;
    };
    let owner = match OwnerScope::new(&owner) {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(%error, "skill lifecycle terminal skipped: invalid owner scope");
            return;
        }
    };
    let outcome = match status {
        TurnTerminalStatus::Completed if awaiting_approval => Some(ExperienceOutcome::Blocked),
        TurnTerminalStatus::Completed => Some(ExperienceOutcome::Success),
        TurnTerminalStatus::Failed => Some(ExperienceOutcome::Failure),
        TurnTerminalStatus::Cancelled => Some(ExperienceOutcome::Cancelled),
        TurnTerminalStatus::Silent => None,
    };
    let workspace = workspace_dir.to_path_buf();
    let terminal_id = terminal_id.to_string();
    let call_in = tokio::task::spawn_blocking(move || {
        let Some(outcome) = outcome else {
            crate::skills::lifecycle::release_task_pins(&owner, &task);
            return Ok(());
        };
        crate::skills::lifecycle::on_task_terminal(
            &workspace,
            &owner,
            &crate::skills::lifecycle::TaskTerminal {
                task_id: &task,
                terminal_receipt_id: &terminal_id,
                outcome,
                observations: Some(&observations),
            },
        )
        .map(|_| ())
    });
    match tokio::time::timeout(SKILL_TERMINAL_BUDGET, call_in).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => tracing::warn!(%error, "skill lifecycle terminal join failed"),
        Ok(Err(error)) => tracing::warn!(%error, "skill lifecycle terminal task failed"),
        Err(_) => tracing::warn!("skill lifecycle terminal join exceeded its time budget; continuing"),
    }
}

async fn finalize_turn_impl(
    fabric: &MemoryFabric,
    commit: TurnTerminalCommit,
    cost_config: &CostConfig,
    workspace_dir: &Path,
    stop_after_marker: bool,
) -> anyhow::Result<TurnTerminalReceipt> {
    boxed_terminal_work(move || async move {
        // Held for the whole finalization: every exit releases the run's pins.
        let _skill_pins = SkillPinGuard::new(&commit.scope);
        anyhow::ensure!(!commit.terminal_id.trim().is_empty(), "terminal_id must not be empty");
        ensure_runtime_authority(&commit.scope, "start").await?;
        let mut execution_trace = None;
        let mut prepared_replay = None;
        commit.decision.validate_terminal_scope(
            fabric,
            &commit.scope,
            commit.history_scope.as_ref(),
            workspace_dir,
            cost_config,
        )?;
        commit
            .decision
            .context
            .validate_silent_terminal(commit.status, commit.provider_outcome.as_ref())?;
        if let Some(served) = commit.decision.context.served_auxiliary() {
            anyhow::ensure!(
                commit.status == TurnTerminalStatus::Completed,
                "auxiliary reply cannot be attached to an incomplete terminal"
            );
            served.validate_reply(
                &commit
                    .history
                    .as_ref()
                    .context("auxiliary reply projection missing")?
                    .assistant_content,
            )?;
        }
        if commit.decision.is_recorded() {
            let decision = &commit.decision;
            let recorded = decision.recorded.as_ref().context("recorded decision is missing")?;
            decision.validate_durable_chain().await?;
            let mut prepared = replay::PreparedReplay::capture(decision, &commit).await?;
            execution_trace = Some(decision.context.terminal_execution_trace());
            // An idempotency key already committed for another decision must not
            // authorize even a usage or cost replay under the new decision.
            let final_key = format!("turn:{}:final", commit.terminal_id);
            if let Some(existing) = fabric
                .memory()
                .find_message_event_by_idempotency_key(fabric.workspace_id(), &final_key)
                .await?
            {
                verify_terminal_replay(
                    fabric.memory().as_ref(),
                    &recorded.reader,
                    &recorded.key,
                    &recorded.store_workspace_id,
                    fabric.workspace_id(),
                    commit.scope.owner_id.as_deref().context("terminal owner is missing")?,
                    &commit.terminal_id,
                    &existing,
                )
                .await?;
                let payload: serde_json::Value = serde_json::from_str(
                    existing
                        .raw_payload_json
                        .as_deref()
                        .context("terminal marker has no payload")?,
                )?;
                anyhow::ensure!(
                    existing.role == "event"
                        && existing.event_type == "turn.finalized"
                        && existing.source == commit.scope.source
                        && existing.workspace_id == fabric.workspace_id()
                        && existing.owner_id == commit.scope.owner_id
                        && existing.session_key == commit.scope.session_key
                        && existing.run_id == commit.scope.run_id
                        && existing.channel == commit.scope.channel
                        && existing.idempotency_key.as_deref() == Some(final_key.as_str())
                        && payload.get("terminal_id").and_then(serde_json::Value::as_str)
                            == Some(commit.terminal_id.as_str())
                        && payload.get("decision_id").and_then(serde_json::Value::as_str)
                            == Some(decision.decision_id.to_string().as_str())
                        && payload.get("status").and_then(serde_json::Value::as_str)
                            == Some(match commit.status {
                                TurnTerminalStatus::Completed => "completed",
                                TurnTerminalStatus::Silent => "silent",
                                TurnTerminalStatus::Failed => "failed",
                                TurnTerminalStatus::Cancelled => "cancelled",
                            }),
                    "existing terminal marker conflicts with the pinned decision or scope"
                );
            }
            // The workspace-scoped idempotency key is unique in the durable
            // message store. Its insert/readback transaction elects one decision
            // before any usage, cost or assistant write, including across
            // processes. A retry of the same decision may reuse the claim.
            let claim_key = format!("turn:{}:decision-claim", commit.terminal_id);
            // A keyed, versioned digest binds the facts that can change terminal
            // side effects. Redaction precedes the digest, and the durable key
            // prevents offline guessing of short private replies or targets.
            let mut claim_outcome = commit.provider_outcome.clone();
            if let Some(outcome) = claim_outcome.as_mut() {
                outcome.redact_provider_labels();
            }
            let trusted = decision
                .context
                .audit_hops()
                .last()
                .context("decision has no final hop")?
                .state
                .trusted
                .parts();
            // Version two prevents a valid claim from being transplanted to
            // another terminal or decision, including across a process restart.
            // Unsigned and version-one claims cannot authorize this path.
            let semantic = redact_persisted_json(serde_json::json!({
                "version": 2,
                "terminal_id": commit.terminal_id,
                "decision_id": decision.decision_id.to_string(),
                "execution_trace": execution_trace,
                "decision_chain": decision.context.hop_ids().iter().map(ToString::to_string).collect::<Vec<_>>(),
                "workspace_id": trusted.workspace_id,
                "source_event_id": trusted.source_event_id,
                "catalog_generation": trusted.catalog_generation.to_hex(),
                "policy_version": trusted.policy_version,
                "status": commit.status,
                "scope": terminal_semantic_scope(&commit.scope),
                "history": commit.history,
                "history_scope": commit.history_scope.as_ref().map(terminal_semantic_scope),
                "provider_outcome": claim_outcome,
                "telemetry": commit.telemetry,
                "delivery_intent": commit.delivery_intent,
            }));
            let semantic = {
                let mut semantic = semantic;
                if let Some(event) = decision
                    .context
                    .served_auxiliary()
                    .and_then(served_auxiliary::ServedAuxiliaryLink::event)
                {
                    use sha2::Digest;
                    semantic
                        .as_object_mut()
                        .context("terminal claim is not an object")?
                        .insert(
                            "served_auxiliary".to_owned(),
                            serde_json::json!({
                                "event_id": event.event_id,
                                "sha256": hex::encode(sha2::Sha256::digest(serde_json::to_vec(event)?)),
                            }),
                        );
                }
                semantic
            };
            let semantic_bytes = serde_json::to_vec(&semantic)?;
            let semantic_mac = hex::encode(
                recorded
                    .key
                    .hmac(KeyDomain::TerminalClaim, &[b"terminal-claim-v2", &semantic_bytes]),
            );
            let claim = fabric
                .record_runtime_event_idempotent(
                    commit.scope.clone(),
                    "turn.decision.claimed",
                    format!("terminal_id={}", commit.terminal_id),
                    Some(
                        serde_json::json!({
                            "terminal_id": commit.terminal_id,
                            "decision_id": decision.decision_id.to_string(),
                            "semantic_mac_v2": semantic_mac,
                        })
                        .to_string(),
                    ),
                    &claim_key,
                )
                .await?;
            let claim_payload: serde_json::Value = serde_json::from_str(
                claim
                    .raw_payload_json
                    .as_deref()
                    .context("decision claim has no payload")?,
            )?;
            anyhow::ensure!(
                claim.role == "event"
                    && claim.event_type == "turn.decision.claimed"
                    && claim.source == commit.scope.source
                    && claim.workspace_id == fabric.workspace_id()
                    && claim.owner_id == commit.scope.owner_id
                    && claim.session_key == commit.scope.session_key
                    && claim.run_id == commit.scope.run_id
                    && claim.channel == commit.scope.channel
                    && claim.idempotency_key.as_deref() == Some(claim_key.as_str())
                    && claim_payload.get("terminal_id").and_then(serde_json::Value::as_str)
                        == Some(commit.terminal_id.as_str())
                    && claim_payload.get("decision_id").and_then(serde_json::Value::as_str)
                        == Some(decision.decision_id.to_string().as_str())
                    && claim_payload.get("semantic_mac_v2").and_then(serde_json::Value::as_str)
                        == Some(semantic_mac.as_str()),
                "terminal decision claim conflicts with the pinned decision or scope"
            );
            prepared.bind_claim(&recorded.key, &claim)?;
            prepared_replay = Some(prepared);
        }
        // Everything below persists the reply and the outcome (the assistant
        // message event, usage settlement, cost ledger, attempt events, the
        // terminal marker). Every surface finalizes here, so both are redacted
        // once, here, at write: the outcome's provider labels lose their URL
        // userinfo, and secrets in the reply and in every outcome string (provider
        // errors included) are replaced by the one redactor, which never
        // truncates (D-054).
        let mut commit = commit;
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
            .and_then(|outcome| usage_settlement(&commit.terminal_id, outcome, cost_config));

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
                        let tracker = crate::cost::tracker::CostTracker::for_workspace(cost_config, &workspace_dir)?;
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
            crate::llm::route_decision::record_provider_outcome_events(fabric, commit.scope.clone(), outcome).await?;
        }

        let decision_id = commit
            .decision
            .is_recorded()
            .then(|| commit.decision.decision_id.to_string());
        let payload = TurnTerminalPayload {
            terminal_id: &commit.terminal_id,
            decision_id: decision_id.as_deref(),
            status: commit.status,
            history: &commit.history,
            history_scope: commit.history_scope.as_ref().map(terminal_semantic_scope),
            execution_trace: execution_trace.as_ref(),
            assistant_event_id: assistant_event.as_ref().map(|event| event.event_id.as_str()),
            provider_outcome: &commit.provider_outcome,
            served_auxiliary: commit
                .decision
                .context
                .served_auxiliary()
                .and_then(served_auxiliary::ServedAuxiliaryLink::projection),
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
        let mut payload_value = redact_persisted_json(serde_json::to_value(&payload)?);
        if let Some(prepared) = prepared_replay {
            let decision = &commit.decision;
            let recorded = decision.recorded.as_ref().context("recorded decision is missing")?;
            prepared.seal(&recorded.key, &mut payload_value, assistant_event.as_ref())?;
        }
        let payload_json = serde_json::to_string(&payload_value)?;
        ensure_runtime_authority(&commit.scope, "terminal-marker").await?;
        let skill_owner = commit.scope.owner_id.clone();
        let skill_task = skill_task_key(&commit.scope);
        let terminal_event = fabric
            .record_runtime_event_idempotent(
                commit.scope,
                "turn.finalized",
                format!("terminal_id={} status={:?}", commit.terminal_id, commit.status),
                Some(payload_json),
                format!("turn:{}:final", commit.terminal_id),
            )
            .await?;

        if commit.decision.is_recorded() {
            let decision = &commit.decision;
            let recorded = decision.recorded.as_ref().context("recorded decision is missing")?;
            verify_terminal_replay(
                fabric.memory().as_ref(),
                &recorded.reader,
                &recorded.key,
                &recorded.store_workspace_id,
                fabric.workspace_id(),
                terminal_event
                    .owner_id
                    .as_deref()
                    .context("terminal owner is missing")?,
                &commit.terminal_id,
                &terminal_event,
            )
            .await?;
        }
        // After the durable marker and before the early return, so a retried
        // finalization joins and releases as well. Restart recovery
        // (`rejoin_terminal`, `rejoin_ingress_terminal`) does not reach this
        // call-in and the lifecycle has no hook there: a run whose process died
        // keeps its experience `incomplete` (its pins died with the process).
        // Recorded as a known gap.
        let awaiting_approval = skill_awaiting_approval(&commit.decision, execution_trace.as_ref());
        let observations = skill_observations(
            &commit.decision,
            execution_trace.as_ref(),
            commit.provider_outcome.as_ref(),
        );
        if !awaiting_approval {
            commit.decision.context.retire_recursive_allowance();
        }
        release_skill_lifecycle(
            workspace_dir,
            skill_owner,
            skill_task,
            &commit.terminal_id,
            commit.status,
            awaiting_approval,
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
        if stop_after_marker {
            return Ok(receipt);
        }
        if commit.decision.is_recorded() {
            let decision = &commit.decision;
            let recorded = decision.recorded.as_ref().context("recorded decision is missing")?;
            let facts = TerminalFacts::from_receipt(&decision.context, &receipt)?;
            anyhow::ensure!(
                OutcomeCorrelator::settle(&recorded.writer, &decision.context, &facts)? == Enqueue::Queued,
                "decision outcome could not be queued after terminal commit"
            );
        }
        Ok(receipt)
    })
    .await
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
mod tests {
    use super::*;
    use crate::llm::route_decision::RouteDecision;
    use crate::memory::{Memory, MemoryPrincipal, MemoryVisibility, SqliteMemory};
    use std::sync::Arc;

    fn completed_commit(terminal_id: &str) -> TurnTerminalDraft {
        let started_at = chrono::Utc::now();
        TurnTerminalDraft {
            decision: None,
            terminal_id: terminal_id.to_string(),
            scope: MessageEventScope::new("agent", MemoryVisibility::Workspace)
                .with_channel("test")
                .with_session_key(terminal_id)
                .with_run_id(terminal_id),
            status: TurnTerminalStatus::Completed,
            history: Some(TurnHistoryProjection {
                assistant_content: "done".to_string(),
                history_commit_len: 2,
            }),
            history_scope: None,
            provider_outcome: Some(ProviderExecutionOutcome::success_for_decision_with_usage(
                &RouteDecision::single_candidate("ollama", "prx-local-model"),
                started_at,
                crate::llm::route_decision::TokenUsage::reported(Some(10), Some(5), Some(15)),
            )),
            telemetry: TurnTerminalTelemetry {
                summary: "test completed".to_string(),
                started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: TurnDeliveryIntent::ReturnToCaller,
        }
    }

    fn boxed_terminal_fixture_turn<'a>(
        history: &'a mut Vec<crate::providers::ChatMessage>,
        request: crate::agent::turn_request::TurnRequest<'a>,
        services: crate::agent::turn_request::TurnServices<'a>,
        delivery: crate::agent::turn_request::TurnDelivery,
    ) -> std::pin::Pin<
        Box<
            impl std::future::Future<
                Output = anyhow::Result<(
                    crate::agent::turn_contracts::ToolLoopOutcome,
                    crate::agent::turn_contracts::ToolLoopTrace,
                )>,
            > + 'a,
        >,
    > {
        Box::pin(crate::agent::turn_engine::run_turn(
            history, request, services, delivery,
        ))
    }

    struct TerminalFixtureProvider {
        outcome: ProviderExecutionOutcome,
        text: String,
        silent: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::providers::Provider for TerminalFixtureProvider {
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            anyhow::bail!("terminal fixture requires the actual traced provider path")
        }
        fn supports_native_tools(&self) -> bool {
            true
        }
        async fn chat_traced(
            &self,
            request: crate::providers::ChatRequest<'_>,
            model: &str,
            _: f64,
        ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(model, self.outcome.final_model);
            if self.silent {
                assert!(request.tools.is_some_and(|tools| {
                    tools
                        .iter()
                        .any(|spec| spec.name == crate::tools::STAY_SILENT_TOOL_NAME)
                }));
            }
            Ok(crate::providers::traits::ChatTrace {
                response: crate::providers::ChatResponse {
                    text: (!self.silent).then(|| self.text.clone()),
                    tool_calls: if self.silent {
                        vec![crate::providers::ToolCall {
                            id: "prx_silent_call".into(),
                            name: crate::tools::STAY_SILENT_TOOL_NAME.into(),
                            arguments: serde_json::json!({"reason": "no group response needed"}).to_string(),
                        }]
                    } else {
                        Vec::new()
                    },
                    reasoning_content: None,
                },
                attempts: self.outcome.attempts.clone(),
                final_provider: self.outcome.final_provider.clone(),
                final_model: self.outcome.final_model.clone(),
                served_member: None,
                tokens_used: self.outcome.tokens_used.clone(),
            })
        }
    }

    // Issue actual source authority, seed only that authenticated memory scope,
    // and let the production decision/turn loop produce its observations.
    async fn bind_terminal_fixture(
        workspace: &Path,
        memory: Arc<dyn Memory>,
        draft: &mut TurnTerminalDraft,
        cost_config: &CostConfig,
    ) -> Arc<crate::runtime::turn_decision::TurnDecisionService> {
        bind_terminal_fixture_mode(workspace, memory, draft, cost_config, true).await
    }

    async fn bind_terminal_fixture_mode(
        workspace: &Path,
        memory: Arc<dyn Memory>,
        draft: &mut TurnTerminalDraft,
        cost_config: &CostConfig,
        enabled: bool,
    ) -> Arc<crate::runtime::turn_decision::TurnDecisionService> {
        let input = "Use memory_search to explain the saved expression `2 + 2`.";
        let selection = draft
            .provider_outcome
            .as_ref()
            .map_or(("ollama", "prx-local-model"), |outcome| {
                (outcome.final_provider.as_str(), outcome.final_model.as_str())
            });
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.config_path = workspace.join("config.toml");
        config.causal_tree.enabled = enabled;
        config.causal_tree.policy.max_decision_hops = 1;
        config.causal_tree.backend.kind =
            crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::Builtin);
        config.causal_tree.backend.remote_enabled = false;
        config.cost = cost_config.clone();
        config
            .providers
            .models
            .push(crate::config::schema::ProviderModelConfig {
                provider: selection.0.to_owned(),
                model: Some(selection.1.to_owned()),
                operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
                native_tools: Some(true),
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: None,
            });
        let generation = crate::config::new_shared(config).pin();
        let registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![
            Box::new(crate::tools::MemorySearchTool::new(
                workspace.to_path_buf(),
                Arc::clone(&memory),
                true,
            )),
            Box::new(crate::tools::StaySilentTool::new()),
        ]);
        let observer = Arc::new(crate::observability::NoopObserver {});
        let hooks = Arc::new(crate::hooks::HookManager::new(workspace.to_path_buf()));
        let service = crate::runtime::turn_publication::TurnDecisionPublication {
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            mcp_tool: None,
            security: Arc::new(crate::security::SecurityPolicy::from_config(
                &generation.effective.autonomy,
                workspace,
            )),
            observer: observer.clone(),
            hooks: Arc::clone(&hooks),
            memory: Arc::clone(&memory),
            plugin_generation: 0,
        }
        .publish(crate::causal_tree::state::TurnSurfaceKind::AgentLoop)
        .await
        .unwrap();
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
            &generation,
            Arc::clone(&memory),
            input,
            &draft.terminal_id,
            selection,
        )
        .await;
        assert!(
            service.model_can_call_tools(&proof),
            "fixture's actual traced provider supports verified native tools"
        );
        let source = memory
            .find_owned_message_event(
                &workspace.to_string_lossy(),
                proof.owner_id(),
                proof.source_event_id().unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.content, input);
        assert_eq!(source.owner_id.as_deref(), Some(proof.owner_id()));
        assert_eq!(source.session_key.as_deref(), Some(proof.session_key()));
        let mut write_scope = proof.memory_scope().clone();
        write_scope.owner_id = Some(proof.owner_id().to_owned());
        write_scope.require_owner_match = true;
        memory
            .store_with_context_and_metadata(
                "prx_terminal_fixture_note",
                "Use memory_search to explain the saved expression `2 + 2`. The expression adds two integers.",
                crate::memory::MemoryCategory::Core,
                Some(proof.session_key()),
                Some(&write_scope),
                crate::memory::MemoryStoreMetadata {
                    workspace_id: Some(source.workspace_id.clone()),
                    owner_id: source.owner_id.clone(),
                    channel: source.channel.clone(),
                    source_event_id: Some(source.event_id.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let context = service
            .decide_agent_initial(
                &proof,
                crate::runtime::turn_decision::AgentTurnKind::Repl,
                &cancellation,
            )
            .await
            .unwrap();
        if enabled {
            assert!(
                matches!(
                    context.directive(),
                    crate::causal_tree::directive::TurnExecutionDirective::AnswerDirectly
                ),
                "fixture requires real scoped retrieval to authorize Answer"
            );
        }
        if draft.status == TurnTerminalStatus::Silent {
            let mut spec = service
                .agent_runtime_dependency_spec(&proof, &context, crate::tools::STAY_SILENT_TOOL_NAME)
                .unwrap();
            let mut cache = crate::agent::turn_context::ToolSnapshotRenderCache::default();
            spec.description.push_str(" changed fixture schema");
            let changed =
                crate::agent::turn_context::CompiledToolContext::compile(true, vec![spec], Vec::new(), &mut cache);
            let error = service.record_agent_silence(&proof, &context, &changed).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("silence dependency changed after provider request")
            );
            let foreign = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                input,
                &format!("{}-foreign", draft.terminal_id),
                selection,
            )
            .await;
            let spec = service
                .agent_runtime_dependency_spec(&proof, &context, crate::tools::STAY_SILENT_TOOL_NAME)
                .unwrap();
            let offered =
                crate::agent::turn_context::CompiledToolContext::compile(true, vec![spec], Vec::new(), &mut cache);
            let error = service.record_agent_silence(&foreign, &context, &offered).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("turn origin does not match the pinned decision service")
            );
        }
        if matches!(draft.status, TurnTerminalStatus::Completed | TurnTerminalStatus::Silent) {
            let provider = TerminalFixtureProvider {
                outcome: draft
                    .provider_outcome
                    .clone()
                    .expect("fixture actual provider response"),
                text: draft
                    .history
                    .as_ref()
                    .map_or_else(|| "done".to_owned(), |history| history.assistant_content.clone()),
                silent: draft.status == TurnTerminalStatus::Silent,
                calls: Default::default(),
            };
            let mut history = vec![crate::providers::ChatMessage::user(input)];
            let result = boxed_terminal_fixture_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "cli",
                    scope: None,
                    mode: Default::default(),
                    silent: true,
                    expose_stay_silent: provider.silent,
                    multimodal: &generation.effective.multimodal,
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 1,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: None,
                    decision: crate::agent::turn_request::TurnDecision {
                        service: &service,
                        context: &context,
                        proof: &proof,
                    },
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: proof.provider_name(),
                    model: proof.model_name(),
                    temperature: 0.0,
                    tools_registry: registry,
                    observer: observer.as_ref(),
                    hooks: hooks.as_ref(),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
                        provider_trace: None,
                        events: None,
                        stream_provider: false,
                        tool_execution_service: None,
                        tool_execution_context: crate::tools::ToolExecutionContext::new(
                            crate::runtime::envelope::RuntimeEnvelope::agent(
                                workspace.to_string_lossy(),
                                proof.run_id(),
                            )
                            .with_config_generation(&generation)
                            .with_source_message_event_id(proof.source_event_id().unwrap()),
                            "private",
                        ),
                        allowed_tool_names: None,
                    }),
                    memory: crate::agent::turn_request::ToolLoopMemory::new(&memory, workspace, None),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: Some(cancellation),
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await
            .unwrap();
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(
                matches!(result.0, crate::agent::turn_contracts::ToolLoopOutcome::Silent { .. }),
                provider.silent
            );
            let original = draft.provider_outcome.as_ref().unwrap();
            assert_eq!(
                result.1.tokens_used, original.tokens_used,
                "real silent/text loop must retain complete reported usage"
            );
            let route = RouteDecision::single_candidate(&original.final_provider, &original.final_model);
            let mut actual = provider_outcome_from_trace(&route, original.started_at, result.1);
            // The upstream fixture's reason remains a redaction probe; execution
            // status, attempts, serving tuple and usage all come from run_turn.
            actual.fallback_reason.clone_from(&original.fallback_reason);
            draft.provider_outcome = Some(actual);
        }
        let decision = service.terminal_settlement_ref(context).unwrap();
        if decision.is_recorded() {
            decision.validate_durable_chain().await.unwrap();
        }
        draft.scope = MessageEventScope::new("agent", MemoryVisibility::Session)
            .with_owner_id(proof.owner_id())
            .with_channel("cli")
            .with_session_key(proof.session_key())
            .with_run_id(proof.run_id());
        draft.decision = Some(decision);
        // The decision writer can enqueue causal events after its own commit.
        // Establish fixture baselines only after both queues acknowledge.
        assert!(service.drain_writes(Instant::now() + Duration::from_secs(5)).await);
        service
    }

    fn terminal_fixture_principal(workspace: &Path, scope: &MessageEventScope) -> MemoryPrincipal {
        MemoryPrincipal {
            require_owner_match: true,
            workspace_id: workspace.to_string_lossy().into_owned(),
            agent_id: None,
            persona_id: None,
            session_key: scope.session_key.clone(),
            channel: scope.channel.clone(),
            sender: None,
            owner_id: scope.owner_id.clone(),
            legacy_session_key: None,
        }
    }

    #[tokio::test]
    async fn inconsistent_silent_status_is_rejected_before_writes_in_recorded_and_disabled_modes() {
        for enabled in [true, false] {
            for issued_silent in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
                let fabric = MemoryFabric::new(Arc::clone(&memory), temp.path().to_string_lossy());
                let mut draft = completed_commit("prx_silent_consistency");
                if issued_silent {
                    draft.status = TurnTerminalStatus::Silent;
                    draft.delivery_intent = TurnDeliveryIntent::None;
                }
                let service = bind_terminal_fixture_mode(
                    temp.path(),
                    Arc::clone(&memory),
                    &mut draft,
                    &CostConfig::default(),
                    enabled,
                )
                .await;
                let principal = terminal_fixture_principal(temp.path(), &draft.scope);
                let before = memory.list_message_events_since(&principal, 0, 128).await.unwrap();
                assert!(
                    before
                        .iter()
                        .any(|event| event.role == "user" && event.event_type == "message.created")
                );
                draft.status = if issued_silent {
                    TurnTerminalStatus::Completed
                } else {
                    TurnTerminalStatus::Silent
                };
                let error = finalize_turn(&fabric, draft, &CostConfig::default(), temp.path())
                    .await
                    .unwrap_err();
                let expected = if issued_silent {
                    "authorized silence cannot be relabeled"
                } else {
                    "silent terminal lacks confirmed runtime execution facts"
                };
                assert!(error.to_string().contains(expected), "{error:#}");
                assert!(service.drain_writes(Instant::now() + Duration::from_secs(5)).await);
                let after = memory.list_message_events_since(&principal, 0, 128).await.unwrap();
                assert_eq!(
                    before.iter().map(|event| &event.event_id).collect::<Vec<_>>(),
                    after.iter().map(|event| &event.event_id).collect::<Vec<_>>(),
                    "before event metadata: {:?}; after event metadata: {:?}",
                    before
                        .iter()
                        .map(|event| (&event.event_id, &event.event_type, &event.role))
                        .collect::<Vec<_>>(),
                    after
                        .iter()
                        .map(|event| (&event.event_id, &event.event_type, &event.role))
                        .collect::<Vec<_>>()
                );
                assert!(
                    !after
                        .iter()
                        .any(|event| event.event_type == "turn.finalized" || event.event_type == "usage.settled")
                );
                assert!(!temp.path().join("state/costs.jsonl").exists());
                service.shutdown(Instant::now() + Duration::from_secs(5)).await;
            }
        }
    }

    #[tokio::test]
    async fn foreign_fabric_workspace_cannot_redirect_issued_cost_settlement() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let cost_config = CostConfig::default();
        let mut commit = completed_commit("foreign-workspace-run");
        let service = bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &cost_config).await;
        let principal = terminal_fixture_principal(temp.path(), &commit.scope);
        let before = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert!(
            before
                .iter()
                .any(|event| event.role == "user" && event.event_type == "message.created")
        );
        // Change only the fabric namespace. The real issued scope, backend,
        // physical settlement directory and cost configuration remain intact.
        let fabric = MemoryFabric::new(Arc::clone(&memory), "foreign-logical-workspace");
        let error = finalize_turn(&fabric, commit, &cost_config, temp.path())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("decision terminal workspace does not match the pinned turn")
        );
        assert!(service.drain_writes(Instant::now() + Duration::from_secs(5)).await);
        let after = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert_eq!(
            after.iter().map(|event| &event.event_id).collect::<Vec<_>>(),
            before.iter().map(|event| &event.event_id).collect::<Vec<_>>(),
            "before event metadata: {:?}; after event metadata: {:?}",
            before
                .iter()
                .map(|event| (&event.event_id, &event.event_type, &event.role))
                .collect::<Vec<_>>(),
            after
                .iter()
                .map(|event| (&event.event_id, &event.event_type, &event.role))
                .collect::<Vec<_>>()
        );
        let mut foreign = principal;
        foreign.workspace_id = "foreign-logical-workspace".to_owned();
        assert!(
            memory
                .list_message_events_since(&foreign, 0, 32)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!temp.path().join("state/costs.jsonl").exists());
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn occupied_cost_state_directory_is_recorded_in_terminal_projection() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let cost_config = CostConfig::default();

        let mut commit = completed_commit("preflight-failure-run");
        let service = bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &cost_config).await;
        let principal = terminal_fixture_principal(temp.path(), &commit.scope);
        std::fs::write(temp.path().join("state"), "occupied").unwrap();
        let receipt = finalize_turn(&fabric, commit, &cost_config, temp.path()).await.unwrap();

        assert!(matches!(
            receipt.cost_settlement,
            Some(CostSettlement::Failed { ref error })
                if error.contains("failed to create cost state directory")
        ));
        let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );
        let terminal_payload: serde_json::Value = serde_json::from_str(
            events
                .iter()
                .find(|event| event.event_type == "turn.finalized")
                .and_then(|event| event.raw_payload_json.as_deref())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            terminal_payload
                .pointer("/cost_settlement/status")
                .and_then(serde_json::Value::as_str),
            Some("failed")
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn lost_runtime_authority_fails_before_terminal_or_cost_projection() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let mut commit = completed_commit("lost-authority-run");
        let service =
            bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &CostConfig::default()).await;
        let principal = terminal_fixture_principal(temp.path(), &commit.scope);
        let before = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert!(
            before
                .iter()
                .any(|event| event.role == "user" && event.event_type == "message.created")
        );
        commit.scope.authority_guard = Some(crate::memory::RuntimeAuthorityGuard::new("lost-authority", || {
            Ok(false)
        }));
        let cost_config = CostConfig::default();

        let error = finalize_turn(&fabric, commit, &cost_config, temp.path())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("runtime execution authority was lost"));
        assert!(service.drain_writes(Instant::now() + Duration::from_secs(5)).await);
        let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert_eq!(
            events.iter().map(|event| &event.event_id).collect::<Vec<_>>(),
            before.iter().map(|event| &event.event_id).collect::<Vec<_>>(),
            "before event metadata: {:?}; after event metadata: {:?}",
            before
                .iter()
                .map(|event| (&event.event_id, &event.event_type, &event.role))
                .collect::<Vec<_>>(),
            events
                .iter()
                .map(|event| (&event.event_id, &event.event_type, &event.role))
                .collect::<Vec<_>>()
        );
        assert!(!temp.path().join("state").join("costs.jsonl").exists());
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn authority_lost_during_projection_never_writes_stale_terminal_or_assistant() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let validations = Arc::new(AtomicUsize::new(0));
        let mut commit = completed_commit("mid-finalize-authority-loss");
        let service =
            bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &CostConfig::default()).await;
        let principal = terminal_fixture_principal(temp.path(), &commit.scope);
        commit.scope.authority_guard = Some(crate::memory::RuntimeAuthorityGuard::new(
            "authority-lost-after-usage",
            {
                let validations = Arc::clone(&validations);
                move || Ok(validations.fetch_add(1, Ordering::SeqCst) == 0)
            },
        ));

        let error = finalize_turn(&fabric, commit, &CostConfig::default(), temp.path())
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("runtime execution authority was lost during terminal finalization phase projection")
        );
        assert_eq!(validations.load(Ordering::SeqCst), 2);
        let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            0
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "message.created" && event.role == "assistant")
                .count(),
            0
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    fn priced_cost_config() -> CostConfig {
        let mut cost_config = CostConfig::default();
        cost_config.prices.insert(
            "ollama/prx-local-model".to_string(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        cost_config
    }

    fn ledger_lines_for(workspace: &Path, terminal_id: &str) -> usize {
        std::fs::read_to_string(workspace.join("state").join("costs.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(terminal_id))
            .count()
    }

    /// Finding 85: the first finalization stores `Recorded`; replaying the same
    /// priced settlement over the committed marker yields `Replayed`. The replay
    /// must succeed with the same receipt and must not charge a second time.
    #[tokio::test]
    async fn priced_completed_finalization_replays_over_its_committed_marker() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let cost_config = priced_cost_config();
        let terminal_id = "priced-replay-run";
        let mut commit = completed_commit(terminal_id);
        let service = bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &cost_config).await;
        assert!(commit.decision.as_ref().is_some_and(|decision| decision.is_recorded()));

        let first = finalize_turn(&fabric, commit.clone(), &cost_config, temp.path())
            .await
            .unwrap();
        assert!(matches!(first.cost_settlement, Some(CostSettlement::Recorded { .. })));
        let charged = ledger_lines_for(temp.path(), terminal_id);
        assert_eq!(charged, 1, "the first finalization charges exactly once");

        let second = finalize_turn(&fabric, commit, &cost_config, temp.path())
            .await
            .expect("a priced replay over the committed marker must finalize");
        assert!(matches!(second.cost_settlement, Some(CostSettlement::Replayed)));
        assert_eq!(first.terminal_event.event_id, second.terminal_event.event_id);
        assert_eq!(first.usage_settlement, second.usage_settlement);
        assert_eq!(ledger_lines_for(temp.path(), terminal_id), charged, "no second charge");
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    /// A replay that carries a genuinely different settlement (other token
    /// counts, hence another amount) is rejected, leaving the ledger untouched.
    /// The provider outcome is part of the terminal decision claim, so the
    /// claim fingerprint refuses it before any cost-settlement comparison runs;
    /// the settlement comparison itself is covered by the unit tests in
    /// `causal_tree::outcome`.
    #[tokio::test]
    async fn priced_replay_with_a_different_settlement_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let cost_config = priced_cost_config();
        let terminal_id = "priced-replay-mismatch-run";
        let mut commit = completed_commit(terminal_id);
        let service = bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &cost_config).await;

        finalize_turn(&fabric, commit.clone(), &cost_config, temp.path())
            .await
            .unwrap();
        let charged = ledger_lines_for(temp.path(), terminal_id);
        assert_eq!(charged, 1);

        let mut different = commit;
        different.provider_outcome = Some(ProviderExecutionOutcome::success_for_decision_with_usage(
            &RouteDecision::single_candidate("ollama", "prx-local-model"),
            different.telemetry.started_at,
            crate::llm::route_decision::TokenUsage::reported(Some(1000), Some(500), Some(1500)),
        ));
        let error = finalize_turn(&fabric, different, &cost_config, temp.path()).await;
        let error = error.expect_err("a different settlement must not replay over the marker");
        assert!(
            error
                .to_string()
                .contains("terminal decision claim conflicts with the pinned decision or scope"),
            "unexpected rejection: {error:#}"
        );
        assert_eq!(
            ledger_lines_for(temp.path(), terminal_id),
            charged,
            "the ledger is unchanged"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unwritable_cost_ledger_is_recorded_in_terminal_projection() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let cost_path = state_dir.join("costs.jsonl");
        std::fs::write(&cost_path, "").unwrap();
        std::fs::set_permissions(&cost_path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let mut cost_config = CostConfig::default();
        cost_config.prices.insert(
            "ollama/prx-local-model".to_string(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );

        let mut commit = completed_commit("cost-write-failure-run");
        let service = bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &cost_config).await;
        let principal = terminal_fixture_principal(temp.path(), &commit.scope);
        let receipt = finalize_turn(&fabric, commit, &cost_config, temp.path()).await.unwrap();

        assert!(matches!(
            receipt.cost_settlement,
            Some(CostSettlement::Failed { ref error })
                if error.contains("Failed to open cost storage")
        ));
        let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cost_lock_wait_does_not_block_runtime_and_usage_precedes_projection() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let cost_path = state_dir.join("costs.jsonl");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(cost_path.with_extension("jsonl.lock"))
            .unwrap();

        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let mut cost_config = CostConfig::default();
        cost_config.prices.insert(
            "ollama/prx-local-model".to_string(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let mut commit = completed_commit("cost-lock-liveness");
        let service = bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &cost_config).await;
        let principal = terminal_fixture_principal(temp.path(), &commit.scope);
        lock_file.lock().unwrap();
        let workspace_dir = temp.path().to_path_buf();
        let task = tokio::spawn({
            let fabric = fabric.clone();
            async move { finalize_turn(&fabric, commit, &cost_config, &workspace_dir).await }
        });

        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert!(!cost_path.exists());
        assert!(!task.is_finished());

        drop(lock_file);
        let receipt = task.await.unwrap().unwrap();
        assert!(matches!(receipt.cost_settlement, Some(CostSettlement::Recorded { .. })));
        assert_eq!(
            std::fs::read_to_string(&cost_path)
                .unwrap()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            1
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn repeated_finalize_writes_one_terminal_one_assistant_and_one_usage_settlement() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let scope = MessageEventScope::new("agent", MemoryVisibility::Workspace)
            .with_channel("test")
            .with_session_key("session-a")
            .with_run_id("run-a");
        let started_at = chrono::Utc::now();
        let outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
            &RouteDecision::single_candidate("ollama", "prx-local-model"),
            started_at,
            crate::llm::route_decision::TokenUsage::reported(Some(10), Some(5), Some(15)),
        );
        let mut commit = TurnTerminalDraft {
            decision: None,
            terminal_id: "run-a".to_string(),
            scope,
            status: TurnTerminalStatus::Completed,
            history: Some(TurnHistoryProjection {
                assistant_content: "done".to_string(),
                history_commit_len: 2,
            }),
            history_scope: None,
            provider_outcome: Some(outcome),
            telemetry: TurnTerminalTelemetry {
                summary: "test completed".to_string(),
                started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: TurnDeliveryIntent::ReturnToCaller,
        };

        let service =
            bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &CostConfig::default()).await;
        let principal = terminal_fixture_principal(temp.path(), &commit.scope);

        let first = finalize_turn(&fabric, commit.clone(), &CostConfig::default(), temp.path())
            .await
            .unwrap();
        let second = finalize_turn(&fabric, commit, &CostConfig::default(), temp.path())
            .await
            .unwrap();

        assert_eq!(first.terminal_event.event_id, second.terminal_event.event_id);
        assert_eq!(
            first
                .usage_settlement
                .as_ref()
                .and_then(|usage| usage.settlement_id.as_deref()),
            Some("run-a")
        );
        assert_eq!(first.cost_settlement, Some(CostSettlement::UnknownPricing));

        let events = memory.list_message_events_since(&principal, 0, 32).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.settled")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "message.created" && event.role == "assistant")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "provider.final_outcome")
                .count(),
            1
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn non_reply_terminal_states_replay_once_without_assistant_projection() {
        for (index, status) in [
            TurnTerminalStatus::Silent,
            TurnTerminalStatus::Failed,
            TurnTerminalStatus::Cancelled,
        ]
        .into_iter()
        .enumerate()
        {
            let temp = tempfile::tempdir().unwrap();
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
            let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
            let session_key = format!("non-reply-{index}");
            let terminal_id = format!("terminal-{index}");
            let scope = MessageEventScope::new("agent", MemoryVisibility::Workspace)
                .with_channel("test")
                .with_session_key(session_key.clone())
                .with_run_id(terminal_id.clone());
            let started_at = chrono::Utc::now();
            let provider_outcome = (status == TurnTerminalStatus::Silent).then(|| {
                ProviderExecutionOutcome::success_for_decision_with_usage(
                    &RouteDecision::single_candidate("ollama", "prx-local-model"),
                    started_at,
                    crate::llm::route_decision::TokenUsage::reported(Some(4), Some(2), Some(6)),
                )
            });
            let mut commit = TurnTerminalDraft {
                decision: None,
                terminal_id: terminal_id.clone(),
                scope,
                status,
                // A defensive history projection on a non-completed status must
                // never create an assistant message.
                history: Some(TurnHistoryProjection {
                    assistant_content: "must not be projected".to_string(),
                    history_commit_len: 2,
                }),
                history_scope: None,
                provider_outcome,
                telemetry: TurnTerminalTelemetry {
                    summary: format!("{status:?}"),
                    started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: TurnDeliveryIntent::None,
            };

            let service =
                bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &CostConfig::default()).await;
            let principal = terminal_fixture_principal(temp.path(), &commit.scope);

            let issued = commit.decision.clone().unwrap();
            let first = finalize_turn(&fabric, commit.clone(), &CostConfig::default(), temp.path())
                .await
                .unwrap();
            let second = finalize_turn(&fabric, commit, &CostConfig::default(), temp.path())
                .await
                .unwrap();
            assert_eq!(first.terminal_event.event_id, second.terminal_event.event_id);
            for receipt in [&first, &second] {
                let payload: serde_json::Value =
                    serde_json::from_str(receipt.terminal_event.raw_payload_json.as_deref().unwrap()).unwrap();
                assert_eq!(payload.get("status"), Some(&serde_json::to_value(status).unwrap()));
            }
            assert!(first.assistant_event.is_none());
            assert_eq!(first.usage_settlement.is_some(), status == TurnTerminalStatus::Silent);
            if status == TurnTerminalStatus::Silent {
                let recorded = issued.recorded.as_ref().unwrap();
                replay::verify_terminal_replay(
                    memory.as_ref(),
                    &recorded.reader,
                    &recorded.key,
                    &recorded.store_workspace_id,
                    fabric.workspace_id(),
                    principal.owner_id.as_deref().unwrap(),
                    &terminal_id,
                    &first.terminal_event,
                )
                .await
                .expect("genuine silent replay authenticates");
                let mut changed = first.terminal_event.clone();
                let mut payload: serde_json::Value =
                    serde_json::from_str(changed.raw_payload_json.as_deref().unwrap()).unwrap();
                let authorized_silent = payload
                    .get_mut("execution_trace")
                    .and_then(|trace| trace.get_mut("authorized_silent"))
                    .expect("genuine Silent terminal contains its authorization marker");
                assert_eq!(*authorized_silent, serde_json::Value::Bool(true));
                *authorized_silent = serde_json::Value::Bool(false);
                changed.raw_payload_json = Some(serde_json::to_string(&payload).unwrap());
                assert!(
                    replay::verify_terminal_replay(
                        memory.as_ref(),
                        &recorded.reader,
                        &recorded.key,
                        &recorded.store_workspace_id,
                        fabric.workspace_id(),
                        principal.owner_id.as_deref().unwrap(),
                        &terminal_id,
                        &changed,
                    )
                    .await
                    .is_err(),
                    "changed marker cannot retain terminal authentication"
                );
            }

            let events = memory.list_message_events_since(&principal, 0, 128).await.unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event.event_type == "turn.finalized" && event.content.contains(terminal_id.as_str())
                    })
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event_type == "message.created" && event.role == "assistant")
                    .count(),
                0
            );
            service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        }
    }

    #[tokio::test]
    async fn unissued_entrypoint_label_drafts_are_rejected_before_side_effects() {
        const ENTRYPOINTS: [&str; 8] = [
            "chat",
            "agent",
            "gateway_webhook",
            "channel",
            "gateway_console",
            "session_worker",
            "sessions_spawn",
            "delegate",
        ];
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a");

        for entrypoint in ENTRYPOINTS {
            let terminal_id = format!("{entrypoint}-run");
            let scope = MessageEventScope::new(entrypoint, MemoryVisibility::Workspace)
                .with_channel(entrypoint)
                .with_session_key(terminal_id.clone())
                .with_run_id(terminal_id.clone());
            let started_at = chrono::Utc::now();
            let outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
                &RouteDecision::single_candidate("ollama", "prx-local-model"),
                started_at,
                crate::llm::route_decision::TokenUsage::reported(Some(3), Some(2), Some(5)),
            );
            let error = finalize_turn(
                &fabric,
                TurnTerminalDraft {
                    decision: None,
                    terminal_id: terminal_id.clone(),
                    scope,
                    status: TurnTerminalStatus::Completed,
                    history: Some(TurnHistoryProjection {
                        assistant_content: format!("{entrypoint} done"),
                        history_commit_len: 2,
                    }),
                    history_scope: None,
                    provider_outcome: Some(outcome),
                    telemetry: TurnTerminalTelemetry {
                        summary: format!("{entrypoint} completed"),
                        started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: TurnDeliveryIntent::ReturnToCaller,
                },
                &CostConfig::default(),
                temp.path(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), "terminal draft has no service-issued decision");

            let principal = MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace-a".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some(terminal_id.clone()),
                channel: Some(entrypoint.to_string()),
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            };
            let events = memory.list_message_events_since(&principal, 0, 128).await.unwrap();
            assert!(events.is_empty(), "unissued {entrypoint} must not persist any event");
            assert!(!temp.path().join("state/costs.jsonl").exists());
            assert!(!temp.path().join("self/causal_tree/decisions.db").exists());
        }
    }

    /// R-C2BU F-6: persisted outcomes name the provider redacted, but the
    /// spend of a password-URL provider must still be priced and recorded
    /// against a `[cost].prices` entry keyed by the configured label.
    /// D-054: the usage settlement a surface records when its terminal commit
    /// failed names the serving provider without URL userinfo.
    #[test]
    fn a_fallback_usage_settlement_names_no_url_password() {
        let outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
            &RouteDecision::single_candidate("custom:http://u:USAGE-SECRET-pw@127.0.0.1:1/v1", "prx-local-model"),
            chrono::Utc::now(),
            crate::llm::route_decision::TokenUsage::reported(Some(10), Some(5), Some(15)),
        );
        let record = usage_settlement("run-fallback", &outcome, &CostConfig::default()).expect("test: usage settles");
        assert!(!format!("{record:?}").contains("USAGE-SECRET-pw"), "{record:?}");
        assert_eq!(record.settlement_id.as_deref(), Some("run-fallback"));
    }

    #[tokio::test]
    async fn spend_through_a_password_url_provider_is_still_priced_and_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let label = "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1";
        let mut cost_config = CostConfig::default();
        cost_config.prices.insert(
            format!("{label}/prx-local-model"),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let scope = MessageEventScope::new("agent", MemoryVisibility::Workspace)
            .with_channel("test")
            .with_session_key("session-priced")
            .with_run_id("run-priced");
        let started_at = chrono::Utc::now();
        let outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
            &RouteDecision::single_candidate(label, "prx-local-model"),
            started_at,
            crate::llm::route_decision::TokenUsage::reported(Some(10), Some(5), Some(15)),
        );
        let mut commit = TurnTerminalDraft {
            decision: None,
            terminal_id: "run-priced".to_string(),
            scope,
            status: TurnTerminalStatus::Completed,
            history: Some(TurnHistoryProjection {
                assistant_content: "done".to_string(),
                history_commit_len: 2,
            }),
            history_scope: None,
            provider_outcome: Some(outcome),
            telemetry: TurnTerminalTelemetry {
                summary: "test completed".to_string(),
                started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: TurnDeliveryIntent::ReturnToCaller,
        };
        let service = bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut commit, &cost_config).await;
        let receipt = finalize_turn(&fabric, commit, &cost_config, temp.path()).await.unwrap();
        let usage = receipt.usage_settlement.as_ref().expect("test: usage settled");
        assert!(
            usage.cost_usd.is_some(),
            "the configured price no longer matches: {usage:?}"
        );
        assert!(
            matches!(receipt.cost_settlement, Some(CostSettlement::Recorded { .. })),
            "the spend was not recorded against the budget: {:?}",
            receipt.cost_settlement
        );
        assert!(!format!("{usage:?}").contains("SECRET-fixture-pw"));
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    use super::persistence_probe::PROBE_SECRET;

    /// Every event `finalize_turn` wrote for `session_key`, content and raw
    /// payload, with the secret nowhere in them.
    async fn assert_no_event_persists_the_probe_secret(memory: &Arc<dyn Memory>, principal: &MemoryPrincipal) {
        let events = memory
            .list_message_events_since(principal, 0, 64)
            .await
            .expect("test: list the session's events");
        assert!(
            events.iter().any(|event| event.event_type == "turn.finalized"),
            "the probe must reach the terminal marker: {events:?}"
        );
        for event in &events {
            assert!(
                !event.content.contains(PROBE_SECRET),
                "{} persisted the secret in its content: {}",
                event.event_type,
                event.content
            );
            let payload = event.raw_payload_json.as_deref().unwrap_or_default();
            assert!(
                !payload.contains(PROBE_SECRET),
                "{} persisted the secret in its payload: {payload}",
                event.event_type
            );
        }
    }

    /// D-053 / REDACT-TURN: every surface finalizes through `finalize_turn`, so
    /// it redacts what it persists. A completed turn whose reply, provider
    /// attempt error, fallback reason and telemetry summary carry a secret, and
    /// a failed turn whose provider error, summary and suppression reason carry
    /// one, persist it in no event: not the assistant `message.created`, the
    /// provider attempt and outcome events, `usage.settled`, nor the
    /// `turn.finalized` marker.
    #[tokio::test]
    async fn finalize_turn_persists_no_secret_from_the_reply_or_the_outcome() {
        let temp = tempfile::tempdir().expect("test: tempdir");
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite memory"));
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let cost_config = CostConfig::default();
        let leaked = format!("upstream said: key {PROBE_SECRET} rejected");

        let mut completed = completed_commit("run-redact-completed");
        if let Some(history) = completed.history.as_mut() {
            history.assistant_content = format!("Your key is {PROBE_SECRET}.");
        }
        if let Some(outcome) = completed.provider_outcome.as_mut() {
            outcome.fallback_reason = Some(leaked.clone());
            for attempt in &mut outcome.attempts {
                attempt.error_message = Some(leaked.clone());
            }
        }
        completed.telemetry.summary = leaked.clone();
        let completed_service =
            bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut completed, &cost_config).await;
        let completed_principal = terminal_fixture_principal(temp.path(), &completed.scope);
        let receipt = finalize_turn(&fabric, completed, &cost_config, temp.path())
            .await
            .expect("test: finalize the completed turn");
        let assistant = receipt.assistant_event.expect("test: the reply is projected");
        assert!(
            assistant.content.contains("[REDACTED]"),
            "the reply is redacted, not dropped: {}",
            assistant.content
        );
        assert_no_event_persists_the_probe_secret(&memory, &completed_principal).await;

        let started_at = chrono::Utc::now();
        let mut failed = completed_commit("run-redact-failed");
        failed.status = TurnTerminalStatus::Failed;
        failed.history = None;
        failed.provider_outcome = Some(ProviderExecutionOutcome::failed_for_decision(
            &RouteDecision::single_candidate("ollama", "prx-local-model"),
            started_at,
            &anyhow::anyhow!("{leaked}"),
        ));
        failed.telemetry.summary = leaked.clone();
        failed.delivery_intent = TurnDeliveryIntent::Suppress { reason: leaked };
        completed_service
            .shutdown(Instant::now() + Duration::from_secs(5))
            .await;
        let service = bind_terminal_fixture(temp.path(), Arc::clone(&memory), &mut failed, &cost_config).await;
        let failed_principal = terminal_fixture_principal(temp.path(), &failed.scope);
        finalize_turn(&fabric, failed, &cost_config, temp.path())
            .await
            .expect("test: finalize the failed turn");
        assert_no_event_persists_the_probe_secret(&memory, &failed_principal).await;
        // and nothing else the finalizer wrote (the cost ledger included)
        super::persistence_probe::assert_nothing_under_persists_the_probe_secret(temp.path());
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    /// R-C2BUf F-6 / D-027: prices keyed by two credentialed spellings of one
    /// endpoint and model are ambiguous once redacted. The rule is
    /// deterministic: an exact key wins; otherwise the unique redacted match,
    /// and an ambiguous match (which validation refuses) charges the highest
    /// matching price.
    #[test]
    fn a_redacted_price_match_is_used_only_when_it_is_unique() {
        let pricing = |input: f64| crate::config::schema::ModelPricing {
            input,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        };
        let usage = crate::llm::route_decision::TokenUsage::reported(Some(1_000_000), Some(0), Some(1_000_000));
        let redacted = "custom:http://[redacted]@127.0.0.1:1/v1";

        let mut one = CostConfig::default();
        one.prices
            .insert("custom:http://a:x@127.0.0.1:1/v1/m".into(), pricing(3.0));
        let record = crate::llm::route_decision::MeteredTokenUsageRecord::from_parts(redacted, "m", &usage, &one)
            .expect("test: usage");
        assert_eq!(record.cost_usd, Some(3.0));

        let mut two = one.clone();
        two.prices
            .insert("custom:http://b:y@127.0.0.1:1/v1/m".into(), pricing(7.0));
        // D-027: validation refuses such a pair; if it is ever reached at
        // runtime, the highest matching price is charged, never none.
        for _ in 0..8 {
            let record = crate::llm::route_decision::MeteredTokenUsageRecord::from_parts(redacted, "m", &usage, &two)
                .expect("test: usage");
            assert_eq!(
                record.cost_usd,
                Some(7.0),
                "an ambiguous match charges the highest price"
            );
        }

        let mut exact = two.clone();
        exact.prices.insert(format!("{redacted}/m"), pricing(5.0));
        let record = crate::llm::route_decision::MeteredTokenUsageRecord::from_parts(redacted, "m", &usage, &exact)
            .expect("test: usage");
        assert_eq!(record.cost_usd, Some(5.0), "an exact key wins");
    }

    /// Lifecycle store with one bound skill in `workspace`; returns a pin of it.
    fn skill_fixture(
        workspace: &Path,
        owner: &str,
    ) -> (
        crate::skills::lifecycle::SkillLifecycleStore,
        crate::skills::lifecycle::SkillId,
        crate::skills::lifecycle::PinnedSkillRevision,
    ) {
        use crate::skills::lifecycle::{Bundle, BundleEntry, OwnerScope, SkillId, SkillLifecycleStore, SourceKind};
        let store = SkillLifecycleStore::open(workspace, OwnerScope::new(owner).unwrap()).unwrap();
        let skill = SkillId::parse("terminal-skill").unwrap();
        let bundle = Bundle::from_entries(vec![BundleEntry {
            path: "SKILL.md".into(),
            bytes: b"---\nname: terminal skill\ndescription: d\n---\nbody\n".to_vec(),
            executable: false,
        }])
        .unwrap();
        crate::skills::lifecycle::test_support::publish_revision(&store, &skill, SourceKind::Operator, &bundle, 0);
        let pin = store.pin_active(&skill).unwrap().unwrap();
        (store, skill, pin)
    }

    /// Whether `task` currently holds a pin for the fixture skill.
    fn holds_pin(owner: &str, task: &str, skill: &crate::skills::lifecycle::SkillId) -> bool {
        let owner = crate::skills::lifecycle::OwnerScope::new(owner).unwrap();
        let mut issued = false;
        crate::skills::lifecycle::task_pin(&owner, task, skill, || {
            issued = true;
            Ok(None)
        })
        .unwrap();
        !issued
    }

    fn pin_for(
        owner: &str,
        task: &str,
        skill: &crate::skills::lifecycle::SkillId,
        pin: &crate::skills::lifecycle::PinnedSkillRevision,
    ) {
        let owner = crate::skills::lifecycle::OwnerScope::new(owner).unwrap();
        crate::skills::lifecycle::task_pin(&owner, task, skill, || Ok(Some(pin.clone()))).unwrap();
    }

    async fn terminal_fixture(
        temp: &Path,
        terminal_id: &str,
        status: TurnTerminalStatus,
    ) -> (
        MemoryFabric,
        CostConfig,
        TurnTerminalDraft,
        Arc<crate::runtime::turn_decision::TurnDecisionService>,
    ) {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.to_string_lossy());
        let mut cost_config = CostConfig::default();
        cost_config.prices.insert(
            "ollama/prx-local-model".to_string(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let mut commit = completed_commit(terminal_id);
        if status != TurnTerminalStatus::Completed {
            commit.status = status;
            commit.provider_outcome = None;
            commit.delivery_intent = TurnDeliveryIntent::None;
        }
        let service = bind_terminal_fixture(temp, memory, &mut commit, &cost_config).await;
        (fabric, cost_config, commit, service)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_terminal_joins_the_skill_experience_and_releases_pins() {
        use crate::skills::lifecycle::{Correlation, ExperienceOutcome};
        let temp = tempfile::tempdir().unwrap();
        let (fabric, cost_config, commit, service) =
            terminal_fixture(temp.path(), "skill-terminal-run", TurnTerminalStatus::Completed).await;
        let owner = commit.scope.owner_id.clone().unwrap();
        let task = commit.scope.run_id.clone().unwrap();
        let (store, skill, pin) = skill_fixture(temp.path(), &owner);
        pin_for(&owner, &task, &skill, &pin);
        store
            .record_hydration(&task, &pin, "SKILL.md", b"x", &Correlation::default())
            .unwrap();
        assert!(holds_pin(&owner, &task, &skill));

        finalize_turn(&fabric, commit, &cost_config, temp.path()).await.unwrap();

        assert!(
            !holds_pin(&owner, &task, &skill),
            "the terminal releases the task's pins"
        );
        let experience = store.read_experience(&task).unwrap().unwrap();
        assert_eq!(experience.outcome, ExperienceOutcome::Success);
        assert_eq!(experience.terminal.unwrap().terminal_receipt_id, "skill-terminal-run");
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_early_return_after_the_marker_still_joins_and_releases() {
        use crate::skills::lifecycle::{Correlation, ExperienceOutcome};
        let temp = tempfile::tempdir().unwrap();
        let (fabric, cost_config, commit, service) =
            terminal_fixture(temp.path(), "skill-marker-run", TurnTerminalStatus::Completed).await;
        let owner = commit.scope.owner_id.clone().unwrap();
        let task = commit.scope.run_id.clone().unwrap();
        let (store, skill, pin) = skill_fixture(temp.path(), &owner);
        pin_for(&owner, &task, &skill, &pin);
        store
            .record_hydration(&task, &pin, "SKILL.md", b"x", &Correlation::default())
            .unwrap();

        // The process stops right after the durable marker (the early-return path).
        finalize_turn_stop_after_marker(&fabric, commit, &cost_config, temp.path())
            .await
            .unwrap();
        assert!(!holds_pin(&owner, &task, &skill));
        assert_eq!(
            store.read_experience(&task).unwrap().unwrap().outcome,
            ExperienceOutcome::Success
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_replayed_finalization_joins_idempotently_and_releases_again() {
        use crate::skills::lifecycle::{Correlation, ExperienceOutcome};
        let temp = tempfile::tempdir().unwrap();
        let (fabric, cost_config, commit, service) =
            terminal_fixture(temp.path(), "skill-replay-run", TurnTerminalStatus::Failed).await;
        let owner = commit.scope.owner_id.clone().unwrap();
        let task = commit.scope.run_id.clone().unwrap();
        let (store, skill, pin) = skill_fixture(temp.path(), &owner);
        pin_for(&owner, &task, &skill, &pin);
        store
            .record_hydration(&task, &pin, "SKILL.md", b"x", &Correlation::default())
            .unwrap();

        finalize_turn(&fabric, commit.clone(), &cost_config, temp.path())
            .await
            .unwrap();
        assert!(!holds_pin(&owner, &task, &skill));

        // The retry replays the same finalization: the join is idempotent and
        // pins taken in between are released again.
        pin_for(&owner, &task, &skill, &pin);
        finalize_turn(&fabric, commit, &cost_config, temp.path()).await.unwrap();
        assert!(!holds_pin(&owner, &task, &skill));
        let experience = store.read_experience(&task).unwrap().unwrap();
        assert_eq!(experience.outcome, ExperienceOutcome::Failure);
        assert_eq!(experience.late_evidence, 0);
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_silent_turn_releases_pins_but_is_not_credited() {
        let temp = tempfile::tempdir().unwrap();
        let owner = "owner:ws:cli:alice";
        let (store, skill, pin) = skill_fixture(temp.path(), owner);
        store
            .record_hydration(
                "silent-run",
                &pin,
                "SKILL.md",
                b"x",
                &crate::skills::lifecycle::Correlation::default(),
            )
            .unwrap();
        pin_for(owner, "silent-run", &skill, &pin);

        release_skill_lifecycle(
            temp.path(),
            Some(owner.to_string()),
            Some("silent-run".to_string()),
            "silent-term",
            TurnTerminalStatus::Silent,
            false,
            crate::skills::lifecycle::TerminalObservations::default(),
        )
        .await;

        assert!(!holds_pin(owner, "silent-run", &skill));
        let experience = store.read_experience("silent-run").unwrap().unwrap();
        assert!(experience.terminal.is_none() && !experience.learning_eligible());
        assert_eq!(
            experience.outcome,
            crate::skills::lifecycle::ExperienceOutcome::Incomplete
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_child_terminal_never_releases_the_parents_pins() {
        let temp = tempfile::tempdir().unwrap();
        let owner = "owner:ws:cli:alice";
        let (_store, skill, pin) = skill_fixture(temp.path(), owner);
        let parent = MessageEventScope::new("agent", MemoryVisibility::Workspace)
            .with_owner_id(owner)
            .with_run_id("run-parent");
        let mut child = MessageEventScope::new("agent", MemoryVisibility::Workspace)
            .with_owner_id(owner)
            .with_run_id("run-child")
            .with_parent_run_id("run-parent");
        // Even a child that inherited the parent's task id is keyed by its own run.
        child.subject = Some(crate::memory::MessageEventSubject::Task("run-parent".to_string()));
        assert_eq!(skill_task_key(&parent).as_deref(), Some("run-parent"));
        assert_eq!(skill_task_key(&child).as_deref(), Some("run-child"));

        pin_for(owner, "run-parent", &skill, &pin);
        pin_for(owner, "run-child", &skill, &pin);
        release_skill_lifecycle(
            temp.path(),
            child.owner_id.clone(),
            skill_task_key(&child),
            "child-term",
            TurnTerminalStatus::Completed,
            false,
            crate::skills::lifecycle::TerminalObservations::default(),
        )
        .await;

        assert!(!holds_pin(owner, "run-child", &skill));
        assert!(holds_pin(owner, "run-parent", &skill), "the parent keeps its pins");
        release_skill_lifecycle(
            temp.path(),
            parent.owner_id.clone(),
            skill_task_key(&parent),
            "parent-term",
            TurnTerminalStatus::Failed,
            false,
            crate::skills::lifecycle::TerminalObservations::default(),
        )
        .await;
        assert!(!holds_pin(owner, "run-parent", &skill));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_turn_waiting_for_approval_is_blocked_not_success() {
        use crate::skills::lifecycle::{Correlation, ExperienceOutcome};
        let temp = tempfile::tempdir().unwrap();
        let owner = "owner:ws:cli:alice";
        let (store, skill, pin) = skill_fixture(temp.path(), owner);
        store
            .record_hydration("approval-run", &pin, "SKILL.md", b"x", &Correlation::default())
            .unwrap();
        pin_for(owner, "approval-run", &skill, &pin);

        // Entrypoints report a turn that stopped for approval as Completed.
        release_skill_lifecycle(
            temp.path(),
            Some(owner.to_string()),
            Some("approval-run".to_string()),
            "approval-term",
            TurnTerminalStatus::Completed,
            true,
            crate::skills::lifecycle::TerminalObservations::default(),
        )
        .await;

        let experience = store.read_experience("approval-run").unwrap().unwrap();
        assert_eq!(experience.outcome, ExperienceOutcome::Blocked);
        assert!(!holds_pin(owner, "approval-run", &skill));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_finalization_that_fails_early_still_releases_pins() {
        let temp = tempfile::tempdir().unwrap();
        let (fabric, cost_config, mut commit, service) =
            terminal_fixture(temp.path(), "skill-lost-authority", TurnTerminalStatus::Completed).await;
        let owner = commit.scope.owner_id.clone().unwrap();
        let task = commit.scope.run_id.clone().unwrap();
        let (_store, skill, pin) = skill_fixture(temp.path(), &owner);
        pin_for(&owner, &task, &skill, &pin);
        commit.scope.authority_guard = Some(crate::memory::RuntimeAuthorityGuard::new("lost-authority", || {
            Ok(false)
        }));

        let error = finalize_turn(&fabric, commit, &cost_config, temp.path())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("runtime execution authority was lost"));
        assert!(
            !holds_pin(&owner, &task, &skill),
            "a failed finalization must not leak pins"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }

    /// The key contract the skill lifecycle depends on: the terminal derives the
    /// same key as `skill_read` does from the same envelope.
    #[test]
    fn the_terminal_key_matches_the_key_skill_read_derives_from_the_same_envelope() {
        use crate::runtime::envelope::RuntimeEnvelope;
        let scope_key = |envelope: &RuntimeEnvelope| skill_task_key(&envelope.message_scope());
        // What `skill_read` sees: the runtime-authored scope fields.
        let read_key = |envelope: &RuntimeEnvelope| {
            crate::skills::lifecycle::task_key(
                envelope.task_id.as_deref(),
                envelope.run_id.as_deref(),
                envelope.parent_run_id.as_deref(),
            )
        };
        let plain = RuntimeEnvelope::cron("ws", "job-1", "attempt-1");
        assert_eq!(scope_key(&plain), read_key(&plain));
        assert_eq!(scope_key(&plain).as_deref(), plain.run_id.as_deref());
        let child = plain.with_parent_run_id("run-parent");
        assert_eq!(scope_key(&child), read_key(&child));
        assert_eq!(scope_key(&child).as_deref(), child.run_id.as_deref());
        let task_only = RuntimeEnvelope::cron("ws", "job-2", "attempt-2");
        let mut task_only = task_only;
        task_only.run_id = None;
        assert_eq!(scope_key(&task_only).as_deref(), Some("job-2"));
        assert_eq!(scope_key(&task_only), read_key(&task_only));
    }
    /// The runtime trace and provider outcome reach the skill lifecycle as plain
    /// observations: an unavailable provider, a refused call and the called tools.
    #[test]
    fn skill_observations_carry_the_trace_and_provider_facts() {
        use crate::causal_tree::store::RecordedToolCall;
        use crate::skills::lifecycle::{ObservedCallStatus, ProviderEnd};
        use crate::tools::capability::execution_trace::ExecutionTraceSnapshot;
        let call = |id: &str, status: &str| RecordedToolCall {
            capability_id: id.to_string(),
            status: status.to_string(),
        };
        let trace = ExecutionTraceSnapshot {
            calls: vec![
                call("native_tool:native:deploy_check", "success"),
                call("shell:native:shell", "denied"),
                call("native_tool:native:odd", "no-such-code"),
            ],
            denied_attempts: 1,
            unavailable_attempts: Some(2),
            ..ExecutionTraceSnapshot::default()
        };
        let down = ProviderExecutionOutcome::failed_for_decision(
            &RouteDecision::single_candidate("ollama", "prx-local-model"),
            chrono::Utc::now(),
            &anyhow::anyhow!("request timed out"),
        );
        let seen = observations_from(&trace, Some(&down));
        assert_eq!(seen.provider, ProviderEnd::Unavailable);
        assert_eq!(seen.denied_attempts, 1);
        assert_eq!(seen.unavailable_attempts, 2);
        assert!(seen.complete);
        assert_eq!(
            seen.tool_calls.iter().map(|c| c.status).collect::<Vec<_>>(),
            vec![ObservedCallStatus::Succeeded, ObservedCallStatus::Denied],
            "a code the lifecycle does not know is dropped, not guessed"
        );

        // No outcome and no attempts: unknown, never served. An open trace is incomplete.
        let open = ExecutionTraceSnapshot {
            incomplete: true,
            ..ExecutionTraceSnapshot::default()
        };
        let seen = observations_from(&open, None);
        assert_eq!(seen.provider, ProviderEnd::Unknown);
        assert!(!seen.complete);
    }
    /// `abort_commit` turns the retained trace of an earlier, served round into a
    /// success outcome even when a later round failed. The execution trace must
    /// overrule it: the run ended in an outage, not a served answer.
    #[test]
    fn a_retained_success_does_not_make_a_later_outage_served() {
        use crate::agent::turn_contracts::{RetainedProviderTrace, ToolLoopTrace};
        use crate::llm::route_decision::{AttemptStatus, ProviderAttempt};
        use crate::skills::lifecycle::ProviderEnd;
        use crate::tools::capability::execution_trace::ExecutionTrace;
        let now = chrono::Utc::now();
        let attempt = ProviderAttempt {
            seq: 1,
            provider: "ollama".to_string(),
            model: "prx-local-model".to_string(),
            started_at: now,
            finished_at: now,
            status: AttemptStatus::Success,
            error_class: None,
            error_message: None,
        };
        let loop_trace = ToolLoopTrace {
            final_provider: Some("ollama".to_string()),
            final_model: Some("prx-local-model".to_string()),
            attempts: vec![attempt.clone()],
            ..ToolLoopTrace::default()
        };
        let retained = RetainedProviderTrace::default();
        retained.begin();
        retained.completed(
            &loop_trace,
            crate::llm::route_decision::TokenUsage::default(),
            false,
            true,
        );
        retained.begin();
        retained.failed();
        let (trace, uncertain) = retained.snapshot();
        assert!(uncertain, "the failed second round is flagged");
        let outcome = provider_outcome_from_trace(
            &RouteDecision::single_candidate("ollama", "prx-local-model"),
            now,
            trace.expect("test: the first round completed"),
        );
        assert!(matches!(
            outcome.status,
            crate::llm::route_decision::ExecutionStatus::Success
                | crate::llm::route_decision::ExecutionStatus::FallbackSuccess
        ));

        // The execution trace saw round 1 served and round 2 fail without an attempt.
        let execution = ExecutionTrace::default();
        execution
            .begin_provider()
            .expect("test: begin")
            .finish(Some(std::slice::from_ref(&attempt)));
        assert_eq!(
            observations_from(&execution.seal(), Some(&outcome)).provider,
            ProviderEnd::Served,
            "one served round with nothing else is served"
        );
        let execution = ExecutionTrace::default();
        execution
            .begin_provider()
            .expect("test: begin")
            .finish(Some(std::slice::from_ref(&attempt)));
        execution.begin_provider().expect("test: begin").finish(None);
        assert_eq!(
            observations_from(&execution.seal(), Some(&outcome)).provider,
            ProviderEnd::Unknown,
            "the outage after the served round is not served"
        );
    }
}
