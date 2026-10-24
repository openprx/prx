//! Chat reducer dispatch and admitted turn execution.
//!
//! - [`ChatDispatcher`] sends Actions through a bounded channel.
//! - [`EffectExecutor`] executes effects with [`EffectDeps`]; its shadow
//!   constructor remains available for isolated reducer tests.
//! - [`StreamChunkCoalescer`] retains the latest stream revision under
//!   backpressure instead of dropping intermediate state.
//!
//! Production terminal chat uses the Pure reducer mode. The admitted turn
//! binding supplies execution authority; an Action or a configured provider
//! alone cannot authorize another model turn. The ingress layer owns source,
//! route and terminal records, while the dispatcher owns stream-state ordering.
//! [`RuntimeDualWriteGuard`] coordinates persistence ownership within that flow;
//! it does not select a second legacy executor. Long-running effects retain
//! their cancellation and completion ownership while returning Actions to the
//! reducer. Shutdown signals enter through the same Action channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex as ParkingMutex;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::sync::CancellationToken;

use crate::channels::Channel;
use crate::chat::action::Action;
use crate::chat::state::{ChatState, Effect};
use crate::hooks::HookManager;
use crate::llm::route_decision::{ProviderUsageAccumulator, TokenUsage};
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric};
use crate::observability::Observer;
use crate::providers::Provider;
use crate::tools::{
    ApprovalStrategy, SecurityEffectPolicy, ToolApprovalDecision, ToolApprovalRequest, ToolExecutionCommand,
    ToolExecutionContext, ToolExecutionPermit, ToolExecutionPreparation, ToolExecutionService,
    TracingToolExecutionAudit,
};

/// Upper bound on the Action channel capacity (Codex P0-3).
///
/// 2048 was chosen to cover the burst of a typical chat session (user input + streaming chunks +
/// tool events) while still triggering backpressure → coalescing before OOM.
pub const ACTION_CHANNEL_CAPACITY: usize = 2048;

// ─── ApprovalRouter (S3 T3-1) ─────────────────────────────────────────────────

/// **S3 T3-1**: tool approval request/response router.
///
/// Before running a tool that needs approval, the driver registers a `tool_id → oneshot::Sender<bool>`;
/// once the reducer has handled `Action::ToolApprovalReceived`, dispatcher_task calls
/// [`Self::resolve`] to hand the decision back to the driver blocked on the oneshot rx.
///
/// Design notes (Codex audit, recommended option B+D):
/// - one oneshot per request, naturally fire-and-forget and never consumed twice
/// - `Arc<ApprovalRouter>` shares ownership across spawn boundaries (driver / dispatcher_task)
/// - parking_lot Mutex: register/resolve are short synchronous operations, the lock is never held across await
/// - reject / timeout / cancel paths are all cleaned up by the driver itself (drop the oneshot tx)
///
/// Invariant: at most one foreground approval is pending at a time. Later requests fail closed and
/// resolve to false; they never override the human approval currently displayed/awaited.
#[derive(Default)]
pub struct ApprovalRouter {
    pending: ParkingMutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
}

impl ApprovalRouter {
    /// Construct an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Register a pending approval on behalf of the driver (`tool_id`→`tx`).
    ///
    /// If any approval is already pending, the new request immediately fails closed (sends false on the
    /// new tx) and returns false; the caller must not render a second approval prompt.
    pub fn register(&self, tool_id: String, tx: tokio::sync::oneshot::Sender<bool>) -> bool {
        let mut guard = self.pending.lock();
        if !guard.is_empty() {
            tracing::warn!(
                tool_id = %tool_id,
                pending = guard.len(),
                "ApprovalRouter::register: rejecting concurrent approval request"
            );
            drop(guard);
            let _ = tx.send(false);
            return false;
        }
        if guard.insert(tool_id.clone(), tx).is_some() {
            tracing::warn!(tool_id = %tool_id, "ApprovalRouter::register: replacing existing pending tx");
        }
        true
    }

    /// True when any foreground approval is waiting for a decision.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    /// Resolve every pending approval with one decision and return their ids.
    ///
    /// Used by session-switch cleanup to fail closed before swapping sessions.
    pub fn resolve_all(&self, approved: bool) -> Vec<String> {
        let pending = std::mem::take(&mut *self.pending.lock());
        let mut resolved = Vec::with_capacity(pending.len());
        for (tool_id, tx) in pending {
            let _ = tx.send(approved);
            resolved.push(tool_id);
        }
        resolved
    }

    /// Called by dispatcher_task: take the pending sender out and resolve the decision.
    ///
    /// Returns false when the `tool_id` is not found (driver already cleaned up on timeout / cancel path).
    pub fn resolve(&self, tool_id: &str, approved: bool) -> bool {
        let tx_opt = self.pending.lock().remove(tool_id);
        tx_opt.map_or_else(
            || {
                tracing::debug!(tool_id = %tool_id, "ApprovalRouter::resolve: no pending entry");
                false
            },
            |tx| {
                if tx.send(approved).is_err() {
                    tracing::debug!(
                        tool_id = %tool_id,
                        "ApprovalRouter::resolve: rx already dropped (driver cancelled)"
                    );
                }
                true
            },
        )
    }
}

/// TUI bridge for the common tool-execution approval stage.
struct ChatTuiApprovalStrategy {
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    router: Arc<ApprovalRouter>,
    action_tx: mpsc::Sender<Action>,
    cancellation: CancellationToken,
    policy: Arc<crate::security::SecurityPolicy>,
}

#[async_trait]
impl ApprovalStrategy for ChatTuiApprovalStrategy {
    async fn resolve(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let tool_id = request.command.operation_id.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let registered = self.router.register(tool_id.clone(), tx);
        if registered
            && self
                .action_tx
                .send(Action::ToolApprovalRequested {
                    task_id: self.task_id,
                    tool_id: tool_id.clone(),
                    name: request.descriptor.public_name.clone(),
                    args: request.command.arguments.to_string(),
                })
                .await
                .is_err()
        {
            let _ = self.router.resolve(&tool_id, false);
            return ToolApprovalDecision::Denied {
                reason: "approval request channel is unavailable; tool rejected for safety".to_string(),
            };
        }

        let approved = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => {
                let _ = self.router.resolve(&tool_id, false);
                return ToolApprovalDecision::Cancelled {
                    reason: "tool approval cancelled with the active turn".to_string(),
                };
            }
            result = rx => result.unwrap_or(false),
        };
        if !approved {
            return ToolApprovalDecision::Denied {
                reason: if registered {
                    "User rejected tool approval".to_string()
                } else {
                    "Another tool approval is already pending; this tool was rejected for safety".to_string()
                },
            };
        }

        let envelope = &request.context.envelope;
        let sender = envelope.sender.as_deref().unwrap_or("unknown");
        let channel = envelope.channel.as_deref().unwrap_or("unknown");
        let scope = crate::agent::turn_contracts::ScopeContext {
            policy: self.policy.as_ref(),
            sender,
            channel,
            chat_type: &request.context.chat_type,
            chat_id: &request.context.chat_id,
            session_key: Some(&envelope.session_key),
            owner_id: envelope.owner_id.as_deref(),
            owner_isolated: envelope.owner_isolated,
            topic_id: envelope.topic_id.as_deref(),
            task_id: envelope.task_id.as_deref(),
            source_message_event_id: envelope.source_message_event_id.as_deref(),
            config_generation_id: envelope.config_generation_id,
            config_source_revision: envelope.config_source_revision.as_deref(),
        };
        let runtime_grant = crate::agent::execution_policy::runtime_approval_grant_for_call(
            &request.descriptor.public_name,
            &request.command.arguments,
            Some(&scope),
        )
        .and_then(|grant| match serde_json::to_value(grant) {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::error!(tool = %request.descriptor.public_name, %error, "failed to serialize runtime approval grant");
                None
            }
        });
        ToolApprovalDecision::Approved {
            runtime_approval_granted: true,
            runtime_grant,
        }
    }
}

/// Chat's UI readiness check is the concrete execution-preparation adapter. It
/// preserves the existing invariant that `ToolStarted` is visible only after
/// policy/approval and before the raw tool future begins.
struct ChatToolExecutionPreparation {
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    action_tx: mpsc::Sender<Action>,
}

#[async_trait]
impl ToolExecutionPreparation for ChatToolExecutionPreparation {
    async fn prepare(
        &self,
        descriptor: &crate::tools::ToolDescriptor,
        command: &ToolExecutionCommand,
        _context: &ToolExecutionContext,
    ) -> Result<ToolExecutionPermit, String> {
        self.action_tx
            .send(Action::ToolStarted {
                task_id: self.task_id,
                sequence: None,
                tool_call_id: Some(command.operation_id.clone()),
                name: descriptor.public_name.clone(),
                args: command.arguments.to_string(),
            })
            .await
            .map_err(|_| "chat action channel closed before tool execution".to_string())?;
        Ok(ToolExecutionPermit {
            strategy: "chat_dispatch_ready".to_string(),
        })
    }
}

#[cfg(test)]
mod approval_router_regression_tests;

// ─── ChatDispatcher ────────────────────────────────────────────────────────────

/// `Action` sender wrapper. Exposes only the `try_send` / `send` policies; unbounded clones are forbidden.
///
/// - `try_send`: non-blocking — for streaming chunks / control Actions (when full the caller should use the coalescer)
/// - `send_blocking`: blocking synchronous path — for critical exit Actions (Ctrl+C / SIGTERM handler)
/// - `send`: async blocking — for critical Actions when the caller is in an async context (e.g. the main loop)
#[derive(Clone)]
pub struct ChatDispatcher {
    action_tx: mpsc::Sender<Action>,
}

/// Result of the `try_send` policy (lets the caller decide whether coalescing / a fallback is needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchResult {
    /// Action was enqueued
    Sent,
    /// Channel is full (the caller should coalesce or drop)
    Backpressured,
    /// Channel is closed (the dispatcher task has exited)
    ChannelClosed,
}

impl ChatDispatcher {
    /// Construct the dispatcher plus its receiver. The receiver is consumed by the dispatcher task
    /// ([`spawn_dispatcher_task_with_signal`]).
    pub fn new() -> (Self, mpsc::Receiver<Action>) {
        let (action_tx, action_rx) = mpsc::channel::<Action>(ACTION_CHANNEL_CAPACITY);
        (Self { action_tx }, action_rx)
    }

    /// Non-blocking send. Returns `Backpressured` when full; the caller decides to coalesce or drop.
    pub fn try_dispatch(&self, action: Action) -> DispatchResult {
        match self.action_tx.try_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(TrySendError::Full(_)) => DispatchResult::Backpressured,
            Err(TrySendError::Closed(_)) => DispatchResult::ChannelClosed,
        }
    }

    /// S2.5 P1-A: `try_dispatch` + tracing::warn on failure + Prometheus counter.
    ///
    /// Main-path callers should prefer this helper over a bare `try_dispatch`, so Actions are not
    /// silently lost when the channel is full / closed. `site_tag` tags the failure log with the call
    /// site (e.g. "chat.banner" / "chat.shutdown_sigint" / "chat.user_input"), so gaps can be located
    /// later by grep. Returns the original `DispatchResult` for further handling by the caller.
    pub fn dispatch_or_log(&self, action: Action, site: &'static str) -> DispatchResult {
        let action_kind = action.kind();
        let result = self.try_dispatch(action);
        match result {
            DispatchResult::Sent => {}
            DispatchResult::Backpressured => {
                tracing::warn!(
                    site = site,
                    action_kind = action_kind,
                    "chat dispatch failed: channel backpressured, action dropped"
                );
                crate::observability::chat_metrics::inc_dispatch_drop("backpressured");
            }
            DispatchResult::ChannelClosed => {
                tracing::warn!(
                    site = site,
                    action_kind = action_kind,
                    "chat dispatch failed: channel closed, action dropped"
                );
                crate::observability::chat_metrics::inc_dispatch_drop("closed");
            }
        }
        result
    }

    /// Synchronous blocking send (only call outside an async context, e.g. the sync part of an OS signal handler).
    ///
    /// **Note**: blocking_send is not allowed inside the tokio runtime, it would panic.
    /// The Ctrl+C / SIGTERM handler runs inside a spawned task (async context), so it should prefer
    /// [`Self::try_dispatch`] or [`Self::dispatch`].
    pub fn blocking_dispatch(&self, action: Action) -> DispatchResult {
        match self.action_tx.blocking_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(_) => DispatchResult::ChannelClosed,
        }
    }

    /// Async blocking send (recommended: safe backpressure on async paths).
    pub async fn dispatch(&self, action: Action) -> DispatchResult {
        match self.action_tx.send(action).await {
            Ok(()) => DispatchResult::Sent,
            Err(_) => DispatchResult::ChannelClosed,
        }
    }

    /// Return a clone of the underlying sender, for subtasks that need to hold an `mpsc::Sender` directly.
    ///
    /// Warning: holding the sender directly bypasses the policy checks in [`Self::try_dispatch`].
    /// Only use it where fine-grained `TrySendError` handling is required, such as the coalescer.
    pub fn sender(&self) -> mpsc::Sender<Action> {
        self.action_tx.clone()
    }
}

// ─── TurnCompletionSignal (Step 5a-4) ─────────────────────────────────────────

/// Semantic result of a turn's termination, written by the dispatcher in
/// [`TurnCompletionSignal::record_and_notify`] and read by chat::run after await to decide UI/hook behaviour.
#[derive(Debug, Clone)]
pub enum TurnOutcomeKind {
    /// The LLM stream completed successfully; `final_text` is the final accumulated visible text, `reasoning`
    /// carries the final reasoning card payload that must be replayed when an
    /// ordered commit gate releases the reducer terminal action.
    Completed { final_text: String, reasoning: String },
    /// The LLM stream failed; `err` is the error description carried by [`Action::StreamFailed`],
    /// `retryable` tells whether the failure was reported as transient.
    Failed { err: String, retryable: bool },
    /// The user cancelled, or shutdown preempted the turn.
    Cancelled,
}

/// Explicit turn-termination signal plus result slot, used by `chat::run` to await turn completion and
/// read the semantic result when the Redux driver path is switched on.
///
/// When the dispatcher task detects a terminal action after `state.reduce(action)`
/// (`StreamCompleted` / `StreamFailed` / `StreamCancelled`) it:
///   1. writes the matching [`TurnOutcomeKind`] into the `outcome` slot
///   2. calls `notify_waiters` to wake every waiter
///
/// Decoupled from `RuntimeDualWriteGuard` — the guard is a dual-write suppression switch, not a turn
/// lifecycle signal; modelling the turn lifecycle as its own `Notify + Mutex<Option<Outcome>>` keeps the
/// semantics clear, testable and free of busy waiting.
///
/// Design choice: `tokio::sync::Notify` rather than `oneshot::channel`:
/// - chat::run reuses one signal across many turns; a oneshot can only fire once
/// - `notify_waiters` is latch-less: the `notified()` future must be acquired before notifying, or it is missed
/// - protocol: before each StartLLMTurn dispatch, chat::run first acquires the `notified()` future
///   and calls `consume_outcome()` to clear the old slot, then dispatches, and finally awaits the future.
#[derive(Clone)]
pub struct TurnCompletionSignal {
    turn_cancellation: CancellationToken,
    commit_failure: Arc<ParkingMutex<Option<String>>>,
    inner: Arc<tokio::sync::Notify>,
    outcome: Arc<ParkingMutex<Option<TurnOutcomeKind>>>,
    usage: Arc<ParkingMutex<ProviderUsageAccumulator>>,
    keyed: Arc<ParkingMutex<KeyedTurnCompletionState>>,
}

#[derive(Default)]
struct KeyedTurnCompletionState {
    task_by_draft: std::collections::HashMap<String, crate::chat::turn_scheduler::TurnTaskId>,
    slots: std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, KeyedTurnCompletionSlot>,
}

struct KeyedTurnCompletionSlot {
    draft_id: String,
    notify: Arc<tokio::sync::Notify>,
    outcome: Option<TurnOutcomeKind>,
    usage: ProviderUsageAccumulator,
}

impl TurnCompletionSignal {
    /// Construct a new signal instance.
    #[must_use]
    pub fn new() -> Self {
        Self::with_turn_cancellation(CancellationToken::new())
    }

    pub(super) fn with_turn_cancellation(turn_cancellation: CancellationToken) -> Self {
        Self {
            turn_cancellation,
            commit_failure: Arc::new(ParkingMutex::new(None)),
            inner: Arc::new(tokio::sync::Notify::new()),
            outcome: Arc::new(parking_lot::Mutex::new(None)),
            usage: Arc::new(ParkingMutex::new(ProviderUsageAccumulator::new())),
            keyed: Arc::new(ParkingMutex::new(KeyedTurnCompletionState::default())),
        }
    }

    pub(super) fn new_turn_cancellation(&self) -> CancellationToken {
        self.turn_cancellation.child_token()
    }

    pub(super) fn cancel_turns_on_exit(&self) -> tokio_util::sync::DropGuard {
        self.turn_cancellation.clone().drop_guard()
    }

    /// A failed history commit invalidates subsequent session projections.
    /// This latch is process-local and cannot be cleared by a new UI action.
    pub(super) fn fail_history_commit(&self, reason: impl Into<String>) {
        let mut failure = self.commit_failure.lock();
        if failure.is_none() {
            *failure = Some(reason.into());
        }
        drop(failure);
        self.turn_cancellation.cancel();
    }

    pub(super) fn ensure_history_commits_healthy(&self) -> anyhow::Result<()> {
        if let Some(reason) = self.commit_failure.lock().as_ref() {
            anyhow::bail!("Chat stopped after an unconfirmed history commit: {reason}");
        }
        Ok(())
    }

    pub fn record_usage(&self, usage: TokenUsage) {
        self.usage.lock().record(usage);
    }

    pub fn register_turn(&self, task_id: crate::chat::turn_scheduler::TurnTaskId, draft_id: impl Into<String>) {
        let draft_id = draft_id.into();
        let mut keyed = self.keyed.lock();
        if let Some(previous) = keyed.task_by_draft.insert(draft_id.clone(), task_id)
            && previous != task_id
            && let Some(slot) = keyed.slots.get_mut(&previous)
        {
            slot.draft_id.clear();
        }
        keyed.slots.insert(
            task_id,
            KeyedTurnCompletionSlot {
                draft_id,
                notify: Arc::new(tokio::sync::Notify::new()),
                outcome: None,
                usage: ProviderUsageAccumulator::new(),
            },
        );
    }

    pub fn unregister_turn(&self, task_id: crate::chat::turn_scheduler::TurnTaskId) {
        let mut keyed = self.keyed.lock();
        if let Some(slot) = keyed.slots.remove(&task_id)
            && !slot.draft_id.is_empty()
        {
            keyed.task_by_draft.remove(&slot.draft_id);
        }
    }

    #[must_use]
    pub fn notified_for(
        &self,
        task_id: crate::chat::turn_scheduler::TurnTaskId,
    ) -> Option<tokio::sync::futures::OwnedNotified> {
        self.keyed
            .lock()
            .slots
            .get(&task_id)
            .map(|slot| slot.notify.clone().notified_owned())
    }

    pub fn record_usage_for_draft(&self, draft_id: &str, usage: TokenUsage) -> bool {
        let mut keyed = self.keyed.lock();
        let Some(task_id) = keyed.task_by_draft.get(draft_id).copied() else {
            return false;
        };
        let Some(slot) = keyed.slots.get_mut(&task_id) else {
            return false;
        };
        slot.usage.record(usage);
        true
    }

    pub fn record_and_notify_for_draft(&self, draft_id: &str, outcome: TurnOutcomeKind) -> bool {
        let notify = {
            let mut keyed = self.keyed.lock();
            let Some(task_id) = keyed.task_by_draft.get(draft_id).copied() else {
                return false;
            };
            let Some(slot) = keyed.slots.get_mut(&task_id) else {
                return false;
            };
            slot.outcome = Some(outcome);
            slot.notify.clone()
        };
        notify.notify_waiters();
        true
    }

    #[must_use]
    pub fn consume_turn_outcome(&self, task_id: crate::chat::turn_scheduler::TurnTaskId) -> Option<TurnOutcomeKind> {
        self.keyed
            .lock()
            .slots
            .get_mut(&task_id)
            .and_then(|slot| slot.outcome.take())
    }

    /// Consume the task-scoped final aggregate collected for one provider turn.
    /// Session-level dedup happens when this aggregate is recorded as
    /// `ProviderUsageRecordKind::FinalAggregate`.
    pub fn consume_turn_usage(&self, task_id: crate::chat::turn_scheduler::TurnTaskId) -> TokenUsage {
        let mut keyed = self.keyed.lock();
        let Some(slot) = keyed.slots.get_mut(&task_id) else {
            return ProviderUsageAccumulator::new().finish();
        };
        let usage = slot.usage.finish();
        slot.usage = ProviderUsageAccumulator::new();
        usage
    }

    /// Called by the dispatcher task: write the outcome and wake the waiters.
    pub fn record_and_notify(&self, outcome: TurnOutcomeKind) {
        *self.outcome.lock() = Some(outcome);
        self.inner.notify_waiters();
    }

    /// Fallback notification (no outcome written, e.g. shutdown preemption).
    /// A waiter that reads `None` must treat the turn as cancelled.
    pub fn notify(&self) {
        self.inner.notify_waiters();
        let notifiers: Vec<_> = self
            .keyed
            .lock()
            .slots
            .values()
            .map(|slot| slot.notify.clone())
            .collect();
        for notify in notifiers {
            notify.notify_waiters();
        }
    }

    /// Return the `Notified` future. chat::run protocol: call before dispatch, await after dispatch.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.inner.notified()
    }

    /// Take the current outcome (consuming). `None` means no terminal event was recorded (shutdown fallback).
    #[must_use]
    pub fn consume_outcome(&self) -> Option<TurnOutcomeKind> {
        self.outcome.lock().take()
    }

    pub fn consume_usage(&self) -> TokenUsage {
        let mut guard = self.usage.lock();
        let usage = guard.finish();
        *guard = ProviderUsageAccumulator::new();
        usage
    }
}

impl Default for TurnCompletionSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TurnCompletionSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnCompletionSignal").finish_non_exhaustive()
    }
}

/// Map an action type to a turn outcome (used by the dispatcher task to extract it before reduce).
#[must_use]
pub fn extract_turn_outcome(action: &Action) -> Option<TurnOutcomeKind> {
    match action {
        Action::StreamCompleted {
            final_text, reasoning, ..
        }
        | Action::ProviderTurnReadyForCommit {
            final_text, reasoning, ..
        } => Some(TurnOutcomeKind::Completed {
            final_text: final_text.clone(),
            reasoning: reasoning.clone(),
        }),
        Action::StreamFailed { err, retryable, .. } => Some(TurnOutcomeKind::Failed {
            err: err.clone(),
            retryable: *retryable,
        }),
        Action::StreamCancelled { .. } => Some(TurnOutcomeKind::Cancelled),
        _ => None,
    }
}

#[must_use]
pub fn extract_stream_usage(action: &Action) -> Option<TokenUsage> {
    match action {
        Action::StreamUsageMetered { usage, .. } => Some(usage.clone()),
        _ => None,
    }
}

#[must_use]
pub fn extract_turn_draft_id(action: &Action) -> Option<&str> {
    match action {
        Action::StreamUsageMetered { draft_id, .. }
        | Action::StreamCompleted { draft_id, .. }
        | Action::ProviderTurnReadyForCommit { draft_id, .. }
        | Action::StreamFailed { draft_id, .. }
        | Action::StreamCancelled { draft_id } => Some(draft_id),
        _ => None,
    }
}

fn record_turn_signal_action(
    sig: &TurnCompletionSignal,
    draft_id: Option<&str>,
    usage: Option<TokenUsage>,
    outcome: Option<TurnOutcomeKind>,
) {
    if let Some(usage) = usage {
        sig.record_usage(usage.clone());
        if let Some(draft_id) = draft_id {
            let _ = sig.record_usage_for_draft(draft_id, usage);
        }
    }
    if let Some(outcome) = outcome {
        sig.record_and_notify(outcome.clone());
        if let Some(draft_id) = draft_id {
            let _ = sig.record_and_notify_for_draft(draft_id, outcome);
        }
    }
}

// ─── RuntimeDualWriteGuard ─────────────────────────────────────────────────────

/// Dual-write suppression counter (Step 5a-1; 5a-5 Codex P1 fix: bool → AtomicU64 counter).
///
/// In Both/Redux mode the legacy path still runs while business Effects really execute. To keep
/// persistent resources such as history / session from being written twice, the reducer path does a +1
/// before executing a business Effect, and the legacy path skips its own write when the counter is > 0.
///
/// **5a-5 fix**: this used to be an `AtomicBool`, which had a serious timing window — when several
/// effects held a `DualWriteGuardScope` concurrently, one scope's drop cleared the whole active state,
/// letting another still-running effect's legacy path through. It is now an `AtomicU64` counter: each
/// scope enters with `fetch_add(1)` and exits with `fetch_sub(1)`, and `is_active()` is simply `> 0`.
///
/// The guard is held by `chat::run` as an `Arc<AtomicU64>` and shared by the dispatcher and the legacy path.
/// It is only constructed in Both/Redux mode; Off mode does not construct it (the legacy path writes once as usual).
///
/// Note: the guard is not a mutex — it is a policy switch, not a lock. When the legacy path finds the
/// counter > 0 it simply `continue`s; there is no wait semantics. That rules out any deadlock during dual writes.
#[derive(Debug, Clone)]
pub struct RuntimeDualWriteGuard {
    /// Active scope count (> 0 → the legacy path skips the matching persistence).
    active: Arc<AtomicU64>,
}

impl RuntimeDualWriteGuard {
    /// Construct a new guard (active=0, the legacy path persists as usual).
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Let the legacy path query whether Redux has preempted it (count > 0).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) > 0
    }

    /// Test observability: return the current active scope count (cfg(test) only, production code does not need it).
    #[cfg(test)]
    #[must_use]
    pub fn active_count(&self) -> u64 {
        self.active.load(Ordering::Acquire)
    }

    /// Create an RAII scope: +1 on entry, automatic -1 on exit (or panic).
    ///
    /// When several scopes exist at once the counter accumulates; it only returns to 0 after every scope drops.
    /// This fixes the "early drop wrongly clears" problem of the pre-5a-4 bool version.
    #[must_use]
    pub fn enter_scope(&self) -> DualWriteGuardScope {
        DualWriteGuardScope::enter(Arc::clone(&self.active))
    }
}

impl Default for RuntimeDualWriteGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII scope for [`RuntimeDualWriteGuard`].
///
/// Entry accumulates via `fetch_add(1)`; `Drop` releases via `fetch_sub(1)`, on both normal exit and
/// panic unwind, so the counter always reflects the number of active scopes.
///
/// Constructed through `RuntimeDualWriteGuard::enter_scope()`.
pub struct DualWriteGuardScope {
    inner: Arc<AtomicU64>,
}

impl DualWriteGuardScope {
    fn enter(inner: Arc<AtomicU64>) -> Self {
        inner.fetch_add(1, Ordering::Release);
        Self { inner }
    }
}

impl Drop for DualWriteGuardScope {
    fn drop(&mut self) {
        // saturating_sub via fetch_update would be safer, but counters are
        // strictly balanced (every fetch_add followed by exactly one Drop),
        // so fetch_sub is correct. Underflow would indicate a logic bug.
        self.inner.fetch_sub(1, Ordering::Release);
    }
}

// ─── ModelSlot (BUG-07: switch model online) ──────────────────────────────────

/// Hot-swappable model name handle.
///
/// BUG-07: `/model <name>` must update the model on the chat::run main loop side, while the code that
/// actually reads the model, `drive_start_turn_stream`, runs in a spawned dispatcher subtask (across the
/// spawn boundary, `EffectDeps` has already been moved into `EffectExecutor`). `Arc<RwLock<Arc<str>>>`
/// provides interior mutability: the main loop calls [`Self::set`] to replace it, and the subtask calls
/// [`Self::current`] at the start of each turn to read the newest value (for switching model on the same
/// provider, the provider itself is called per-call with a model, so no rebuild is needed). Reads happen
/// once per turn, so RwLock overhead is negligible, and it is naturally Send + Sync with no extra bounds.
#[derive(Clone)]
pub struct ModelSlot(Arc<parking_lot::RwLock<Arc<str>>>);

impl ModelSlot {
    /// Construct with the initial model name.
    #[must_use]
    pub fn new(model: Arc<str>) -> Self {
        Self(Arc::new(parking_lot::RwLock::new(model)))
    }

    /// Read the current model name. Returns an owned `Arc<str>` (clone is just an Arc bump) to hold across await.
    #[must_use]
    pub fn current(&self) -> Arc<str> {
        Arc::clone(&self.0.read())
    }

    /// Replace the model name. The next turn's `current()` reads the new value.
    pub fn set(&self, model: Arc<str>) {
        *self.0.write() = model;
    }
}

impl From<Arc<str>> for ModelSlot {
    fn from(model: Arc<str>) -> Self {
        Self::new(model)
    }
}

impl From<&str> for ModelSlot {
    fn from(model: &str) -> Self {
        Self::new(Arc::from(model))
    }
}

// ─── ProviderSlot (Bug #3: switch provider online) ───────────────────────────────

/// Hot-swappable provider handle.
///
/// Bug #3: `/provider <name>` must rebuild the provider on the chat::run main loop side, while the code
/// that actually reads the provider, `drive_start_turn_stream`, runs in a spawned dispatcher subtask
/// (across the spawn boundary, `EffectDeps` has already been moved into `EffectExecutor`). Same shape as
/// [`ModelSlot`]: `Arc<RwLock<Arc<dyn Provider>>>` provides interior mutability. After the main loop
/// rebuilds the provider it calls [`Self::set`] to swap it atomically, and the subtask calls
/// [`Self::current`] to read the newest handle each turn. Reads happen once per turn, so RwLock overhead is negligible.
///
/// Note: `provider` itself is wrapped in an `Arc`, so clone is just an Arc bump; the whole provider is
/// replaced (not just the model) because providers differ entirely in auth/base-url/protocol, so it must be rebuilt.
#[derive(Clone)]
pub struct ProviderSlot(Arc<parking_lot::RwLock<Arc<dyn Provider>>>);

impl ProviderSlot {
    /// Construct with the initial provider handle.
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self(Arc::new(parking_lot::RwLock::new(provider)))
    }

    /// Read the current provider handle. Returns an owned `Arc` (clone is just an Arc bump) to hold across await.
    #[must_use]
    pub fn current(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.0.read())
    }

    /// Replace the provider handle. The next turn's `current()` reads the new value.
    pub fn set(&self, provider: Arc<dyn Provider>) {
        *self.0.write() = provider;
    }
}

#[derive(Debug)]
pub enum ProviderTurnLifecycleEvent {
    HandleAttached {
        task_id: crate::chat::turn_scheduler::TurnTaskId,
        lease_id: u64,
        abort_handle: tokio::task::AbortHandle,
    },
    Started {
        task_id: crate::chat::turn_scheduler::TurnTaskId,
        lease_id: u64,
    },
    Exited {
        task_id: crate::chat::turn_scheduler::TurnTaskId,
        lease_id: u64,
    },
}

static PROVIDER_TURN_EXECUTION_LEASE_IDS: AtomicU64 = AtomicU64::new(1);

fn next_provider_turn_execution_lease_id() -> u64 {
    PROVIDER_TURN_EXECUTION_LEASE_IDS.fetch_add(1, Ordering::Relaxed)
}

// ─── EffectDeps ────────────────────────────────────────────────────────────────

/// Dependencies required for real business execution in EffectExecutor.
///
/// Collected by `chat::run` at startup; cloning costs only an Arc bump, so it is safely passed to spawned
/// subtasks. Missing any field equals shadow mode (construction forces `new_with_deps` to take every field).
#[derive(Clone)]
pub struct EffectDeps {
    /// Current provider (LLM calls) — in Step 5a-1 only StartTurn uses it, later effects reuse it
    pub provider: Arc<dyn Provider>,
    /// memory backend (SaveSession / PersistToMemory)
    pub memory: Arc<dyn Memory>,
    /// Message-event policy shared with the chat ingress. Request-context
    /// provenance must obey the same persistence switch as the transcript.
    pub memory_event_recording: MemoryEventRecording,
    /// current channel (EmitChannelMessage / SendDraftFinalize / CancelDraft)
    pub channel: Arc<dyn Channel>,
    /// hook manager (NotifyHook)
    pub hooks: Arc<HookManager>,
    /// observability observer (structured events)
    pub observer: Arc<dyn Observer>,
    /// Action feedback channel sender (streaming callbacks of the StartTurn subtask)
    pub action_tx: mpsc::Sender<Action>,
    /// Provider task lifecycle event bridge back to chat::run. This is separate
    /// from action_tx because it is orchestration metadata, not reducer state.
    ///
    /// Bounded at [`crate::chat::provider_turns::PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY`]; a
    /// full queue parks the emitting turn rather than growing without bound.
    pub provider_turn_lifecycle_tx: Option<mpsc::Sender<ProviderTurnLifecycleEvent>>,
    /// dual-write suppression guard (set before persistence effects in Both/Redux mode)
    pub dual_write_guard: RuntimeDualWriteGuard,
    /// render redraw channel (RequestRedraw wakes the main loop)
    /// mpsc::Sender<()> rather than broadcast, because we only need to nudge the main loop
    pub redraw_tx: Option<mpsc::Sender<()>>,
    /// TUI mirror used only to surface foreground approval prompts on the
    /// interactive terminal path. The ApprovalRouter remains the execution gate.
    #[cfg(feature = "terminal-tui")]
    pub tui_mirror: Option<Arc<ParkingMutex<crate::chat::tui::TuiState>>>,
    /// shutdown signal (triggered by Effect::Quit)
    pub shutdown: CancellationToken,
    /// Step 5a-4 (Codex P1): current LLM model name, used by drive_start_turn_stream to call
    /// `provider.stream_chat_with_history(_, model, _, _)`. It used to be hard-coded to an empty
    /// string, which real providers (OpenAI/Anthropic) reject; mock provider tests hid the problem.
    pub model: ModelSlot,
    /// current temperature (injected from CLI args by default; passed to the stream API together with model).
    pub temperature: f64,
    /// **5a-6**: tool registry — the driver looks up a tool by name and calls it when executing a tool_call.
    /// `None` means tool calls are not allowed this turn (the driver emits StreamFailed on a tool_call).
    /// `Arc<Vec<Box<dyn Tool>>>` rather than a slice: shares ownership across spawn, clone is just an Arc bump.
    pub tools_registry: Option<Arc<Vec<Box<dyn crate::tools::Tool>>>>,
    /// **S3 T3-1**: approval request/response router (driver↔dispatcher oneshot bridge).
    ///
    /// The driver registers a oneshot tx before running a tool that needs approval; after the reducer has
    /// handled `Action::ToolApprovalReceived`, dispatcher_task calls `resolve()` to send the decision back.
    /// `Arc` shares ownership across the spawn boundary.
    pub approval_router: Arc<ApprovalRouter>,
    /// Authoritative tool authorization policy used by ToolExecutionService.
    /// The TUI router above is only the human confirmation adapter; it cannot
    /// widen ACL or autonomy decisions.
    pub tool_security_policy: Arc<crate::security::SecurityPolicy>,
    /// Capability-routing policy for this chat session's turns.
    ///
    /// Every other entry point (channels, gateway, console, worker, spawn,
    /// delegate) hands `[tool_tiering]` to the shared tool loop; the redux chat
    /// driver used to pass `None`, which disables intent routing entirely and
    /// republished the whole registry to the provider on every single turn.
    /// Carrying it here puts terminal chat back on the same boundary.
    pub tool_tiering: crate::config::ToolTieringConfig,
}

// ─── EffectExecutor (5a-1: real-mode + shadow-mode) ───────────────────────────

/// `Effect` executor. Two construction shapes:
/// - shadow construction (`new_shadow`): every business Effect except `LogTrace` is a no-op;
///   used by isolated reducer tests
/// - real mode (`new_with_deps`): holds [`EffectDeps`] and really executes business Effects; long-running
///   operations spawn a subtask that feeds Actions back (Codex P0-1)
///
/// P0-2 fix: `redraw_tx` is injected later through a shared `Arc<parking_lot::Mutex<Option<mpsc::Sender<()>>>>`.
/// `chat::run` constructs the EffectExecutor first (redraw_tx does not exist yet), and after spawning the
/// dispatcher task injects `redraw_tx` through the Arc returned by `redraw_handle()`, solving the ordering problem.
pub struct EffectExecutor {
    cleanup_owner: Option<Arc<crate::runtime::turn_cleanup::CleanupTracker>>,
    /// shadow mode flag. When `true` every business Effect skips execution.
    shadow_mode: bool,
    /// Real business dependencies. Some means deps mode, None means shadow mode.
    deps: Option<EffectDeps>,
    /// P0-2: late-injectable redraw_tx handle. Filled in with the real sender by chat::run after spawn.
    /// In real mode both sides share the same Arc, allowing injection while the dispatcher task runs.
    redraw_slot: Arc<ParkingMutex<Option<mpsc::Sender<()>>>>,
    /// Bug #3: provider hot-swap slot in real mode (initial value = `deps.provider`).
    /// `chat::run` takes it via `provider_handle()` and calls `set()` with the new handle on
    /// `/provider <name>`, so later turns' `drive_start_turn_stream` reads the new provider.
    /// `None` in shadow mode.
    provider_slot: Option<ProviderSlot>,
    /// Error from the most recently executed SaveSession effect. The
    /// dispatcher consumes this immediately after reducing one Action so a
    /// terminal notification cannot claim success when durable persistence
    /// failed.
    persistence_error: Arc<ParkingMutex<Option<String>>>,
}

impl EffectExecutor {
    /// Construct a shadow-mode executor (Step 5b compatibility, unit tests, Off mode).
    #[must_use]
    pub fn new_shadow() -> Self {
        Self {
            cleanup_owner: None,
            shadow_mode: true,
            deps: None,
            redraw_slot: Arc::new(parking_lot::Mutex::new(None)),
            provider_slot: None,
            persistence_error: Arc::new(ParkingMutex::new(None)),
        }
    }

    /// Construct the business executor with its actual runtime dependencies.
    #[must_use]
    pub fn new_with_deps(deps: EffectDeps) -> Self {
        let provider_slot = Some(ProviderSlot::new(Arc::clone(&deps.provider)));
        Self {
            cleanup_owner: None,
            shadow_mode: false,
            deps: Some(deps),
            redraw_slot: Arc::new(parking_lot::Mutex::new(None)),
            provider_slot,
            persistence_error: Arc::new(ParkingMutex::new(None)),
        }
    }

    #[cfg(feature = "terminal-tui")]
    pub(crate) fn with_cleanup_owner(mut self, owner: Arc<crate::runtime::turn_cleanup::CleanupTracker>) -> Self {
        self.cleanup_owner = Some(owner);
        self
    }

    fn outcome_after_effects(
        &self,
        outcome: Option<TurnOutcomeKind>,
        requires_persistence: bool,
        save_requested: bool,
    ) -> Option<TurnOutcomeKind> {
        let persistence_error = self.persistence_error.lock().take();
        if requires_persistence && (!save_requested || self.shadow_mode || self.deps.is_none()) {
            return Some(TurnOutcomeKind::Failed {
                err: "session completion did not execute persistence".to_owned(),
                retryable: false,
            });
        }
        match (outcome, persistence_error) {
            (Some(TurnOutcomeKind::Completed { .. }), Some(err)) => Some(TurnOutcomeKind::Failed {
                err: format!("session persistence failed: {err}"),
                retryable: true,
            }),
            (outcome, _) => outcome,
        }
    }

    /// P0-2 fix: return the shared redraw_tx slot Arc for chat::run to inject into after TUI init.
    ///
    /// The caller holds this Arc and, once `redraw_tx` exists, calls `*slot.lock() = Some(tx)`.
    /// Injection still works after the dispatcher task is spawned, because the Arc crosses the spawn boundary.
    ///
    /// Injection has no effect in shadow mode (execute_shadow does not read this slot).
    #[must_use]
    pub fn redraw_handle(&self) -> Arc<ParkingMutex<Option<mpsc::Sender<()>>>> {
        Arc::clone(&self.redraw_slot)
    }

    /// BUG-07: return the model hot-swap slot from deps (real mode only).
    ///
    /// `chat::run` uses this handle to atomically replace the model name on `/model <name>`, so later turns'
    /// `drive_start_turn_stream` reads the new value. Shadow mode has no deps and returns None.
    #[must_use]
    pub fn model_handle(&self) -> Option<ModelSlot> {
        self.deps.as_ref().map(|d| d.model.clone())
    }

    /// Bug #3: return the provider hot-swap slot (real mode only).
    ///
    /// `chat::run` uses this handle to atomically replace the provider handle on `/provider <name>`, so later
    /// turns' `drive_start_turn_stream` issues requests with the new provider. Shadow mode returns None.
    #[must_use]
    pub fn provider_handle(&self) -> Option<ProviderSlot> {
        self.provider_slot.clone()
    }

    /// **S3 T3-1**: return the approval router from deps (real mode only).
    ///
    /// `spawn_dispatcher_task_with_signal` uses this handle to send the decision back to the driver's pending
    /// oneshot after `Action::ToolApprovalReceived` has entered the reducer. Shadow mode has no deps and returns None.
    #[must_use]
    pub fn approval_router(&self) -> Option<Arc<ApprovalRouter>> {
        self.deps.as_ref().map(|d| Arc::clone(&d.approval_router))
    }

    /// Test observability: whether we are in shadow mode.
    #[cfg(test)]
    #[must_use]
    pub const fn is_shadow(&self) -> bool {
        self.shadow_mode
    }

    /// Execute a single Effect.
    ///
    /// - shadow mode: only `LogTrace` really executes (structured logging is a required observability tool),
    ///   `RequestRedraw` emits a trace, and every other business Effect emits a debug log.
    /// - real mode: each business Effect takes the real path of its deps; long-running effects such as
    ///   StartTurn / SaveSession `tokio::spawn` a subtask that feeds back, so the main loop is not blocked.
    ///
    /// Dual-write suppression: when entering a business Effect with deps, the dual_write_guard is set first
    /// (so the legacy path skips its matching write). Persistence effects such as SaveSession /
    /// PersistToMemory / EmitChannelMessage are reset by the caller once done (typically at turn end).
    pub async fn execute(&self, effect: Effect) {
        // S2.5 T2.5-2: instrument every Effect entry with prx_chat_effects_total{effect_kind=...}.
        crate::observability::chat_metrics::inc_effect(effect.kind());
        // LogTrace really executes in both modes (observability)
        if let Effect::LogTrace { level, msg } = &effect {
            Self::emit_trace(*level, msg);
            return;
        }
        match (self.shadow_mode, &self.deps) {
            (true, _) | (_, None) => self.execute_shadow(effect),
            (false, Some(deps)) => self.execute_real(effect, deps).await,
        }
    }

    /// shadow mode branch: every business Effect is a no-op plus a debug log.
    fn execute_shadow(&self, effect: Effect) {
        match &effect {
            Effect::RequestRedraw => {
                tracing::trace!("effect: RequestRedraw (shadow no-op)");
            }
            other => {
                tracing::debug!(effect = ?other, "effect skipped (shadow mode)");
            }
        }
    }

    /// real mode branch: dispatch by Effect type to the real business execution.
    async fn execute_real(&self, effect: Effect, deps: &EffectDeps) {
        match effect {
            Effect::RequestRedraw => {
                // P0-2 fix: prefer redraw_slot (late-injected), fall back to deps.redraw_tx (injected at construction).
                // redraw_slot is filled in with the real sender by chat::run once TUI init completes,
                // ensuring RequestRedraw really triggers a redraw instead of being a no-op.
                let slot_guard = self.redraw_slot.lock();
                let tx = slot_guard.as_ref().or(deps.redraw_tx.as_ref());
                if let Some(tx) = tx {
                    let _ = tx.try_send(());
                } else {
                    tracing::trace!("RequestRedraw: redraw_tx not yet injected (P0-2)");
                }
            }
            Effect::SurfaceNotice { text } => {
                // With a renderer attached the reducer already holds the line in
                // its transcript ledger, so a redraw is all that is owed. With
                // no renderer (`--plain`, piped, non-TUI build) the ledger is
                // never drawn, and printing is the only way the notice is seen.
                let tx = {
                    let slot_guard = self.redraw_slot.lock();
                    slot_guard.as_ref().or(deps.redraw_tx.as_ref()).cloned()
                };
                match tx {
                    Some(tx) => {
                        let _ = tx.try_send(());
                    }
                    None => crate::chat::print_fallback_chat_output(&text),
                }
            }
            Effect::StartTurn {
                provider_turn_task_id,
                draft_id,
                history,
                compaction_guard_history,
                compaction_config,
                cancel,
                chat_mode,
                turn_spawn_ctx,
                turn_message_send_ctx,
                routing_input: _,
                turn_binding,
            } => {
                // Step 5a-2 — long-running: spawn a subtask that really calls provider.stream_chat_with_history
                // and feeds chunk / completed / failed / cancelled events back to the reducer via deps.action_tx,
                // replacing the old `delta_tx → draft_updater → coalescer` chain.
                //
                // Design notes (consistent with plan Step 5a-2):
                //   1. `tokio::pin!` pins the stream, `tokio::select!` watches cancel + chunk at the same time
                //   2. version strictly increases from a local counter (matching the reducer's strict-monotonic rule)
                //   3. Reasoning is not mixed into the main text stream (matching production chat::run behaviour)
                //   4. The RAII `DualWriteGuardScope` covers the whole turn; subtask exit resets it automatically
                //   5. Errors on any branch use `action_tx.send().await` (no chunk loss, natural backpressure)
                //
                // Note: StartTurn is currently **not** triggered automatically by the reducer — this path only
                // applies when the caller explicitly spawns `Effect::StartTurn { ... }` (e.g. unit tests, or the
                // ratatui path after 5a-3 wiring). The chat::run main loop's legacy tool-loop path still calls `run_turn`
                // (legacy path); `dual_write_guard` already guards the reducer persistence effects.
                // Bug #3: prefer the hot-swappable provider slot (updated by
                // `/provider <name>`); fall back to the construction-time provider
                // when no slot is present (e.g. shadow construction edge cases).
                let provider = turn_binding.as_ref().map_or_else(
                    || {
                        self.provider_slot
                            .as_ref()
                            .map(ProviderSlot::current)
                            .unwrap_or_else(|| Arc::clone(&deps.provider))
                    },
                    |binding| binding.provider(),
                );
                let action_tx = deps.action_tx.clone();
                let guard_scope = deps.dual_write_guard.enter_scope();
                // Codex P1 fix: take the real model + temperature from deps and pass them to the stream API.
                // BUG-07: read the current ModelSlot value at the start of every turn, so after /model <name>
                // later turns automatically use the new model (same provider, different model).
                let model = turn_binding.as_ref().map_or_else(
                    || deps.model.current().to_string(),
                    |binding| binding.model().to_owned(),
                );
                let temperature = deps.temperature;
                // 5a-6: pass the tool registry through (None → the driver degrades to a plain text stream).
                let tools_registry = deps.tools_registry.as_ref().map(Arc::clone);
                let tool_security_policy = Arc::clone(&deps.tool_security_policy);
                // Validation enforces the document this provider actually put
                // on the wire; see `ToolExecutionContext::schema_dialect`.
                let tool_execution_context = turn_binding
                    .as_ref()
                    .map_or_else(
                        || {
                            chat_tool_execution_context(
                                tool_security_policy.as_ref(),
                                turn_spawn_ctx.as_ref(),
                                provider_turn_task_id,
                                &draft_id,
                            )
                        },
                        |binding| {
                            ToolExecutionContext::new(binding.envelope().clone(), "private")
                                .with_chat_id("terminal:user")
                        },
                    )
                    .with_schema_dialect(provider.tool_schema_dialect_for(&model));
                let request_event_fabric = MemoryFabric::new(
                    Arc::clone(&deps.memory),
                    tool_execution_context.envelope.workspace_id.clone(),
                )
                .with_event_recording(deps.memory_event_recording);
                let compaction_audit = Some(crate::agent::document_ingest::DocumentIngestRuntime::from_envelope(
                    Arc::clone(&deps.memory),
                    &tool_execution_context.envelope,
                ));
                let tool_execution_service = tools_registry.as_ref().map(|registry| {
                    Arc::new(chat_tool_execution_service(
                        Arc::clone(registry),
                        Some(Arc::clone(&deps.memory)),
                        Arc::clone(&tool_security_policy),
                        Arc::clone(&deps.approval_router),
                        action_tx.clone(),
                        cancel.clone(),
                        provider_turn_task_id,
                    ))
                });
                let provider_turn_task_id_for_trace = provider_turn_task_id.map(|id| id.get());
                let provider_turn_lifecycle_tx = deps.provider_turn_lifecycle_tx.clone();
                let provider_turn_handle_tx = deps.provider_turn_lifecycle_tx.clone();
                let observer = Arc::clone(&deps.observer);
                let hooks = Arc::clone(&deps.hooks);
                let provider_turn_execution_lease_id =
                    provider_turn_task_id.map(|_| next_provider_turn_execution_lease_id());
                // Task locals do not cross spawn; carry the chat owner's tracker.
                let cleanup_owner = self.cleanup_owner.clone();
                let provider_task_handle = tokio::spawn(async move {
                    let work = async move {
                        if let (Some(task_id), Some(lease_id), Some(tx)) = (
                            provider_turn_task_id,
                            provider_turn_execution_lease_id,
                            provider_turn_lifecycle_tx.as_ref(),
                        ) {
                            // Backpressure point: awaiting here slows turn startup
                            // when chat::run is behind on lifecycle bookkeeping,
                            // which is exactly the desired coupling — the registry
                            // must not fall arbitrarily far behind reality.
                            let _ = tx.send(ProviderTurnLifecycleEvent::Started { task_id, lease_id }).await;
                        }
                        tracing::debug!(
                            provider_turn_task_id = provider_turn_task_id_for_trace,
                            provider_turn_execution_lease_id,
                            draft_id = %draft_id,
                            "provider turn worker task started"
                        );
                        // RAII scope: automatically resets dual_write_guard when the subtask exits (including on panic).
                        let _scope = guard_scope;
                        if cancel.is_cancelled() {
                            // already cancelled before start: send StreamCancelled directly, do not issue an LLM request.
                            if let Err(e) = action_tx.send(Action::StreamCancelled { draft_id }).await {
                                tracing::debug!(error = %e, "StartTurn: action_tx closed on pre-cancel");
                            }
                            if let (Some(task_id), Some(lease_id), Some(tx)) = (
                                provider_turn_task_id,
                                provider_turn_execution_lease_id,
                                provider_turn_lifecycle_tx.as_ref(),
                            ) {
                                let _ = tx.send(ProviderTurnLifecycleEvent::Exited { task_id, lease_id }).await;
                            }
                            return;
                        }
                        let compaction_guard_history = compaction_guard_history.unwrap_or_else(|| history.clone());
                        let driver = drive_start_turn_stream(
                            turn_binding.clone(),
                            provider_turn_task_id,
                            provider,
                            history,
                            compaction_guard_history,
                            model,
                            temperature,
                            compaction_config,
                            cancel.clone(),
                            draft_id.clone(),
                            action_tx.clone(),
                            tools_registry,
                            tool_execution_service,
                            tool_execution_context,
                            request_event_fabric,
                            compaction_audit,
                            chat_mode,
                            observer,
                            hooks,
                        );
                        // D8-4 (redux path real fix): mirror the scope wrapper around
                        // the legacy tool-loop path's `run_turn` call in `chat::run` — seed the
                        // turn-root spawn execution context so any `sessions_spawn`
                        // tool call executed inside this turn's tool loop reads
                        // `SPAWN_EXECUTION_CONTEXT.try_with(..)` = Ok → `parent_run_id =
                        // turn run_id` → origin = Model. The legacy path scoped this at
                        // `chat::run`, but the redux driver `continue`s before reaching
                        // it, so the scope must be applied here (the redux turn's actual
                        // tool-execution site). When `turn_spawn_ctx` is `None` (e.g.
                        // non-turn callers / tests / the `/bg` slash command which never
                        // dispatches `StartLLMTurn`), the driver runs unscoped and
                        // spawned sub-agents fall back to user origin — the correct
                        // behavior for those paths.
                        let scoped_spawn_driver = async move {
                            match turn_spawn_ctx {
                                Some(ctx) => {
                                    crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                                        .scope(ctx, driver)
                                        .await;
                                }
                                None => driver.await,
                            }
                        };
                        let scoped_driver = async move {
                            match turn_message_send_ctx {
                                Some(ctx) => {
                                    crate::tools::message_send::MESSAGE_SEND_EXECUTION_CONTEXT
                                        .scope(ctx, scoped_spawn_driver)
                                        .await;
                                }
                                None => scoped_spawn_driver.await,
                            }
                        };
                        // No wall clock: a provider turn ends when it finishes, when
                        // `cancel` fires, or when the stall detector in
                        // `crate::agent::idle` finds it has stopped making progress.
                        scoped_driver.await;
                        tracing::debug!(
                            provider_turn_task_id = provider_turn_task_id_for_trace,
                            provider_turn_execution_lease_id,
                            "provider turn worker task exited"
                        );
                        if let (Some(task_id), Some(lease_id), Some(tx)) = (
                            provider_turn_task_id,
                            provider_turn_execution_lease_id,
                            provider_turn_lifecycle_tx.as_ref(),
                        ) {
                            let _ = tx.send(ProviderTurnLifecycleEvent::Exited { task_id, lease_id }).await;
                        }
                    };
                    if let Some(owner) = cleanup_owner {
                        owner.scope(work).await;
                    } else {
                        work.await;
                    }
                });
                if let (Some(task_id), Some(lease_id), Some(tx)) = (
                    provider_turn_task_id,
                    provider_turn_execution_lease_id,
                    provider_turn_handle_tx.as_ref(),
                ) {
                    let _ = tx
                        .send(ProviderTurnLifecycleEvent::HandleAttached {
                            task_id,
                            lease_id,
                            abort_handle: provider_task_handle.abort_handle(),
                        })
                        .await;
                }
            }
            Effect::SaveSession(session) => {
                *self.persistence_error.lock() = None;
                let session = crate::chat::sanitize::sanitize_session_content(&session);
                // T3-3-fixB D1: inline await instead of tokio::spawn, so the serialization of the main loop's
                // executor.execute(effect).await carries all the way through, closing the inconsistency window
                // where RequestRedraw had already refreshed the screen while SaveSession was still writing to disk.
                // The RAII scope shares the lifetime of the inline await: once the await completes, _scope drops
                // and releases the guard, so the legacy path can single-write again (serial effects never overlap).
                let _scope = deps.dual_write_guard.enter_scope();
                let memory = Arc::clone(&deps.memory);
                let action_tx = deps.action_tx.clone();
                let session_id = session.id.clone();
                let json = match session.to_json() {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!(error = %e, "SaveSession effect: serialize failed");
                        *self.persistence_error.lock() = Some(e.to_string());
                        return;
                    }
                };
                let mut store_result = memory
                    .store(
                        &session.memory_key(),
                        &json,
                        crate::memory::MemoryCategory::Conversation,
                        Some(&session.id),
                    )
                    .await;
                // The SQLite memory backend may briefly report BUSY/LOCKED
                // while the same completed turn is committing its message
                // events. Yield and retry a small bounded number of times;
                // this is contention recovery, not a timing delay. Every
                // other error remains fail-fast and is propagated through the
                // terminal acknowledgement below.
                for _ in 0..3 {
                    let retryable = store_result.as_ref().is_err_and(|error| {
                        let message = error.to_string().to_ascii_lowercase();
                        message.contains("database is locked")
                            || message.contains("database is busy")
                            || message.contains("sqlite_busy")
                            || message.contains("sqlite_locked")
                    });
                    if !retryable {
                        break;
                    }
                    tokio::task::yield_now().await;
                    store_result = memory
                        .store(
                            &session.memory_key(),
                            &json,
                            crate::memory::MemoryCategory::Conversation,
                            Some(&session.id),
                        )
                        .await;
                }
                match store_result {
                    Ok(()) => {
                        let _ = action_tx.try_send(Action::SessionSaved { id: session_id });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "SaveSession effect: store failed");
                        *self.persistence_error.lock() = Some(e.to_string());
                    }
                }
            }
            Effect::SendDraftFinalize { draft_id, text } => {
                // dual-write suppression RAII scope: automatically reset when the subtask exits.
                let guard_scope = deps.dual_write_guard.enter_scope();
                let channel = Arc::clone(&deps.channel);
                tokio::spawn(async move {
                    let _scope = guard_scope;
                    let recipient = "user";
                    tracing::debug!(
                        draft_id = %draft_id,
                        text_len = text.len(),
                        channel = %channel.name(),
                        "SendDraftFinalize effect: calling channel.finalize_draft"
                    );
                    if let Err(e) = channel.finalize_draft(recipient, &draft_id, &text).await {
                        tracing::warn!(
                            error = %e,
                            draft_id = %draft_id,
                            "SendDraftFinalize effect: finalize_draft failed"
                        );
                    }
                });
            }
            Effect::CancelDraft(draft_id) => {
                // call channel.cancel_draft directly (short synchronous path, no spawn needed)
                let channel = Arc::clone(&deps.channel);
                let recipient = "user".to_string();
                if let Err(e) = channel.cancel_draft(&recipient, &draft_id).await {
                    tracing::debug!(error = %e, draft_id = %draft_id, "CancelDraft effect: channel returned err");
                }
            }
            Effect::CancelToken(token) => {
                // S2-B Step 2: really trigger the underlying CancellationToken — so the LLM stream / tool loop
                // immediately receives the cancel signal and returns a cancelled error. No spawn needed (cancel
                // itself does not block) and no dual_write_guard needed (cancel is idempotent, repeats are safe).
                tracing::info!("effect: CancelToken -> token.cancel()");
                token.cancel();
            }
            Effect::EmitChannelMessage(send_msg) => {
                let guard_scope = deps.dual_write_guard.enter_scope();
                let channel = Arc::clone(&deps.channel);
                tokio::spawn(async move {
                    let _scope = guard_scope;
                    if let Err(e) = channel.send(&send_msg).await {
                        tracing::warn!(error = %e, "EmitChannelMessage effect: send failed");
                    }
                });
            }
            Effect::PersistToMemory { key, value, category } => {
                let guard_scope = deps.dual_write_guard.enter_scope();
                let memory = Arc::clone(&deps.memory);
                tokio::spawn(async move {
                    let _scope = guard_scope;
                    if let Err(e) = memory.store(&key, &value, category, None).await {
                        tracing::warn!(error = %e, key = %key, "PersistToMemory effect: store failed");
                    }
                });
            }
            Effect::NotifyHook { event, payload } => {
                let guard_scope = deps.dual_write_guard.enter_scope();
                let hooks = Arc::clone(&deps.hooks);
                tokio::spawn(async move {
                    let _scope = guard_scope;
                    hooks.emit(event, payload).await;
                });
            }
            Effect::DisplayMedia { kind, path } => {
                // Media display is a user-visible short synchronous path; record it with tracing (the observer has
                // no generic trace variant, and in stage 5a-1 the legacy path still does the real media display).
                tracing::debug!(kind = %kind, path = %path, "DisplayMedia effect");
                let _ = deps.observer.name(); // placeholder so the deps.observer field is not warned about
            }
            Effect::AutoTitleSession(title) => {
                tracing::debug!(title = %title, "AutoTitleSession effect");
            }
            Effect::RequestApproval {
                task_id,
                tool_id,
                name,
                args,
            } => {
                #[cfg(not(feature = "terminal-tui"))]
                let _ = task_id;
                #[cfg(feature = "terminal-tui")]
                {
                    let interactive_tui = self.redraw_slot.lock().is_some() || deps.redraw_tx.is_some();
                    if interactive_tui {
                        if let Some(mirror) = deps.tui_mirror.as_ref() {
                            {
                                let mut state = mirror.lock();
                                state.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
                                    task_id,
                                    tool_id,
                                    name,
                                    args,
                                    selected_approval: false,
                                });
                                state.focus = crate::chat::sessions::FocusTarget::Approval;
                                state.switcher = None;
                            }
                            let tx = {
                                let slot_guard = self.redraw_slot.lock();
                                slot_guard.as_ref().or(deps.redraw_tx.as_ref()).cloned()
                            };
                            if let Some(tx) = tx {
                                let _ = tx.try_send(());
                            }
                            tracing::info!("RequestApproval effect: foreground approval TUI surfaced");
                            return;
                        }
                        tracing::warn!(
                            tool_id = %tool_id,
                            name = %name,
                            args_len = args.len(),
                            "RequestApproval effect: TUI active but no approval surface, failing closed"
                        );
                        let action_tx = deps.action_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = action_tx
                                .send(Action::ToolApprovalReceived {
                                    tool_id,
                                    approved: false,
                                })
                                .await
                            {
                                tracing::debug!(error = %e, "RequestApproval fail-closed: action_tx closed");
                            }
                        });
                        return;
                    }
                }
                let env_value = std::env::var("OPENPRX_APPROVAL_OVERRIDE").ok();
                let approved = resolve_supervised_approval_override(env_value.as_deref());
                if env_value.is_none() {
                    tracing::warn!(
                        tool_id = %tool_id,
                        name = %name,
                        args_len = args.len(),
                        "RequestApproval effect: non-TUI approval has no override, failing closed"
                    );
                } else {
                    tracing::info!(
                        security_event = "openprx_approval_override_applied",
                        tool_id = %tool_id,
                        name = %name,
                        args_len = args.len(),
                        approved,
                        "RequestApproval effect: non-TUI OPENPRX_APPROVAL_OVERRIDE applied"
                    );
                }
                let _ = args;
                let action_tx = deps.action_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = action_tx.send(Action::ToolApprovalReceived { tool_id, approved }).await {
                        tracing::debug!(error = %e, "RequestApproval stub: action_tx closed");
                    }
                });
            }
            Effect::ResolveApproval { tool_id, approved } => {
                deps.approval_router.resolve(&tool_id, approved);
            }
            Effect::Quit => {
                // shutdown signal: the real cancel + drop implicit protocol is finished off by chat::run
                tracing::info!("effect: Quit -> shutdown.cancel()");
                deps.shutdown.cancel();
            }
            Effect::LogTrace { .. } => {
                // already handled by the branch at the top of execute()
            }
        }
    }

    /// Dispatch a [`tracing::Level`] to the matching macro (avoiding dyn dispatch).
    fn emit_trace(level: tracing::Level, msg: &str) {
        if level == tracing::Level::ERROR {
            tracing::error!("{}", msg);
        } else if level == tracing::Level::WARN {
            tracing::warn!("{}", msg);
        } else if level == tracing::Level::INFO {
            tracing::info!("{}", msg);
        } else if level == tracing::Level::DEBUG {
            tracing::debug!("{}", msg);
        } else {
            tracing::trace!("{}", msg);
        }
    }
}

// ─── S5 P0-3: supervised approval override ────────────────────────────────────

/// Parse the `OPENPRX_APPROVAL_OVERRIDE` env var to decide the approval result in supervised mode.
///
/// S5 P0-3 (BREAKING): TUI card rendering + Y/N keyboard wiring (full T5-1) is left to Task #11;
/// silently auto-approving before the UI is connected is a security hole (Codex: "never silently auto-approve").
///
/// - `Some("allow" | "y" | "yes" | "1")` → `true` (explicit allow)
/// - `Some("deny" | "n" | "no" | "0")` → `false` (explicit deny)
/// - `None` or any other value → `false` (fail-safe deny, BREAKING — the old behaviour was true)
///
/// Case-insensitive; surrounding whitespace is ignored.
#[must_use]
pub(crate) fn resolve_supervised_approval_override(raw: Option<&str>) -> bool {
    let Some(value) = raw else {
        return false;
    };
    matches!(value.trim().to_ascii_lowercase().as_str(), "allow" | "y" | "yes" | "1")
}

// ─── StartTurn streaming driver (Step 5a-2) ────────────────────────────────────

/// The detached Redux driver receives a provider/guard snapshot that already
/// contains the pending user message. Reducer state intentionally does not add
/// that message until ordered commit, so the patch sent back to the reducer
/// guards the pre-turn reducer history and replaces it with the exact compacted
/// durable target minus that pending turn. Ordered commit then appends the user
/// message without shifting patch indices or weakening stale-state detection.
fn compaction_patch_for_reducer_without_pending_user(
    patch: &crate::agent::turn_contracts::CompactionPatch,
    source_history: &[crate::providers::traits::ChatMessage],
    compacted_history: &[crate::providers::traits::ChatMessage],
) -> crate::agent::turn_contracts::CompactionPatch {
    let Some(pending_index) = source_history.len().checked_sub(1) else {
        return patch.clone();
    };
    if source_history
        .get(pending_index)
        .is_none_or(|message| message.role != "user")
        || patch.range_end > pending_index
    {
        return patch.clone();
    }
    let Some(reducer_source) = source_history.get(..pending_index) else {
        return patch.clone();
    };
    let Some(guard) = crate::agent::compaction::compaction_patch_guard_for(reducer_source, 0, reducer_source.len())
    else {
        return patch.clone();
    };
    let mut reducer_target_with_pending = compacted_history.to_vec();
    if reducer_target_with_pending
        .last()
        .is_none_or(|message| message.role != "user")
    {
        return patch.clone();
    }
    reducer_target_with_pending.pop();
    crate::agent::turn_contracts::CompactionPatch {
        range_start: 0,
        range_end: reducer_source.len(),
        replacement: reducer_target_with_pending,
        append_after: Vec::new(),
        guard,
    }
}

/// What a redux rollover attempt left behind for its caller.
///
/// `replacement_len` keeps the previous return value: `Some` when a compaction
/// patch was applied, `None` when the caller still owes the history its own
/// token-aware trim. `degraded` is the part that used to be invisible — the
/// rollover could not stay lossless, so whatever the caller trims next is
/// context nobody can get back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ContextRolloverOutcome {
    replacement_len: Option<usize>,
    degraded: bool,
}

/// Tell the user, once per turn, that context was dropped for good.
async fn send_context_degraded_notice(
    action_tx: &mpsc::Sender<Action>,
    reason: crate::chat::action::CompactReason,
    dropped_messages: usize,
) -> Result<(), ()> {
    if let Err(error) = action_tx
        .send(Action::HistoryCompactionDegraded {
            reason,
            dropped_messages,
        })
        .await
    {
        tracing::debug!(%error, "StartTurn: action_tx closed on compaction-degraded notice");
        return Err(());
    }
    Ok(())
}

async fn apply_redux_context_rollover(
    provider: &dyn Provider,
    history: &mut Vec<crate::providers::traits::ChatMessage>,
    compaction_guard_history: &mut Vec<crate::providers::traits::ChatMessage>,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&crate::agent::document_ingest::DocumentIngestRuntime>,
    action_tx: &mpsc::Sender<Action>,
    reason: crate::chat::action::CompactReason,
    trigger: &str,
) -> Result<ContextRolloverOutcome, ()> {
    if matches!(config.mode, crate::config::AgentCompactionMode::Off) {
        return Ok(ContextRolloverOutcome::default());
    }

    let patch = match crate::agent::compaction::build_configurable_compaction_patch_with_source_history(
        history,
        compaction_guard_history,
        provider,
        model,
        config,
        audit,
        trigger,
    )
    .await
    {
        Ok(Some(patch)) => patch,
        // A `switch` rollover that cannot produce an exact handoff must not end
        // the turn. Failing the draft here left every session containing tool
        // calls answering each user message with the same non-retryable error
        // while history never shrank. Returning `Ok(None)` hands the caller its
        // ordinary token-aware trim fallback.
        // A `switch` rollover that could not resolve exact provenance produced
        // no patch at all: the caller's trim is the whole remediation, and it is
        // lossy. Summary mode returns `None` when there is simply nothing to
        // compact, which is not a degradation.
        Ok(None) => {
            let degraded = matches!(config.mode, crate::config::AgentCompactionMode::Switch);
            if degraded {
                tracing::warn!(
                    trigger,
                    "context switch could not create an exact transcript handoff; continuing on a lossy trim"
                );
                let budget = crate::agent::context_budget::plan_context_budget(
                    history,
                    config,
                    crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
                );
                crate::agent::compaction::persist_context_switch_degradation(
                    audit,
                    trigger,
                    "handoff_unavailable",
                    budget.used_tokens,
                    budget.used_tokens,
                    budget.available_input_tokens,
                )
                .await;
            }
            return Ok(ContextRolloverOutcome {
                replacement_len: None,
                degraded,
            });
        }
        Err(error) => {
            tracing::warn!(error = %error, trigger, "redux driver context rollover failed; falling back to trim");
            let budget = crate::agent::context_budget::plan_context_budget(
                history,
                config,
                crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
            );
            crate::agent::compaction::persist_context_switch_degradation(
                audit,
                trigger,
                "handoff_failed",
                budget.used_tokens,
                budget.used_tokens,
                budget.available_input_tokens,
            )
            .await;
            return Ok(ContextRolloverOutcome {
                replacement_len: None,
                degraded: true,
            });
        }
    };

    let replacement_len = patch.replacement.len();
    let reducer_source_history = compaction_guard_history.clone();
    crate::agent::compaction::apply_compaction_patch_exact(history, &patch);
    crate::agent::compaction::apply_compaction_patch_exact(compaction_guard_history, &patch);
    let budget = crate::agent::context_budget::plan_context_budget(
        history,
        config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    let mut degraded_dropped = 0usize;
    if budget.over_hard_limit {
        if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
            tracing::warn!(
                trigger,
                used_tokens = budget.used_tokens,
                hard_limit = budget.available_input_tokens,
                "context switch remained above the hard limit; degraded to a lossy trim"
            );
        }
        let before_trim = history.len();
        let trimmed =
            crate::agent::context_budget::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                history,
                config,
                replacement_len,
            );
        degraded_dropped = before_trim.saturating_sub(history.len());
        tracing::warn!(
            used_tokens = budget.used_tokens,
            hard_limit = budget.available_input_tokens,
            trimmed,
            "redux driver context rollover applied preserving trim"
        );
        if degraded_dropped > 0 {
            let after = crate::agent::context_budget::plan_context_budget(
                history,
                config,
                crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
            );
            crate::agent::compaction::persist_context_switch_degradation(
                audit,
                trigger,
                "over_hard_limit_after_handoff",
                budget.used_tokens,
                after.used_tokens,
                budget.available_input_tokens,
            )
            .await;
        }
    }
    let reducer_patch =
        compaction_patch_for_reducer_without_pending_user(&patch, &reducer_source_history, compaction_guard_history);
    if let Err(error) = action_tx
        .send(Action::HistoryCompactionPatchApplied {
            reason,
            patch: reducer_patch,
            compaction_config: config.clone(),
        })
        .await
    {
        tracing::debug!(%error, "StartTurn: action_tx closed on summary-compaction-patch");
        return Err(());
    }
    // The patch itself is lossless; only the extra trim above is not, so the
    // notice is sent here with the exact count that trim removed rather than
    // left to the caller, which cannot tell the two apart.
    if degraded_dropped > 0 {
        send_context_degraded_notice(action_tx, reason, degraded_dropped).await?;
    }
    Ok(ContextRolloverOutcome {
        replacement_len: Some(replacement_len),
        degraded: false,
    })
}

fn chat_history_turn_count(history: &[crate::providers::traits::ChatMessage]) -> usize {
    history
        .len()
        .saturating_sub(usize::from(history.first().is_some_and(|msg| msg.role == "system")))
}

async fn send_redux_compaction_feedback(
    action_tx: &mpsc::Sender<Action>,
    turns_before: usize,
    tokens_before: usize,
    history: &[crate::providers::traits::ChatMessage],
    config: &crate::config::AgentCompactionConfig,
    context: &'static str,
    last_feedback: &mut Option<String>,
) -> Result<bool, ()> {
    let turns_after = chat_history_turn_count(history);
    let tokens_after = super::estimate_chat_history_tokens(history);
    let text = super::format_compact_feedback(
        turns_before,
        turns_after,
        tokens_before,
        tokens_after,
        config.max_context_tokens,
    );
    if last_feedback.as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    if let Err(error) = action_tx.send(Action::SystemMessageAdded { text: text.clone() }).await {
        tracing::debug!(%error, context, "StartTurn: action_tx closed on compaction feedback");
        return Err(());
    }
    *last_feedback = Some(text);
    Ok(true)
}

fn format_injection_overbudget_diagnostic(injected_tokens: usize) -> String {
    format!(
        "Note: your @path/memory context (~{injected_tokens} tokens) exceeded the model's context budget and was trimmed for this turn; the reply may miss some injected context."
    )
}

fn redux_injection_overbudget_diagnostic_text(
    enriched_history: &[crate::providers::traits::ChatMessage],
    original_history: &[crate::providers::traits::ChatMessage],
    config: &crate::config::AgentCompactionConfig,
) -> Option<String> {
    let enriched_budget = crate::agent::context_budget::plan_context_budget(
        enriched_history,
        config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    if !enriched_budget.over_hard_limit {
        return None;
    }
    let original_budget = crate::agent::context_budget::plan_context_budget(
        original_history,
        config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    if original_budget.over_hard_limit {
        return None;
    }
    let injected_tokens = enriched_budget.used_tokens.saturating_sub(original_budget.used_tokens);
    if injected_tokens == 0 {
        return None;
    }
    Some(format_injection_overbudget_diagnostic(injected_tokens))
}

async fn send_redux_injection_overbudget_diagnostic(
    action_tx: &mpsc::Sender<Action>,
    text: Option<String>,
    context: &'static str,
    last_feedback: &mut Option<String>,
) -> Result<bool, ()> {
    let Some(text) = text else {
        return Ok(false);
    };
    if last_feedback.as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    if let Err(error) = action_tx.send(Action::SystemMessageAdded { text: text.clone() }).await {
        tracing::debug!(%error, context, "StartTurn: action_tx closed on injection-overbudget diagnostic");
        return Err(());
    }
    *last_feedback = Some(text);
    Ok(true)
}

async fn send_redux_context_window_update(
    action_tx: &mpsc::Sender<Action>,
    history: &[crate::providers::traits::ChatMessage],
    config: &crate::config::AgentCompactionConfig,
    context: &'static str,
) -> Result<(), ()> {
    let budget = crate::agent::context_budget::plan_context_budget(
        history,
        config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    if let Err(error) = action_tx
        .send(Action::ContextWindowUpdated {
            used_context_tokens: Some(budget.used_tokens),
            max_context_tokens: Some(budget.max_context_tokens),
        })
        .await
    {
        tracing::debug!(%error, context, "StartTurn: action_tx closed on context-window update");
        return Err(());
    }
    Ok(())
}

fn trim_redux_driver_context_budget_after_rollover(
    history: &mut Vec<crate::providers::traits::ChatMessage>,
    compaction_guard_history: &mut Vec<crate::providers::traits::ChatMessage>,
    config: &crate::config::AgentCompactionConfig,
    replacement_len: Option<usize>,
) -> bool {
    let histories_matched_before_trim = history.len() == compaction_guard_history.len()
        && history
            .iter()
            .zip(compaction_guard_history.iter())
            .all(|(left, right)| left.role == right.role && left.content == right.content);
    let trimmed = if let Some(replacement_len) = replacement_len {
        crate::agent::context_budget::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
            history,
            config,
            replacement_len,
        )
    } else {
        crate::agent::context_budget::trim_history_to_context_budget(history, config)
    };
    if histories_matched_before_trim {
        *compaction_guard_history = history.clone();
    }
    trimmed
}

fn chat_tool_execution_context(
    policy: &crate::security::SecurityPolicy,
    turn_spawn_ctx: Option<&crate::tools::sessions_spawn::SpawnExecutionContext>,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    draft_id: &str,
) -> ToolExecutionContext {
    let workspace_id = policy.workspace_dir.to_string_lossy().to_string();
    let session_key = turn_spawn_ctx.map_or_else(
        || format!("chat:redux:{draft_id}"),
        |context| context.session_scope_key.clone(),
    );
    // Message events are written under the recipient-aware canonical chat key.
    // Build the Redux tool/compaction envelope from the same constructor so
    // exact transcript provenance can see those events after a session resume;
    // `chat_canonical` also carries `chat:{id}` for pre-migration read-merge.
    let chat_session_id = super::chat_session_id_from_key(&session_key);
    let mut envelope = crate::runtime::envelope::RuntimeEnvelope::chat_canonical(
        workspace_id,
        chat_session_id,
        crate::memory::MemoryVisibility::Workspace,
    )
    .with_sender("user")
    .with_channel("terminal");
    if let Some(context) = turn_spawn_ctx {
        envelope = envelope.with_run_id(context.run_id.clone());
        if let Some(owner_id) = &context.owner_id {
            envelope = envelope.with_owner_id(owner_id.clone());
        }
        if let Some(topic_id) = &context.topic_id {
            envelope = envelope.with_topic_id(topic_id.clone());
        }
        if let Some(event_id) = &context.source_message_event_id {
            envelope = envelope.with_source_message_event_id(event_id.clone());
        }
    }
    if let Some(task_id) = task_id {
        envelope = envelope.with_task_id(task_id.get().to_string());
    }
    ToolExecutionContext::new(envelope, "private").with_chat_id("terminal:user")
}

fn chat_tool_execution_service(
    registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    idempotency_memory: Option<Arc<dyn Memory>>,
    policy: Arc<crate::security::SecurityPolicy>,
    approval_router: Arc<ApprovalRouter>,
    action_tx: mpsc::Sender<Action>,
    cancellation: CancellationToken,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) -> ToolExecutionService {
    let workspace_dir = policy.workspace_dir.clone();
    let service = ToolExecutionService::from_shared_boxed_registry(
        registry,
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&policy))),
        Arc::new(ChatTuiApprovalStrategy {
            task_id,
            router: approval_router,
            action_tx: action_tx.clone(),
            cancellation,
            policy,
        }),
        Arc::new(ChatToolExecutionPreparation { task_id, action_tx }),
        Arc::new(TracingToolExecutionAudit),
    );
    // The chat memory backend is whatever the operator configured; the ledger is
    // resolved from it so a non-ledger backend still gets a durable ledger
    // instead of refusing every side-effecting tool.
    match idempotency_memory.and_then(|memory| crate::memory::tool_execution_ledger(&memory, &workspace_dir)) {
        Some(memory) => service.with_idempotency_memory(memory),
        None => service,
    }
}

/// Minimum wall-clock gap between two live thinking-progress actions.
///
/// Reasoning deltas arrive far more finely grained than visible text deltas
/// (k3-class models emit hundreds of fragments over a 4-40s thinking burst), so
/// they are batched here instead of producing one action per fragment. This
/// keeps the reducer/redraw rate at or below the existing text-delta rate.
const REASONING_PROGRESS_MIN_INTERVAL_MS: i64 = 120;

/// Batches reasoning deltas so the TUI progress counter updates at most once
/// per [`REASONING_PROGRESS_MIN_INTERVAL_MS`].
///
/// Pure logic (the clock is an argument) so the throttle is unit-testable
/// without sleeping.
#[derive(Debug, Default)]
struct ReasoningProgressBatcher {
    pending: String,
    last_emit_ms: Option<i64>,
}

impl ReasoningProgressBatcher {
    /// Fold one delta in; returns the batch to publish when the throttle window
    /// has elapsed, otherwise `None` (the delta stays buffered for the next
    /// window).
    fn push(&mut self, delta: &str, now_ms: i64) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        self.pending.push_str(delta);
        let due = self
            .last_emit_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= REASONING_PROGRESS_MIN_INTERVAL_MS);
        if !due {
            return None;
        }
        self.last_emit_ms = Some(now_ms);
        Some(std::mem::take(&mut self.pending))
    }

    /// Emit whatever is still buffered when the stream ends.
    ///
    /// Without this the last window's worth of reasoning — everything after the
    /// final 120 ms boundary — was simply dropped, so the live counter stopped
    /// short of the body the terminal action goes on to publish.
    /// [`StreamChunkCoalescer`] has always had this; the reasoning batcher did
    /// not.
    fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.pending))
    }
}

struct ReduxToolLoopEventSink {
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    draft_id: String,
    action_tx: mpsc::Sender<Action>,
    version: Arc<AtomicU64>,
    /// Authoritative full reasoning body for this turn — the single source of
    /// truth replayed in the terminal action. The reducer only ever receives
    /// deltas for its live counter, never a second full copy.
    reasoning: Arc<ParkingMutex<String>>,
    /// Shared with the driver so the tail of the last throttle window is still
    /// published when the loop returns.
    reasoning_progress: Arc<ParkingMutex<ReasoningProgressBatcher>>,
}

#[async_trait]
impl crate::agent::turn_contracts::ToolLoopEventSink for ReduxToolLoopEventSink {
    async fn emit(&self, event: crate::agent::turn_contracts::ToolLoopEvent) -> anyhow::Result<()> {
        let action = match event {
            crate::agent::turn_contracts::ToolLoopEvent::TextDelta(delta) => Action::StreamChunkReceived {
                draft_id: self.draft_id.clone(),
                delta,
                version: self.version.fetch_add(1, Ordering::Relaxed).saturating_add(1),
            },
            crate::agent::turn_contracts::ToolLoopEvent::ReasoningDelta(delta) => {
                self.reasoning.lock().push_str(&delta);
                let now_ms = chrono::Utc::now().timestamp_millis();
                let Some(batch) = self.reasoning_progress.lock().push(&delta, now_ms) else {
                    return Ok(());
                };
                Action::StreamReasoningReceived {
                    draft_id: self.draft_id.clone(),
                    delta: batch,
                    version: self.version.fetch_add(1, Ordering::Relaxed).saturating_add(1),
                }
            }
            crate::agent::turn_contracts::ToolLoopEvent::RetryAttempt { attempt, reason } => {
                Action::StreamRetryAttempt { attempt, reason }
            }
            crate::agent::turn_contracts::ToolLoopEvent::ContextCompacted => Action::HistoryCompacted {
                reason: crate::chat::action::CompactReason::ContextOverflow,
            },
            crate::agent::turn_contracts::ToolLoopEvent::ContextCompactionPatch {
                patch,
                config,
                turns_before,
                tokens_before,
                turns_after,
                tokens_after,
                used_context_tokens,
            } => {
                self.action_tx
                    .send(Action::HistoryCompactionPatchApplied {
                        reason: crate::chat::action::CompactReason::ContextOverflow,
                        patch,
                        compaction_config: config.clone(),
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("chat action channel closed"))?;
                self.action_tx
                    .send(Action::ContextWindowUpdated {
                        used_context_tokens: Some(used_context_tokens),
                        max_context_tokens: Some(config.max_context_tokens),
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("chat action channel closed"))?;
                self.action_tx
                    .send(Action::SystemMessageAdded {
                        text: super::format_compact_feedback(
                            turns_before,
                            turns_after,
                            tokens_before,
                            tokens_after,
                            config.max_context_tokens,
                        ),
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("chat action channel closed"))?;
                return Ok(());
            }
            crate::agent::turn_contracts::ToolLoopEvent::ToolStarted {
                tool_call_id,
                name,
                args,
            } => Action::ToolStarted {
                task_id: self.provider_turn_task_id,
                sequence: None,
                tool_call_id: Some(tool_call_id),
                name,
                args,
            },
            crate::agent::turn_contracts::ToolLoopEvent::ToolFinished {
                tool_call_id,
                name,
                success,
                duration_ms,
                result,
            } => Action::ToolFinished {
                task_id: self.provider_turn_task_id,
                sequence: None,
                tool_call_id: Some(tool_call_id),
                name,
                success,
                duration_ms,
                result: Some(result),
            },
        };
        self.action_tx
            .send(action)
            .await
            .map_err(|_| anyhow::anyhow!("chat action channel closed"))
    }
}

/// Chat adapter over the existing Agent turn owner.
///
/// Redux remains the UI/state/persistence owner. Provider iterations, provider
/// streaming, overflow recovery, tool execution, usage aggregation, history
/// mutation and cancellation are all delegated to `run_turn`.
#[allow(clippy::too_many_arguments)]
/// Publish the tail of the last throttle window before a terminal action
/// retires the draft, since the reducer drops reasoning deltas once it has.
///
/// Both terminal paths use it: a turn that fails still produced the reasoning
/// that led up to the failure, and dropping it there hid exactly the part a
/// reader needs.
async fn flush_trailing_reasoning(
    action_tx: &mpsc::Sender<Action>,
    draft_id: &str,
    reasoning_progress: &Arc<ParkingMutex<ReasoningProgressBatcher>>,
    stream_version: &Arc<AtomicU64>,
) {
    // The lock is released by the end of this statement, before the await
    // below: a batcher lock must never be held across a send.
    let trailing = reasoning_progress.lock().flush();
    if let Some(batch) = trailing {
        let _ = action_tx
            .send(Action::StreamReasoningReceived {
                draft_id: draft_id.to_string(),
                delta: batch,
                version: stream_version.fetch_add(1, Ordering::Relaxed).saturating_add(1),
            })
            .await;
    }
}

async fn drive_start_turn_stream(
    turn_binding: Option<Arc<super::decision_source::ChatTurnBinding>>,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    provider: Arc<dyn Provider>,
    mut history: Vec<crate::providers::traits::ChatMessage>,
    mut compaction_guard_history: Vec<crate::providers::traits::ChatMessage>,
    model: String,
    temperature: f64,
    compaction_config: Option<crate::config::AgentCompactionConfig>,
    cancel: CancellationToken,
    draft_id: String,
    action_tx: mpsc::Sender<Action>,
    tools_registry: Option<Arc<Vec<Box<dyn crate::tools::Tool>>>>,
    tool_execution_service: Option<Arc<ToolExecutionService>>,
    tool_execution_context: ToolExecutionContext,
    request_event_fabric: MemoryFabric,
    compaction_audit: Option<crate::agent::document_ingest::DocumentIngestRuntime>,
    chat_mode: crate::agent::turn_contracts::ChatMode,
    observer: Arc<dyn Observer>,
    hooks: Arc<HookManager>,
) {
    let Some(binding) = turn_binding.as_ref() else {
        let _ = action_tx
            .send(Action::StreamFailed {
                draft_id,
                err: "chat runtime turn requires an admitted decision".to_owned(),
                retryable: false,
            })
            .await;
        return;
    };
    if let Err(error) = binding.verify_execution_mode(chat_mode) {
        let _ = action_tx
            .send(Action::StreamFailed {
                draft_id,
                err: error.to_string(),
                retryable: false,
            })
            .await;
        return;
    }
    let Some(routing) = binding.decision_routing() else {
        let _ = action_tx
            .send(Action::StreamFailed {
                draft_id,
                err: "chat runtime turn requires an admitted decision".to_owned(),
                retryable: false,
            })
            .await;
        return;
    };
    // Fixed directives bypass compaction, provider execution and tool setup.
    // Completion still travels through the task-scoped ordered history owner.
    match binding.fixed_reply() {
        Ok(Some(reply)) => {
            let action = if cancel.is_cancelled() {
                Action::StreamCancelled { draft_id }
            } else if provider_turn_task_id.is_some() {
                Action::ProviderTurnReadyForCommit {
                    draft_id,
                    final_text: reply.to_owned(),
                    reasoning: String::new(),
                }
            } else {
                if action_tx
                    .send(Action::RecordAssistantTurn {
                        task_id: None,
                        content: reply.to_owned(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                Action::StreamCompleted {
                    draft_id,
                    final_text: reply.to_owned(),
                    reasoning: String::new(),
                }
            };
            let _ = action_tx.send(action).await;
            return;
        }
        Ok(None) => {}
        Err(error) => {
            let _ = action_tx
                .send(Action::StreamFailed {
                    draft_id,
                    err: error.to_string(),
                    retryable: false,
                })
                .await;
            return;
        }
    }
    // Redux-specific preflight projection stays in the adapter because it must
    // publish the exact compaction patch and injection diagnostic into reducer
    // state before the shared owner starts the visible provider turn. The
    // provider/tool iteration itself remains exclusively in agent::turn_engine.
    if let Some(config) = compaction_config.as_ref()
        && !matches!(
            routing.context.directive(),
            crate::causal_tree::directive::TurnExecutionDirective::CollectApprovalArguments(_)
        )
    {
        let budget = crate::agent::context_budget::plan_context_budget(
            &history,
            config,
            crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
        );
        if budget.over_hard_limit {
            let turns_before = chat_history_turn_count(&history);
            let tokens_before = super::estimate_chat_history_tokens(&history);
            let injection_diagnostic =
                redux_injection_overbudget_diagnostic_text(&history, &compaction_guard_history, config);
            let compaction_off = matches!(config.mode, crate::config::AgentCompactionMode::Off);
            let messages_before_rollover = history.len();
            let outcome = if compaction_off {
                ContextRolloverOutcome::default()
            } else {
                match apply_redux_context_rollover(
                    provider.as_ref(),
                    &mut history,
                    &mut compaction_guard_history,
                    &model,
                    config,
                    compaction_audit.as_ref(),
                    &action_tx,
                    crate::chat::action::CompactReason::ContextOverflow,
                    "redux_preflight",
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(()) => return,
                }
            };
            let replacement_len = outcome.replacement_len;
            let after_compact = crate::agent::context_budget::plan_context_budget(
                &history,
                config,
                crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
            );
            if compaction_off || after_compact.over_hard_limit {
                trim_redux_driver_context_budget_after_rollover(
                    &mut history,
                    &mut compaction_guard_history,
                    config,
                    replacement_len,
                );
            }
            // The rollover produced no patch, so every message the trim above
            // removed is gone without a summary or an event to recover it from.
            if outcome.degraded
                && send_context_degraded_notice(
                    &action_tx,
                    crate::chat::action::CompactReason::ContextOverflow,
                    messages_before_rollover.saturating_sub(history.len()),
                )
                .await
                .is_err()
            {
                return;
            }
            let mut injection_feedback = None;
            if send_redux_injection_overbudget_diagnostic(
                &action_tx,
                injection_diagnostic,
                "redux_preflight_injection_overbudget_diagnostic",
                &mut injection_feedback,
            )
            .await
            .is_err()
            {
                return;
            }
            if send_redux_context_window_update(&action_tx, &history, config, "redux_preflight_context_window_updated")
                .await
                .is_err()
            {
                return;
            }
            let mut compaction_feedback = None;
            if send_redux_compaction_feedback(
                &action_tx,
                turns_before,
                tokens_before,
                &history,
                config,
                "redux_preflight_compaction_feedback",
                &mut compaction_feedback,
            )
            .await
            .is_err()
            {
                return;
            }
        }
    }

    let reasoning = Arc::new(ParkingMutex::new(String::new()));
    let reasoning_progress = Arc::new(ParkingMutex::new(ReasoningProgressBatcher::default()));
    let stream_version = Arc::new(AtomicU64::new(0));
    let events: Arc<dyn crate::agent::turn_contracts::ToolLoopEventSink> = Arc::new(ReduxToolLoopEventSink {
        provider_turn_task_id,
        draft_id: draft_id.clone(),
        action_tx: action_tx.clone(),
        version: Arc::clone(&stream_version),
        reasoning: Arc::clone(&reasoning),
        reasoning_progress: Arc::clone(&reasoning_progress),
    });
    let runtime_adapter = crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
        verify_chat_completion: true,
        provider_trace: binding.retained_provider_trace(),
        events: Some(events),
        stream_provider: true,
        tool_execution_service,
        tool_execution_context,
        allowed_tool_names: None,
    };
    let tools = tools_registry.unwrap_or_else(|| Arc::new(Vec::new()));

    let multimodal = crate::config::MultimodalConfig::default();
    let work = crate::agent::turn_engine::run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
            scope: None,
            mode: chat_mode,
            silent: true,
            expose_stay_silent: false,
            multimodal: &multimodal,
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 1,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: compaction_config.as_ref(),
            decision: routing,
        },
        crate::agent::turn_request::TurnServices {
            provider: provider.as_ref(),
            provider_name: binding.provider_name(),
            model: &model,
            temperature,
            tools_registry: tools,
            observer: observer.as_ref(),
            hooks: hooks.as_ref(),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(runtime_adapter),
            // The adapter's ToolExecutionService already carries the resolved ledger.
            // The scope lets an approval collection record its request event; the
            // legacy chat loop (chat/run.rs) sets the same scope.
            memory: crate::agent::turn_request::ToolLoopMemory::none()
                .with_event_fabric(request_event_fabric)
                .with_request_event_scope(
                    binding
                        .envelope()
                        .message_scope()
                        .with_recipient(format!("{}/{model}", binding.provider_name())),
                ),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: Some(cancel.clone()),
            on_delta: None,
            on_tool_call: None,
        },
    );
    // Child cleanup must settle before reducer terminal actions can publish
    // completion or permit the ordered history commit.
    let result = if let Some(owner) = crate::runtime::turn_cleanup::current_tracker() {
        match crate::runtime::turn_cleanup::CleanupScope::new(Some(&owner), CancellationToken::new()) {
            Ok(cleanup) => cleanup.run(work).await,
            Err(error) => Err(error.context(crate::runtime::turn_cleanup::CleanupIncomplete)),
        }
    } else {
        work.await
    };

    let (outcome, trace) = match result {
        Ok(result) => result,
        Err(error) if crate::agent::turn_contracts::is_tool_loop_event_sink_closed(&error) => {
            return;
        }
        Err(error)
            if error
                .downcast_ref::<crate::runtime::turn_cleanup::CleanupIncomplete>()
                .is_some() =>
        {
            flush_trailing_reasoning(&action_tx, &draft_id, &reasoning_progress, &stream_version).await;
            let _ = action_tx
                .send(Action::StreamFailed {
                    draft_id,
                    err: "Child terminal cleanup is unconfirmed".to_owned(),
                    retryable: false,
                })
                .await;
            return;
        }
        Err(error) if crate::agent::turn_contracts::is_tool_loop_cancelled(&error) || cancel.is_cancelled() => {
            let _ = action_tx.send(Action::StreamCancelled { draft_id }).await;
            return;
        }
        Err(error) => {
            let retryable = crate::agent::provider_round::tool_loop_error_is_retryable(&error);
            // A failed turn keeps whatever reasoning it produced before it
            // broke, and that tail is often the only account of why it broke.
            // `StreamFailed` retires the draft, so the flush has to come first
            // here for the same reason it does on the success path.
            flush_trailing_reasoning(&action_tx, &draft_id, &reasoning_progress, &stream_version).await;
            let _ = action_tx
                .send(Action::StreamFailed {
                    draft_id,
                    err: error.to_string(),
                    retryable,
                })
                .await;
            return;
        }
    };

    if let Err(error) = binding.capture_trace(&trace) {
        flush_trailing_reasoning(&action_tx, &draft_id, &reasoning_progress, &stream_version).await;
        let _ = action_tx
            .send(Action::StreamFailed {
                draft_id,
                err: error.to_string(),
                retryable: false,
            })
            .await;
        return;
    }
    let final_text = outcome.into_text();
    // Publish the tail of the last throttle window before the terminal action
    // retires the draft: after it the reducer drops reasoning deltas as stale.
    flush_trailing_reasoning(&action_tx, &draft_id, &reasoning_progress, &stream_version).await;
    if trace.tokens_used.is_reported()
        && action_tx
            .send(Action::StreamUsageMetered {
                draft_id: draft_id.clone(),
                usage: trace.tokens_used,
            })
            .await
            .is_err()
    {
        return;
    }
    let empty_response = crate::agent::response_guard::is_empty_assistant_response(&final_text, false);
    if empty_response {
        if action_tx
            .send(Action::SystemMessageAdded {
                text: crate::agent::response_guard::EMPTY_ASSISTANT_RESPONSE_MESSAGE.to_string(),
            })
            .await
            .is_err()
        {
            return;
        }
    } else if provider_turn_task_id.is_none()
        && action_tx
            .send(Action::RecordAssistantTurn {
                task_id: provider_turn_task_id,
                content: final_text.clone(),
            })
            .await
            .is_err()
    {
        return;
    }
    let reasoning = if empty_response {
        String::new()
    } else {
        reasoning.lock().clone()
    };
    let terminal = if provider_turn_task_id.is_some() {
        Action::ProviderTurnReadyForCommit {
            draft_id,
            final_text,
            reasoning,
        }
    } else {
        Action::StreamCompleted {
            draft_id,
            final_text,
            reasoning,
        }
    };
    let _ = action_tx.send(terminal).await;
}

// ─── Dispatcher task ───────────────────────────────────────────────────────────

/// Step 5a-4: Spawn dispatcher task with optional [`TurnCompletionSignal`].
///
/// When a signal is present, the dispatcher calls `signal.notify()` after reducing any turn-terminal
/// action (`StreamCompleted` / `StreamFailed` / `StreamCancelled`), waking the await point in the
/// `chat::run` main loop that waits for turn completion.
///
/// The dispatcher is completely unaware of the concrete driver implementation and only fires
/// turn-boundary events by action type.
pub fn spawn_dispatcher_task_with_signal(
    initial_state: ChatState,
    action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
    executor: EffectExecutor,
    turn_signal: Option<TurnCompletionSignal>,
) -> tokio::task::JoinHandle<DispatcherStats> {
    spawn_dispatcher_task_full(
        initial_state,
        action_rx,
        shutdown,
        executor,
        turn_signal,
        #[cfg(feature = "terminal-tui")]
        None,
    )
}

/// S4-A wrap-up P1: factor out the snapshot construction + push block duplicated in two places in the dispatcher.
/// send_replace simply overwrites; the monotonic increase of snapshot_rev is guaranteed by reduce ordering.
#[cfg(feature = "terminal-tui")]
fn push_snapshot_if_dirty(
    state: &mut ChatState,
    snapshot_tx: &Option<tokio::sync::watch::Sender<Arc<crate::chat::state::UiSnapshot>>>,
    snapshot_rev: &std::sync::atomic::AtomicU64,
    dirty: bool,
) {
    use std::sync::atomic::Ordering as AtomicOrdering;
    if !dirty {
        return;
    }
    let Some(tx) = snapshot_tx.as_ref() else {
        return;
    };
    let next_rev = snapshot_rev.fetch_add(1, AtomicOrdering::Relaxed).saturating_add(1);
    let new_snap = Arc::new(state.build_ui_snapshot(next_rev));
    tx.send_replace(new_snap);
    tracing::trace!(rev = next_rev, "s4_a snapshot pushed");
}

/// **S4-A Commit 3**: `spawn_dispatcher_task_with_signal` + optional UiSnapshot push.
///
/// Pure mode passes `snapshot_tx: Some(watch::Sender<Arc<UiSnapshot>>)`, and the dispatcher builds a new
/// snapshot and calls send_if_modified once reduce finishes with `ui_dirty=true`;
/// Callers without a snapshot consumer pass None; this does not choose an execution mode.
///
/// snapshot_rev: AtomicU64, monotonically increasing. watch send_if_modified compares the revision to
/// skip identical frames; the revision never goes backwards, so a receiver can never see a stale frame.
#[cfg(feature = "terminal-tui")]
pub fn spawn_dispatcher_task_full(
    initial_state: ChatState,
    mut action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
    executor: EffectExecutor,
    turn_signal: Option<TurnCompletionSignal>,
    snapshot_tx: Option<tokio::sync::watch::Sender<Arc<crate::chat::state::UiSnapshot>>>,
) -> tokio::task::JoinHandle<DispatcherStats> {
    use std::sync::atomic::AtomicU64;
    // S3 T3-1: take the approval_router handle out early (Arc clone); it is used later, after the reducer has
    // handled `Action::ToolApprovalReceived`, to hand the decision to the oneshot the driver is waiting on.
    let approval_router = executor.approval_router();
    tokio::spawn(async move {
        let mut state = initial_state;
        let mut stats = DispatcherStats::default();
        // S4-A Commit 3: the revision counter is only used when snapshot_tx is present.
        let snapshot_rev = AtomicU64::new(0);

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    // Drain whatever is left (best-effort) before exit so
                    // late-arriving Actions still hit the reducer for
                    // observability. Bounded by remaining queue depth, not by
                    // network/disk I/O.
                    while let Ok(action) = action_rx.try_recv() {
                        stats.actions_seen = stats.actions_seen.saturating_add(1);
                        let outcome = extract_turn_outcome(&action);
                        let usage = extract_stream_usage(&action);
                        let draft_id = extract_turn_draft_id(&action).map(str::to_string);
                        let approval_response = extract_approval_response(&action);
                        let requires_persistence = matches!(&action, Action::StreamCompleted { .. });
                        let (effects, ui_dirty) = state.reduce_tracked(action);
                        let save_requested = effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_)));
                        stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                        for effect in effects {
                            executor.execute(effect).await;
                        }
                        let outcome = executor.outcome_after_effects(outcome, requires_persistence, save_requested);
                        push_snapshot_if_dirty(&mut state, &snapshot_tx, &snapshot_rev, ui_dirty);
                        if let (Some((tool_id, approved)), Some(router)) =
                            (approval_response, approval_router.as_ref())
                        {
                            router.resolve(&tool_id, approved);
                        }
                        // turn_signal must fire during shutdown too, otherwise the last turn before shutdown
                        // preempts the chat::run await and hangs forever (causing the round 2 hang
                        // regression). The terminal action carries the outcome — main.rs:888
                        // shutdown_timeout is the backstop that guarantees the main process finally exits.
                        if let Some(ref sig) = turn_signal {
                            record_turn_signal_action(sig, draft_id.as_deref(), usage, outcome);
                        }
                    }
                    // Fallback: during shutdown chat::run may still be awaiting turn_signal.notified(),
                    // so notify once to let it detect shutdown.cancelled() and leave the select (no outcome
                    // → the waiter interprets it as cancelled).
                    if let Some(ref sig) = turn_signal {
                        sig.notify();
                    }
                    tracing::debug!(
                        actions = stats.actions_seen,
                        effects = stats.effects_seen,
                        "redux dispatcher task: shutdown drained"
                    );
                    break;
                }
                maybe_action = action_rx.recv() => {
                    match maybe_action {
                        Some(action) => {
                            stats.actions_seen = stats.actions_seen.saturating_add(1);
                            let outcome = extract_turn_outcome(&action);
                            let usage = extract_stream_usage(&action);
                            let draft_id = extract_turn_draft_id(&action).map(str::to_string);
                            let approval_response = extract_approval_response(&action);
                            let requires_persistence = matches!(&action, Action::StreamCompleted { .. });
                        let (effects, ui_dirty) = state.reduce_tracked(action);
                        let save_requested = effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_)));
                            stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                            for effect in effects {
                                executor.execute(effect).await;
                            }
                            let outcome = executor.outcome_after_effects(outcome, requires_persistence, save_requested);
                            push_snapshot_if_dirty(&mut state, &snapshot_tx, &snapshot_rev, ui_dirty);
                            // S3 T3-1: once the reducer has handled ToolApprovalReceived, hand the decision
                            // to the driver's pending oneshot through approval_router.
                            if let (Some((tool_id, approved)), Some(router)) =
                                (approval_response, approval_router.as_ref())
                            {
                                router.resolve(&tool_id, approved);
                            }
                            if let Some(ref sig) = turn_signal {
                                record_turn_signal_action(sig, draft_id.as_deref(), usage, outcome);
                            }
                        }
                        None => {
                            // Channel closed: every dispatcher sender has been dropped.
                            // Fallback notify so chat::run never awaits forever.
                            if let Some(ref sig) = turn_signal {
                                sig.notify();
                            }
                            tracing::debug!(
                                actions = stats.actions_seen,
                                effects = stats.effects_seen,
                                "redux dispatcher task: channel closed, exiting"
                            );
                            break;
                        }
                    }
                }
            }
        }

        stats
    })
}

/// Placeholder for spawn_dispatcher_task_full without the terminal-tui feature (no snapshot push).
#[cfg(not(feature = "terminal-tui"))]
pub fn spawn_dispatcher_task_full(
    initial_state: ChatState,
    mut action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
    executor: EffectExecutor,
    turn_signal: Option<TurnCompletionSignal>,
) -> tokio::task::JoinHandle<DispatcherStats> {
    let approval_router = executor.approval_router();
    tokio::spawn(async move {
        let mut state = initial_state;
        let mut stats = DispatcherStats::default();

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    while let Ok(action) = action_rx.try_recv() {
                        stats.actions_seen = stats.actions_seen.saturating_add(1);
                        let outcome = extract_turn_outcome(&action);
                        let usage = extract_stream_usage(&action);
                        let draft_id = extract_turn_draft_id(&action).map(str::to_string);
                        let approval_response = extract_approval_response(&action);
                        let requires_persistence = matches!(&action, Action::StreamCompleted { .. });
                        let effects = state.reduce(action);
                        let save_requested = effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_)));
                        stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                        for effect in effects {
                            executor.execute(effect).await;
                        }
                        let outcome = executor.outcome_after_effects(outcome, requires_persistence, save_requested);
                        if let (Some((tool_id, approved)), Some(router)) =
                            (approval_response, approval_router.as_ref())
                        {
                            router.resolve(&tool_id, approved);
                        }
                        if let Some(ref sig) = turn_signal {
                            record_turn_signal_action(sig, draft_id.as_deref(), usage, outcome);
                        }
                    }
                    if let Some(ref sig) = turn_signal {
                        sig.notify();
                    }
                    break;
                }
                maybe_action = action_rx.recv() => {
                    match maybe_action {
                        Some(action) => {
                            stats.actions_seen = stats.actions_seen.saturating_add(1);
                            let outcome = extract_turn_outcome(&action);
                            let usage = extract_stream_usage(&action);
                            let draft_id = extract_turn_draft_id(&action).map(str::to_string);
                            let approval_response = extract_approval_response(&action);
                            let requires_persistence = matches!(&action, Action::StreamCompleted { .. });
                        let effects = state.reduce(action);
                        let save_requested = effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_)));
                            stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                            for effect in effects {
                                executor.execute(effect).await;
                            }
                            let outcome = executor.outcome_after_effects(outcome, requires_persistence, save_requested);
                            if let (Some((tool_id, approved)), Some(router)) =
                                (approval_response, approval_router.as_ref())
                            {
                                router.resolve(&tool_id, approved);
                            }
                            if let Some(ref sig) = turn_signal {
                                record_turn_signal_action(sig, draft_id.as_deref(), usage, outcome);
                            }
                        }
                        None => {
                            if let Some(ref sig) = turn_signal {
                                sig.notify();
                            }
                            break;
                        }
                    }
                }
            }
        }

        stats
    })
}

/// **S3 T3-1**: extract the (tool_id, approved) tuple from `Action::ToolApprovalReceived`.
///
/// Only used for approval_router forwarding before / after the reducer handles it; other Actions return None.
/// Borrows to avoid cloning early — the tuple is extracted before the reducer consumes the action.
fn extract_approval_response(action: &Action) -> Option<(String, bool)> {
    match action {
        Action::ToolApprovalReceived { tool_id, approved } => Some((tool_id.clone(), *approved)),
        _ => None,
    }
}

/// Lightweight stats returned by [`spawn_dispatcher_task_with_signal`] on shutdown
/// (for integration tests and metrics).
#[derive(Debug, Default, Clone, Copy)]
pub struct DispatcherStats {
    pub actions_seen: u64,
    pub effects_seen: u64,
}

// ─── StreamChunkCoalescer ──────────────────────────────────────────────────────

/// `StreamChunkReceived` delta coalescer (Codex P0-3, for a full channel).
///
/// How it works:
/// 1. `try_send_chunk` first attempts `try_send`; on success it clears pending
/// 2. when full (Backpressured), the delta is accumulated into the `pending` buffer
/// 3. on the next `try_send_chunk`, pending (already accumulated) is sent first, then the current delta
/// 4. `flush` force-flushes pending at shutdown or stream end
///
/// Design choices:
/// - only coalesce within the same draft_id; across drafts the old pending is dropped (defensive)
/// - **version takes the newest** (Codex P2 fix): matching the reducer's strict-monotonic rule at
///   `state.rs:540` — `version <= draft.version` is always dropped. Taking the earliest would make a
///   higher version that arrived first be dropped by the reducer because `merged.version <=
///   draft.version`, losing the delta permanently. Merging takes `max(pending.version, new.version)`
///   so the merged Action can at least move the reducer forward.
pub struct StreamChunkCoalescer {
    /// pending: (draft_id, accumulated_delta, latest_version)
    pending: Option<(String, String, u64)>,
    sender: mpsc::Sender<Action>,
}

impl StreamChunkCoalescer {
    pub const fn new(sender: mpsc::Sender<Action>) -> Self {
        Self { pending: None, sender }
    }

    /// Try to send one chunk Action. When the channel is full, accumulate into pending.
    ///
    /// Returns a `DispatchResult` for the caller to observe (on Closed the caller should stop pumping chunks).
    pub fn try_send_chunk(&mut self, draft_id: String, delta: String, version: u64) -> DispatchResult {
        // 1. If pending exists, first try to send the accumulated result + the current delta as one merged Action
        if let Some((p_draft, p_delta, p_version)) = self.pending.take() {
            if p_draft == draft_id {
                // same draft: accumulate the delta, take max of version (matching the reducer's strict-monotonic rule)
                let merged_delta = format!("{p_delta}{delta}");
                let merged_version = p_version.max(version);
                let action = Action::StreamChunkReceived {
                    draft_id: draft_id.clone(),
                    delta: merged_delta.clone(),
                    version: merged_version,
                };
                return match self.sender.try_send(action) {
                    Ok(()) => DispatchResult::Sent,
                    Err(TrySendError::Full(_)) => {
                        // still full: accumulate into pending (version stays at max)
                        self.pending = Some((draft_id, merged_delta, merged_version));
                        DispatchResult::Backpressured
                    }
                    Err(TrySendError::Closed(_)) => DispatchResult::ChannelClosed,
                };
            }
            // different draft: the old pending is meaningless, drop it and take the fast path for the current delta
            tracing::warn!(
                old_draft = %p_draft,
                new_draft = %draft_id,
                "coalescer cross-draft pending dropped (defensive)"
            );
        }

        // 2. No pending (or just cleared): try_send the current delta directly
        let action = Action::StreamChunkReceived {
            draft_id: draft_id.clone(),
            delta: delta.clone(),
            version,
        };
        match self.sender.try_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(TrySendError::Full(_)) => {
                self.pending = Some((draft_id, delta, version));
                DispatchResult::Backpressured
            }
            Err(TrySendError::Closed(_)) => DispatchResult::ChannelClosed,
        }
    }

    /// Force-flush pending at stream end or shutdown (best-effort).
    pub fn flush(&mut self) -> DispatchResult {
        let Some((draft_id, delta, version)) = self.pending.take() else {
            return DispatchResult::Sent;
        };
        let action = Action::StreamChunkReceived {
            draft_id,
            delta,
            version,
        };
        match self.sender.try_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(TrySendError::Full(_)) => DispatchResult::Backpressured,
            Err(TrySendError::Closed(_)) => DispatchResult::ChannelClosed,
        }
    }

    /// Test observability of the pending state.
    #[cfg(test)]
    pub const fn pending_for_test(&self) -> Option<&(String, String, u64)> {
        self.pending.as_ref()
    }
}

/// Workspace for the chat tool tests, unique per process run.
///
/// The tool-execution ledger lives inside the workspace and is durable on
/// purpose, so a fixed path would let one run's terminal records replay into the
/// next run and skip the tool the tests are there to observe.
#[cfg(test)]
fn chat_test_workspace() -> std::path::PathBuf {
    static WORKSPACE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    WORKSPACE
        .get_or_init(|| {
            std::env::temp_dir().join(format!(
                "prx-chat-tool-tests-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7()
            ))
        })
        .clone()
}

#[cfg(test)]
fn tool_security_policy(level: crate::security::AutonomyLevel) -> Arc<crate::security::SecurityPolicy> {
    Arc::new(crate::security::SecurityPolicy {
        autonomy: level,
        workspace_dir: chat_test_workspace(),
        ..crate::security::SecurityPolicy::default()
    })
}

#[cfg(test)]
fn full_tool_security_policy() -> Arc<crate::security::SecurityPolicy> {
    tool_security_policy(crate::security::AutonomyLevel::Full)
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

// ─── Step 5b integration tests (dispatcher + reducer + coalescer + EffectExecutor end to end) ─

#[cfg(test)]
mod integration_tests;

// ─── Step 5a-1 real business execution tests (EffectExecutor::new_with_deps) ──────────────────

#[cfg(test)]
mod real_mode_tests;

#[cfg(test)]
mod turn_characterization_tests;

#[cfg(test)]
mod tool_tiering_tests;

#[cfg(test)]
mod stream_native_tools_tests;

#[cfg(test)]
mod assessment_fixture;

// ─── S4-A Commit 3: dispatcher snapshot push ────────────────────────────────

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod s4_a_3;
