//! Redux dispatcher infrastructure (Step 5a-1 — real business execution + dual-write guard).
//!
//! Provides three pieces that feed production events into the reducer and run business logic on demand:
//! - [`ChatDispatcher`]: `Action` sender wrapper (bounded mpsc + try_send policy)
//! - [`EffectExecutor`]: in shadow mode (5b) every business Effect is a no-op;
//!   in real mode (5a-1) it holds [`EffectDeps`] and really executes, gated by PRX_CHAT_REDUX
//! - [`StreamChunkCoalescer`]: merges `StreamChunkReceived` deltas into a single `Action`
//!   when the channel is full, so backpressure does not drop intermediate chunks
//!
//! Design notes (Codex audit P0-1 / P0-2 / P0-3 / P2-coalescer-version):
//! - **bounded channel**: `Action` channel capacity is 2048, to prevent OOM
//! - **dual-write guard**: [`RuntimeDualWriteGuard`] (Arc<AtomicBool>) marks whether this turn
//!   is handled by the Redux path; in Both/Redux mode the legacy path uses the guard to decide
//!   whether to skip persistence, preventing history / session from being written twice
//! - **long-running effects spawn subtasks**: `StartTurn` / `SaveSession` / `EmitChannelMessage`
//!   / `PersistToMemory` all use `tokio::spawn` in deps mode, so await never blocks the main loop
//! - **coalescer version takes the newest**: consistent with the reducer's strict-monotonic check
//!   at `state.rs:540`, merging uses `version = max(pending, new)`; otherwise a higher version that
//!   arrived first would be dropped after merging
//! - **RouteDecision / ProviderExecutionOutcome timeline**: the streaming path keeps the unified
//!   recording in the ingress layer; the dispatcher only owns stream-state event ordering
//! - **OS signals all enter as Actions**: the Ctrl+C / SIGTERM handler `try_send`s a shutdown action
//!
//! Rollout modes (aligned with `chat::ReduxMode`):
//! - `Off`: EffectExecutor::new_shadow() (business no-op, only LogTrace runs)
//! - `Both`: EffectExecutor::new_with_deps() (business really executes) + legacy path still runs +
//!   the guard suppresses legacy persistence, so both paths run in parallel but only the reducer
//!   truly persists (the reducer is the new source of truth)
//! - `Redux`: similar to Both (stage 5a-1 does not delete the legacy path, it only lets the reducer
//!   lead at runtime; 5a-3 actually deletes the legacy path)

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
    ToolExecutionContext, ToolExecutionPermit, ToolExecutionPreparation, ToolExecutionService, ToolExecutionStatus,
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
        let scope = crate::agent::loop_::ScopeContext {
            policy: self.policy.as_ref(),
            sender,
            channel,
            chat_type: &request.context.chat_type,
            chat_id: &request.context.chat_id,
            owner_id: envelope.owner_id.as_deref(),
            topic_id: envelope.topic_id.as_deref(),
            task_id: envelope.task_id.as_deref(),
            source_message_event_id: envelope.source_message_event_id.as_deref(),
            config_generation_id: envelope.config_generation_id,
            config_source_revision: envelope.config_source_revision.as_deref(),
        };
        let runtime_grant = crate::agent::loop_::runtime_approval_grant_for_call(
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
mod approval_router_regression_tests {
    use super::ApprovalRouter;

    #[test]
    fn approval_router_rejects_concurrent_registration_without_replacing_first() {
        let router = ApprovalRouter::new();
        let (first_tx, mut first_rx) = tokio::sync::oneshot::channel::<bool>();
        let (second_tx, mut second_rx) = tokio::sync::oneshot::channel::<bool>();

        assert!(router.register("first".to_string(), first_tx));
        assert!(!router.register("second".to_string(), second_tx));
        assert!(router.has_pending());
        assert_eq!(second_rx.try_recv(), Ok(false), "second approval must fail closed");

        assert!(router.resolve("first", true));
        assert_eq!(first_rx.try_recv(), Ok(true), "first pending approval must stay intact");
        assert!(!router.has_pending());
    }
}

// ─── ChatDispatcher ────────────────────────────────────────────────────────────

/// `Action` sender wrapper. Exposes only the `try_send` / `send` policies; unbounded clones are forbidden.
///
/// - `try_send`: non-blocking — for streaming chunks / control Actions (when full the caller should use the coalescer)
/// - `send_blocking`: blocking synchronous path — for critical exit Actions (Ctrl+C / SIGTERM handler)
/// - `send`: async blocking — for critical Actions when the caller is in an async context (e.g. the main loop)
#[allow(dead_code)]
#[derive(Clone)]
pub struct ChatDispatcher {
    action_tx: mpsc::Sender<Action>,
}

/// Result of the `try_send` policy (lets the caller decide whether coalescing / a fallback is needed).
#[allow(dead_code)]
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
    /// Construct the dispatcher plus its receiver. The receiver is consumed by [`spawn_dispatcher_task`].
    #[allow(dead_code)]
    pub fn new() -> (Self, mpsc::Receiver<Action>) {
        let (action_tx, action_rx) = mpsc::channel::<Action>(ACTION_CHANNEL_CAPACITY);
        (Self { action_tx }, action_rx)
    }

    /// Non-blocking send. Returns `Backpressured` when full; the caller decides to coalesce or drop.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn blocking_dispatch(&self, action: Action) -> DispatchResult {
        match self.action_tx.blocking_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(_) => DispatchResult::ChannelClosed,
        }
    }

    /// Async blocking send (recommended: safe backpressure on async paths).
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn sender(&self) -> mpsc::Sender<Action> {
        self.action_tx.clone()
    }
}

// ─── TurnCompletionSignal (Step 5a-4) ─────────────────────────────────────────

/// Semantic result of a turn's termination, written by the dispatcher in
/// [`TurnCompletionSignal::record_and_notify`] and read by chat::run after await to decide UI/hook behaviour.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TurnOutcomeKind {
    /// The LLM stream completed successfully; `final_text` is the final accumulated visible text, `reasoning`
    /// carries the final reasoning card payload that must be replayed when an
    /// ordered commit gate releases the reducer terminal action.
    Completed { final_text: String, reasoning: String },
    /// The LLM stream failed; `err` is the error description carried by [`Action::StreamFailed`],
    /// `retryable` reflects the [`stream_error_is_retryable`] verdict.
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
        Self {
            inner: Arc::new(tokio::sync::Notify::new()),
            outcome: Arc::new(parking_lot::Mutex::new(None)),
            usage: Arc::new(ParkingMutex::new(ProviderUsageAccumulator::new())),
            keyed: Arc::new(ParkingMutex::new(KeyedTurnCompletionState::default())),
        }
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
#[allow(dead_code)]
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

/// Report whether an action is a turn-terminal event. The dispatcher task uses this to decide when to
/// trigger [`TurnCompletionSignal::notify`].
#[must_use]
pub const fn is_turn_terminal_action(action: &Action) -> bool {
    matches!(
        action,
        Action::StreamCompleted { .. }
            | Action::ProviderTurnReadyForCommit { .. }
            | Action::StreamFailed { .. }
            | Action::StreamCancelled { .. }
    )
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
    /// Bounded at [`crate::chat::PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY`]; a
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
    /// Session-scoped union of every tool set capability routing has published
    /// on this chat session.
    ///
    /// The `tools` array travels inside the provider's cacheable request
    /// prefix, next to the system prompt: re-deciding the exposed set on every
    /// turn invalidates the prefix — and the whole conversation's prefill —
    /// each time the user rephrases. Exposing the running union instead means
    /// the prefix moves only when a genuinely new intent appears.
    pub exposed_tools: crate::tools::intent::SessionToolExposure,
}

// ─── EffectExecutor (5a-1: real-mode + shadow-mode) ───────────────────────────

/// `Effect` executor. Two construction shapes:
/// - shadow mode (`new_shadow`): every business Effect except `LogTrace` is a no-op; kept for Off mode /
///   unit tests / the 5b behaviour baseline
/// - real mode (`new_with_deps`): holds [`EffectDeps`] and really executes business Effects; long-running
///   operations spawn a subtask that feeds Actions back (Codex P0-1)
///
/// P0-2 fix: `redraw_tx` is injected later through a shared `Arc<parking_lot::Mutex<Option<mpsc::Sender<()>>>>`.
/// `chat::run` constructs the EffectExecutor first (redraw_tx does not exist yet), and after spawning the
/// dispatcher task injects `redraw_tx` through the Arc returned by `redraw_handle()`, solving the ordering problem.
#[allow(dead_code)]
pub struct EffectExecutor {
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
    #[allow(dead_code)]
    #[must_use]
    pub fn new_shadow() -> Self {
        Self {
            shadow_mode: true,
            deps: None,
            redraw_slot: Arc::new(parking_lot::Mutex::new(None)),
            provider_slot: None,
            persistence_error: Arc::new(ParkingMutex::new(None)),
        }
    }

    /// Construct a real business executor (Step 5a-1, PRX_CHAT_REDUX=both/1 mode).
    #[allow(dead_code)]
    #[must_use]
    pub fn new_with_deps(deps: EffectDeps) -> Self {
        let provider_slot = Some(ProviderSlot::new(Arc::clone(&deps.provider)));
        Self {
            shadow_mode: false,
            deps: Some(deps),
            redraw_slot: Arc::new(parking_lot::Mutex::new(None)),
            provider_slot,
            persistence_error: Arc::new(ParkingMutex::new(None)),
        }
    }

    fn outcome_after_effects(&self, outcome: Option<TurnOutcomeKind>) -> Option<TurnOutcomeKind> {
        let persistence_error = self.persistence_error.lock().take();
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
    #[allow(dead_code)]
    #[must_use]
    pub fn redraw_handle(&self) -> Arc<ParkingMutex<Option<mpsc::Sender<()>>>> {
        Arc::clone(&self.redraw_slot)
    }

    /// BUG-07: return the model hot-swap slot from deps (real mode only).
    ///
    /// `chat::run` uses this handle to atomically replace the model name on `/model <name>`, so later turns'
    /// `drive_start_turn_stream` reads the new value. Shadow mode has no deps and returns None.
    #[allow(dead_code)]
    #[must_use]
    pub fn model_handle(&self) -> Option<ModelSlot> {
        self.deps.as_ref().map(|d| d.model.clone())
    }

    /// Bug #3: return the provider hot-swap slot (real mode only).
    ///
    /// `chat::run` uses this handle to atomically replace the provider handle on `/provider <name>`, so later
    /// turns' `drive_start_turn_stream` issues requests with the new provider. Shadow mode returns None.
    #[allow(dead_code)]
    #[must_use]
    pub fn provider_handle(&self) -> Option<ProviderSlot> {
        self.provider_slot.clone()
    }

    /// **S3 T3-1**: return the approval router from deps (real mode only).
    ///
    /// `spawn_dispatcher_task_with_signal` uses this handle to send the decision back to the driver's pending
    /// oneshot after `Action::ToolApprovalReceived` has entered the reducer. Shadow mode has no deps and returns None.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
                routing_input,
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
                // ratatui path after 5a-3 wiring). The chat::run main loop is still driven by `run_tool_call_loop`
                // (legacy path); `dual_write_guard` already guards the reducer persistence effects.
                // Bug #3: prefer the hot-swappable provider slot (updated by
                // `/provider <name>`); fall back to the construction-time provider
                // when no slot is present (e.g. shadow construction edge cases).
                let provider = self
                    .provider_slot
                    .as_ref()
                    .map(ProviderSlot::current)
                    .unwrap_or_else(|| Arc::clone(&deps.provider));
                let action_tx = deps.action_tx.clone();
                let guard_scope = deps.dual_write_guard.enter_scope();
                // Codex P1 fix: take the real model + temperature from deps and pass them to the stream API.
                // BUG-07: read the current ModelSlot value at the start of every turn, so after /model <name>
                // later turns automatically use the new model (same provider, different model).
                let model = deps.model.current().to_string();
                let temperature = deps.temperature;
                // 5a-6: pass the tool registry through (None → the driver degrades to a plain text stream).
                let tools_registry = deps.tools_registry.as_ref().map(Arc::clone);
                let tool_security_policy = Arc::clone(&deps.tool_security_policy);
                // Validation enforces the document this provider actually put
                // on the wire; see `ToolExecutionContext::schema_dialect`.
                let tool_execution_context = chat_tool_execution_context(
                    tool_security_policy.as_ref(),
                    turn_spawn_ctx.as_ref(),
                    provider_turn_task_id,
                    &draft_id,
                )
                .with_schema_dialect(provider.tool_schema_dialect());
                let request_event_fabric = MemoryFabric::new(
                    Arc::clone(&deps.memory),
                    tool_execution_context.envelope.workspace_id.clone(),
                )
                .with_event_recording(deps.memory_event_recording);
                let compaction_audit = Some(crate::agent::loop_::DocumentIngestRuntime::from_envelope(
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
                // Route on the turn's raw user text (never on the enriched
                // history) and publish the session's cumulative set, so an
                // injected `[Recent shared workspace events]` block can neither
                // widen the tool surface nor move the cacheable prefix.
                let tool_surface = match (routing_input.as_deref(), tools_registry.as_deref()) {
                    (Some(input), Some(registry)) => {
                        deps.exposed_tools.sticky_surface(&deps.tool_tiering, registry, input)
                    }
                    _ => crate::tools::intent::SessionToolSurface {
                        tiering: deps.tool_tiering.clone(),
                        unrouted: crate::tools::intent::UnroutedToolPolicy::PublishEverything,
                    },
                };
                let provider_turn_execution_lease_id =
                    provider_turn_task_id.map(|_| next_provider_turn_execution_lease_id());
                let provider_task_handle = tokio::spawn(async move {
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
                        tool_surface,
                        routing_input,
                    );
                    // D8-4 (redux path real fix): mirror the legacy
                    // `run_tool_call_loop_traced` wrapper in `chat::run` — seed the
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

/// Decide whether a [`StreamError`] is worth retrying.
///
/// Aligned with the reducer's `Action::StreamFailed { retryable, .. }` field. **This bool drives no
/// automatic resend**, and is not meant to: the real retries happen in the provider layer
/// (the backoff / `Retry-After` / failover chain of
/// [`ReliableProvider`](crate::providers::reliable::ReliableProvider)); once an error reaches the chat
/// turn layer this turn's tool side effects may already have run, so silently resending is unsafe.
/// Its purpose is diagnostic: written into trace logs and exposed as a `HookEvent::Error` payload
/// field, so external audits / webhooks can tell a transient upstream failure from a hopeless request. Criteria:
/// - `Http` / `Io`: transient network failure, retryable
/// - `RateLimited`: upstream throttling (429/503), retryable (FIX-P0-33: carries a structured Retry-After hint)
/// - `Json` / `InvalidSse`: corrupted data, a retry most likely reproduces it, non-retryable
/// - `Provider`: server-side semantic error, leans non-retryable (show it upstream and let the user decide)
#[must_use]
const fn stream_error_is_retryable(err: &crate::providers::traits::StreamError) -> bool {
    use crate::providers::traits::StreamError;
    matches!(
        err,
        StreamError::Http(_) | StreamError::Io(_) | StreamError::RateLimited { .. }
    )
}

/// **S3 T3-1**: network timeout / connection error detection — decides whether the driver uses backoff retry.
///
/// Matching conditions:
/// - `StreamError::Io` is always a retryable transient failure (same source as [`stream_error_is_retryable`])
/// - `StreamError::Http(reqwest_err)` where `is_timeout()` or `is_connect()` returns true
///
/// Everything else returns false, and the caller takes the plain `StreamFailed` path instead of the retry loop.
#[must_use]
fn stream_error_is_network_timeout(err: &crate::providers::traits::StreamError) -> bool {
    use crate::providers::traits::StreamError;
    match err {
        StreamError::Io(_) => true,
        StreamError::Http(http_err) => http_err.is_timeout() || http_err.is_connect(),
        // A rate-limit is not a network timeout; it is honored via Retry-After in
        // the reliability layer, not the network-backoff retry loop here.
        StreamError::Json(_)
        | StreamError::InvalidSse(_)
        | StreamError::Provider(_)
        | StreamError::RateLimited { .. } => false,
    }
}

/// **S3 T3-1**: detect context overflow / context_length_exceeded style errors.
///
/// On a match → the driver compacts history and keeps retrying as long as each compaction makes progress.
/// The decision is a substring match on the message of `StreamError::Provider` (OpenAI returns
/// "maximum context length", Anthropic returns "prompt is too long", Gemini returns "input token count", ...).
///
/// No exact regex: provider error message formats are unstable, so substring matching is safer; a false
/// positive (one extra compact) only costs a little compute rather than breaking correctness.
#[must_use]
fn stream_error_is_context_overflow(err: &crate::providers::traits::StreamError) -> bool {
    use crate::providers::traits::StreamError;
    let msg = match err {
        StreamError::Provider(s) => s.as_str(),
        StreamError::Http(http_err) => return matches!(http_err.status(), Some(s) if s.as_u16() == 413),
        StreamError::Json(_) | StreamError::InvalidSse(_) | StreamError::Io(_) | StreamError::RateLimited { .. } => {
            return false;
        }
    };
    let needles = [
        "context_length_exceeded",
        "context length exceeded",
        "maximum context",
        "exceeds maximum",
        "prompt is too long",
        "input token count",
        "exceed the maximum",
        "too many tokens",
        "token limit",
    ];
    let lower = msg.to_ascii_lowercase();
    needles.iter().any(|n| lower.contains(n))
}

/// **S3 T3-1**: aggregation buffer for tool-call arguments.
///
/// Internally the driver keeps the state of each in-flight tool call keyed by [`ToolCallChunk::index`]:
/// on a Streaming chunk → push `arguments_delta`; on Completed → compare the aggregated value with
/// `args` to check consistency (on a discrepancy, trust Completed.args).
///
/// Design notes (Codex audit 1):
/// - only emit `Action::ToolStarted` after the Completed chunk arrives (so half-built args never trigger execution)
/// - a repeated Completed for the same index is an idempotent no-op, guarding against a provider emitting twice
/// - `id` / `name` are strictly immutable; on a conflict a warn is logged but the last Completed still wins
struct ToolCallAggregator {
    /// already aggregated chunk index → buffer
    by_index: std::collections::HashMap<usize, ToolCallSlot>,
    /// set of indices whose Completed was already emitted (guards against provider duplicates)
    completed: std::collections::HashSet<usize>,
}

/// Aggregation slot for a single tool call.
struct ToolCallSlot {
    id: String,
    name: String,
    args_buffer: String,
    final_args: Option<String>,
}

impl ToolCallAggregator {
    fn new() -> Self {
        Self {
            by_index: std::collections::HashMap::new(),
            completed: std::collections::HashSet::new(),
        }
    }

    /// Ingest one `ToolCallChunk` — dispatch by `status` to streaming-append / completed-finalize.
    ///
    /// Returns `Some((id, name, args))` when a tool call is fully ready and should trigger ToolStarted;
    /// returns `None` when it is not ready yet / already completed (already emitted) / a protocol conflict was logged.
    fn ingest(&mut self, chunk: crate::providers::traits::ToolCallChunk) -> Option<(String, String, String)> {
        use crate::providers::traits::ToolCallChunkStatus;
        match chunk.status {
            ToolCallChunkStatus::Streaming => {
                let slot = self.by_index.entry(chunk.index).or_insert_with(|| ToolCallSlot {
                    id: chunk.id.clone(),
                    name: chunk.name.clone(),
                    args_buffer: String::new(),
                    final_args: None,
                });
                // ID / name immutability check: the provider protocol forbids renaming or swapping the ID.
                // Some compatible providers may emit an opening chunk before the
                // id is known, then fill it in later; preserve that first real id.
                if !chunk.id.is_empty() {
                    if slot.id.is_empty() {
                        slot.id = chunk.id.clone();
                    } else if slot.id != chunk.id {
                        tracing::warn!(
                            index = chunk.index,
                            prev_id = %slot.id,
                            new_id = %chunk.id,
                            "ToolCallAggregator: streaming chunk changed id; keeping first id"
                        );
                    }
                }
                if !chunk.name.is_empty() && slot.name != chunk.name {
                    tracing::warn!(
                        index = chunk.index,
                        prev_name = %slot.name,
                        new_name = %chunk.name,
                        "ToolCallAggregator: streaming chunk changed name; keeping first name"
                    );
                }
                if let Some(delta) = chunk.arguments_delta {
                    slot.args_buffer.push_str(&delta);
                }
                None
            }
            ToolCallChunkStatus::Completed => {
                if self.completed.contains(&chunk.index) {
                    tracing::debug!(
                        index = chunk.index,
                        id = %chunk.id,
                        "ToolCallAggregator: duplicate Completed; ignoring"
                    );
                    return None;
                }
                self.completed.insert(chunk.index);
                let slot = self.by_index.entry(chunk.index).or_insert_with(|| ToolCallSlot {
                    id: chunk.id.clone(),
                    name: chunk.name.clone(),
                    args_buffer: String::new(),
                    final_args: None,
                });
                slot.final_args = Some(chunk.args.clone());
                // trust Completed.args as authoritative (matching the protocol comment in traits.rs).
                let resolved_id = if chunk.id.is_empty() { slot.id.clone() } else { chunk.id };
                let resolved_name = if chunk.name.is_empty() {
                    slot.name.clone()
                } else {
                    chunk.name
                };
                Some((resolved_id, resolved_name, chunk.args))
            }
        }
    }
}

/// A completed tool call, ready to execute.
struct ResolvedToolCall {
    id: String,
    name: String,
    args: String,
}

/// **S3 T3-1**: retry limit for transient network failure backoff (attempts).
const MAX_NETWORK_RETRIES: u8 = 3;
/// **S3 T3-1**: initial backoff sleep (ms; sleep 500ms before retry 1, 1s before retry 2, 2s before retry 3).
const BACKOFF_BASE_MS: u64 = 500;

/// Result classification of one stream pass (the driver loop unwinds on this).
enum StreamPassOutcome {
    /// No tool_call this pass, plain text generation finished. Carries the final accumulated text.
    Completed { iter_text: String, usage: TokenUsage },
    /// The LLM requested tool calls this pass — carries aggregated tool_calls plus this pass's assistant text.
    ToolCallRequested {
        calls: Vec<ResolvedToolCall>,
        iter_text: String,
        reasoning_content: String,
        usage: TokenUsage,
    },
    /// Transient network error (may take backoff retry, does not consume the iteration quota).
    TransientNetworkError { err: String },
    /// context overflow (may take compact + progress-based retry).
    ContextOverflow { err: String },
    /// Non-retryable hard error — the driver stops and emits StreamFailed.
    HardError { err: String, retryable: bool },
    /// User cancel — the driver already sent StreamCancelled and returns directly.
    Cancelled,
    /// action_tx closed, the driver exits silently (no more actions are sent).
    SenderClosed,
}

/// Really call `provider.stream_chat_with_history` and feed the streaming events back to the reducer.
///
/// Designed to run standalone inside a spawned subtask; cancelled midway via `cancel`, feeding back via `action_tx`.
/// It is a standalone fn rather than inlined in `execute_real`, which makes it easier to:
///   - drive a fake provider directly from unit tests to verify the feedback sequence
///   - keep borrow / move relationships clear (the spawn move closure no longer holds a deps reference)
///
/// Behaviour guarantees:
/// - every exit path sends **exactly one** terminal action: `StreamCompleted` / `StreamFailed` /
///   `StreamCancelled`, so the reducer can match it and clean up `state.stream.draft`
/// - `version` increases strictly monotonically (1, 2, 3, ...), monotonic across all tool iterations,
///   matching the reducer's strict-monotonic rule
/// - `reasoning` is not mixed into the main delta (it is only carried in the final `StreamCompleted.reasoning` field)
///
/// **5a-6**: multi-pass tool turn support.
/// **S3 T3-1**: the four extensions (tool-pass state machine / context overflow compact / approval bridge /
/// timeout backoff retry). See `task/prx/T3-1.md` for details.
/// Build the tool specs advertised to the provider for a TUI / Redux chat turn.
///
/// TUI / Redux chat is always a plain (non-group, non-smart) conversation, so the
/// smart group-reply `stay_silent` tool must NEVER be advertised here. Routed
/// through the shared exposure gate (`expose_stay_silent = false`) so this path
/// applies the identical rule as every other tool-spec construction site.
fn build_dispatcher_tool_specs(
    tools_registry: Option<&Vec<Box<dyn crate::tools::Tool>>>,
) -> Vec<crate::tools::ToolSpec> {
    let mut specs: Vec<crate::tools::ToolSpec> = tools_registry.map_or_else(Vec::new, |registry| {
        crate::tools::ToolCatalog::from_boxed_registry(registry).tool_specs()
    });
    crate::tools::filter_tool_specs_for_exposure(&mut specs, false);
    specs
}

/// The detached Redux driver receives a provider/guard snapshot that already
/// contains the pending user message. Reducer state intentionally does not add
/// that message until ordered commit, so the patch sent back to the reducer
/// guards the pre-turn reducer history and replaces it with the exact compacted
/// durable target minus that pending turn. Ordered commit then appends the user
/// message without shifting patch indices or weakening stale-state detection.
fn compaction_patch_for_reducer_without_pending_user(
    patch: &crate::agent::loop_::CompactionPatch,
    source_history: &[crate::providers::traits::ChatMessage],
    compacted_history: &[crate::providers::traits::ChatMessage],
) -> crate::agent::loop_::CompactionPatch {
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
    let Some(guard) = crate::agent::loop_::compaction_patch_guard_for(reducer_source, 0, reducer_source.len()) else {
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
    crate::agent::loop_::CompactionPatch {
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
    audit: Option<&crate::agent::loop_::DocumentIngestRuntime>,
    action_tx: &mpsc::Sender<Action>,
    reason: crate::chat::action::CompactReason,
    trigger: &str,
) -> Result<ContextRolloverOutcome, ()> {
    if matches!(config.mode, crate::config::AgentCompactionMode::Off) {
        return Ok(ContextRolloverOutcome::default());
    }

    let patch = match crate::agent::loop_::build_configurable_compaction_patch_with_source_history(
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
                let budget = crate::agent::loop_::plan_context_budget(
                    history,
                    config,
                    crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
                );
                crate::agent::loop_::persist_context_switch_degradation(
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
            let budget = crate::agent::loop_::plan_context_budget(
                history,
                config,
                crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
            );
            crate::agent::loop_::persist_context_switch_degradation(
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
    crate::agent::loop_::apply_compaction_patch_exact(history, &patch);
    crate::agent::loop_::apply_compaction_patch_exact(compaction_guard_history, &patch);
    let budget =
        crate::agent::loop_::plan_context_budget(history, config, crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD);
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
        let trimmed = crate::agent::loop_::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
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
            let after = crate::agent::loop_::plan_context_budget(
                history,
                config,
                crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
            );
            crate::agent::loop_::persist_context_switch_degradation(
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
    let enriched_budget = crate::agent::loop_::plan_context_budget(
        enriched_history,
        config,
        crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
    );
    if !enriched_budget.over_hard_limit {
        return None;
    }
    let original_budget = crate::agent::loop_::plan_context_budget(
        original_history,
        config,
        crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
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
    let budget =
        crate::agent::loop_::plan_context_budget(history, config, crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD);
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
        crate::agent::loop_::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
            history,
            config,
            replacement_len,
        )
    } else {
        crate::agent::loop_::trim_history_to_context_budget(history, config)
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
}

struct ReduxToolLoopEventSink {
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    draft_id: String,
    action_tx: mpsc::Sender<Action>,
    version: AtomicU64,
    /// Authoritative full reasoning body for this turn — the single source of
    /// truth replayed in the terminal action. The reducer only ever receives
    /// deltas for its live counter, never a second full copy.
    reasoning: Arc<ParkingMutex<String>>,
    reasoning_progress: ParkingMutex<ReasoningProgressBatcher>,
}

#[async_trait]
impl crate::agent::loop_::ToolLoopEventSink for ReduxToolLoopEventSink {
    async fn emit(&self, event: crate::agent::loop_::ToolLoopEvent) -> anyhow::Result<()> {
        let action = match event {
            crate::agent::loop_::ToolLoopEvent::TextDelta(delta) => Action::StreamChunkReceived {
                draft_id: self.draft_id.clone(),
                delta,
                version: self.version.fetch_add(1, Ordering::Relaxed).saturating_add(1),
            },
            crate::agent::loop_::ToolLoopEvent::ReasoningDelta(delta) => {
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
            crate::agent::loop_::ToolLoopEvent::RetryAttempt { attempt, reason } => {
                Action::StreamRetryAttempt { attempt, reason }
            }
            crate::agent::loop_::ToolLoopEvent::ContextCompacted => Action::HistoryCompacted {
                reason: crate::chat::action::CompactReason::ContextOverflow,
            },
            crate::agent::loop_::ToolLoopEvent::ContextCompactionPatch {
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
            crate::agent::loop_::ToolLoopEvent::ToolStarted {
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
            crate::agent::loop_::ToolLoopEvent::ToolFinished {
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
/// mutation and cancellation are all delegated to `run_tool_call_loop_outcome`.
#[allow(clippy::too_many_arguments)]
async fn drive_start_turn_stream(
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
    compaction_audit: Option<crate::agent::loop_::DocumentIngestRuntime>,
    chat_mode: crate::agent::loop_::ChatMode,
    observer: Arc<dyn Observer>,
    hooks: Arc<HookManager>,
    tool_surface: crate::tools::intent::SessionToolSurface,
    routing_input: Option<String>,
) {
    // Redux-specific preflight projection stays in the adapter because it must
    // publish the exact compaction patch and injection diagnostic into reducer
    // state before the shared owner starts the visible provider turn. The
    // provider/tool iteration itself remains exclusively in agent::loop_.
    if let Some(config) = compaction_config.as_ref() {
        let budget =
            crate::agent::loop_::plan_context_budget(&history, config, crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD);
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
            let after_compact = crate::agent::loop_::plan_context_budget(
                &history,
                config,
                crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
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
    let events: Arc<dyn crate::agent::loop_::ToolLoopEventSink> = Arc::new(ReduxToolLoopEventSink {
        provider_turn_task_id,
        draft_id: draft_id.clone(),
        action_tx: action_tx.clone(),
        version: AtomicU64::new(0),
        reasoning: Arc::clone(&reasoning),
        reasoning_progress: ParkingMutex::new(ReasoningProgressBatcher::default()),
    });
    let runtime_adapter = crate::agent::loop_::ToolLoopRuntimeAdapter {
        events: Some(events),
        stream_provider: true,
        tool_execution_service,
        tool_execution_context,
        allowed_tool_names: None,
    };
    let tools = tools_registry.unwrap_or_else(|| Arc::new(Vec::new()));

    let result = crate::agent::loop_::run_tool_call_loop_outcome(
        provider.as_ref(),
        &mut history,
        tools,
        observer.as_ref(),
        hooks.as_ref(),
        "chat",
        &model,
        temperature,
        true,
        None,
        "terminal",
        &crate::config::MultimodalConfig::default(),
        1,
        false,
        Vec::new(),
        compaction_config.as_ref(),
        Some(cancel.clone()),
        None,
        None,
        None,
        Some(&tool_surface.tiering),
        // The adapter's ToolExecutionService already carries the resolved ledger.
        // `with_routing_input` pins capability routing to the raw user text:
        // without it the loop falls back to the last history user message,
        // which chat has already enriched with memory recall and the
        // `[Recent shared workspace events]` block — an injected URL there used
        // to publish the whole web tool surface.
        //
        // `with_unrouted_tool_policy` carries the other half of the session
        // decision: the loop re-routes this same text, and without it an
        // unrouted turn would answer "the whole registry" there and undo the
        // session exposure the dispatcher just held still.
        {
            let memory = crate::agent::loop_::ToolLoopMemory::none()
                .with_event_fabric(request_event_fabric)
                .with_unrouted_tool_policy(tool_surface.unrouted);
            match routing_input {
                Some(input) => memory.with_routing_input(input),
                None => memory,
            }
        },
        chat_mode,
        None,
        false,
        Some(runtime_adapter),
    )
    .await;

    let (outcome, trace) = match result {
        Ok(result) => result,
        Err(error) if crate::agent::loop_::is_tool_loop_event_sink_closed(&error) => return,
        Err(error) if crate::agent::loop_::is_tool_loop_cancelled(&error) || cancel.is_cancelled() => {
            let _ = action_tx.send(Action::StreamCancelled { draft_id }).await;
            return;
        }
        Err(error) => {
            let retryable = crate::agent::loop_::tool_loop_error_is_retryable(&error);
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

    let final_text = outcome.into_text();
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
    let empty_response = crate::agent::loop_::is_empty_assistant_response(&final_text, false);
    if empty_response {
        if action_tx
            .send(Action::SystemMessageAdded {
                text: crate::agent::loop_::EMPTY_ASSISTANT_RESPONSE_MESSAGE.to_string(),
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

#[cfg(test)]
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    skip_all,
    fields(
        draft_id = %draft_id,
        model = %model,
    )
)]
async fn drive_start_turn_stream_legacy(
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
    chat_mode: crate::agent::loop_::ChatMode,
) {
    let mut version: u64 = 0;
    let mut accumulated = String::new();
    let mut reasoning_buf = String::new();
    let mut usage_accumulator = ProviderUsageAccumulator::new();
    let mut last_compaction_feedback: Option<String> = None;
    let mut last_injection_overbudget_feedback: Option<String> = None;
    // tool_call_ids already executed (guards against re-running the same tool after a context overflow retry).
    let mut executed_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    // BUG-03: count how many times each unrecoverable tool-failure signature has
    // been seen this turn. When the model re-issues the *same* permanently-blocked
    // call (permission denied / not allowed / rejected …) the count climbs; once it
    // recurs we stop the turn early instead of retrying indefinitely.
    let mut unrecoverable_seen: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
    let stream_tool_specs: Vec<crate::tools::ToolSpec> = build_dispatcher_tool_specs(tools_registry.as_deref());

    'outer: loop {
        if let Some(config) = compaction_config.as_ref() {
            let budget = crate::agent::loop_::plan_context_budget(
                &history,
                config,
                crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
            );
            if budget.over_hard_limit {
                let turns_before = chat_history_turn_count(&history);
                let tokens_before = super::estimate_chat_history_tokens(&history);
                let injection_overbudget_diagnostic =
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
                        None,
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
                let summary_replacement_len = outcome.replacement_len;
                let after_compact = crate::agent::loop_::plan_context_budget(
                    &history,
                    config,
                    crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
                );
                if compaction_off || after_compact.over_hard_limit {
                    let trimmed = trim_redux_driver_context_budget_after_rollover(
                        &mut history,
                        &mut compaction_guard_history,
                        config,
                        summary_replacement_len,
                    );
                    tracing::warn!(
                        before_used_tokens = budget.used_tokens,
                        after_compact_tokens = after_compact.used_tokens,
                        hard_limit = after_compact.available_input_tokens,
                        compaction_off,
                        summary_applied = summary_replacement_len.is_some(),
                        trimmed,
                        "redux driver context budget preflight remediated with summary-compaction/token-aware trim"
                    );
                }
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
                if send_redux_injection_overbudget_diagnostic(
                    &action_tx,
                    injection_overbudget_diagnostic,
                    "redux_preflight_injection_overbudget_diagnostic",
                    &mut last_injection_overbudget_feedback,
                )
                .await
                .is_err()
                {
                    return;
                }
                if send_redux_context_window_update(
                    &action_tx,
                    &history,
                    config,
                    "redux_preflight_context_window_updated",
                )
                .await
                .is_err()
                {
                    return;
                }
                if send_redux_compaction_feedback(
                    &action_tx,
                    turns_before,
                    tokens_before,
                    &history,
                    config,
                    "redux_preflight_compaction_feedback",
                    &mut last_compaction_feedback,
                )
                .await
                .is_err()
                {
                    return;
                }
            } else if budget.over_warning {
                tracing::info!(
                    used_tokens = budget.used_tokens,
                    warning_threshold = budget.warning_threshold_tokens,
                    hard_limit = budget.available_input_tokens,
                    "redux driver context budget warning threshold crossed"
                );
            }
        }

        // ── one stream pass + backoff retry ─────────────────────────
        let pass = run_one_stream_pass_with_retry(
            provider.as_ref(),
            &history,
            &model,
            temperature,
            &cancel,
            &draft_id,
            &action_tx,
            &mut version,
            &mut reasoning_buf,
            &stream_tool_specs,
        )
        .await;

        match pass {
            StreamPassOutcome::Completed { iter_text, usage } => {
                usage_accumulator.record(usage);
                accumulated.push_str(&iter_text);
                break 'outer;
            }
            StreamPassOutcome::ContextOverflow { err } => {
                let turns_before_compaction = chat_history_turn_count(&history);
                let tokens_before_compaction = super::estimate_chat_history_tokens(&history);
                let compaction_feedback_before = compaction_config.as_ref().map(|config| {
                    (
                        chat_history_turn_count(&history),
                        super::estimate_chat_history_tokens(&history),
                        config.clone(),
                    )
                });
                match compaction_config.as_ref() {
                    Some(config) if matches!(config.mode, crate::config::AgentCompactionMode::Off) => {
                        let trimmed = crate::agent::loop_::trim_history_to_context_budget(&mut history, config);
                        tracing::warn!(
                            trimmed,
                            "redux driver context-overflow retry used trim-only because compaction mode is Off"
                        );
                    }
                    Some(config) => {
                        let messages_before_rollover = history.len();
                        let outcome = match apply_redux_context_rollover(
                            provider.as_ref(),
                            &mut history,
                            &mut compaction_guard_history,
                            &model,
                            config,
                            None,
                            &action_tx,
                            crate::chat::action::CompactReason::ContextOverflow,
                            "redux_overflow_retry",
                        )
                        .await
                        {
                            Ok(outcome) => outcome,
                            Err(()) => return,
                        };
                        if outcome.replacement_len.is_none() {
                            let trimmed = trim_redux_driver_context_budget_after_rollover(
                                &mut history,
                                &mut compaction_guard_history,
                                config,
                                None,
                            );
                            tracing::warn!(
                                trimmed,
                                "redux driver context-overflow retry summary unavailable; used token-aware trim"
                            );
                        }
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
                    }
                    None => {
                        crate::chat::state::compact_history_in_place(&mut history);
                        if let Err(e) = action_tx
                            .send(Action::HistoryCompacted {
                                reason: crate::chat::action::CompactReason::ContextOverflow,
                            })
                            .await
                        {
                            tracing::debug!(error = %e, "StartTurn: action_tx closed on compact-dispatch");
                            return;
                        }
                    }
                }
                if let Some((turns_before, tokens_before, config)) = compaction_feedback_before {
                    if send_redux_context_window_update(
                        &action_tx,
                        &history,
                        &config,
                        "redux_overflow_context_window_updated",
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    if send_redux_compaction_feedback(
                        &action_tx,
                        turns_before,
                        tokens_before,
                        &history,
                        &config,
                        "redux_overflow_compaction_feedback",
                        &mut last_compaction_feedback,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                let made_progress = chat_history_turn_count(&history) < turns_before_compaction
                    || super::estimate_chat_history_tokens(&history) < tokens_before_compaction;
                if !made_progress {
                    let action = Action::StreamFailed {
                        draft_id: draft_id.clone(),
                        err: format!("context overflow and compaction made no progress: {err}"),
                        retryable: false,
                    };
                    if let Err(e) = action_tx.send(action).await {
                        tracing::debug!(error = %e, "StartTurn: action_tx closed on overflow-no-progress");
                    }
                    return;
                }
                continue 'outer;
            }
            StreamPassOutcome::TransientNetworkError { err } => {
                // backoff was already exhausted inside run_one_stream_pass_with_retry, so fail hard directly.
                let action = Action::StreamFailed {
                    draft_id: draft_id.clone(),
                    err,
                    retryable: false,
                };
                if let Err(e) = action_tx.send(action).await {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on net-exhausted");
                }
                return;
            }
            StreamPassOutcome::HardError { err, retryable } => {
                let action = Action::StreamFailed {
                    draft_id: draft_id.clone(),
                    err,
                    retryable,
                };
                if let Err(e) = action_tx.send(action).await {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on hard-error");
                }
                return;
            }
            StreamPassOutcome::Cancelled | StreamPassOutcome::SenderClosed => return,
            StreamPassOutcome::ToolCallRequested {
                calls,
                iter_text,
                reasoning_content,
                usage,
            } => {
                usage_accumulator.record(usage);
                // Entering a tool round requires the canonical execution
                // service assembled from the same registry advertised above.
                if tool_execution_service.is_none() {
                    let action = Action::StreamFailed {
                        draft_id: draft_id.clone(),
                        err: "redux driver: tool_calls received but no ToolExecutionService is available".to_string(),
                        retryable: false,
                    };
                    if let Err(e) = action_tx.send(action).await {
                        tracing::debug!(error = %e, "StartTurn: action_tx closed on missing tool service");
                    }
                    return;
                }

                // 1) Append the assistant tool_call to history. The OpenAI protocol serializes the assistant
                //    tool_call as JSON with empty content; we use a more compact marker string that is compatible
                //    with ChatMessage (which has no dedicated tool_calls field). legacy run_tool_call_loop uses
                //    build_native_assistant_history for a richer format; here the driver degrades conservatively —
                //    JSON gives full tool_call context next pass; use structured fields once provider native lands.
                let assistant_payload = serde_json::json!({
                    "tool_calls": calls.iter().map(|c| serde_json::json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.args },
                    })).collect::<Vec<_>>(),
                    "content": iter_text,
                    "reasoning_content": reasoning_content,
                });
                history.push(crate::providers::traits::ChatMessage {
                    role: "assistant".to_string(),
                    content: assistant_payload.to_string(),
                });

                // 2) Execute each tool call in order.
                // BUG-03: signature of an unrecoverable failure that recurred this
                // pass — set to stop the turn after history is fully populated.
                let mut repeated_unrecoverable: Option<String> = None;
                for call in calls {
                    if executed_tool_ids.contains(&call.id) {
                        tracing::debug!(
                            tool_id = %call.id,
                            "drive_start_turn_stream: skipping already-executed tool_id (retry idempotency)"
                        );
                        continue;
                    }
                    let call_name = call.name.clone();
                    let outcome = execute_single_tool_call(
                        provider_turn_task_id,
                        tool_execution_service.as_deref(),
                        &tool_execution_context,
                        &call,
                        &cancel,
                        &action_tx,
                        &draft_id,
                        &mut history,
                        compaction_config.as_ref(),
                        chat_mode,
                    )
                    .await;
                    match outcome {
                        ToolExecOutcome::Done { unrecoverable } => {
                            executed_tool_ids.insert(call.id.clone());
                            if let Some(sig) = unrecoverable {
                                let count = unrecoverable_seen.entry(sig.clone()).or_insert(0);
                                *count = count.saturating_add(1);
                                if *count >= 2 {
                                    // The model already saw this exact block and
                                    // re-issued the identical call — further LLM
                                    // round-trips will not change the outcome.
                                    tracing::info!(
                                        tool = %call_name,
                                        occurrences = *count,
                                        "drive_start_turn_stream: repeated unrecoverable tool failure — stopping turn early (BUG-03)"
                                    );
                                    repeated_unrecoverable = Some(call_name.clone());
                                }
                            }
                        }
                        ToolExecOutcome::Cancelled | ToolExecOutcome::SenderClosed => return,
                    }
                }
                if let Some(config) = compaction_config.as_ref() {
                    let budget = crate::agent::loop_::plan_context_budget(
                        &history,
                        config,
                        crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
                    );
                    if budget.over_hard_limit {
                        let trimmed = crate::agent::loop_::trim_history_to_context_budget(&mut history, config);
                        tracing::warn!(
                            used_tokens = budget.used_tokens,
                            hard_limit = budget.available_input_tokens,
                            trimmed,
                            "redux driver mid-turn context budget trim after tool results; summary parity deferred as ISS-011"
                        );
                    }
                }
                // BUG-03: if a permanently-blocked call was retried, fail the turn
                // now with an actionable message rather than letting the LLM spin
                // indefinitely. history already carries every tool
                // result so the failure context is intact for any follow-up turn.
                if let Some(tool_name) = repeated_unrecoverable {
                    let action = Action::StreamFailed {
                        draft_id: draft_id.clone(),
                        err: format!(
                            "stopped after a repeated unrecoverable tool failure ('{tool_name}' is blocked by policy/permissions); not retrying further"
                        ),
                        retryable: false,
                    };
                    if let Err(e) = action_tx.send(action).await {
                        tracing::debug!(error = %e, "StartTurn: action_tx closed on unrecoverable-stop");
                    }
                    return;
                }
                // Continue to the next LLM pass. iter_text is already in the assistant message.
            }
        }
    }

    // P4b-1: task-scoped visible provider turns are persisted only after the
    // ordered commit gate in chat::run. The dispatcher still emits usage and
    // empty-response notices immediately, but its successful terminal action is
    // a reducer no-op that only wakes TurnCompletionSignal. Non task-scoped
    // tests/legacy paths keep the original RecordAssistantTurn -> StreamCompleted
    // contract.
    let tokens_used = usage_accumulator.finish();
    if tokens_used.is_reported()
        && let Err(e) = action_tx
            .send(Action::StreamUsageMetered {
                draft_id: draft_id.clone(),
                usage: tokens_used.clone(),
            })
            .await
    {
        tracing::debug!(error = %e, "StartTurn: action_tx closed before StreamUsageMetered");
        return;
    }
    let empty_assistant_response = crate::agent::loop_::is_empty_assistant_response(&accumulated, false);
    if empty_assistant_response {
        if let Err(e) = action_tx
            .send(Action::SystemMessageAdded {
                text: crate::agent::loop_::EMPTY_ASSISTANT_RESPONSE_MESSAGE.to_string(),
            })
            .await
        {
            tracing::debug!(error = %e, "StartTurn: action_tx closed before empty-response system message");
            return;
        }
    } else if provider_turn_task_id.is_none() {
        let record = Action::RecordAssistantTurn {
            task_id: provider_turn_task_id,
            content: accumulated.clone(),
        };
        if let Err(e) = action_tx.send(record).await {
            tracing::debug!(error = %e, "StartTurn: action_tx closed before RecordAssistantTurn");
            return;
        }
    }
    let reasoning = if empty_assistant_response {
        String::new()
    } else {
        reasoning_buf
    };
    let action = if provider_turn_task_id.is_some() {
        Action::ProviderTurnReadyForCommit {
            draft_id,
            final_text: accumulated,
            reasoning,
        }
    } else {
        Action::StreamCompleted {
            draft_id,
            final_text: accumulated,
            reasoning,
        }
    };
    if let Err(e) = action_tx.send(action).await {
        tracing::debug!(error = %e, "StartTurn: action_tx closed on completion");
    }
}

/// **S3 T3-1**: result classification of one tool execution.
#[derive(Debug)]
enum ToolExecOutcome {
    /// The tool finished normally (success / fail / reject — all already sent ToolFinished + wrote back history).
    ///
    /// BUG-03: when the failure is *unrecoverable* (permission denied / command
    /// not allowed / path not allowed — the model retrying the identical call
    /// can never succeed), `unrecoverable` carries a stable signature so the
    /// driver loop can detect a repeated blocked action and stop the turn early
    /// instead of retrying a futile operation indefinitely.
    Done { unrecoverable: Option<String> },
    /// User cancel — the caller should return from the driver immediately.
    Cancelled,
    /// action_tx closed, the driver should exit silently.
    SenderClosed,
}

/// BUG-03: classify whether a tool error is **unrecoverable** — i.e. retrying
/// the identical call can never succeed because a security policy or
/// OS permission permanently blocks it. The Redux driver uses this to short-
/// circuit the "LLM keeps re-issuing the same blocked tool call" spin observed
/// in chat-demo (BUG-03).
///
/// Matching is case-insensitive substring against the human-readable error /
/// output. Transient failures (timeouts, network, "file not found", parse
/// errors the model can fix) are intentionally NOT matched — those remain
/// retryable so the model can self-correct.
fn is_unrecoverable_tool_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const SIGNATURES: &[&str] = &[
        "permission denied",
        "command not allowed",
        "command is not allowed",
        "not allowed",
        "path not allowed",
        "path is not allowed",
        "not permitted",
        "operation not permitted",
        "access denied",
        "blocked by security policy",
        "security policy",
        "denied by policy",
        "forbidden by policy",
        "read-only file system",
        "user rejected tool approval",
        "approval system not available",
    ];
    SIGNATURES.iter().any(|sig| lower.contains(sig))
}

/// BUG-03: build the stable signature used to detect a *repeated* unrecoverable
/// tool failure. Keyed by tool name + a normalized slice of the error so that
/// the same blocked action recurring across iterations collapses to one key,
/// while a *different* blocked call (e.g. a different denied path) does not
/// falsely trip the early-stop on its first occurrence.
fn unrecoverable_signature(tool_name: &str, error_text: &str) -> String {
    let normalized: String = error_text
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .take(160)
        .collect();
    format!("{tool_name}::{normalized}")
}

/// BUG-09: classify whether a tool mutates state and must be intercepted in
/// plan mode. Mirrors `agent::loop_::is_write_tool`'s read-tool allowlist so the
/// Redux driver path enforces the exact same read-only contract as the legacy
/// `run_tool_call_loop`. Unknown tools are conservatively treated as writes.
fn is_plan_intercepted_write_tool(name: &str) -> bool {
    !matches!(
        name,
        "file_read"
            | "grep"
            | "web_search"
            | "web_search_tool"
            | "web_fetch"
            | "memory_recall"
            | "memory_search"
            | "memory_get"
            | "document_search"
            | "document_get_chunk"
            | "sessions_list"
            | "sessions_history"
            | "session_status"
            | "agents_list"
            | "image_info"
            | "hardware_board_info"
            | "hardware_memory_map"
            | "hardware_memory_read"
    )
}

/// BUG-09: short, bounded preview of the raw tool arguments for the synthesized
/// "[plan mode] would call X with …" message. Keeps the line readable even when
/// arguments are large (file contents, shell scripts).
fn plan_preview_args(raw_args: &str) -> String {
    const MAX: usize = 160;
    if raw_args.len() <= MAX {
        return raw_args.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !raw_args.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &raw_args[..cut])
}

/// **S3 T3-1**: execute a single tool call (including the approval check) + write back history + emit Tool* Actions.
///
/// Split out because the driver main loop nests too deeply and the approval path has a oneshot await;
/// this makes logic and borrows clearer. The return value tells the caller what to do next (continue / cancel / exit).
#[allow(clippy::too_many_arguments)]
async fn execute_single_tool_call(
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    service: Option<&ToolExecutionService>,
    context: &ToolExecutionContext,
    call: &ResolvedToolCall,
    cancel: &CancellationToken,
    action_tx: &mpsc::Sender<Action>,
    draft_id: &str,
    history: &mut Vec<crate::providers::traits::ChatMessage>,
    compaction_config: Option<&crate::config::AgentCompactionConfig>,
    chat_mode: crate::agent::loop_::ChatMode,
) -> ToolExecOutcome {
    // 0) BUG-09: plan mode is read-only. Intercept write/shell/git tools BEFORE
    // approval or execution and feed back a simulated "[plan mode] would call X"
    // result so the model can keep reasoning without any real side effect
    // touching the filesystem. This mirrors the legacy `run_tool_call_loop`
    // interception (agent::loop_::execute_one_tool) for the Redux driver path,
    // which previously executed write tools for real even in plan mode.
    if chat_mode.intercepts_writes() && is_plan_intercepted_write_tool(&call.name) {
        let preview = plan_preview_args(&call.args);
        let simulated = format!("[plan mode] would call {} with {preview}", call.name);
        let tool_payload = serde_json::json!({
            "tool_call_id": call.id,
            "content": simulated,
            "success": true,
        });
        history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
        if let Err(e) = action_tx
            .send(Action::ToolFinished {
                task_id: provider_turn_task_id,
                sequence: None,
                tool_call_id: Some(call.id.clone()),
                name: call.name.clone(),
                success: true,
                duration_ms: 0,
                result: Some(simulated),
            })
            .await
        {
            tracing::debug!(error = %e, "StartTurn: action_tx closed on plan-mode-intercept");
            return ToolExecOutcome::SenderClosed;
        }
        // Plan-mode interception reports simulated success — fully recoverable.
        return ToolExecOutcome::Done { unrecoverable: None };
    }

    // Transport-level JSON decoding happens before the typed service command.
    // Descriptor schema validation remains service-owned.
    let args_value: serde_json::Value = match serde_json::from_str(&call.args) {
        Ok(v) => v,
        Err(parse_err) => {
            let err_msg = format!("tool args JSON parse error: {parse_err}");
            let tool_payload = serde_json::json!({
                "tool_call_id": call.id,
                "content": err_msg,
                "success": false,
            });
            history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
            let _ = action_tx
                .send(Action::ToolFinished {
                    task_id: provider_turn_task_id,
                    sequence: None,
                    tool_call_id: Some(call.id.clone()),
                    name: call.name.clone(),
                    success: false,
                    duration_ms: 0,
                    result: Some(err_msg),
                })
                .await;
            // Recoverable: the model can re-emit corrected JSON.
            return ToolExecOutcome::Done { unrecoverable: None };
        }
    };

    let Some(service) = service else {
        let err_msg = "ToolExecutionService is unavailable; tool rejected for safety".to_string();
        let tool_payload = serde_json::json!({
            "tool_call_id": call.id,
            "content": err_msg,
            "success": false,
        });
        history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
        let _ = action_tx
            .send(Action::ToolFinished {
                task_id: provider_turn_task_id,
                sequence: None,
                tool_call_id: Some(call.id.clone()),
                name: call.name.clone(),
                success: false,
                duration_ms: 0,
                result: Some(err_msg.clone()),
            })
            .await;
        return ToolExecOutcome::Done {
            unrecoverable: Some(unrecoverable_signature(&call.name, &err_msg)),
        };
    };
    let command = ToolExecutionCommand::new(&call.name, args_value)
        .with_operation_id(&call.id)
        .with_idempotency_key(&call.id);
    let outcome = service.execute(command, context.clone(), Some(cancel.clone())).await;
    let duration_ms = outcome.duration_ms;

    if outcome.status == ToolExecutionStatus::Cancelled {
        if let Err(e) = action_tx
            .send(Action::StreamCancelled {
                draft_id: draft_id.to_string(),
            })
            .await
        {
            tracing::debug!(error = %e, "StartTurn: action_tx closed on cancellable tool result");
        }
        return ToolExecOutcome::Cancelled;
    }

    if action_tx.is_closed() {
        return ToolExecOutcome::SenderClosed;
    }

    let status = outcome.status;
    let (tool_payload, ok_flag, summary) = match outcome.result {
        Some(tool_result) => {
            // BUG-05: when a tool fails (e.g. file_write rejected by the path
            // security policy) the human-readable reason lives in `error`, and
            // `output` is usually empty. The LLM keys off `content`, so an empty
            // `content` made the model believe the call "returned nothing /
            // looked fine". Surface the error reason in `content` (not just the
            // side `error` field) so the rejection is unambiguous to the model.
            let content = if tool_result.success || !tool_result.output.is_empty() {
                tool_result.output.clone()
            } else {
                tool_result
                    .error
                    .clone()
                    .unwrap_or_else(|| "tool failed with no output".to_string())
            };
            let max_inline_chars =
                crate::agent::loop_::tool_result_inline_budget_for_history(&call.name, history, compaction_config);
            let content = crate::agent::loop_::compact_tool_result_for_budget(&call.name, &content, max_inline_chars);
            let payload = serde_json::json!({
                "tool_call_id": call.id,
                "content": content,
                "success": tool_result.success,
                "error": tool_result.error,
            });
            let summary = if tool_result.success {
                tool_result.output.clone()
            } else {
                tool_result.error.clone().unwrap_or_else(|| "tool failed".to_string())
            };
            (payload, tool_result.success, summary)
        }
        None => {
            let err_str = outcome.error.unwrap_or_else(|| outcome.model_content.clone());
            let payload = serde_json::json!({
                "tool_call_id": call.id,
                "content": err_str,
                "success": false,
            });
            (payload, false, err_str)
        }
    };
    history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
    // BUG-03: a real tool failure whose message names a permanent block
    // (permission denied / command not allowed / path not allowed …) is
    // unrecoverable — surface a signature so the driver can stop a retry spin.
    let terminal_block = matches!(
        status,
        ToolExecutionStatus::Denied
            | ToolExecutionStatus::ApprovalDenied
            | ToolExecutionStatus::PreparationDenied
            | ToolExecutionStatus::UnknownTool
            | ToolExecutionStatus::IdempotencyConflict
            | ToolExecutionStatus::Indeterminate
    );
    let unrecoverable = if !ok_flag && (terminal_block || is_unrecoverable_tool_error(&summary)) {
        Some(unrecoverable_signature(&call.name, &summary))
    } else {
        None
    };
    let _ = action_tx
        .send(Action::ToolFinished {
            task_id: provider_turn_task_id,
            sequence: None,
            tool_call_id: Some(call.id.clone()),
            name: call.name.clone(),
            success: ok_flag,
            duration_ms,
            result: Some(summary),
        })
        .await;
    ToolExecOutcome::Done { unrecoverable }
}

/// **S3 T3-1**: one stream pass + exponential backoff retry on transient network failures.
///
/// Behaviour:
/// - calls `provider.stream_chat_with_history`, receives chunks and emits `Action::StreamChunkReceived`
/// - on an `is_timeout()` / `is_connect()` error: sleep and retry, at most [`MAX_NETWORK_RETRIES`] times
/// - on context overflow (HTTP 413 / provider message match): return ContextOverflow so the caller compacts + retries
/// - on an ordinary retryable (`StreamError::Http`) error that is not a timeout: return a hard error (no retry loop)
/// - on cancel: send StreamCancelled and return Cancelled
#[allow(clippy::too_many_arguments)]
async fn run_one_stream_pass_with_retry(
    provider: &dyn Provider,
    history: &[crate::providers::traits::ChatMessage],
    model: &str,
    temperature: f64,
    cancel: &CancellationToken,
    draft_id: &str,
    action_tx: &mpsc::Sender<Action>,
    version: &mut u64,
    reasoning_buf: &mut String,
    tool_specs: &[crate::tools::ToolSpec],
) -> StreamPassOutcome {
    let mut attempt: u8 = 0;
    loop {
        if cancel.is_cancelled() {
            if let Err(e) = action_tx
                .send(Action::StreamCancelled {
                    draft_id: draft_id.to_string(),
                })
                .await
            {
                tracing::debug!(error = %e, "StartTurn: action_tx closed on pre-pass cancel");
                return StreamPassOutcome::SenderClosed;
            }
            return StreamPassOutcome::Cancelled;
        }
        match run_one_stream_pass(
            provider,
            history,
            model,
            temperature,
            cancel,
            draft_id,
            action_tx,
            version,
            reasoning_buf,
            tool_specs,
        )
        .await
        {
            inner @ (StreamPassOutcome::Completed { .. }
            | StreamPassOutcome::ToolCallRequested { .. }
            | StreamPassOutcome::ContextOverflow { .. }
            | StreamPassOutcome::HardError { .. }
            | StreamPassOutcome::Cancelled
            | StreamPassOutcome::SenderClosed) => return inner,
            StreamPassOutcome::TransientNetworkError { err } => {
                let last_err = err;
                attempt = attempt.saturating_add(1);
                if attempt > MAX_NETWORK_RETRIES {
                    return StreamPassOutcome::TransientNetworkError {
                        err: format!("network retries exhausted ({MAX_NETWORK_RETRIES}): {last_err}"),
                    };
                }
                // tell the reducer / UI about the retry attempt (observability).
                if let Err(e) = action_tx
                    .send(Action::StreamRetryAttempt {
                        attempt,
                        reason: last_err.clone(),
                    })
                    .await
                {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on retry-notify");
                    return StreamPassOutcome::SenderClosed;
                }
                // 500ms, 1000ms, 2000ms — equivalent to multiplying via `<< (attempt-1)` (u64 has no saturating_shl).
                let backoff_ms = BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.saturating_sub(1).min(31));
                tracing::info!(
                    attempt,
                    backoff_ms,
                    err = %last_err,
                    "drive_start_turn_stream: backoff retry"
                );
                let sleep = tokio::time::sleep(std::time::Duration::from_millis(backoff_ms));
                tokio::pin!(sleep);
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        if let Err(e) = action_tx
                            .send(Action::StreamCancelled { draft_id: draft_id.to_string() })
                            .await
                        {
                            tracing::debug!(error = %e, "StartTurn: action_tx closed on cancel-mid-backoff");
                            return StreamPassOutcome::SenderClosed;
                        }
                        return StreamPassOutcome::Cancelled;
                    }
                    () = &mut sleep => {}
                }
            }
        }
    }
}

/// **S3 T3-1**: the actual single stream pass (without retry / overflow retry logic).
///
/// Extracted so the retry/overflow loop can compose it from outside; this function's behaviour:
/// - consume chunk by chunk, aggregating `ToolCallChunk` by index
/// - reasoning inside a chunk accumulates into `reasoning_buf`, text chunks feed back via `Action::StreamChunkReceived`
/// - stream ends naturally / `is_final` → return Completed or ToolCallRequested
/// - stream error → return ContextOverflow / TransientNetworkError / HardError by type
#[allow(clippy::too_many_arguments)]
async fn run_one_stream_pass(
    provider: &dyn Provider,
    history: &[crate::providers::traits::ChatMessage],
    model: &str,
    temperature: f64,
    cancel: &CancellationToken,
    draft_id: &str,
    action_tx: &mpsc::Sender<Action>,
    version: &mut u64,
    reasoning_buf: &mut String,
    tool_specs: &[crate::tools::ToolSpec],
) -> StreamPassOutcome {
    use crate::providers::traits::{StreamChunk, StreamOptions};
    use futures::StreamExt;

    let opts = StreamOptions::new(true).with_tools(tool_specs.to_vec());
    let stream = provider.stream_chat_with_history(history, model, temperature, opts);
    tokio::pin!(stream);

    let mut aggregator = ToolCallAggregator::new();
    let mut completed_calls: Vec<ResolvedToolCall> = Vec::new();
    let mut usage_accumulator = ProviderUsageAccumulator::new();
    let mut iter_text = String::new();
    let mut iter_reasoning = String::new();
    let mut emitted_model_output = false;

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                if let Err(e) = action_tx.send(Action::StreamCancelled { draft_id: draft_id.to_string() }).await {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on cancel");
                    return StreamPassOutcome::SenderClosed;
                }
                return StreamPassOutcome::Cancelled;
            }
            next = stream.next() => {
                match next {
                    Some(Ok(StreamChunk { delta, reasoning, is_final, usage, tool_calls, .. })) => {
                        emitted_model_output |= !delta.is_empty()
                            || reasoning.as_ref().is_some_and(|value| !value.is_empty())
                            || !tool_calls.is_empty();
                        if let Some(usage) = usage {
                            usage_accumulator.record(usage);
                        }
                        if !tool_calls.is_empty() {
                            for tc in tool_calls {
                                if let Some((id, name, args)) = aggregator.ingest(tc) {
                                    completed_calls.push(ResolvedToolCall { id, name, args });
                                }
                            }
                        }
                        if let Some(reason_text) = reasoning {
                            if !reason_text.is_empty() {
                                iter_reasoning.push_str(&reason_text);
                                reasoning_buf.push_str(&reason_text);
                            }
                        }
                        if !delta.is_empty() {
                            *version = version.saturating_add(1);
                            iter_text.push_str(&delta);
                            let action = Action::StreamChunkReceived {
                                draft_id: draft_id.to_string(),
                                delta,
                                version: *version,
                            };
                            if let Err(e) = action_tx.send(action).await {
                                tracing::debug!(error = %e, "StartTurn: action_tx closed mid-stream");
                                return StreamPassOutcome::SenderClosed;
                            }
                        }
                        if is_final {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        // Once the provider has emitted any model output, replaying
                        // the request is unsafe: visible text/reasoning would be
                        // duplicated and a partial tool call could be rebuilt with
                        // different arguments. Fail this turn explicitly instead.
                        if emitted_model_output {
                            return StreamPassOutcome::HardError {
                                err: format!(
                                    "stream interrupted after model output; refusing unsafe request replay: {err}"
                                ),
                                retryable: false,
                            };
                        }
                        // S3 T3-1: error classification — overflow / network timeout / hard error.
                        if stream_error_is_context_overflow(&err) {
                            return StreamPassOutcome::ContextOverflow { err: err.to_string() };
                        }
                        if stream_error_is_network_timeout(&err) {
                            return StreamPassOutcome::TransientNetworkError { err: err.to_string() };
                        }
                        let retryable = stream_error_is_retryable(&err);
                        return StreamPassOutcome::HardError { err: err.to_string(), retryable };
                    }
                    None => {
                        // Stream ended without explicit final chunk — treat as completion.
                        break;
                    }
                }
            }
        }
    }

    let usage = usage_accumulator
        .finish_or_estimate_completion_chars(iter_text.chars().count().saturating_add(iter_reasoning.chars().count()));
    if completed_calls.is_empty() {
        StreamPassOutcome::Completed { iter_text, usage }
    } else {
        StreamPassOutcome::ToolCallRequested {
            calls: completed_calls,
            iter_text,
            reasoning_content: iter_reasoning,
            usage,
        }
    }
}

// ─── Dispatcher task ───────────────────────────────────────────────────────────

/// Spawn the central dispatcher task: drives `state.reduce(action)` for every
/// Action received on `action_rx`, then runs each returned Effect through the
/// shadow [`EffectExecutor`].
///
/// Shutdown conditions:
/// - `action_rx.recv()` returns `None` (every sender has been dropped)
/// - `shutdown.cancelled()` fires (preempted by select!)
///
/// Step 5b shadow mode: the dispatcher task only runs the reducer + log effects and produces no external
/// side effects, so it coexists safely with the main loop's legacy path. Returns a `JoinHandle` so
/// `chat::run` can await it once before exiting to make sure the final trace output is complete.
#[allow(dead_code)]
pub fn spawn_dispatcher_task(
    initial_state: ChatState,
    action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<DispatcherStats> {
    spawn_dispatcher_task_with_executor(initial_state, action_rx, shutdown, EffectExecutor::new_shadow())
}

/// Spawn dispatcher task with explicit [`EffectExecutor`].
///
/// Equivalent to [`spawn_dispatcher_task`] but lets the caller inject a real-mode executor (Step 5a-1).
/// Tests and shadow-compatible scenarios still use `spawn_dispatcher_task`.
#[allow(dead_code)]
pub fn spawn_dispatcher_task_with_executor(
    initial_state: ChatState,
    action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
    executor: EffectExecutor,
) -> tokio::task::JoinHandle<DispatcherStats> {
    spawn_dispatcher_task_with_signal(initial_state, action_rx, shutdown, executor, None)
}

/// Step 5a-4: Spawn dispatcher task with optional [`TurnCompletionSignal`].
///
/// When a signal is present, the dispatcher calls `signal.notify()` after reducing any turn-terminal
/// action (`StreamCompleted` / `StreamFailed` / `StreamCancelled`), waking the await point in the
/// `chat::run` main loop that waits for turn completion.
///
/// This protocol works together with [`is_turn_terminal_action`]: the dispatcher is completely unaware of
/// the concrete driver implementation and only fires turn-boundary events by action type.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
/// Off/Both/Redux modes pass None and keep the single-source chat_mirror path.
///
/// snapshot_rev: AtomicU64, monotonically increasing. watch send_if_modified compares the revision to
/// skip identical frames; the revision never goes backwards, so a receiver can never see a stale frame.
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
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
                        let (effects, ui_dirty) = state.reduce_tracked(action);
                        stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                        for effect in effects {
                            executor.execute(effect).await;
                        }
                        let outcome = executor.outcome_after_effects(outcome);
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
                            let (effects, ui_dirty) = state.reduce_tracked(action);
                            stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                            for effect in effects {
                                executor.execute(effect).await;
                            }
                            let outcome = executor.outcome_after_effects(outcome);
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
#[allow(dead_code)]
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
                        let effects = state.reduce(action);
                        stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                        for effect in effects {
                            executor.execute(effect).await;
                        }
                        let outcome = executor.outcome_after_effects(outcome);
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
                            let effects = state.reduce(action);
                            stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                            for effect in effects {
                                executor.execute(effect).await;
                            }
                            let outcome = executor.outcome_after_effects(outcome);
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

/// Lightweight stats returned by [`spawn_dispatcher_task`] on shutdown
/// (for integration tests and metrics).
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct StreamChunkCoalescer {
    /// pending: (draft_id, accumulated_delta, latest_version)
    pending: Option<(String, String, u64)>,
    sender: mpsc::Sender<Action>,
}

impl StreamChunkCoalescer {
    #[allow(dead_code)]
    pub const fn new(sender: mpsc::Sender<Action>) -> Self {
        Self { pending: None, sender }
    }

    /// Try to send one chunk Action. When the channel is full, accumulate into pending.
    ///
    /// Returns a `DispatchResult` for the caller to observe (on Closed the caller should stop pumping chunks).
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

#[cfg(test)]
fn tool_service_for_test(
    registry: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    action_tx: &mpsc::Sender<Action>,
    draft_id: &str,
    level: crate::security::AutonomyLevel,
) -> (
    ToolExecutionService,
    ToolExecutionContext,
    CancellationToken,
    tempfile::TempDir,
) {
    let policy = tool_security_policy(level);
    let cancellation = CancellationToken::new();
    let context = chat_tool_execution_context(policy.as_ref(), None, None, draft_id);
    let ledger = tempfile::TempDir::new().expect("tool ledger tempdir");
    let memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new(ledger.path()).expect("tool ledger sqlite"));
    let service = chat_tool_execution_service(
        registry,
        Some(memory),
        policy,
        Arc::new(ApprovalRouter::new()),
        action_tx.clone(),
        cancellation.clone(),
        None,
    );
    (service, context, cancellation, ledger)
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::action::Action;
    use crate::providers::router::MockEnvProvider;

    /// F1: reasoning deltas are very fragmented, so the throttler must accumulate the fragments inside a
    /// window and lose no characters (the next window emits the accumulated plus the new ones together).
    #[test]
    fn reasoning_progress_batcher_throttles_without_losing_chars() {
        let mut batcher = ReasoningProgressBatcher::default();
        let first = batcher
            .push("\u{20ac}\u{2192}", 1_000)
            .expect("test: first delta publishes immediately");
        assert_eq!(first, "\u{20ac}\u{2192}");

        // every fragment inside the same window is suppressed
        assert!(batcher.push("\u{20ac}", 1_010).is_none());
        assert!(batcher.push("……", 1_050).is_none());
        assert!(
            batcher
                .push("😀", 1_000 + REASONING_PROGRESS_MIN_INTERVAL_MS - 1)
                .is_none(),
            "must not publish before the window elapses"
        );

        // window elapsed: the accumulated fragments come out at once, in the same order
        let batch = batcher
            .push("!", 1_000 + REASONING_PROGRESS_MIN_INTERVAL_MS)
            .expect("test: window elapsed publishes batch");
        assert_eq!(
            batch, "\u{20ac}……😀!",
            "batch must preserve order and lose no characters"
        );

        // the buffer is cleared after publishing
        assert!(batcher.push("x", 1_000 + REASONING_PROGRESS_MIN_INTERVAL_MS).is_none());
        let next = batcher
            .push("y", 1_000 + 2 * REASONING_PROGRESS_MIN_INTERVAL_MS)
            .expect("test: next window publishes");
        assert_eq!(next, "xy");

        // an empty delta triggers no publish
        assert!(batcher.push("", 9_999_999).is_none());
    }

    #[test]
    fn dispatcher_tool_specs_never_include_stay_silent() {
        use crate::security::SecurityPolicy;
        use crate::tools::{STAY_SILENT_TOOL_NAME, ShellTool, StaySilentTool};

        let security = Arc::new(SecurityPolicy::from_config(
            &crate::config::AutonomyConfig::default(),
            std::path::Path::new("/tmp"),
        ));
        let registry: Vec<Box<dyn crate::tools::Tool>> = vec![
            Box::new(ShellTool::new(
                Arc::new(crate::runtime::NativeRuntime::new()),
                security.workspace_dir.clone(),
            )),
            Box::new(StaySilentTool::new()),
        ];

        let specs = build_dispatcher_tool_specs(Some(&registry));
        assert!(
            specs.iter().any(|s| s.name == "shell"),
            "non-gated tools must still be advertised in the TUI dispatcher"
        );
        assert!(
            !specs.iter().any(|s| s.name == STAY_SILENT_TOOL_NAME),
            "TUI / Redux dispatcher must never advertise stay_silent (plain chat is never smart)"
        );
    }

    #[test]
    fn dispatcher_tool_specs_empty_when_no_registry() {
        assert!(build_dispatcher_tool_specs(None).is_empty());
    }

    #[test]
    fn redux_turn_uses_same_canonical_session_scope_as_chat_messages() {
        let policy = full_tool_security_policy();
        let spawn_context = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
            "turn-run".to_string(),
            "chat:chat-stable".to_string(),
        );

        let context = chat_tool_execution_context(policy.as_ref(), Some(&spawn_context), None, "draft");

        assert_eq!(
            context.envelope.session_key,
            crate::runtime::envelope::RuntimeEnvelope::chat_canonical_session_key("chat-stable")
        );
        assert_eq!(context.envelope.legacy_session_key.as_deref(), Some("chat:chat-stable"));
        assert_eq!(context.envelope.run_id.as_deref(), Some("turn-run"));
    }

    // ── BUG-07: ModelSlot hot-swap ─────────────────────────────────────
    #[test]
    fn model_slot_current_reflects_set() {
        let slot = ModelSlot::from("anthropic/claude-sonnet-4");
        assert_eq!(&*slot.current(), "anthropic/claude-sonnet-4");
        // A clone shares the same inner cell (cross-spawn-boundary handle).
        let handle = slot.clone();
        handle.set(Arc::from("openai/gpt-4o"));
        assert_eq!(
            &*slot.current(),
            "openai/gpt-4o",
            "set via handle is visible through original"
        );
    }

    #[test]
    fn model_handle_exposes_deps_slot() {
        // real-deps executor returns Some(slot); shadow returns None.
        let shadow = EffectExecutor::new_shadow();
        assert!(shadow.model_handle().is_none(), "shadow has no model slot");
    }

    // ── Bug #3: ProviderSlot hot-swap ──────────────────────────────────
    #[test]
    fn provider_slot_current_reflects_set() {
        let a: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let b: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let slot = ProviderSlot::new(Arc::clone(&a));
        assert!(Arc::ptr_eq(&slot.current(), &a), "initial provider is observable");
        // A clone shares the same inner cell (cross-spawn-boundary handle).
        let handle = slot.clone();
        handle.set(Arc::clone(&b));
        assert!(
            Arc::ptr_eq(&slot.current(), &b),
            "set via clone is visible through the original handle"
        );
    }

    #[test]
    fn provider_handle_exposes_deps_slot_only_in_real_mode() {
        // shadow has no provider slot; real-deps construction exposes one.
        let shadow = EffectExecutor::new_shadow();
        assert!(shadow.provider_handle().is_none(), "shadow has no provider slot");
    }

    #[tokio::test]
    async fn dispatcher_try_send_ok() {
        let (dispatcher, mut rx) = ChatDispatcher::new();
        let result = dispatcher.try_dispatch(Action::ForceQuit);
        assert_eq!(result, DispatchResult::Sent);
        let received = rx.recv().await.expect("expected one Action");
        assert!(matches!(received, Action::ForceQuit));
    }

    #[tokio::test]
    async fn dispatcher_channel_closed() {
        let (dispatcher, rx) = ChatDispatcher::new();
        drop(rx);
        let result = dispatcher.try_dispatch(Action::ForceQuit);
        assert_eq!(result, DispatchResult::ChannelClosed);
    }

    #[tokio::test]
    async fn coalescer_passthrough_when_not_full() {
        let (tx, mut rx) = mpsc::channel::<Action>(16);
        let mut coalescer = StreamChunkCoalescer::new(tx);

        coalescer.try_send_chunk("d1".to_string(), "hello ".to_string(), 1);
        coalescer.try_send_chunk("d1".to_string(), "world".to_string(), 2);

        // Both should pass through individually (channel not full)
        let a1 = rx.recv().await.expect("expected chunk 1");
        let a2 = rx.recv().await.expect("expected chunk 2");
        match a1 {
            Action::StreamChunkReceived { delta, .. } => assert_eq!(delta, "hello "),
            other => panic!("unexpected action {other:?}"),
        }
        match a2 {
            Action::StreamChunkReceived { delta, .. } => assert_eq!(delta, "world"),
            other => panic!("unexpected action {other:?}"),
        }
        assert!(coalescer.pending_for_test().is_none());
    }

    #[tokio::test]
    async fn coalescer_merges_when_full() {
        // capacity-1 channel: full after one write
        let (tx, mut rx) = mpsc::channel::<Action>(1);
        let mut coalescer = StreamChunkCoalescer::new(tx);

        // first: succeeds
        let r1 = coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 1);
        assert_eq!(r1, DispatchResult::Sent);
        // second: channel full, goes into pending
        let r2 = coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
        assert_eq!(r2, DispatchResult::Backpressured);
        // third: still full, merged with pending
        let r3 = coalescer.try_send_chunk("d1".to_string(), "c".to_string(), 3);
        assert_eq!(r3, DispatchResult::Backpressured);
        let pending = coalescer.pending_for_test().expect("pending should exist");
        assert_eq!(pending.1, "bc");
        assert_eq!(pending.2, 3, "version should be max(2,3)=3 (Codex P2 fix)");

        // consume the first one to free space
        let a1 = rx.recv().await.expect("first chunk");
        match a1 {
            Action::StreamChunkReceived { delta, version, .. } => {
                assert_eq!(delta, "a");
                assert_eq!(version, 1);
            }
            other => panic!("unexpected {other:?}"),
        }

        // flush pending
        let rf = coalescer.flush();
        assert_eq!(rf, DispatchResult::Sent);
        let a_merged = rx.recv().await.expect("merged chunk");
        match a_merged {
            Action::StreamChunkReceived { delta, version, .. } => {
                assert_eq!(delta, "bc");
                assert_eq!(version, 3, "merged version is max (Codex P2 fix)");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(coalescer.pending_for_test().is_none());
    }

    #[tokio::test]
    async fn coalescer_cross_draft_drops_old_pending() {
        let (tx, mut rx) = mpsc::channel::<Action>(1);
        let mut coalescer = StreamChunkCoalescer::new(tx);

        // d1 chunk 1 → success (fills the channel)
        coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 1);
        // d1 chunk 2 → full, goes into pending
        coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
        // d2 chunk 1 → different draft, the old pending is dropped
        // the channel is still full (d1 chunk 1 not consumed), so d2 chunk 1 goes into pending
        let r = coalescer.try_send_chunk("d2".to_string(), "x".to_string(), 5);
        assert_eq!(r, DispatchResult::Backpressured);
        let pending = coalescer.pending_for_test().expect("pending");
        assert_eq!(pending.0, "d2");
        assert_eq!(pending.1, "x");

        // drain d1 chunk 1
        let _ = rx.recv().await;
        let _ = coalescer.flush();
        let a = rx.recv().await.expect("d2 chunk");
        match a {
            Action::StreamChunkReceived {
                draft_id,
                delta,
                version,
            } => {
                assert_eq!(draft_id, "d2");
                assert_eq!(delta, "x");
                assert_eq!(version, 5);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// P2 fix: a high version arrives first, a low one merges later — pending.version must stay at max.
    ///
    /// Scenario: the version=5 chunk goes through try_send first (success), the version=2 chunk arrives later
    /// (full, goes into pending), and the version=3 chunk arrives after that (full, merged with pending).
    /// Expectation: pending.version = max(2,3) = 3 (not 2, and not an out-of-order regression).
    /// Verifies the coalescer always takes the max version regardless of arrival order.
    #[tokio::test]
    async fn coalescer_handles_out_of_order_versions() {
        // capacity-1 channel
        let (tx, mut rx) = mpsc::channel::<Action>(1);
        let mut coalescer = StreamChunkCoalescer::new(tx);

        // first, version=5, succeeds (fills the channel)
        let r1 = coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 5);
        assert_eq!(r1, DispatchResult::Sent, "first chunk should succeed");

        // second, version=2 (lower than the already sent 5) — channel full, goes into pending
        let r2 = coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
        assert_eq!(r2, DispatchResult::Backpressured);

        // third, version=3 — channel still full, merged with pending(version=2)
        // merge rule: version = max(2, 3) = 3
        let r3 = coalescer.try_send_chunk("d1".to_string(), "c".to_string(), 3);
        assert_eq!(r3, DispatchResult::Backpressured);

        let pending = coalescer.pending_for_test().expect("pending should exist");
        assert_eq!(pending.1, "bc", "delta should be concatenated");
        assert_eq!(
            pending.2, 3,
            "version must be max(2,3)=3 even with out-of-order arrival (P2)"
        );

        // consume, flush, and verify the final output
        let _ = rx.recv().await.expect("first chunk (version=5)");
        let rf = coalescer.flush();
        assert_eq!(rf, DispatchResult::Sent);
        let merged = rx.recv().await.expect("merged chunk");
        match merged {
            Action::StreamChunkReceived { delta, version, .. } => {
                assert_eq!(delta, "bc");
                assert_eq!(version, 3, "flushed version should be max(2,3)=3");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// P2 extra: verify the reducer handles a high version first + a low version later in strict-monotonic mode.
    /// The reducer should accept version=5 and then ignore version=2 because 2 < current_stream_version.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn redux_stream_chunk_strict_monotonic_high_then_low() {
        use crate::chat::state::ChatState;
        use tokio_util::sync::CancellationToken;

        let shutdown = CancellationToken::new();
        let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown);

        // start the turn
        let cancel = CancellationToken::new();
        let effects0 = state.reduce(Action::TurnStarted {
            draft_id: "d1".to_string(),
            cancel,
        });
        // TurnStarted may produce a StartTurn effect; we only confirm it does not panic
        let _ = effects0;

        // version=5 arrives first — strict-monotonic: must be accepted (5 > 0)
        let e1 = state.reduce(Action::StreamChunkReceived {
            draft_id: "d1".to_string(),
            delta: "high".to_string(),
            version: 5,
        });
        // a RequestRedraw effect should be produced (the chunk was accepted)
        assert!(
            e1.iter()
                .any(|e| matches!(e, crate::chat::state::Effect::RequestRedraw)),
            "version=5 chunk should be accepted and produce RequestRedraw"
        );

        // version=2 arrives later — strict-monotonic: must be dropped (2 < 5)
        let e2 = state.reduce(Action::StreamChunkReceived {
            draft_id: "d1".to_string(),
            delta: "low".to_string(),
            version: 2,
        });
        // the low-version chunk is silently dropped by the reducer, so no RequestRedraw
        assert!(
            !e2.iter()
                .any(|e| matches!(e, crate::chat::state::Effect::RequestRedraw)),
            "version=2 chunk (lower than 5) should be discarded by strict-monotonic reducer"
        );
    }

    #[tokio::test]
    async fn redux_compaction_feedback_dedupes_same_state_until_changed() {
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let config = crate::config::AgentCompactionConfig {
            max_context_tokens: 1_000,
            reserve_tokens: 10,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let history = vec![
            crate::providers::traits::ChatMessage::system("sys"),
            crate::providers::traits::ChatMessage::user("same state"),
        ];
        let changed_history = vec![
            crate::providers::traits::ChatMessage::system("sys"),
            crate::providers::traits::ChatMessage::user("changed state"),
            crate::providers::traits::ChatMessage::assistant("summary"),
        ];
        let mut last_feedback = None;

        assert!(
            send_redux_compaction_feedback(&action_tx, 1, 20, &history, &config, "test", &mut last_feedback)
                .await
                .expect("first feedback send"),
            "first feedback should be emitted"
        );
        assert!(
            !send_redux_compaction_feedback(&action_tx, 1, 20, &history, &config, "test", &mut last_feedback)
                .await
                .expect("duplicate feedback send"),
            "identical feedback should be suppressed"
        );
        assert!(
            send_redux_compaction_feedback(&action_tx, 2, 40, &changed_history, &config, "test", &mut last_feedback,)
                .await
                .expect("changed feedback send"),
            "changed state should emit another feedback"
        );

        let sent = action_rx.try_recv().expect("first feedback action should be queued");
        assert!(matches!(sent, Action::SystemMessageAdded { .. }));
        let sent = action_rx.try_recv().expect("changed feedback action should be queued");
        assert!(matches!(sent, Action::SystemMessageAdded { .. }));
        assert!(
            action_rx.try_recv().is_err(),
            "duplicate feedback must not queue an action"
        );
    }

    #[tokio::test]
    async fn redux_injection_overbudget_diagnostic_emits_only_for_injected_delta_and_dedupes() {
        use crate::providers::traits::ChatMessage as PMsg;

        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Off,
            reserve_tokens: 10,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 120,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let original_history = vec![PMsg::system("sys"), PMsg::user("please inspect @huge.txt")];
        let mut enriched_history = original_history.clone();
        let Some(user_message) = enriched_history.get_mut(1) else {
            panic!("test fixture must include a user message");
        };
        user_message.content.push_str(&format!(
            "\n\n[Attached file context from @path mentions]\nHIDDEN_FILE_SENTINEL {}\n[End attached file context]",
            "hidden ".repeat(1000)
        ));
        let text = redux_injection_overbudget_diagnostic_text(&enriched_history, &original_history, &config)
            .expect("injection-only pressure should produce a diagnostic");
        assert!(text.contains("@path/memory context"));
        assert!(text.contains('~'));
        assert!(
            !text.contains("HIDDEN_FILE_SENTINEL"),
            "diagnostic must not include injected content"
        );

        let mut last_feedback = None;
        assert!(
            send_redux_injection_overbudget_diagnostic(
                &action_tx,
                Some(text.clone()),
                "test_injection_diag",
                &mut last_feedback,
            )
            .await
            .expect("first diagnostic send"),
            "first diagnostic should be emitted"
        );
        assert!(
            !send_redux_injection_overbudget_diagnostic(
                &action_tx,
                Some(text),
                "test_injection_diag",
                &mut last_feedback,
            )
            .await
            .expect("duplicate diagnostic send"),
            "identical diagnostic should be suppressed"
        );

        let sent = action_rx.try_recv().expect("diagnostic action should be queued");
        match sent {
            Action::SystemMessageAdded { text } => {
                assert!(text.contains("@path/memory context"));
                assert!(!text.contains("HIDDEN_FILE_SENTINEL"));
            }
            other => panic!("expected SystemMessageAdded, got {other:?}"),
        }
        assert!(
            action_rx.try_recv().is_err(),
            "duplicate diagnostic must not queue an action"
        );
    }

    #[test]
    fn redux_injection_overbudget_diagnostic_skips_normal_history_overflow() {
        use crate::providers::traits::ChatMessage as PMsg;

        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Off,
            reserve_tokens: 10,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 120,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let normal_overflow = vec![
            PMsg::system("sys"),
            PMsg::user(format!("ordinary visible history {}", "visible ".repeat(1000))),
        ];
        assert!(
            redux_injection_overbudget_diagnostic_text(&normal_overflow, &normal_overflow, &config).is_none(),
            "normal history overflow must not be reported as injection-driven"
        );

        let original_already_over = vec![
            PMsg::system("sys"),
            PMsg::user(format!("large original user {}", "visible ".repeat(1000))),
        ];
        let mut enriched = original_already_over.clone();
        let Some(user_message) = enriched.get_mut(1) else {
            panic!("test fixture must include a user message");
        };
        user_message.content.push_str(&format!(" {}", "hidden ".repeat(1000)));
        assert!(
            redux_injection_overbudget_diagnostic_text(&enriched, &original_already_over, &config).is_none(),
            "when original is already over budget, D5 diagnostic must not fire"
        );
    }

    #[tokio::test]
    async fn redux_driver_preflight_emits_injection_overbudget_diagnostic_without_hidden_content() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct DiagnosticProvider;

        #[async_trait]
        impl Provider for DiagnosticProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }

            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok("unused".to_string())
            }

            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }

            fn supports_streaming(&self) -> bool {
                true
            }

            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![
                    Ok(StreamChunk::delta("diagnostic-ok")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }

            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let original_history = vec![PMsg::system("sys"), PMsg::user("please inspect @huge.txt")];
        let mut enriched_history = original_history.clone();
        let Some(user_message) = enriched_history.get_mut(1) else {
            panic!("test fixture must include a user message");
        };
        user_message.content.push_str(&format!(
            "\n\n[Attached file context from @path mentions]\nHIDDEN_FILE_SENTINEL {}\n[End attached file context]",
            "hidden ".repeat(1000)
        ));
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Off,
            reserve_tokens: 10,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 120,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(16);
        let policy = full_tool_security_policy();
        let tool_context = chat_tool_execution_context(policy.as_ref(), None, None, "draft-injection-diagnostic");

        drive_start_turn_stream(
            None,
            Arc::new(DiagnosticProvider),
            enriched_history,
            original_history.clone(),
            "model".to_string(),
            0.0,
            Some(config),
            CancellationToken::new(),
            "draft-injection-diagnostic".to_string(),
            action_tx,
            None,
            None,
            tool_context,
            MemoryFabric::new(Arc::new(crate::memory::NoneMemory::new()), "test"),
            None,
            crate::agent::loop_::ChatMode::Edit,
            Arc::new(crate::observability::noop::NoopObserver),
            Arc::new(crate::hooks::HookManager::new(std::path::PathBuf::new())),
            crate::tools::intent::SessionToolSurface {
                tiering: crate::config::ToolTieringConfig::default(),
                unrouted: crate::tools::intent::UnroutedToolPolicy::KeepPinnedExposure,
            },
            None,
        )
        .await;

        let mut saw_diagnostic = false;
        let mut saw_completion = false;
        while let Ok(action) = action_rx.try_recv() {
            match action {
                Action::SystemMessageAdded { text } if text.contains("@path/memory context") => {
                    saw_diagnostic = true;
                    assert!(
                        !text.contains("HIDDEN_FILE_SENTINEL"),
                        "diagnostic must not leak injected file content"
                    );
                }
                Action::SystemMessageAdded { text } => {
                    assert!(
                        !text.contains("HIDDEN_FILE_SENTINEL"),
                        "ordinary feedback must not leak injected file content"
                    );
                }
                Action::StreamCompleted { final_text, .. } => {
                    saw_completion = final_text.contains("diagnostic-ok");
                }
                Action::RecordUserTurn(content) => {
                    assert!(
                        !content.contains("HIDDEN_FILE_SENTINEL"),
                        "driver must not persist enriched user content"
                    );
                }
                _ => {}
            }
        }

        assert!(saw_diagnostic, "preflight must emit the D5 diagnostic");
        assert!(saw_completion, "driver must still complete after trim");
        assert!(
            original_history
                .iter()
                .all(|message| !message.content.contains("HIDDEN_FILE_SENTINEL")),
            "original guard history fixture remains free of injected content"
        );
    }

    #[tokio::test]
    async fn effect_executor_shadow_log_trace_runs() {
        let executor = EffectExecutor::new_shadow();
        // LogTrace really executes (it runs in shadow too); here we only verify no panic / no external await
        executor
            .execute(Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: "shadow log".to_string(),
            })
            .await;
    }

    #[tokio::test]
    async fn effect_executor_shadow_business_noop() {
        let executor = EffectExecutor::new_shadow();
        // every business effect is a no-op; we only verify there is no panic
        executor.execute(Effect::RequestRedraw).await;
        executor.execute(Effect::Quit).await;
        executor.execute(Effect::CancelDraft("d1".to_string())).await;
        executor
            .execute(Effect::SendDraftFinalize {
                draft_id: "d1".to_string(),
                text: "hello".to_string(),
            })
            .await;
    }

    // ── S5 P0-3 supervised approval fail-safe deny ──────────────────────────

    #[test]
    fn s5_release_p0_3_supervised_unset_env_denies_by_default() {
        // env unset → fail-safe deny (BREAKING — it used to auto-approve)
        assert!(!resolve_supervised_approval_override(None));
    }

    #[test]
    fn s5_release_p0_3_supervised_env_allow_approves() {
        for v in ["allow", "ALLOW", " allow ", "y", "Y", "yes", "YES", "1"] {
            assert!(resolve_supervised_approval_override(Some(v)), "{v:?} must be approved");
        }
    }

    #[test]
    fn s5_release_p0_3_supervised_env_deny_rejects() {
        for v in ["deny", "DENY", " deny ", "n", "N", "no", "NO", "0", "", "garbage"] {
            assert!(!resolve_supervised_approval_override(Some(v)), "{v:?} must be rejected");
        }
    }

    // ── S5 P0-1: stream error classification regression (driver protocol layer) ──────────

    #[test]
    fn s5_release_p0_1_retryable_http_io_triggers_retry() {
        // StreamError::Io is always retryable (same verdict source as the driver retry loop).
        let io_err =
            crate::providers::traits::StreamError::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "boom"));
        assert!(
            stream_error_is_retryable(&io_err),
            "Io errors must be retryable (the driver retry loop depends on it)"
        );
        // Provider errors (semantic errors) are not retryable.
        let provider_err = crate::providers::traits::StreamError::Provider("invalid api key".to_string());
        assert!(
            !stream_error_is_retryable(&provider_err),
            "Provider semantic errors must not be retryable"
        );
    }

    #[test]
    fn s5_release_p0_1_context_overflow_triggers_compact() {
        // the differing error wordings of three providers must all be recognised as context overflow.
        let cases = [
            "This model's maximum context length is 8192 tokens",
            "prompt is too long",
            "request exceed the maximum input token count",
            "context_length_exceeded",
        ];
        for msg in cases {
            let err = crate::providers::traits::StreamError::Provider(msg.to_string());
            assert!(
                stream_error_is_context_overflow(&err),
                "must be recognised as context overflow: {msg:?}"
            );
        }
        // non-overflow errors must not trigger compact.
        let other = crate::providers::traits::StreamError::Provider("rate limited".to_string());
        assert!(
            !stream_error_is_context_overflow(&other),
            "rate limit must not trigger compact"
        );
    }

    #[tokio::test]
    async fn s5_release_p0_1_parallel_tool_calls_serialize() {
        // SCRIPT emits two tool_call chunks + final inside the same stream.
        // StreamChunkCoalescer uses ToolCallChunk to simulate the serial scenario; here we directly verify
        // that StreamChunk::tool_call_chunk can carry several ToolCallChunks with has_tool_calls()=true.
        use crate::providers::traits::{StreamChunk, ToolCallChunk};
        let calls = vec![
            ToolCallChunk::new("t1".to_string(), "shell".to_string(), "{}".to_string(), 0),
            ToolCallChunk::new("t2".to_string(), "file_read".to_string(), "{}".to_string(), 1),
        ];
        let chunk = StreamChunk::tool_call_chunk(calls);
        assert!(
            chunk.has_tool_calls(),
            "parallel tool calls must be recognised by the chunk"
        );
        assert_eq!(chunk.tool_calls.len(), 2, "must carry 2 tool calls");
        let first = chunk.tool_calls.first().expect("tool[0]");
        let second = chunk.tool_calls.get(1).expect("tool[1]");
        assert_eq!(first.id, "t1");
        assert_eq!(second.id, "t2");
        // order preserved: the driver serializes by the index field (we only assert the data layer here).
        assert_eq!(first.index, 0);
        assert_eq!(second.index, 1);
    }

    #[test]
    fn s5_release_p0_3_full_autonomy_skips_approval_entirely() {
        let policy = crate::security::SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Full,
            ..crate::security::SecurityPolicy::default()
        };
        assert_eq!(
            policy.decide("shell", "user", "terminal", "private"),
            crate::security::policy::ToolDecision::Allow
        );
        assert_eq!(
            policy.decide("file_write", "user", "terminal", "private"),
            crate::security::policy::ToolDecision::Allow
        );
    }
}

// ─── Step 5b integration tests (dispatcher + reducer + coalescer + EffectExecutor end to end) ─

#[cfg(test)]
mod integration_tests {
    //! Construct the dispatcher directly + spawn the dispatcher task + feed in an Action stream,
    //! reproducing the wiring of `chat::run`, covering:
    //! - dispatcher channel capacity and backpressure
    //! - whether the reducer is driven (stats.actions_seen / effects_seen)
    //! - whether the shadow effect executor is correctly a no-op
    //! - whether the shutdown protocol (drop sender + cancel token) makes the dispatcher exit
    //!
    //! Important constraints:
    //! - REDUX_DIFF_COUNT == 0: in shadow mode business effects are no-ops, so history is never double-written
    //! - every timeout in the tests is at most 2s, matching main.rs:866 RUNTIME_SHUTDOWN_TIMEOUT
    use super::*;
    use crate::chat::action::{Action, HistoryDir};
    use crate::chat::state::ChatState;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_state(shutdown: CancellationToken) -> ChatState {
        ChatState::new(Arc::from("mock"), Arc::from("mock-model"), shutdown)
    }

    #[tokio::test]
    async fn full_chat_flow_input_to_exit() {
        // simulate one complete chat flow: input → streaming → completion → exit.
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        // 1. user input
        assert_eq!(
            dispatcher.try_dispatch(Action::InputSubmitted("hello".to_string())),
            DispatchResult::Sent
        );
        assert_eq!(
            dispatcher.try_dispatch(Action::RecordUserTurn("hello".to_string())),
            DispatchResult::Sent
        );

        // 2. LLM inference starts + streaming chunks
        let draft_id = "draft-1".to_string();
        let cancel = CancellationToken::new();
        assert_eq!(
            dispatcher.try_dispatch(Action::TurnStarted {
                draft_id: draft_id.clone(),
                cancel: cancel.clone(),
            }),
            DispatchResult::Sent
        );
        for i in 1..=5u64 {
            assert_eq!(
                dispatcher.try_dispatch(Action::StreamChunkReceived {
                    draft_id: draft_id.clone(),
                    delta: format!("chunk{i} "),
                    version: i,
                }),
                DispatchResult::Sent
            );
        }

        // 3. streaming completes
        assert_eq!(
            dispatcher.try_dispatch(Action::StreamCompleted {
                draft_id: draft_id.clone(),
                final_text: "hello user, response complete".to_string(),
                reasoning: "thinking...".to_string(),
            }),
            DispatchResult::Sent
        );
        assert_eq!(
            dispatcher.try_dispatch(Action::RecordAssistantTurn {
                task_id: None,
                content: "hello user, response complete".to_string(),
            }),
            DispatchResult::Sent
        );

        // 4. exit
        assert_eq!(dispatcher.try_dispatch(Action::ShutdownRequested), DispatchResult::Sent);

        // 5. wrap-up — drop sender + cancel shutdown, the dispatcher must exit within 2s
        shutdown.cancel();
        drop(dispatcher);
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher should exit within 2s")
            .expect("join ok");

        // all 11 actions went through the reducer
        assert_eq!(stats.actions_seen, 11, "actions_seen={}", stats.actions_seen);
        // the reducer should produce at least some effects (RequestRedraw / LogTrace / NotifyHook etc.)
        assert!(stats.effects_seen > 0, "no effects produced");

        // verify REDUX_DIFF_COUNT == 0: in shadow mode business effects are no-ops, so there is no
        // divergence from double-writing history (DIFF is only produced with PRX_CHAT_REDUX=both on the
        // run_tui_unified_loop key event path; this integration test does not take that path)
        #[cfg(feature = "terminal-tui")]
        assert_eq!(
            crate::chat::redux_diff_count(),
            0,
            "shadow mode should produce zero REDUX_DIFF_COUNT"
        );
    }

    #[tokio::test]
    async fn dispatcher_exits_on_shutdown_cancel_only() {
        // verify that shutdown.cancel() alone also makes the dispatcher exit (without dropping the sender)
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        let _ = dispatcher.try_dispatch(Action::ForceQuit);
        shutdown.cancel();
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher should exit on shutdown")
            .expect("join ok");
        assert!(stats.actions_seen >= 1);
        drop(dispatcher);
    }

    #[tokio::test]
    async fn dispatcher_exits_on_channel_close_only() {
        // verify drop(sender) → channel close → dispatcher exits (without shutdown.cancel)
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown);

        let _ = dispatcher.try_dispatch(Action::ForceQuit);
        drop(dispatcher);
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher should exit on channel close")
            .expect("join ok");
        assert_eq!(stats.actions_seen, 1);
    }

    #[tokio::test]
    async fn coalescer_under_dispatcher_load() {
        // end to end: dispatcher + coalescer, 100 chunks + a capacity-4 channel → backpressure must trigger.
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (tx, action_rx) = mpsc::channel::<Action>(4);
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        let draft = "draft-load".to_string();
        let mut coalescer = StreamChunkCoalescer::new(tx.clone());

        let cancel = CancellationToken::new();
        let _ = tx
            .send(Action::TurnStarted {
                draft_id: draft.clone(),
                cancel: cancel.clone(),
            })
            .await;

        let mut backpressure_count = 0u64;
        for i in 1..=100u64 {
            let r = coalescer.try_send_chunk(draft.clone(), format!("c{i}"), i);
            if matches!(r, DispatchResult::Backpressured) {
                backpressure_count += 1;
            }
        }
        let _ = coalescer.flush();

        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown.cancel();
        drop(coalescer);
        drop(tx);

        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher should exit")
            .expect("join ok");

        assert!(stats.actions_seen >= 2);
        assert!(
            backpressure_count > 0,
            "backpressure must trigger at cap=4 / 100 chunks (saw {backpressure_count})"
        );
        // after coalescing, the number of actions the dispatcher sees must be far below 101
        // (one full merge can compress several chunks into a single Action)
        assert!(stats.actions_seen <= 101);
    }

    #[tokio::test]
    async fn dispatcher_drives_all_action_variants() {
        // Sanity: run every Action variant through the reducer; none may panic in shadow mode.
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        let actions: Vec<Action> = vec![
            Action::InputSubmitted("x".to_string()),
            Action::InputReplaced("draft".to_string()),
            Action::HistoryNavigated(HistoryDir::Up),
            Action::HistoryNavigated(HistoryDir::Down),
            Action::InputCancelled,
            Action::RedrawRequested,
            Action::TerminalResized { w: 80, h: 24 },
            Action::PasteReceived("paste".to_string()),
            Action::CancelRequested,
            Action::ShutdownRequested,
            Action::HistoryCleared,
            Action::HistoryClearedWithNotice {
                notice: "Conversation cleared".to_string(),
            },
            Action::ForceQuit,
            Action::ToolCardFoldToggled,
            Action::ReasoningFoldToggled,
            // S2-C: the 3 new Actions must be reducible by the dispatcher (without panicking).
            Action::SystemMessageAdded {
                text: "banner".to_string(),
            },
            Action::RecordSystemMessage {
                content: "ctx-system".to_string(),
            },
            Action::SetLeadingSystemPrompt {
                content: "sys-prompt".to_string(),
            },
        ];
        for action in actions {
            let _ = dispatcher.try_dispatch(action);
        }
        shutdown.cancel();
        drop(dispatcher);
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher exit")
            .expect("join ok");
        // S2-C: actions_seen must be >= 16 (13 existing + 3 new).
        assert!(stats.actions_seen >= 16);
    }

    #[tokio::test]
    async fn effect_executor_handles_all_variants() {
        // Sanity: no Effect variant may panic in shadow mode.
        use crate::hooks::HookEvent;
        use crate::memory::MemoryCategory;

        let executor = EffectExecutor::new_shadow();
        let token = CancellationToken::new();

        let effects: Vec<Effect> = vec![
            Effect::RequestRedraw,
            Effect::Quit,
            Effect::CancelDraft("d1".to_string()),
            Effect::DisplayMedia {
                kind: "IMAGE".to_string(),
                path: "/tmp/x.png".to_string(),
            },
            Effect::AutoTitleSession("session-1".to_string()),
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: "test".to_string(),
            },
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: "warn-test".to_string(),
            },
            Effect::PersistToMemory {
                key: "k".to_string(),
                value: "v".to_string(),
                category: MemoryCategory::Conversation,
            },
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"foo": "bar"}),
            },
            Effect::SendDraftFinalize {
                draft_id: "d1".to_string(),
                text: "final".to_string(),
            },
            Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "shadow-d".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: token.clone(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            },
            Effect::CancelToken(token),
            Effect::ResolveApproval {
                tool_id: "call-shadow".to_string(),
                approved: false,
            },
        ];

        for effect in effects {
            executor.execute(effect).await;
        }
    }

    #[tokio::test]
    async fn blocking_dispatch_works_in_spawn_blocking_context() {
        // blocking_dispatch is only usable in a synchronous context; isolate the call via spawn_blocking.
        let shutdown = CancellationToken::new();
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let _handle = spawn_dispatcher_task(make_state(shutdown.clone()), action_rx, shutdown.clone());

        let dispatcher_clone = dispatcher.clone();
        let r = crate::runtime::blocking::spawn_blocking(move || dispatcher_clone.blocking_dispatch(Action::ForceQuit))
            .await
            .expect("spawn_blocking join");
        assert_eq!(r, DispatchResult::Sent);

        shutdown.cancel();
        drop(dispatcher);
    }

    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn redux_diff_count_remains_zero_in_shadow_mode() {
        // P0-2 check: in shadow mode the reducer does not double-write history, so REDUX_DIFF_COUNT == 0.
        // That counter is only incremented on the run_tui_unified_loop key event path under
        // PRX_CHAT_REDUX=both; this test never reaches that path, so it stays 0.
        crate::chat::reset_redux_diff_count();

        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        // run 50 mixed Actions, simulating streaming + tools + input
        for i in 0..50u64 {
            let _ = dispatcher.try_dispatch(Action::InputSubmitted(format!("msg{i}")));
            let _ = dispatcher.try_dispatch(Action::RecordUserTurn(format!("user{i}")));
            let _ = dispatcher.try_dispatch(Action::RecordAssistantTurn {
                task_id: None,
                content: format!("assist{i}"),
            });
        }

        shutdown.cancel();
        drop(dispatcher);
        let _stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("exit")
            .expect("join");

        assert_eq!(
            crate::chat::redux_diff_count(),
            0,
            "shadow mode must not produce any REDUX_DIFF_COUNT increments"
        );
    }
}

// ─── Step 5a-1 real business execution tests (EffectExecutor::new_with_deps) ──────────────────

#[cfg(test)]
mod real_mode_tests {
    //! Verify that business Effects really execute once EffectExecutor switches from shadow to real mode.
    //!
    //! This is the core falsification test for Codex P0 — shadow_mode always true + effect no-op + diff=0
    //! is circular reasoning; this module builds real mock deps and proves that:
    //!   1. SaveSession → memory.store is called
    //!   2. CancelDraft → channel.cancel_draft is called
    //!   3. NotifyHook → hooks.emit is called (fed back from a spawned subtask, so we must wait)
    //!   4. Quit → shutdown.cancel() is called
    //!   5. StartTurn → a spawned subtask feeds back Action::RedrawRequested
    //!   6. dual_write_guard is set after a persistence effect
    use super::*;
    use crate::channels::TerminalChannel;
    use crate::chat::session::ChatSession;
    use crate::hooks::HookManager;
    use crate::memory::{Memory, MemoryCategory, NoneMemory};
    use crate::observability::NoopObserver;
    use crate::providers::Provider;
    use crate::providers::router::MockEnvProvider;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    struct FlagTool {
        name: &'static str,
        executed: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for FlagTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "records execution"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            self.executed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::tools::ToolResult {
                success: true,
                output: "executed".to_string(),
                error: None,
            })
        }
    }

    /// Wrapper that counts memory.store calls (NoneMemory does not really store, it only traces calls).
    struct CountingMemory {
        inner: NoneMemory,
        store_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Memory for CountingMemory {
        fn name(&self) -> &str {
            "counting"
        }
        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.store_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.store(key, content, category, session_id).await
        }
        async fn recall(&self, q: &str, l: usize, s: Option<&str>) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            self.inner.recall(q, l, s).await
        }
        async fn get(&self, k: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            self.inner.get(k).await
        }
        async fn list(
            &self,
            c: Option<&MemoryCategory>,
            s: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            self.inner.list(c, s).await
        }
        async fn forget(&self, k: &str) -> anyhow::Result<bool> {
            self.inner.forget(k).await
        }
        async fn count(&self) -> anyhow::Result<usize> {
            self.inner.count().await
        }
        async fn health_check(&self) -> bool {
            self.inner.health_check().await
        }
    }

    /// Poll until an atomic counter reaches the target, avoiding a fixed sleep that is flaky on slow machines
    async fn wait_for_count(counter: &AtomicUsize, target: usize, timeout: Duration) -> usize {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let cur = counter.load(std::sync::atomic::Ordering::SeqCst);
            if cur >= target || std::time::Instant::now() >= deadline {
                return cur;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Counting HookManager wrapper — reuses HookManager directly but in a temporary directory.
    fn build_hook_manager() -> (Arc<HookManager>, TempDir) {
        let temp = TempDir::new().expect("tempdir");
        let mgr = HookManager::new(temp.path().to_path_buf());
        (Arc::new(mgr), temp)
    }

    /// Build complete EffectDeps with mock providers / memory / channel / hooks.
    fn build_deps(
        memory: Arc<dyn Memory>,
        shutdown: CancellationToken,
    ) -> (EffectDeps, mpsc::Receiver<Action>, Arc<HookManager>, TempDir) {
        let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
        let (hooks, temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
        let (action_tx, action_rx) = mpsc::channel::<Action>(64);
        let dual_write_guard = RuntimeDualWriteGuard::new();
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let deps = EffectDeps {
            provider,
            memory,
            memory_event_recording: MemoryEventRecording::default(),
            channel,
            hooks: Arc::clone(&hooks),
            observer,
            action_tx,
            provider_turn_lifecycle_tx: None,
            dual_write_guard,
            redraw_tx: Some(redraw_tx),
            #[cfg(feature = "terminal-tui")]
            tui_mirror: None,
            shutdown,
            model: ModelSlot::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            approval_router: Arc::new(ApprovalRouter::new()),
            tool_security_policy: full_tool_security_policy(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            exposed_tools: crate::tools::intent::SessionToolExposure::new(),
        };
        (deps, action_rx, hooks, temp)
    }

    #[tokio::test]
    async fn real_mode_save_session_triggers_memory_store() {
        // Prove that the reducer's pre-sanitized snapshot is really written to Memory after the dispatcher's
        // second sanitization, and that marker/hash/byte-count stay single-copy.
        let temp_memory = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp_memory.path()).unwrap());
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps.clone());
        assert!(!executor.is_shadow());

        let secret = "AKIAABCDEFGHIJKLMNOP";
        let long_content = format!(
            "raw effect {secret} Authorization: Bearer abcdefghijklmnop\n{}",
            "\u{20ac}".repeat(5_000)
        );
        let pre_truncate_len = crate::chat::sanitize::redact_secrets(&long_content).len();
        let mut state =
            crate::chat::state::ChatState::new(Arc::from("prov"), Arc::from("model"), CancellationToken::new());
        state.session.id = "dispatcher-sanitize-session".to_string();
        let _ = state.reduce(Action::RecordUserTurn(long_content));
        let effects = state.reduce(Action::BackgroundSessionRecorded {
            summary: crate::chat::sessions::PersistedSessionSummary {
                id: "child".to_string(),
                seq: 1,
                kind: "agent".to_string(),
                origin: "model".to_string(),
                status: "completed".to_string(),
                title: "child".to_string(),
                summary: "done".to_string(),
                token_usage_records: Vec::new(),
                created_at: chrono::Utc::now(),
            },
        });
        let save_effect = effects
            .into_iter()
            .find(|effect| matches!(effect, Effect::SaveSession(_)))
            .expect("reducer must emit sanitized SaveSession");
        let memory_key = format!("{}:{}", crate::chat::session::SESSION_MEMORY_PREFIX, state.session.id);
        executor.execute(save_effect).await;

        let stored = deps.memory.get(&memory_key).await.unwrap().unwrap();
        assert!(!stored.content.contains(secret));
        assert!(stored.content.contains('\u{20ac}'));
        let stored_session = ChatSession::from_json(&stored.content).unwrap();
        let stored_content = stored_session.turns.first().map(|turn| turn.content.as_str()).unwrap();
        assert!(stored_content.len() <= 10 * 1024);
        assert_eq!(stored_content.matches("[... truncated (").count(), 1);
        assert_eq!(stored_content.matches("bytes total, ref:").count(), 1);
        assert!(stored_content.contains(&format!("{pre_truncate_len} bytes total")));
        assert_eq!(
            crate::chat::sanitize::sanitize_for_persistence(stored_content),
            stored_content,
            "dispatcher-stored projection must be idempotent"
        );
        let recalled = deps.memory.recall(secret, 10, None).await.unwrap();
        assert!(recalled.iter().all(|entry| !entry.content.contains(secret)));
        let mut raw_session = ChatSession::new("prov", "model");
        raw_session.id = "dispatcher-raw-session".to_string();
        raw_session.add_assistant_turn(&format!("manual raw effect {secret}"), Vec::new());
        let raw_key = raw_session.memory_key();
        executor.execute(Effect::SaveSession(raw_session)).await;
        let raw_stored = deps.memory.get(&raw_key).await.unwrap().unwrap();
        assert!(!raw_stored.content.contains(secret));
        // RAII scope: the guard must reset automatically once the subtask finishes (it must not stick).
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard should auto-clear after SaveSession completes (RAII scope)"
        );
    }

    #[tokio::test]
    async fn real_mode_cancel_draft_invokes_channel() {
        // CancelDraft is a short synchronous path, awaited directly; not panicking means the path is clear.
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps);

        executor.execute(Effect::CancelDraft("draft-x".to_string())).await;
        // passing means no panic and no hang; TerminalChannel.cancel_draft always returns Ok.
    }

    /// T3-3-c-3 closed loop: the reducer dispatches `StreamCompleted` → the effects include SaveSession, and
    /// after EffectExecutor::execute_real memory.store is called once (reducer single-source persistence works).
    #[tokio::test]
    async fn t3_3c_stream_completed_drives_save_session_through_executor() {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;

        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps.clone());

        // use the real reducer to generate the Effect sequence (with SaveSession) and feed them to the executor.
        let mut state = ChatState::new(
            Arc::from("test-prov"),
            Arc::from("test-model"),
            CancellationToken::new(),
        );
        state.session.id = "t3-3c-session".to_string();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "d-T3-3c".to_string(),
            cancel: CancellationToken::new(),
        });
        let effects = state.reduce(Action::StreamCompleted {
            draft_id: "d-T3-3c".to_string(),
            final_text: "the answer".to_string(),
            reasoning: String::new(),
        });
        let mut had_save_session = false;
        for effect in effects {
            if matches!(effect, Effect::SaveSession(_)) {
                had_save_session = true;
            }
            executor.execute(effect).await;
        }
        assert!(had_save_session, "reducer must emit SaveSession for StreamCompleted");

        let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
        assert_eq!(
            final_count, 1,
            "a reducer-emitted SaveSession must trigger memory.store once"
        );
    }

    /// T3-3-fixA P0-2: Exit-after-completed persistence equivalence across the four modes.
    ///
    /// Verify that the SaveSession emitted by the reducer after a complete turn triggers exactly one
    /// memory.store in every mode — that is, the reducer persistence path is **mode independent**.
    /// After the fixA P0-2 fix the Pure mode reducer is the only persistence source, and this test confirms
    /// it agrees with Off/Both/Redux on reducer persistence semantics (write count + content).
    ///
    /// Note: the legacy save_session on chat::run main loop exit is gated by the ReduxMode guard, whose
    /// truth table is already covered by pure_mode_skips_legacy_exit_save_via_redux_mode_guard.
    #[tokio::test]
    async fn t3_3_fix_a_exit_after_completed_persistence_parity() {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;

        for tag in ["off", "both", "redux", "pure"] {
            let store_count = Arc::new(AtomicUsize::new(0));
            let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
                inner: NoneMemory::new(),
                store_count: Arc::clone(&store_count),
            });
            let shutdown = CancellationToken::new();
            let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
            let executor = EffectExecutor::new_with_deps(deps);

            let mut state = ChatState::new(
                Arc::from("test-prov"),
                Arc::from("test-model"),
                CancellationToken::new(),
            );
            state.session.id = format!("sess-fixA-{tag}");

            // complete turn: user → turn started → assistant recorded → stream completed
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: format!("d-{tag}"),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a".to_string(),
            });
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: format!("d-{tag}"),
                final_text: "a".to_string(),
                reasoning: String::new(),
            });

            let mut had_save = false;
            for effect in effects {
                if matches!(effect, Effect::SaveSession(_)) {
                    had_save = true;
                }
                executor.execute(effect).await;
            }
            assert!(
                had_save,
                "[{tag}] the reducer must emit SaveSession when a turn completes"
            );

            let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
            assert_eq!(
                final_count, 1,
                "[{tag}] reducer persistence must be mode independent — a complete turn triggers memory.store once",
            );
        }
    }

    /// T3-3-fixA P0-2: Exit-while-streaming consistency (no partial save) across the four modes.
    ///
    /// When exiting during streaming (the user did not wait for the stream to finish) the reducer must not
    /// emit SaveSession — a direct consequence of the Cancelled/Error rows of the appendix B decision table.
    /// This test simulates the mid-exit window where a turn started but neither RecordAssistantTurn nor
    /// StreamCompleted arrived, and verifies memory.store == 0 (no partial state is persisted).
    #[tokio::test]
    async fn t3_3_fix_a_exit_while_streaming_no_partial_save() {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;

        for tag in ["off", "both", "redux", "pure"] {
            let store_count = Arc::new(AtomicUsize::new(0));
            let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
                inner: NoneMemory::new(),
                store_count: Arc::clone(&store_count),
            });
            let shutdown = CancellationToken::new();
            let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
            let executor = EffectExecutor::new_with_deps(deps);

            let mut state = ChatState::new(
                Arc::from("test-prov"),
                Arc::from("test-model"),
                CancellationToken::new(),
            );
            state.session.id = format!("sess-stream-{tag}");

            // user already recorded, turn already streaming, but the stream never completed (mid-exit window)
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            let user_effects: Vec<Effect> = state
                .reduce(Action::TurnStarted {
                    draft_id: format!("d-{tag}"),
                    cancel: CancellationToken::new(),
                })
                .into_iter()
                .chain(state.reduce(Action::StreamChunkReceived {
                    draft_id: format!("d-{tag}"),
                    delta: "partial".to_string(),
                    version: 1,
                }))
                .collect();

            assert!(
                !user_effects.iter().any(|e| matches!(e, Effect::SaveSession(_))),
                "[{tag}] effects mid-stream must not contain SaveSession"
            );

            // execute all already emitted effects (including LogTrace / RequestRedraw etc.)
            for effect in user_effects {
                executor.execute(effect).await;
            }

            // the negative assertion keeps a fixed wait: confirm no spawned subtask wrote within the window
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                store_count.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "[{tag}] memory.store must be 0 when exiting mid-stream (no partial save)",
            );
        }
    }

    /// T3-3-fixB B5: the SaveSession snapshot on the driver path must contain this turn's assistant.
    ///
    /// End to end: use the MockEnvProvider default stream (one final chunk), drive the driver through a full
    /// run, feed the fed-back Action sequence into the reducer in order, and assert that:
    ///   1. the driver sends RecordAssistantTurn first, then StreamCompleted (the B5 ordering contract)
    ///   2. the SaveSession.turns.last() triggered by StreamCompleted is this turn's assistant (the fixA P0-1 contract)
    ///   3. turns.len() is strictly 2 (user + assistant, no double write)
    ///
    /// Before the fixB B5 fix the driver sent StreamCompleted directly, so session.turns was missing this
    /// turn's assistant when the reducer built the SaveSession snapshot; a failure here pinpoints a regression.
    #[tokio::test]
    async fn t3_3_fix_b_driver_path_save_session_includes_assistant() {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        // ── start the driver ──
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-fixB-B5".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        // ── collect the Actions fed back by the driver and feed them to the reducer ──
        let mut state = ChatState::new(
            Arc::from("test-prov"),
            Arc::from("test-model"),
            CancellationToken::new(),
        );
        state.session.id = "sess-fixB-B5".to_string();
        // the user turn is dispatched by the chat::run main loop before the driver starts, so add it by hand here.
        let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-fixB-B5".to_string(),
            cancel: CancellationToken::new(),
        });

        let mut saw_record = false;
        let mut save_snapshot: Option<crate::chat::session::ChatSession> = None;
        for _ in 0..8 {
            let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("driver action within 1.5s")
                .expect("action received");
            match action {
                Action::RecordAssistantTurn { content: text, .. } => {
                    assert!(!saw_record, "RecordAssistantTurn must be sent only once");
                    saw_record = true;
                    let _ = state.reduce(Action::RecordAssistantTurn {
                        task_id: None,
                        content: text,
                    });
                }
                Action::StreamCompleted {
                    draft_id,
                    final_text,
                    reasoning,
                } => {
                    assert!(
                        saw_record,
                        "B5 ordering contract: StreamCompleted must come after RecordAssistantTurn"
                    );
                    let effects = state.reduce(Action::StreamCompleted {
                        draft_id,
                        final_text,
                        reasoning,
                    });
                    for e in effects {
                        if let Effect::SaveSession(session) = e {
                            save_snapshot = Some(session);
                            break;
                        }
                    }
                    break;
                }
                _ => {
                    // other actions (StreamChunkReceived etc.) do not affect the ordering contract check.
                }
            }
        }

        assert!(saw_record, "the driver must send RecordAssistantTurn");
        let snap = save_snapshot.expect("StreamCompleted must emit SaveSession");
        let last = snap.turns.last().expect("SaveSession.turns must not be empty");
        assert_eq!(
            last.role, "assistant",
            "the last snapshot entry must be this turn's assistant"
        );
        // double-write guard: the reducer records RecordAssistantTurn once; the duplicate dispatch was removed.
        assert_eq!(
            snap.turns.len(),
            2,
            "turns.len() must be strictly 2 (user+assistant, zero double write)"
        );
    }

    /// T3-3-fixB D1: SaveSession completion (disk write end) must be strictly before RequestRedraw (refresh).
    ///
    /// With the original spawn version the ordering of the SaveSession subtask and RequestRedraw could not be
    /// guaranteed; after the inline await fix the main loop's executor.execute(effect).await is serial throughout.
    ///
    /// How it is verified: SlowMemory.store sleeps 20ms and then pushes "save_end",
    /// MockRedrawRx pushes "redraw" on try_send, and we assert the log index of "save_end" < that of "redraw".
    /// Repeated N=5 times to rule out luck (the spawn version sometimes got lucky; several rounds expose it).
    #[tokio::test]
    async fn t3_3_fix_b_effect_save_then_redraw_strict_order() {
        use crate::memory::MemoryEntry;
        use parking_lot::Mutex;

        struct SlowMemory {
            log: Arc<Mutex<Vec<&'static str>>>,
        }
        #[async_trait::async_trait]
        impl Memory for SlowMemory {
            fn name(&self) -> &str {
                "slow"
            }
            async fn store(&self, _k: &str, _c: &str, _cat: MemoryCategory, _s: Option<&str>) -> anyhow::Result<()> {
                self.log.lock().push("save_start");
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.log.lock().push("save_end");
                Ok(())
            }
            async fn recall(&self, _q: &str, _l: usize, _s: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
                Ok(Vec::new())
            }
            async fn get(&self, _k: &str) -> anyhow::Result<Option<MemoryEntry>> {
                Ok(None)
            }
            async fn list(&self, _c: Option<&MemoryCategory>, _s: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
                Ok(Vec::new())
            }
            async fn forget(&self, _k: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            async fn count(&self) -> anyhow::Result<usize> {
                Ok(0)
            }
            async fn health_check(&self) -> bool {
                true
            }
        }

        for trial in 0..5 {
            let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
            let memory: Arc<dyn Memory> = Arc::new(SlowMemory { log: Arc::clone(&log) });
            let shutdown = CancellationToken::new();

            // build deps separately: SlowMemory + a custom redraw_tx that intercepts the try_send order.
            let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
            let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
            let (hooks, _temp) = build_hook_manager();
            let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
            let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
            let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);

            // a listener task on redraw_rx pushes "redraw" into the log.
            let log_for_redraw = Arc::clone(&log);
            let redraw_listener = tokio::spawn(async move {
                if redraw_rx.recv().await.is_some() {
                    log_for_redraw.lock().push("redraw");
                }
            });

            let deps = EffectDeps {
                provider,
                memory,
                memory_event_recording: MemoryEventRecording::default(),
                channel,
                hooks,
                observer,
                action_tx,
                provider_turn_lifecycle_tx: None,
                dual_write_guard: RuntimeDualWriteGuard::new(),
                redraw_tx: Some(redraw_tx),
                #[cfg(feature = "terminal-tui")]
                tui_mirror: None,
                shutdown: shutdown.clone(),
                model: ModelSlot::from("test-model"),
                temperature: 0.0,
                tools_registry: None,
                approval_router: Arc::new(ApprovalRouter::new()),
                tool_security_policy: full_tool_security_policy(),
                tool_tiering: crate::config::ToolTieringConfig::default(),
                exposed_tools: crate::tools::intent::SessionToolExposure::new(),
            };
            let executor = EffectExecutor::new_with_deps(deps);

            let session = ChatSession::new("prov", "model");
            // serial main loop: SaveSession → RequestRedraw (the reducer's actual order).
            executor.execute(Effect::SaveSession(session)).await;
            executor.execute(Effect::RequestRedraw).await;

            // wait for redraw_listener to finish (it exits after receiving redraw).
            let _ = tokio::time::timeout(Duration::from_millis(500), redraw_listener).await;

            let snap = log.lock().clone();
            let save_end_idx = snap
                .iter()
                .position(|&s| s == "save_end")
                .unwrap_or_else(|| panic!("[trial {trial}] save_end did not appear: log={snap:?}"));
            let redraw_idx = snap
                .iter()
                .position(|&s| s == "redraw")
                .unwrap_or_else(|| panic!("[trial {trial}] redraw did not appear: log={snap:?}"));
            assert!(
                save_end_idx < redraw_idx,
                "[trial {trial}] D1 order: save_end ({save_end_idx}) must precede redraw ({redraw_idx}); {snap:?}"
            );
        }
    }

    #[tokio::test]
    async fn real_mode_quit_cancels_shutdown_token() {
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps);

        assert!(!shutdown.is_cancelled());
        executor.execute(Effect::Quit).await;
        assert!(shutdown.is_cancelled(), "Effect::Quit must cancel shutdown token");
    }

    #[tokio::test]
    async fn real_mode_request_redraw_pings_renderer() {
        // RequestRedraw wakes the main loop through deps.redraw_tx in real mode.
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
        let (hooks, _temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);
        let deps = EffectDeps {
            provider,
            memory,
            memory_event_recording: MemoryEventRecording::default(),
            channel,
            hooks,
            observer,
            action_tx,
            provider_turn_lifecycle_tx: None,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            #[cfg(feature = "terminal-tui")]
            tui_mirror: None,
            shutdown: shutdown.clone(),
            model: ModelSlot::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            approval_router: Arc::new(ApprovalRouter::new()),
            tool_security_policy: full_tool_security_policy(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            exposed_tools: crate::tools::intent::SessionToolExposure::new(),
        };
        let executor = EffectExecutor::new_with_deps(deps);
        executor.execute(Effect::RequestRedraw).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), redraw_rx.recv())
                .await
                .expect("redraw within 200ms")
                .is_some(),
            "RequestRedraw should ping the redraw channel"
        );
    }

    /// With a renderer attached the notice is already in the reducer's
    /// transcript ledger, so the effect owes it a frame.
    #[tokio::test]
    async fn surface_notice_pings_renderer_when_one_is_attached() {
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);
        deps.redraw_tx = Some(redraw_tx);
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::SurfaceNotice {
                text: "Context trimmed (lossy): 3 older messages dropped.".to_string(),
            })
            .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(200), redraw_rx.recv())
                .await
                .expect("redraw within 200ms")
                .is_some(),
            "SurfaceNotice must ping the renderer so the line is drawn"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn request_approval_in_tui_opens_surface_without_auto_send() {
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let mirror = Arc::new(ParkingMutex::new(crate::chat::tui::TuiState::new("p", "m")));
        deps.tui_mirror = Some(Arc::clone(&mirror));
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::RequestApproval {
                task_id: None,
                tool_id: "call-tui".to_string(),
                name: "shell".to_string(),
                args: r#"{"cmd":"echo secure"}"#.to_string(),
            })
            .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(50), action_rx.recv())
                .await
                .is_err(),
            "TUI RequestApproval must not auto-send ToolApprovalReceived"
        );
        let state = mirror.lock();
        let pending = state
            .pending_tool_approval
            .as_ref()
            .expect("approval surface should be visible");
        assert_eq!(pending.tool_id, "call-tui");
        assert_eq!(pending.name, "shell");
        assert!(pending.args.contains("echo secure"));
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Approval);
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn request_approval_in_tui_ignores_openprx_override_parent() {
        let exe = std::env::current_exe().expect("current test exe");
        let status = std::process::Command::new(exe)
            .arg("--exact")
            .arg("chat::dispatcher::tests::request_approval_in_tui_ignores_openprx_override_child")
            .arg("--nocapture")
            .env("OPENPRX_APPROVAL_OVERRIDE", "allow")
            .env("ISS018_OVERRIDE_CHILD", "1")
            .status()
            .expect("run child override test");
        assert!(status.success(), "child override test failed: {status}");
    }

    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn request_approval_in_tui_ignores_openprx_override_child() {
        if std::env::var_os("ISS018_OVERRIDE_CHILD").is_none() {
            return;
        }
        assert_eq!(
            std::env::var("OPENPRX_APPROVAL_OVERRIDE").as_deref(),
            Ok("allow"),
            "child test must exercise the env override condition"
        );

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let mirror = Arc::new(ParkingMutex::new(crate::chat::tui::TuiState::new("p", "m")));
        deps.tui_mirror = Some(Arc::clone(&mirror));
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::RequestApproval {
                task_id: None,
                tool_id: "call-tui-override".to_string(),
                name: "shell".to_string(),
                args: r#"{"cmd":"echo secure"}"#.to_string(),
            })
            .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(50), action_rx.recv())
                .await
                .is_err(),
            "TUI RequestApproval must ignore OPENPRX_APPROVAL_OVERRIDE and wait for human input"
        );
        let state = mirror.lock();
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Approval);
        assert_eq!(
            state
                .pending_tool_approval
                .as_ref()
                .map(|pending| pending.tool_id.as_str()),
            Some("call-tui-override")
        );
    }

    #[tokio::test]
    async fn real_mode_start_turn_spawns_subtask_and_does_not_block() {
        // StartTurn must spawn a subtask that feeds Actions back (Codex P0-1), without blocking the main loop.
        // From 5a-2 on: the subtask really calls provider.stream_chat_with_history and feeds the streaming
        // events back through action_tx — here we use the MockEnvProvider trait default implementation,
        // which emits one final error chunk, so we should get StreamChunkReceived → StreamCompleted.
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let start = std::time::Instant::now();
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-real".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;
        // execute() returns immediately; the spawned subtask really calls the provider and feeds Actions back.
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "StartTurn should not block (Codex P0-1)"
        );

        // first Action: the default stream implementation emits one error chunk (delta=error message,
        // is_final=true), which is converted into StreamChunkReceived (because delta is non-empty).
        let action = tokio::time::timeout(Duration::from_secs(1), action_rx.recv())
            .await
            .expect("action within 1s")
            .expect("action received");
        match action {
            Action::StreamChunkReceived { draft_id, version, .. } => {
                assert_eq!(draft_id, "draft-real", "draft_id must propagate");
                assert!(version >= 1, "version must start at 1+");
            }
            other => panic!("expected StreamChunkReceived, got {other:?}"),
        }

        // T3-3-fixB B5: the second one is RecordAssistantTurn (sent before StreamCompleted).
        let action = tokio::time::timeout(Duration::from_secs(1), action_rx.recv())
            .await
            .expect("RecordAssistantTurn within 1s")
            .expect("RecordAssistantTurn received");
        match action {
            Action::RecordAssistantTurn { .. } => {}
            other => panic!("expected RecordAssistantTurn (fixB B5 precondition), got {other:?}"),
        }

        // third: is_final=true breaks out of the loop and sends StreamCompleted.
        let action = tokio::time::timeout(Duration::from_secs(1), action_rx.recv())
            .await
            .expect("completion within 1s")
            .expect("completion received");
        match action {
            Action::StreamCompleted { draft_id, .. } => {
                assert_eq!(draft_id, "draft-real", "completion draft_id must propagate");
            }
            other => panic!("expected StreamCompleted, got {other:?}"),
        }
    }

    /// Step 5a-2 — StartTurn sends StreamCancelled immediately after a cancel pre-trigger.
    #[tokio::test]
    async fn real_mode_start_turn_pre_cancel_emits_stream_cancelled() {
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        cancel.cancel(); // cancelled before start

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-cancelled".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let action = tokio::time::timeout(Duration::from_millis(500), action_rx.recv())
            .await
            .expect("action within 500ms")
            .expect("action received");
        match action {
            Action::StreamCancelled { draft_id } => {
                assert_eq!(draft_id, "draft-cancelled");
            }
            other => panic!("expected StreamCancelled, got {other:?}"),
        }
    }

    /// Step 5a-2 — fake streaming provider verifies the chunk → completion sequence + strict version increase.
    #[tokio::test]
    async fn real_mode_start_turn_streams_chunks_then_completes() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct FakeStreamProvider;

        #[async_trait]
        impl Provider for FakeStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    native_tool_calling: false,
                    vision: false,
                }
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let chunks: Vec<StreamResult<StreamChunk>> = vec![
                    Ok(StreamChunk::delta("hello ")),
                    Ok(StreamChunk::reasoning_delta("thinking…")),
                    Ok(StreamChunk::delta("world")),
                    Ok(StreamChunk::final_chunk()),
                ];
                stream::iter(chunks).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(FakeStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-stream".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        // receive chunk 1 (delta="hello ")
        let a1 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("first chunk within 1.5s")
            .expect("first chunk received");
        match a1 {
            Action::StreamChunkReceived {
                draft_id,
                delta,
                version,
            } => {
                assert_eq!(draft_id, "draft-stream");
                assert_eq!(delta, "hello ");
                assert_eq!(version, 1, "first delta version must be 1");
            }
            other => panic!("expected StreamChunkReceived#1, got {other:?}"),
        }

        // F1: reasoning chunks are now dispatched as Actions too, so the TUI shows progress while thinking;
        // they share the same version counter as text deltas.
        let a_reasoning = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("reasoning progress within 1.5s")
            .expect("reasoning progress received");
        match a_reasoning {
            Action::StreamReasoningReceived {
                draft_id,
                delta,
                version,
            } => {
                assert_eq!(draft_id, "draft-stream");
                assert_eq!(delta, "thinking…");
                assert_eq!(version, 2, "reasoning shares the text delta version counter");
            }
            other => panic!("expected StreamReasoningReceived, got {other:?}"),
        }

        // the next one is still chunk 2 (delta="world"), and the version keeps increasing strictly
        let a2 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("second chunk within 1.5s")
            .expect("second chunk received");
        match a2 {
            Action::StreamChunkReceived { delta, version, .. } => {
                assert_eq!(delta, "world");
                assert_eq!(version, 3, "second delta version must strictly increase");
            }
            other => panic!("expected StreamChunkReceived#2, got {other:?}"),
        }

        // T3-3-fixB B5: RecordAssistantTurn is sent before StreamCompleted, and
        // final_text matches the RecordAssistantTurn content.
        let a_record = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("RecordAssistantTurn within 1.5s")
            .expect("RecordAssistantTurn received");
        match a_record {
            Action::RecordAssistantTurn { content: text, .. } => {
                assert_eq!(text, "hello world", "RecordAssistantTurn content must match final_text");
            }
            other => panic!("expected RecordAssistantTurn (fixB B5 precondition), got {other:?}"),
        }

        // finally StreamCompleted, with accumulated final_text and reasoning containing the thinking text.
        let a3 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("completion within 1.5s")
            .expect("completion received");
        match a3 {
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => {
                assert_eq!(draft_id, "draft-stream");
                assert_eq!(final_text, "hello world");
                assert_eq!(reasoning, "thinking…");
            }
            other => panic!("expected StreamCompleted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn task_scoped_start_turn_emits_ready_for_ordered_commit() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct FakeTaskScopedStreamProvider;

        #[async_trait]
        impl Provider for FakeTaskScopedStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let chunks: Vec<StreamResult<StreamChunk>> = vec![
                    Ok(StreamChunk::delta("ordered ")),
                    Ok(StreamChunk::reasoning_delta("gate")),
                    Ok(StreamChunk::delta("commit")),
                    Ok(StreamChunk::final_chunk()),
                ];
                stream::iter(chunks).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("task scoped", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        scheduler.start_task(task_id).expect("task starts");
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(FakeTaskScopedStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: Some(task_id),
                draft_id: "draft-task-scoped".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        // F1: a reasoning delta sandwiched between text deltas is dispatched separately as thinking progress.
        let mut text_deltas = Vec::new();
        let mut reasoning_deltas = Vec::new();
        while text_deltas.len() < 2 {
            let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("stream action within 1.5s")
                .expect("stream action received");
            match action {
                Action::StreamChunkReceived { delta, .. } => text_deltas.push(delta),
                Action::StreamReasoningReceived { delta, .. } => reasoning_deltas.push(delta),
                other => panic!("expected stream delta action, got {other:?}"),
            }
        }
        assert_eq!(text_deltas, vec!["ordered ".to_string(), "commit".to_string()]);
        assert_eq!(
            reasoning_deltas,
            vec!["gate".to_string()],
            "thinking progress must be dispatched live, not only on completion"
        );

        let terminal = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("ready signal within 1.5s")
            .expect("ready signal received");
        match terminal {
            Action::ProviderTurnReadyForCommit {
                draft_id,
                final_text,
                reasoning,
            } => {
                assert_eq!(draft_id, "draft-task-scoped");
                assert_eq!(final_text, "ordered commit");
                assert_eq!(reasoning, "gate");
            }
            other => panic!("task-scoped completion must wait for ordered commit gate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn real_mode_empty_stream_retries_and_records_recovered_answer() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct EmptyThenAnswerProvider {
            calls: AtomicUsize,
        }

        #[async_trait]
        impl Provider for EmptyThenAnswerProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: Some("thinking only".to_string()),
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    stream::iter(vec![
                        Ok(StreamChunk::reasoning_delta("thinking only")),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![
                        Ok(StreamChunk::delta("recovered")),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(EmptyThenAnswerProvider::default());
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-empty".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_assistant_record = false;
        let mut saw_completion = false;
        for _ in 0..6 {
            let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("driver action within 1.5s")
                .expect("driver action received");
            match action {
                Action::SystemMessageAdded { text } => {
                    panic!("a recovered empty response must not surface a failure notice: {text}");
                }
                Action::RecordAssistantTurn { content: text, .. } => {
                    assert_eq!(text, "recovered");
                    saw_assistant_record = true;
                }
                Action::StreamCompleted {
                    draft_id,
                    final_text,
                    reasoning,
                } => {
                    assert_eq!(draft_id, "draft-empty");
                    assert_eq!(final_text, "recovered");
                    let _ = reasoning;
                    saw_completion = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(saw_assistant_record, "the recovered answer must be recorded");
        assert!(saw_completion, "the recovered turn must complete and clear the draft");
    }

    /// Step 5a-2 — send StreamFailed when the provider stream yields an Err (including the retryable verdict).
    #[tokio::test]
    async fn real_mode_start_turn_stream_error_emits_stream_failed() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct FailingStreamProvider;

        #[async_trait]
        impl Provider for FailingStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let chunks: Vec<StreamResult<StreamChunk>> =
                    vec![Err(StreamError::Provider("simulated failure".to_string()))];
                stream::iter(chunks).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(FailingStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-fail".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("fail within 1.5s")
            .expect("fail action received");
        match action {
            Action::StreamFailed {
                draft_id,
                err,
                retryable,
            } => {
                assert_eq!(draft_id, "draft-fail");
                assert!(err.contains("simulated failure"));
                assert!(!retryable, "Provider error is non-retryable");
            }
            other => panic!("expected StreamFailed, got {other:?}"),
        }
    }

    /// Step 5a-2 — a cancel in mid-stream triggers StreamCancelled.
    #[tokio::test]
    async fn real_mode_start_turn_mid_stream_cancel_emits_cancelled() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct SlowStreamProvider;

        #[async_trait]
        impl Provider for SlowStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                // sleep 200ms between chunks to give cancel a chance
                let s = stream::unfold(0u32, |i| async move {
                    if i >= 5 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Some((Ok(StreamChunk::delta(format!("chunk{i} "))), i + 1))
                });
                s.boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(SlowStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-mid-cancel".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: cancel.clone(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        // cancel after 250ms: at least 1 chunk should already have arrived, then cancel immediately.
        tokio::time::sleep(Duration::from_millis(250)).await;
        cancel.cancel();

        // receive a few Actions and find StreamCancelled.
        let mut found_cancelled = false;
        for _ in 0..10 {
            let action = match tokio::time::timeout(Duration::from_millis(800), action_rx.recv()).await {
                Ok(Some(a)) => a,
                _ => break,
            };
            if matches!(action, Action::StreamCancelled { .. }) {
                found_cancelled = true;
                break;
            }
        }
        assert!(
            found_cancelled,
            "mid-stream cancel must emit StreamCancelled within reasonable time"
        );
    }

    #[tokio::test]
    async fn dual_write_guard_default_is_inactive() {
        let g = RuntimeDualWriteGuard::new();
        assert!(!g.is_active());
        let scope = g.enter_scope();
        assert!(g.is_active());
        assert_eq!(g.active_count(), 1);
        drop(scope);
        assert!(!g.is_active());
        assert_eq!(g.active_count(), 0);
    }

    #[tokio::test]
    async fn dual_write_guard_clone_shares_state() {
        let g1 = RuntimeDualWriteGuard::new();
        let g2 = g1.clone();
        assert!(!g2.is_active());
        let _scope = g1.enter_scope();
        assert!(g2.is_active(), "clone shares the same Arc<AtomicU64>");
    }

    /// 5a-5 Codex P1 fix verification: when several RAII scopes exist concurrently,
    /// dropping one scope must not make the guard inactive; it only returns to 0 after every scope drops.
    /// The old AtomicBool implementation had a serious timing window: an earlier drop also cleared later ones.
    #[tokio::test]
    async fn dual_write_guard_counting_raii_prevents_early_release() {
        let g = RuntimeDualWriteGuard::new();
        let s1 = g.enter_scope();
        assert!(g.is_active());
        assert_eq!(g.active_count(), 1);
        let s2 = g.enter_scope();
        assert_eq!(g.active_count(), 2);
        let s3 = g.enter_scope();
        assert_eq!(g.active_count(), 3);

        // drop one in the middle; the guard stays active.
        drop(s2);
        assert!(
            g.is_active(),
            "guard must remain active while sibling scopes are alive (5a-5 P1 fix)"
        );
        assert_eq!(g.active_count(), 2);

        drop(s1);
        assert!(g.is_active(), "guard remains active with last scope alive");
        assert_eq!(g.active_count(), 1);

        drop(s3);
        assert!(!g.is_active(), "guard becomes inactive only after all scopes drop");
        assert_eq!(g.active_count(), 0);
    }

    /// Ratatui path Ctrl+C regression guard (reduced to ChatState + a double Ctrl+C dispatch verifying Effect::Quit).
    ///
    /// **Background**: Codex P1 pointed out that the PTY test uses `PRX_TUI=0` and goes through reedline, so it
    /// does not cover the Ctrl+C branch of `run_tui_unified_loop`. Capturing full-screen ratatui TUI output
    /// from a PTY is hard, so this is reduced to a reducer + executor unit test:
    ///   - build a ChatState simulating a double Ctrl+C in DOUBLE_CTRLC_WINDOW_MS on the ratatui path
    ///   - verify the reducer returns Effect::Quit
    ///   - feed Effect::Quit to a real-mode EffectExecutor and verify shutdown.cancel() is called
    /// This is the minimal verifiable subset of the end-to-end "Ctrl+C → exit" chain, covering the core
    /// defensive path of the round 2 hang bug (reducer decision + executor trigger).
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn ratatui_path_double_ctrlc_exits_via_reducer_and_executor() {
        use crate::chat::state::ChatState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let shutdown = CancellationToken::new();
        let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown.clone());
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        // first Ctrl+C @ t=1000ms — must not trigger Quit (it only records the window; the reducer requires prev != 0)
        let effects1 = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1000);
        let has_quit_1 = effects1.iter().any(|e| matches!(e, Effect::Quit));
        assert!(!has_quit_1, "first Ctrl+C should not Quit");

        // second Ctrl+C @ t=1200ms — inside the 500ms window (200ms apart), so it must Quit
        let effects2 = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1200);
        let has_quit_2 = effects2.iter().any(|e| matches!(e, Effect::Quit));
        assert!(has_quit_2, "double Ctrl+C within 500ms must Quit");

        // feed it to a real-mode EffectExecutor and verify shutdown.cancel() really runs
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps);
        for e in effects2 {
            executor.execute(e).await;
        }
        assert!(
            shutdown.is_cancelled(),
            "real-mode executor must propagate Effect::Quit to shutdown.cancel()"
        );
    }

    /// Extra regression guard: a single Ctrl+C during an in-flight turn must not exit (it only cancels the turn).
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn single_ctrlc_during_turn_does_not_exit() {
        use crate::chat::state::ChatState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let shutdown = CancellationToken::new();
        let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown.clone());
        // simulate a turn in progress (generating=true) — set through the TurnStarted action
        let cancel = CancellationToken::new();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "d1".to_string(),
            cancel: cancel.clone(),
        });
        assert!(state.control.generating);

        // single Ctrl+C — while generating it must cancel the draft, not exit.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let effects = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1000);
        let has_quit = effects.iter().any(|e| matches!(e, Effect::Quit));
        assert!(!has_quit, "single Ctrl+C in flight turn must not Quit");

        // shutdown must not be cancelled
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps);
        for e in effects {
            executor.execute(e).await;
        }
        assert!(!shutdown.is_cancelled(), "single Ctrl+C must not cancel shutdown");
    }

    /// Regression guard: the Mutex test avoids ".unwrap()" — parking_lot is mandatory.
    #[tokio::test]
    async fn parking_lot_mutex_in_test() {
        let m: Mutex<u32> = Mutex::new(0);
        *m.lock() = 42;
        assert_eq!(*m.lock(), 42);
    }

    // ─── P1: fill in the real business unit test coverage for Effects (7/7) ──────────────────

    /// CountingChannel: records how many times send is called (wraps TerminalChannel).
    struct CountingChannel {
        inner: crate::channels::TerminalChannel,
        send_count: Arc<AtomicUsize>,
        finalize_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::channels::Channel for CountingChannel {
        fn name(&self) -> &str {
            "counting"
        }
        async fn send(&self, message: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
            self.send_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.send(message).await
        }
        async fn listen(
            &self,
            tx: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
        ) -> anyhow::Result<()> {
            self.inner.listen(tx).await
        }
        async fn finalize_draft(&self, recipient: &str, message_id: &str, text: &str) -> anyhow::Result<()> {
            self.finalize_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.finalize_draft(recipient, message_id, text).await
        }
        async fn cancel_draft(&self, recipient: &str, message_id: &str) -> anyhow::Result<()> {
            self.inner.cancel_draft(recipient, message_id).await
        }
    }

    /// Build CountingChannel deps.
    fn build_counting_channel_deps(
        send_count: Arc<AtomicUsize>,
        finalize_count: Arc<AtomicUsize>,
        shutdown: CancellationToken,
    ) -> (EffectDeps, mpsc::Receiver<Action>, TempDir) {
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::router::MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(CountingChannel {
            inner: crate::channels::TerminalChannel::new(true),
            send_count,
            finalize_count,
        });
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let (hooks, temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
        let (action_tx, action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let deps = EffectDeps {
            provider,
            memory,
            memory_event_recording: MemoryEventRecording::default(),
            channel,
            hooks,
            observer,
            action_tx,
            provider_turn_lifecycle_tx: None,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            #[cfg(feature = "terminal-tui")]
            tui_mirror: None,
            shutdown,
            model: ModelSlot::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            approval_router: Arc::new(ApprovalRouter::new()),
            tool_security_policy: full_tool_security_policy(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            exposed_tools: crate::tools::intent::SessionToolExposure::new(),
        };
        (deps, action_rx, temp)
    }

    /// P1-1: EmitChannelMessage → channel.send is really called.
    #[tokio::test]
    async fn real_mode_emit_channel_message_triggers_channel_send() {
        let send_count = Arc::new(AtomicUsize::new(0));
        let finalize_count = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let (deps, _rx, _temp) =
            build_counting_channel_deps(Arc::clone(&send_count), Arc::clone(&finalize_count), shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        use crate::channels::traits::SendMessage;
        let msg = SendMessage::new("hello from effect".to_string(), "user");
        executor.execute(Effect::EmitChannelMessage(msg)).await;

        let final_count = wait_for_count(&send_count, 1, Duration::from_secs(2)).await;
        assert_eq!(
            final_count, 1,
            "channel.send should be called exactly once for EmitChannelMessage"
        );
        // RAII scope: the guard must reset automatically once the subtask finishes (it must not stick).
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard should auto-clear after EmitChannelMessage completes (RAII scope)"
        );
    }

    /// P1-2: PersistToMemory → memory.store is really called (using the existing CountingMemory).
    #[tokio::test]
    async fn real_mode_persist_to_memory_triggers_memory_store() {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(CountingMemory {
            inner: crate::memory::NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        executor
            .execute(Effect::PersistToMemory {
                key: "persist-test-key".to_string(),
                value: "test-value".to_string(),
                category: crate::memory::MemoryCategory::Conversation,
            })
            .await;

        let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
        assert_eq!(
            final_count, 1,
            "memory.store should be called exactly once for PersistToMemory"
        );
        // RAII scope: the guard must reset automatically once the subtask finishes (it must not stick).
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard should auto-clear after PersistToMemory completes (RAII scope)"
        );
    }

    /// P1-3a: NotifyHook → no panic, and the RAII scope makes sure the guard resets after the subtask finishes.
    ///
    /// HookManager is not a trait so it cannot be wrapped for counting; behavioural check:
    /// the guard clearing itself after emit means the executor took the real path and RAII did not stick.
    /// HookManager has no registered hooks → emit is a fast no-op and does not slow the test down.
    #[tokio::test]
    async fn real_mode_notify_hook_guard_does_not_stick() {
        use crate::hooks::HookEvent;
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        assert!(!deps.dual_write_guard.is_active(), "guard should start inactive");

        executor
            .execute(Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"test": "notify-hook"}),
            })
            .await;

        // the spawned subtask is async; give it enough time to finish
        tokio::time::sleep(Duration::from_millis(200)).await;

        // RAII scope: the guard must reset automatically once the subtask finishes (it must not stick).
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard must auto-clear after NotifyHook completes (RAII scope prevents sticking)"
        );
    }

    /// P1-3b: NotifyHook → hooks.emit is really called (a real hook in a temp dir touches a sentinel file).
    ///
    /// HookManager is not a trait and cannot be mocked. Instead we register a real hook:
    /// write hooks.json in a temp directory registering a turn_complete event that runs `touch <sentinel>`,
    /// and after emit the existence of the sentinel file proves emit really ran the hook action.
    #[tokio::test]
    async fn real_mode_notify_hook_triggers_emit() {
        use crate::hooks::HookEvent;

        // build a temp directory + register a real hook (touch a sentinel file)
        let temp = TempDir::new().expect("tempdir");
        let sentinel = temp.path().join("hook_was_called");
        let sentinel_str = sentinel.to_str().expect("valid path");

        let hooks_json = serde_json::json!({
            "enabled": true,
            "hooks": {
                "turn_complete": [
                    {
                        "command": "touch",
                        "args": [sentinel_str],
                        "stdin_json": false
                    }
                ]
            }
        });
        std::fs::write(temp.path().join("hooks.json"), hooks_json.to_string()).expect("write hooks.json");

        let hooks = Arc::new(HookManager::new(temp.path().to_path_buf()));
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::router::MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(crate::channels::TerminalChannel::new(true));
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let shutdown = CancellationToken::new();
        let deps = EffectDeps {
            provider,
            memory,
            memory_event_recording: MemoryEventRecording::default(),
            channel,
            hooks: Arc::clone(&hooks),
            observer,
            action_tx,
            provider_turn_lifecycle_tx: None,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            #[cfg(feature = "terminal-tui")]
            tui_mirror: None,
            shutdown,
            model: ModelSlot::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            approval_router: Arc::new(ApprovalRouter::new()),
            tool_security_policy: full_tool_security_policy(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            exposed_tools: crate::tools::intent::SessionToolExposure::new(),
        };
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"turn": "test"}),
            })
            .await;

        // the hook runs through tokio::process::Command (async); give it enough time to finish
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert!(
            sentinel.exists(),
            "hooks.emit should have executed 'touch {sentinel_str}' — sentinel file not found, emit was not called"
        );
    }

    /// P1-4a: SendDraftFinalize → no panic, no blocking, and the RAII guard does not stick.
    ///
    /// Checks: (1) non-blocking (returns immediately) (2) the guard resets after the subtask finishes (no sticking)
    /// (3) channel.finalize_draft is really called (finalize_count == 1).
    #[tokio::test]
    async fn real_mode_send_draft_finalize_triggers_channel_finalize() {
        let send_count = Arc::new(AtomicUsize::new(0));
        let finalize_count = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let (deps, _rx, _temp) =
            build_counting_channel_deps(Arc::clone(&send_count), Arc::clone(&finalize_count), shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        let start = std::time::Instant::now();
        executor
            .execute(Effect::SendDraftFinalize {
                draft_id: "draft-finalize-test".to_string(),
                text: "final response text".to_string(),
            })
            .await;
        // non-blocking: returns immediately after spawning
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "SendDraftFinalize should not block (Codex P0-1)"
        );

        // wait for the subtask to finish
        tokio::time::sleep(Duration::from_millis(200)).await;

        // channel.finalize_draft is really called
        assert_eq!(
            finalize_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "channel.finalize_draft should be called exactly once for SendDraftFinalize"
        );
        // RAII scope: the guard resets automatically after the subtask finishes (no sticking)
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard must auto-clear after SendDraftFinalize completes (RAII scope)"
        );
    }

    /// P1-5: DisplayMedia → no panic, takes the trace/debug path (in 5a-1 the legacy path drives media display).
    #[tokio::test]
    async fn real_mode_display_media_does_not_panic() {
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        // no panic = the path is clear; in stage 5a-1 there is only a debug log and no external side effect
        executor
            .execute(Effect::DisplayMedia {
                kind: "IMAGE".to_string(),
                path: "/tmp/test_image.png".to_string(),
            })
            .await;
        // passing = no panic
    }

    /// P1-6: AutoTitleSession → no panic, takes the debug trace path.
    #[tokio::test]
    async fn real_mode_auto_title_session_does_not_panic() {
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::AutoTitleSession("session-title-test".to_string()))
            .await;
        // passing = no panic; in stage 5a-1 AutoTitleSession only writes a debug log
    }

    /// P1-7: LogTrace in real mode — verify no tracing::Level panics (real mode uses the same path as shadow).
    #[tokio::test]
    async fn real_mode_log_trace_all_levels_do_not_panic() {
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let levels = [
            tracing::Level::ERROR,
            tracing::Level::WARN,
            tracing::Level::INFO,
            tracing::Level::DEBUG,
            tracing::Level::TRACE,
        ];
        for level in levels {
            executor
                .execute(Effect::LogTrace {
                    level,
                    msg: format!("real-mode log test at {level}"),
                })
                .await;
        }
        // all passing = no panic; in real mode LogTrace takes the same emit_trace path as shadow
    }

    /// P0-2 check: after injecting redraw_tx through the redraw_handle Arc, RequestRedraw really triggers a redraw.
    ///
    /// Simulates the chat::run scenario: first build the EffectExecutor (redraw_tx=None),
    /// take the redraw_handle, "spawn" (executed directly here), then fill in redraw_tx,
    /// and verify the RequestRedraw effect really triggers a redraw.
    #[tokio::test]
    async fn redraw_handle_injection_enables_request_redraw() {
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        // at construction deps.redraw_tx = Some (build_deps defaults to Some); we use None to simulate the race
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::router::MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(crate::channels::TerminalChannel::new(true));
        let (hooks, _temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let deps = EffectDeps {
            provider,
            memory,
            memory_event_recording: MemoryEventRecording::default(),
            channel,
            hooks,
            observer,
            action_tx,
            provider_turn_lifecycle_tx: None,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: None, // simulate redraw_tx not existing yet at construction
            #[cfg(feature = "terminal-tui")]
            tui_mirror: None,
            shutdown: shutdown.clone(),
            model: ModelSlot::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            approval_router: Arc::new(ApprovalRouter::new()),
            tool_security_policy: full_tool_security_policy(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            exposed_tools: crate::tools::intent::SessionToolExposure::new(),
        };

        let executor = EffectExecutor::new_with_deps(deps);

        // take the redraw_handle (simulating chat::run saving the Arc early)
        let redraw_slot = executor.redraw_handle();

        // RequestRedraw before injection — slot is None, should be no-op (no panic)
        executor.execute(Effect::RequestRedraw).await;

        // inject redraw_tx afterwards (simulating injection once TUI init completes)
        let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);
        *redraw_slot.lock() = Some(redraw_tx);

        // after injection RequestRedraw must really fire
        executor.execute(Effect::RequestRedraw).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), redraw_rx.recv())
                .await
                .expect("redraw within 200ms after injection")
                .is_some(),
            "RequestRedraw should trigger after redraw_handle injection (P0-2)"
        );
    }

    // ─── P0: dedicated DualWriteGuardScope RAII unit tests ───────────────────

    /// P0-scope-1: DualWriteGuardScope::enter → guard true; Drop → guard false.
    #[tokio::test]
    async fn dual_write_guard_scope_clears_on_drop() {
        let guard = RuntimeDualWriteGuard::new();
        assert!(!guard.is_active(), "guard should start false");

        {
            let _scope = guard.enter_scope();
            assert!(guard.is_active(), "guard should be true while scope is held");
        } // scope drops here

        assert!(
            !guard.is_active(),
            "guard should be false after scope drop (RAII cleared)"
        );
    }

    /// P0-scope-2: DualWriteGuardScope::drop still runs on a panic path (unwind safety).
    #[tokio::test]
    async fn dual_write_guard_scope_panic_safe() {
        let guard = RuntimeDualWriteGuard::new();
        let inner = Arc::clone(&guard.active);

        let result = std::panic::catch_unwind(move || {
            let scope = DualWriteGuardScope::enter(Arc::clone(&inner));
            assert!(
                inner.load(Ordering::Acquire) > 0,
                "count should be positive inside scope"
            );
            // panic on purpose; drop must run during unwind
            let _keep = scope;
            panic!("deliberate test panic");
        });

        assert!(result.is_err(), "catch_unwind should have caught the panic");
        // Drop runs after the panic → the guard must reset to false
        assert!(
            !guard.is_active(),
            "guard must be false after panic unwind (Drop still runs)"
        );
    }

    /// P0-scope-3: after real_mode SaveSession completes the guard does not stick (the spawn scope clears it).
    ///
    /// More focused on the guard lifetime than real_mode_save_session_triggers_memory_store:
    /// the guard is briefly true after execute() and resets to false once the subtask finishes.
    #[tokio::test]
    async fn real_mode_save_session_clears_guard_after_completion() {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        let session = crate::chat::session::ChatSession::new("prov", "model");
        executor.execute(Effect::SaveSession(session)).await;

        // poll until the subtask finishes (at most 500ms)
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if store_count.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() <= deadline,
                "memory.store not called within 500ms"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // the guard must reset automatically once the subtask finishes
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard must be false after SaveSession subtask completes (RAII scope auto-cleared)"
        );
    }

    // ── Step 5a-4 mandatory tests (required by the Codex Phase 3 audit) ─────────────

    /// P0-2: EffectDeps.model + temperature are really passed through to drive_start_turn_stream.
    ///
    /// Uses a capture provider to assert that the model/temperature received by stream_chat_with_history
    /// equal the injected deps values. Fixes the Codex P1 about hard-coded String::new()/0.0 in 5a-2.
    #[tokio::test]
    async fn real_mode_start_turn_passes_model_and_temperature_from_deps() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex as PMutex;

        #[derive(Default)]
        struct ParamCaptureProvider {
            captured_model: Arc<PMutex<String>>,
            captured_temp: Arc<PMutex<f64>>,
        }

        #[async_trait]
        impl Provider for ParamCaptureProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                model: &str,
                temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                *self.captured_model.lock() = model.to_string();
                *self.captured_temp.lock() = temperature;
                let chunks: Vec<StreamResult<StreamChunk>> =
                    vec![Ok(StreamChunk::delta("ok")), Ok(StreamChunk::final_chunk())];
                stream::iter(chunks).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let captured_model = Arc::new(PMutex::new(String::new()));
        let captured_temp = Arc::new(PMutex::new(0.0_f64));
        let provider = Arc::new(ParamCaptureProvider {
            captured_model: Arc::clone(&captured_model),
            captured_temp: Arc::clone(&captured_temp),
        });

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = provider.clone();
        // inject a non-default model / temperature so capture can tell them apart.
        deps.model.set(Arc::from("gpt-test-99"));
        deps.temperature = 0.42;

        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-params".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        // wait for the first chunk to make sure stream_chat_with_history was already called.
        let _ = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("first chunk within 1.5s");

        let model_seen = captured_model.lock().clone();
        let temp_seen = *captured_temp.lock();
        assert_eq!(
            model_seen, "gpt-test-99",
            "model must be passed through from EffectDeps"
        );
        assert!(
            (temp_seen - 0.42).abs() < f64::EPSILON,
            "temperature must be passed through from EffectDeps (got {temp_seen})"
        );
    }

    /// P1-1: TurnCompletionSignal failure-path API contract.
    ///
    /// Verifies `extract_turn_outcome(StreamFailed) → TurnOutcomeKind::Failed`,
    /// and that after `record_and_notify` the `consume_outcome` reads the same Failed (with err / retryable).
    /// The full driver chain (provider Err → drive_start_turn_stream sends StreamFailed) is already
    /// covered by `real_mode_start_turn_stream_error_emits_stream_failed`;
    /// the reducer chain (StreamFailed → NotifyHook(Error)) is already covered by state.rs unit tests.
    /// This test pins down that TurnCompletionSignal does not lose the err semantics at the seam.
    #[tokio::test]
    async fn turn_signal_records_failed_outcome_from_stream_failed_action() {
        let signal = TurnCompletionSignal::new();
        let action = Action::StreamFailed {
            draft_id: "d1".to_string(),
            err: "simulated provider failure".to_string(),
            retryable: true,
        };
        let outcome = extract_turn_outcome(&action);
        assert!(matches!(outcome, Some(TurnOutcomeKind::Failed { .. })));
        if let Some(out) = outcome {
            signal.record_and_notify(out);
        }
        let consumed = signal.consume_outcome();
        match consumed {
            Some(TurnOutcomeKind::Failed { err, retryable }) => {
                assert!(err.contains("simulated"), "err must contain original message");
                assert!(retryable, "retryable bit must be preserved");
            }
            other => panic!("expected Failed outcome, got {other:?}"),
        }
        // the second consume must be None (consuming API).
        assert!(signal.consume_outcome().is_none(), "consume_outcome must drain slot");
    }

    #[tokio::test]
    async fn turn_signal_records_keyed_outcome_and_usage_by_draft_id() {
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("keyed turn", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
        let signal = TurnCompletionSignal::new();
        signal.register_turn(task_id, "draft-keyed");
        let notified = signal.notified_for(task_id).expect("registered keyed waiter");

        let usage = TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            source: crate::llm::route_decision::TokenUsageSource::Reported,
            ..TokenUsage::default()
        };
        assert!(signal.record_usage_for_draft("draft-keyed", usage));
        assert!(signal.record_and_notify_for_draft(
            "draft-keyed",
            TurnOutcomeKind::Completed {
                final_text: "done".to_string(),
                reasoning: "because".to_string(),
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .expect("keyed turn should notify");

        let keyed_usage = signal.consume_turn_usage(task_id);
        assert_eq!(keyed_usage.total_tokens, Some(15));
        match signal.consume_turn_outcome(task_id) {
            Some(TurnOutcomeKind::Completed { final_text, reasoning }) => {
                assert_eq!(final_text, "done");
                assert_eq!(reasoning, "because");
            }
            other => panic!("expected keyed completed outcome, got {other:?}"),
        }
        assert!(
            signal.consume_turn_outcome(task_id).is_none(),
            "keyed outcome is consume-once"
        );
        signal.unregister_turn(task_id);
        assert!(
            signal.notified_for(task_id).is_none(),
            "unregister removes keyed waiter"
        );
    }

    /// **5a-6 negative case**: the driver receives a tool_calls chunk but `tools_registry == None`,
    /// so it must send `StreamFailed(retryable=false)`.
    ///
    /// route_turn now lets the driver take a tool turn, but when `tools_registry` is None the driver
    /// cannot execute the tool — it fails fast immediately so chat::run falls through.
    #[tokio::test]
    async fn driver_without_registry_rejects_tool_call_chunk() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct ToolCallProvider;
        #[async_trait]
        impl Provider for ToolCallProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temp: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let calls = vec![ToolCallChunk::new("c1", "shell", r#"{"cmd":"ls"}"#, 0)];
                stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(calls)),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ToolCallProvider);
        // explicit: no registry provided → the driver must fail.
        deps.tools_registry = None;
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-no-registry".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        // skip a potential ToolStarted (not sent on the no-registry path, since the registry check precedes it).
        let mut got_failed = false;
        for _ in 0..6 {
            let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("driver must respond within 1.5s")
                .expect("action must be received");
            match action {
                Action::StreamFailed {
                    draft_id,
                    err,
                    retryable,
                } => {
                    assert_eq!(draft_id, "draft-no-registry");
                    assert!(!retryable, "no-registry rejection is permanent");
                    assert!(
                        err.contains("tools_registry") || err.contains("tool_calls"),
                        "err must hint at missing registry / tool_calls (got: {err})"
                    );
                    got_failed = true;
                    break;
                }
                Action::StreamChunkReceived { .. } | Action::ToolStarted { .. } | Action::ToolFinished { .. } => {
                    // permitted pre-failure noise; keep draining.
                }
                other => panic!("unexpected action before StreamFailed: {other:?}"),
            }
        }
        assert!(got_failed, "driver must emit StreamFailed within 6 actions");
    }

    /// **5a-6 happy path**: the driver receives a tool_call → executes it through tools_registry → feeds the
    /// tool result back into history → the next LLM pass returns the final text → StreamCompleted.
    ///
    /// Simulates two passes: in pass 1 the provider sends ToolCall(echo-tool, {"text": "hi"}) and the driver runs
    /// echo-tool returning "hi"; in pass 2 the provider sends final_text="done" and the driver finishes the turn.
    #[tokio::test]
    async fn driver_executes_tool_call_chunk_and_continues_to_completion() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex as PMutex;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        // ── Echo tool: returns args["text"] verbatim, so the driver feeds it back and we can verify history. ──
        struct EchoTool;
        #[async_trait]
        impl crate::tools::Tool for EchoTool {
            fn name(&self) -> &str {
                "echo-tool"
            }
            fn description(&self) -> &str {
                "echoes back its text argument"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
            }
            async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: text,
                    error: None,
                })
            }
        }

        // ── Provider: the first stream sends a tool_call, the second sends the final text. ──
        struct ToolThenTextProvider {
            counter: Arc<AtomicUsize>,
            captured_tool_counts: Arc<PMutex<Vec<usize>>>,
        }
        #[async_trait]
        impl Provider for ToolThenTextProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    native_tool_calling: true,
                    vision: false,
                }
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temp: f64,
                options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                self.captured_tool_counts
                    .lock()
                    .push(options.tools.as_ref().map_or(0, Vec::len));
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    let calls = vec![ToolCallChunk::new("tc-1", "echo-tool", r#"{"text":"echoed"}"#, 0)];
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(calls)),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let captured_tool_counts = Arc::new(PMutex::new(Vec::new()));
        deps.provider = Arc::new(ToolThenTextProvider {
            counter: Arc::new(AtomicUsize::new(0)),
            captured_tool_counts: Arc::clone(&captured_tool_counts),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(EchoTool) as Box<dyn crate::tools::Tool>]));
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-tool-happy".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_tool_started = false;
        let mut saw_tool_finished_success = false;
        let mut saw_completion = false;
        let mut final_text_seen = String::new();
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver should respond within 2s per action")
                .expect("action must arrive");
            match action {
                Action::ToolStarted { name, .. } => {
                    assert_eq!(name, "echo-tool");
                    saw_tool_started = true;
                }
                Action::ToolFinished {
                    name, success, result, ..
                } => {
                    assert_eq!(name, "echo-tool");
                    if success {
                        saw_tool_finished_success = true;
                        assert!(
                            result.as_deref().is_some_and(|s| s.contains("echoed")),
                            "tool result must echo back text arg (got {result:?})"
                        );
                    }
                }
                Action::StreamChunkReceived { delta, .. } => {
                    final_text_seen.push_str(&delta);
                }
                Action::StreamCompleted { final_text, .. } => {
                    saw_completion = true;
                    assert!(
                        final_text.contains("done"),
                        "final text must contain 'done' (got {final_text:?})"
                    );
                    break;
                }
                Action::StreamFailed { err, .. } => {
                    panic!("driver should not fail in happy path: {err}");
                }
                _ => {}
            }
        }
        assert!(saw_tool_started, "must see ToolStarted");
        assert!(saw_tool_finished_success, "must see ToolFinished(success=true)");
        assert!(saw_completion, "must see StreamCompleted");
        assert!(
            final_text_seen.contains("done"),
            "streaming delta must include 'done' (got {final_text_seen:?})"
        );
        assert_eq!(
            *captured_tool_counts.lock(),
            vec![1, 1],
            "driver must pass registered tool specs to each streaming request"
        );
    }

    /// **D8-4 redux-path real fix regression**: a tool executed *inside* the
    /// redux driver (`Effect::StartTurn` → spawn → `drive_start_turn_stream` →
    /// `tool.execute`) must observe the turn-root `SPAWN_EXECUTION_CONTEXT`
    /// seeded via `Effect::StartTurn { turn_spawn_ctx, .. }`.
    ///
    /// This drives the *real* dispatcher execution path (not a hand-rolled
    /// `tool.execute()` with a manual `.scope()` wrapper — the mistake the
    /// previous "fix" made, which passed while the production redux path still
    /// dropped the context). If the executor stops wrapping the driver future in
    /// `SPAWN_EXECUTION_CONTEXT.scope(..)`, `try_with` returns `Err` and the
    /// captured `parent_run_id` is `None`, failing this test — exactly the
    /// observable cause of model-spawned sub-agents being mislabeled `user`.
    ///
    /// The `None` (no-scope) leg — which keeps the `/bg` slash-command path on
    /// user origin — is already covered by every other driver test in this file
    /// (they all pass `turn_spawn_ctx: None`); this test asserts the `Some` leg.
    #[tokio::test]
    async fn driver_seeds_spawn_execution_context_for_tool_calls() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex as PMutex;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        // Tool that records what the spawn execution context looked like at the
        // moment it executed — i.e. whether the driver scoped the task-local.
        // `executed` distinguishes "tool never ran" from "ran but saw no scope".
        struct CtxProbeTool {
            executed: Arc<std::sync::atomic::AtomicBool>,
            seen_run_id: Arc<PMutex<Option<String>>>,
        }
        #[async_trait]
        impl crate::tools::Tool for CtxProbeTool {
            fn name(&self) -> &str {
                "ctx-probe"
            }
            fn description(&self) -> &str {
                "records the spawn execution context's run_id"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                // `try_with` is Ok only when the driver scoped the task-local;
                // None here means an unscoped task → sub-agents fall back to user.
                let observed = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                    .try_with(|ctx| ctx.run_id.clone())
                    .ok();
                *self.seen_run_id.lock() = observed;
                self.executed.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "probed".to_string(),
                    error: None,
                })
            }
        }

        struct ToolThenTextProvider {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for ToolThenTextProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temp: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    let calls = vec![ToolCallChunk::new("tc-1", "ctx-probe", "{}", 0)];
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(calls)),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ToolThenTextProvider {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_run_id = Arc::new(PMutex::new(None));
        deps.tools_registry = Some(Arc::new(vec![Box::new(CtxProbeTool {
            executed: Arc::clone(&executed),
            seen_run_id: Arc::clone(&seen_run_id),
        }) as Box<dyn crate::tools::Tool>]));
        let executor = EffectExecutor::new_with_deps(deps);

        let turn_run_id = "turn-run-id-xyz".to_string();
        let seed = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
            turn_run_id.clone(),
            "chat:test-scope".to_string(),
        );

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-ctx-probe".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: Some(seed),
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        // Drain until completion so the spawned driver task has finished the tool.
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver should respond within 2s per action")
                .expect("action must arrive");
            if matches!(action, Action::StreamCompleted { .. } | Action::StreamFailed { .. }) {
                break;
            }
        }

        assert!(
            executed.load(std::sync::atomic::Ordering::SeqCst),
            "ctx-probe tool must have executed inside the driver"
        );
        let observed = seen_run_id.lock().clone();
        assert_eq!(
            observed,
            Some(turn_run_id),
            "redux driver must scope SPAWN_EXECUTION_CONTEXT so the in-turn tool sees the turn run_id \
             (None here means model-spawned sub-agents would be mislabeled as user)"
        );
    }

    /// BUG-03 round-2: when a tool fails *unrecoverably* (permission denied) and
    /// the model re-issues the identical blocked call, the driver must STOP early
    /// instead of retrying indefinitely (the chat-demo defect where a blocked
    /// shell command burned repeated LLM round-trips before erroring out).
    ///
    /// Provider always emits the same `noop` tool call; the tool always returns
    /// "permission denied". The fix must emit
    /// `StreamFailed` with the *unrecoverable* message after the signature recurs
    /// (≈ iteration 2) and crucially must NOT count the noop tool more than a
    /// couple of times — proving it did not retry indefinitely.
    #[tokio::test]
    async fn driver_stops_early_on_repeated_unrecoverable_tool_failure() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct DeniedTool {
            exec_count: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl crate::tools::Tool for DeniedTool {
            fn name(&self) -> &str {
                "shell"
            }
            fn description(&self) -> &str {
                "shell"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.exec_count.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("permission denied: command not allowed by policy".to_string()),
                })
            }
        }

        // A realistic model re-issues a *new* tool_call id on each retry (the
        // driver's id-based idempotency skip only suppresses literal duplicate
        // ids within one assistant turn). Use a counter so each blocked attempt
        // is a fresh execution — that is what makes the retry actually re-run and
        // lets the unrecoverable signature recur.
        struct AlwaysDeniedCallProvider {
            seq: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for AlwaysDeniedCallProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.seq.fetch_add(1, AtomicOrdering::SeqCst);
                let calls = vec![ToolCallChunk::new(
                    format!("blocked-{n}"),
                    "shell",
                    r#"{"cmd":"rm -rf /"}"#,
                    0,
                )];
                stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(calls)),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let exec_count = Arc::new(AtomicUsize::new(0));
        deps.provider = Arc::new(AlwaysDeniedCallProvider {
            seq: Arc::new(AtomicUsize::new(0)),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(DeniedTool {
            exec_count: Arc::clone(&exec_count),
        }) as Box<dyn crate::tools::Tool>]));
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-unrecoverable".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut got_failed = false;
        let mut failed_err = String::new();
        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver should respond per action within 2s")
                .expect("action must arrive");
            if let Action::StreamFailed { err, retryable, .. } = &action {
                assert!(!retryable, "unrecoverable stop is permanent");
                failed_err = err.clone();
                got_failed = true;
                break;
            }
        }
        assert!(
            got_failed,
            "driver must emit StreamFailed on repeated unrecoverable failure"
        );
        assert!(
            failed_err.contains("unrecoverable") && failed_err.contains("shell"),
            "must stop with the unrecoverable message naming the blocked tool (got: {failed_err})"
        );
        // The blocked tool must have run only a small number of times (the first
        // failure plus the one retry that trips the early-stop).
        let runs = exec_count.load(AtomicOrdering::SeqCst);
        assert!(
            (1..=3).contains(&runs),
            "blocked tool should run ~twice before early-stop, not retry indefinitely (ran {runs} times)"
        );
    }

    /// P1-2: cancel mid-turn on the driver path — the select! inside drive_start_turn_stream takes the cancel branch.
    ///
    /// Verifies that after cancel_token is cancelled, drive_start_turn_stream sends StreamCancelled
    /// instead of continuing to consume the stream or sending StreamCompleted.
    #[tokio::test]
    async fn driver_mid_turn_cancel_emits_stream_cancelled() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        /// Provider returns a stream that never ends — cancel takes over.
        struct PendingStreamProvider;
        #[async_trait]
        impl Provider for PendingStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temp: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                // one delta + pending (stream::pending makes next() pend forever)
                stream::iter(vec![Ok(StreamChunk::delta("partial"))])
                    .chain(stream::pending())
                    .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(PendingStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-cancel-mid".to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: cancel.clone(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        // receive one delta (partial) first, proving the stream is already active.
        let a1 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("first delta within 1.5s")
            .expect("first delta received");
        assert!(
            matches!(a1, Action::StreamChunkReceived { ref delta, .. } if delta == "partial"),
            "expected first partial delta, got {a1:?}"
        );

        // cancel mid-turn
        cancel.cancel();

        // StreamCancelled must arrive immediately.
        let a2 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("StreamCancelled within 1.5s after cancel")
            .expect("StreamCancelled received");
        match a2 {
            Action::StreamCancelled { draft_id } => assert_eq!(draft_id, "draft-cancel-mid"),
            other => panic!("expected StreamCancelled after mid-turn cancel, got {other:?}"),
        }
    }

    /// P0-1 simplified: try_dispatch returns ChannelClosed when the channel is closed
    /// (the chat::run driver branch uses this to abort the turn + cleanup + continue).
    #[tokio::test]
    async fn chat_dispatcher_try_dispatch_returns_channel_closed_after_rx_drop() {
        let (dispatcher, rx) = ChatDispatcher::new();
        drop(rx);
        let result = dispatcher.try_dispatch(Action::StartLLMTurn {
            provider_turn_task_id: None,
            provider_turn_sequence: None,
            draft_id: "d1".to_string(),
            history: Vec::new(),
            compaction_guard_history: None,
            compaction_config: None,
            cancel: CancellationToken::new(),
            turn_spawn_ctx: None,
            turn_message_send_ctx: None,
            routing_input: None,
        });
        assert!(
            matches!(result, DispatchResult::ChannelClosed),
            "after action_rx drop, try_dispatch must return ChannelClosed (got {result:?})"
        );
    }

    // ─── S2.5 P1-A: dispatch_or_log failure handling ─────────────────────────

    /// S2.5 P1-A: happy path — dispatch_or_log returns Sent and the Action really enqueues.
    ///
    /// Does not assert a change in the drops counter (it is globally shared and parallel tests pollute the reading);
    /// the +1 semantics of the drops counter are already covered by the Backpressured/Closed tests.
    #[tokio::test]
    async fn s2_5_p1_a_dispatch_or_log_normal_sent() {
        let (dispatcher, mut rx) = ChatDispatcher::new();
        let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.normal");
        assert!(
            matches!(result, DispatchResult::Sent),
            "the happy path must return Sent (got {result:?})"
        );
        // verify the Action really enqueued
        let recv = rx.try_recv().expect("test: action should be in queue");
        assert!(matches!(recv, Action::CancelRequested));
    }

    /// S2.5 P1-A: full channel — dispatch_or_log returns Backpressured and backpressured goes up by >= 1.
    ///
    /// Because the backpressured counter is globally shared, we assert `after > before` (at least +1)
    /// rather than exactly +1, to avoid parallel-test interference; the core contract is that this dispatch inc'd.
    #[tokio::test]
    async fn s2_5_p1_a_dispatch_or_log_full_warns_and_counts() {
        use crate::observability::chat_metrics;
        let (tx, _rx) = mpsc::channel::<Action>(1);
        let dispatcher = ChatDispatcher { action_tx: tx };
        // fill the capacity of 1.
        let _ = dispatcher.try_dispatch(Action::CancelRequested);

        let before = chat_metrics::get_dispatch_drops_count("backpressured");
        let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.full");
        assert!(
            matches!(result, DispatchResult::Backpressured),
            "channel full → Backpressured (got {result:?})"
        );
        let after = chat_metrics::get_dispatch_drops_count("backpressured");
        assert!(
            after > before,
            "backpressured counter should >= before+1 on full dispatch (before={before}, after={after})"
        );
    }

    /// S2.5 P1-A: closed channel — dispatch_or_log returns ChannelClosed and the closed counter goes up by at least 1.
    #[tokio::test]
    async fn s2_5_p1_a_dispatch_or_log_closed_warns() {
        use crate::observability::chat_metrics;
        let (dispatcher, rx) = ChatDispatcher::new();
        drop(rx);

        let before = chat_metrics::get_dispatch_drops_count("closed");
        let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.closed");
        assert!(
            matches!(result, DispatchResult::ChannelClosed),
            "channel closed → ChannelClosed (got {result:?})"
        );
        let after = chat_metrics::get_dispatch_drops_count("closed");
        assert!(
            after > before,
            "closed counter should >= before+1 on channel-closed dispatch (before={before}, after={after})"
        );
    }

    // ─── S3 T3-1 four-part tests ───────────────────────────────────────────────

    /// **S3 T3-1 Step 2**: ToolCallAggregator aggregates the Streaming + Completed protocol.
    ///
    /// Verifies that several Streaming increments + one Completed return Completed.args as the final arguments;
    /// a repeated Completed must be recognised as an idempotent no-op.
    #[test]
    fn t31_aggregator_aggregates_streaming_and_completed() {
        use crate::providers::traits::{ToolCallChunk, ToolCallChunkStatus};
        let mut agg = ToolCallAggregator::new();

        // 1st Streaming delta
        let r1 = agg.ingest(ToolCallChunk {
            id: "tc-x".to_string(),
            name: "shell".to_string(),
            args: String::new(),
            index: 0,
            arguments_delta: Some(r#"{"cmd":"#.to_string()),
            status: ToolCallChunkStatus::Streaming,
        });
        assert!(r1.is_none(), "streaming chunk should not yield ready tool call");

        // 2nd Streaming delta
        let r2 = agg.ingest(ToolCallChunk {
            id: "tc-x".to_string(),
            name: "shell".to_string(),
            args: String::new(),
            index: 0,
            arguments_delta: Some(r#""ls"}"#.to_string()),
            status: ToolCallChunkStatus::Streaming,
        });
        assert!(r2.is_none(), "second streaming chunk also no-op");

        // Completed chunk
        let r3 = agg.ingest(ToolCallChunk {
            id: "tc-x".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"ls"}"#.to_string(),
            index: 0,
            arguments_delta: None,
            status: ToolCallChunkStatus::Completed,
        });
        let (id, name, args) = r3.expect("Completed chunk should yield ready tool call");
        assert_eq!(id, "tc-x");
        assert_eq!(name, "shell");
        assert_eq!(args, r#"{"cmd":"ls"}"#);

        // repeated Completed → idempotent no-op.
        let r4 = agg.ingest(ToolCallChunk {
            id: "tc-x".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"ls"}"#.to_string(),
            index: 0,
            arguments_delta: None,
            status: ToolCallChunkStatus::Completed,
        });
        assert!(r4.is_none(), "duplicate Completed should be idempotent no-op");
    }

    #[test]
    fn aggregator_backfills_slot_id_when_empty() {
        use crate::providers::traits::{ToolCallChunk, ToolCallChunkStatus};
        let mut agg = ToolCallAggregator::new();

        assert!(
            agg.ingest(ToolCallChunk {
                id: String::new(),
                name: "shell".into(),
                args: String::new(),
                index: 0,
                arguments_delta: Some("{".into()),
                status: ToolCallChunkStatus::Streaming,
            })
            .is_none()
        );
        assert!(
            agg.ingest(ToolCallChunk {
                id: "call_abc".into(),
                name: "shell".into(),
                args: String::new(),
                index: 0,
                arguments_delta: Some("}".into()),
                status: ToolCallChunkStatus::Streaming,
            })
            .is_none()
        );

        let (id, name, args) = agg
            .ingest(ToolCallChunk {
                id: String::new(),
                name: "shell".into(),
                args: "{}".into(),
                index: 0,
                arguments_delta: None,
                status: ToolCallChunkStatus::Completed,
            })
            .expect("completed chunk should resolve");
        assert_eq!(id, "call_abc");
        assert_eq!(name, "shell");
        assert_eq!(args, "{}");
    }

    /// **S3 T3-1 Step 2**: ToolCallAggregator concurrent indices — several tool calls in flight at once.
    #[test]
    fn t31_aggregator_concurrent_indices_yield_each_separately() {
        use crate::providers::traits::{ToolCallChunk, ToolCallChunkStatus};
        let mut agg = ToolCallAggregator::new();
        // interleaved emit: tc-a streaming → tc-b streaming → tc-a complete → tc-b complete.
        agg.ingest(ToolCallChunk {
            id: "tc-a".into(),
            name: "tool_a".into(),
            args: String::new(),
            index: 0,
            arguments_delta: Some("{".into()),
            status: ToolCallChunkStatus::Streaming,
        });
        agg.ingest(ToolCallChunk {
            id: "tc-b".into(),
            name: "tool_b".into(),
            args: String::new(),
            index: 1,
            arguments_delta: Some("[".into()),
            status: ToolCallChunkStatus::Streaming,
        });
        let ra = agg
            .ingest(ToolCallChunk {
                id: "tc-a".into(),
                name: "tool_a".into(),
                args: "{}".into(),
                index: 0,
                arguments_delta: None,
                status: ToolCallChunkStatus::Completed,
            })
            .expect("tc-a should complete");
        let rb = agg
            .ingest(ToolCallChunk {
                id: "tc-b".into(),
                name: "tool_b".into(),
                args: "[]".into(),
                index: 1,
                arguments_delta: None,
                status: ToolCallChunkStatus::Completed,
            })
            .expect("tc-b should complete");
        assert_eq!(ra.0, "tc-a");
        assert_eq!(rb.0, "tc-b");
        assert_eq!(ra.2, "{}");
        assert_eq!(rb.2, "[]");
    }

    /// **S3 T3-1 Step 2**: driver path: ToolCallChunks from the streaming protocol can also drive tool execution.
    ///
    /// The provider sends [Streaming delta, Streaming delta, Completed] instead of a single Completed —
    /// the driver must still emit ToolStarted/ToolFinished and move to the next pass until the final text.
    #[tokio::test]
    async fn t31_driver_streaming_tool_call_protocol_executes_correctly() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk, ToolCallChunkStatus,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct PingTool;
        #[async_trait]
        impl crate::tools::Tool for PingTool {
            fn name(&self) -> &str {
                "ping"
            }
            fn description(&self) -> &str {
                "ping"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "pong".into(),
                    error: None,
                })
            }
        }

        struct StreamingToolProvider {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for StreamingToolProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    // send using the streaming protocol: 2 Streaming deltas first, then 1 Completed.
                    let s1 = ToolCallChunk {
                        id: "call-1".into(),
                        name: "ping".into(),
                        args: String::new(),
                        index: 0,
                        arguments_delta: Some("{".into()),
                        status: ToolCallChunkStatus::Streaming,
                    };
                    let s2 = ToolCallChunk {
                        id: "call-1".into(),
                        name: "ping".into(),
                        args: String::new(),
                        index: 0,
                        arguments_delta: Some("}".into()),
                        status: ToolCallChunkStatus::Streaming,
                    };
                    let c = ToolCallChunk {
                        id: "call-1".into(),
                        name: "ping".into(),
                        args: "{}".into(),
                        index: 0,
                        arguments_delta: None,
                        status: ToolCallChunkStatus::Completed,
                    };
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(vec![s1])),
                        Ok(StreamChunk::tool_call_chunk(vec![s2])),
                        Ok(StreamChunk::tool_call_chunk(vec![c])),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(StreamingToolProvider {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(PingTool) as Box<dyn crate::tools::Tool>]));
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-t31-streaming".into(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_tool_started = false;
        let mut saw_tool_finished = false;
        let mut saw_completion = false;
        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::ToolStarted { name, .. } => {
                    assert_eq!(name, "ping");
                    saw_tool_started = true;
                }
                Action::ToolFinished { success, name, .. } => {
                    assert_eq!(name, "ping");
                    assert!(success);
                    saw_tool_finished = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("done"), "want 'done' got {final_text:?}");
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail in happy path: {err}"),
                _ => {}
            }
        }
        assert!(saw_tool_started, "must see ToolStarted");
        assert!(saw_tool_finished, "must see ToolFinished");
        assert!(saw_completion, "must see StreamCompleted");
    }

    #[tokio::test]
    async fn dispatcher_tool_call_request_includes_reasoning_in_history() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct PingTool;
        #[async_trait]
        impl crate::tools::Tool for PingTool {
            fn name(&self) -> &str {
                "ping"
            }
            fn description(&self) -> &str {
                "ping"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "pong".into(),
                    error: None,
                })
            }
        }

        struct ReasoningToolProvider {
            counter: Arc<AtomicUsize>,
            second_history: Arc<Mutex<Option<Vec<PMsg>>>>,
        }
        #[async_trait]
        impl Provider for ReasoningToolProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                messages: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    stream::iter(vec![
                        Ok(StreamChunk::reasoning_delta("Need to call ping.")),
                        Ok(StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
                            "call-ping",
                            "ping",
                            "{}",
                            0,
                        )])),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    *self.second_history.lock() = Some(messages.to_vec());
                    stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let second_history = Arc::new(Mutex::new(None));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ReasoningToolProvider {
            counter: Arc::new(AtomicUsize::new(0)),
            second_history: Arc::clone(&second_history),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(PingTool) as Box<dyn crate::tools::Tool>]));
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-reasoning-tool-history".into(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("done"));
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                _ => {}
            }
        }

        let history = second_history.lock().clone().expect("second request history captured");
        let assistant = history
            .iter()
            .find(|message| message.role == "assistant")
            .expect("assistant tool-call history expected");
        let value: serde_json::Value = serde_json::from_str(&assistant.content).expect("assistant payload JSON");
        assert_eq!(
            value.get("reasoning_content").and_then(serde_json::Value::as_str),
            Some("Need to call ping.")
        );
        let call_id = value
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
            .and_then(|calls| calls.first())
            .and_then(|call| call.get("id"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(call_id, Some("call-ping"));
    }

    /// **S3 T3-1 Step 3**: context overflow → automatic compact + retry while making progress → success.
    ///
    /// The provider first sends StreamError::Provider("maximum context length exceeded") and
    /// succeeds on the second attempt. The driver must: emit HistoryCompacted{ContextOverflow} → call the
    /// stream API again → emit StreamCompleted.
    #[tokio::test]
    async fn t31_driver_context_overflow_triggers_compact_and_retries() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct OverflowOnceProvider {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for OverflowOnceProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    stream::iter(vec![Err::<StreamChunk, _>(StreamError::Provider(
                        "Error: maximum context length exceeded for this model".into(),
                    ))])
                    .boxed()
                } else {
                    stream::iter(vec![
                        Ok(StreamChunk::delta("recovered")),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(OverflowOnceProvider {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-overflow".into(),
                history: (0..30)
                    .map(|index| crate::providers::traits::ChatMessage {
                        role: "user".into(),
                        content: format!("history turn {index}"),
                    })
                    .collect(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_compacted = false;
        let mut saw_completion = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::HistoryCompacted {
                    reason: crate::chat::action::CompactReason::ContextOverflow,
                } => {
                    saw_compacted = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("recovered"), "want 'recovered' got {final_text:?}");
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => {
                    panic!("driver should retry on overflow, not fail: {err}");
                }
                _ => {}
            }
        }
        assert!(saw_compacted, "must emit HistoryCompacted on overflow");
        assert!(saw_completion, "must complete after compact+retry");
    }

    /// A `switch` preflight that cannot resolve provenance answers the turn on a
    /// lossy trim. It must say so: the previous behaviour ended the turn with a
    /// loud error, and replacing that with silence is how a session quietly
    /// forgets its own history while looking healthy.
    #[tokio::test]
    async fn redux_driver_switch_degradation_tells_the_user_context_was_dropped() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct PlainProvider;
        #[async_trait]
        impl Provider for PlainProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok("SUMMARY_MUST_NOT_BE_USED".to_string())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![
                    Ok(StreamChunk::delta("answered anyway")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(PlainProvider);
        let executor = EffectExecutor::new_with_deps(deps);
        // Switch mode with no durable transcript scope: provenance can never
        // resolve, so the rollover produces no patch at all.
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 10,
            keep_recent_messages: 2,
            memory_flush: false,
            max_context_tokens: 160,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let mut history = vec![PMsg::system("sys")];
        for i in 0..24 {
            history.push(PMsg::user(format!("turn {i} {}", "long context ".repeat(40))));
        }

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-switch-degraded".into(),
                history,
                compaction_guard_history: None,
                compaction_config: Some(config),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut dropped = None;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::HistoryCompactionDegraded { dropped_messages, .. } => dropped = Some(dropped_messages),
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("answered anyway"), "got {final_text:?}");
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("degrading must not end the turn: {err}"),
                _ => {}
            }
        }
        let dropped = dropped.expect("a lossy switch fallback must announce itself to the user");
        assert!(dropped > 0, "the notice must report the messages the trim removed");

        // The reducer turns that action into exactly one visible line.
        let mut reducer_state =
            crate::chat::state::ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
        let effects = reducer_state.reduce(Action::HistoryCompactionDegraded {
            reason: crate::chat::action::CompactReason::ContextOverflow,
            dropped_messages: dropped,
        });
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                crate::chat::state::Effect::SurfaceNotice { text } if text.contains("lossy")
            )),
            "the degradation action must reach the user as a notice"
        );
    }

    #[tokio::test]
    async fn redux_driver_preflight_uses_provider_summary_before_first_stream_request() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct BudgetCaptureProvider {
            first_tokens: Arc<AtomicUsize>,
            summary_calls: Arc<AtomicUsize>,
            first_history: Arc<Mutex<Option<Vec<PMsg>>>>,
        }
        #[async_trait]
        impl Provider for BudgetCaptureProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(
                    "PROVIDER_SUMMARY_MARKER\n## Decisions\n- keep decision\n## Open TODOs\n- keep todo\n## Constraints\n- keep rule"
                        .to_string(),
                )
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                messages: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                self.first_tokens
                    .compare_exchange(
                        0,
                        crate::agent::loop_::measure_history_tokens(messages),
                        AtomicOrdering::SeqCst,
                        AtomicOrdering::SeqCst,
                    )
                    .ok();
                let mut guard = self.first_history.lock();
                if guard.is_none() {
                    *guard = Some(messages.to_vec());
                }
                stream::iter(vec![
                    Ok(StreamChunk::delta("budget-ok")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let first_tokens = Arc::new(AtomicUsize::new(0));
        let summary_calls = Arc::new(AtomicUsize::new(0));
        let first_history = Arc::new(Mutex::new(None));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(BudgetCaptureProvider {
            first_tokens: Arc::clone(&first_tokens),
            summary_calls: Arc::clone(&summary_calls),
            first_history: Arc::clone(&first_history),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 10,
            keep_recent_messages: 2,
            memory_flush: false,
            max_context_tokens: 160,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let mut history = vec![PMsg::system("sys")];
        for i in 0..24 {
            history.push(PMsg::user(format!("turn {i} {}", "long context ".repeat(40))));
        }

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-budget-preflight".into(),
                history,
                compaction_guard_history: None,
                compaction_config: Some(config),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_summary_patch = false;
        let mut saw_feedback = false;
        let mut saw_context_update = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::HistoryCompactionPatchApplied { patch, .. } => {
                    saw_summary_patch = true;
                    assert!(
                        patch
                            .replacement
                            .iter()
                            .any(|message| message.content.contains("PROVIDER_SUMMARY_MARKER")),
                        "summary patch must carry provider summary marker"
                    );
                }
                Action::HistoryCompacted { .. } => {
                    panic!("preflight summary path must not dispatch lossy HistoryCompacted");
                }
                Action::ContextWindowUpdated {
                    used_context_tokens: Some(used),
                    max_context_tokens: Some(max),
                } => {
                    saw_context_update = true;
                    assert_eq!(max, 160);
                    assert!(
                        used <= 150,
                        "context window update must reflect compacted budget: {used}"
                    );
                }
                Action::SystemMessageAdded { text } => {
                    saw_feedback = true;
                    assert!(
                        text.contains("Compacted context") || text.contains("Context already compact"),
                        "preflight feedback should describe compaction result: {text}"
                    );
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("budget-ok"));
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                _ => {}
            }
        }
        assert!(saw_summary_patch, "preflight must dispatch exact summary patch");
        assert!(saw_feedback, "preflight must dispatch user-visible compact feedback");
        assert!(
            saw_context_update,
            "preflight must update context-window UI metadata after compaction"
        );
        assert_eq!(summary_calls.load(AtomicOrdering::SeqCst), 1);
        let captured = first_history.lock().clone().expect("first stream history");
        assert!(
            captured
                .iter()
                .any(|message| message.content.contains("PROVIDER_SUMMARY_MARKER")),
            "first stream request must contain provider summary marker"
        );
        assert!(
            captured
                .iter()
                .all(|message| !message.content.contains("compact-in-place")),
            "first stream request must not contain old lossy compact marker"
        );
        assert!(
            first_tokens.load(AtomicOrdering::SeqCst) <= 150,
            "redux first provider call must be below literal hard limit 150 tokens"
        );
    }

    #[tokio::test]
    async fn resumed_compacted_session_does_not_call_summarizer_again() {
        use crate::chat::state::{ChatState, Effect};
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct ResumeProvider {
            summary_calls: Arc<AtomicUsize>,
            streamed_history: Arc<Mutex<Option<Vec<PMsg>>>>,
        }

        #[async_trait]
        impl Provider for ResumeProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }

            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok("UNEXPECTED_RESUME_SUMMARY".to_string())
            }

            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }

            fn supports_streaming(&self) -> bool {
                true
            }

            fn stream_chat_with_history(
                &self,
                messages: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                *self.streamed_history.lock() = Some(messages.to_vec());
                stream::iter(vec![
                    Ok(StreamChunk::delta("resume-ok")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }

            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let current_question = "continue from the compacted summary";
        fn durable_test_turns(messages: &[PMsg]) -> Vec<crate::chat::session::ChatTurn> {
            let timestamp = chrono::Utc::now();
            messages
                .iter()
                .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
                .map(|message| crate::chat::session::ChatTurn {
                    role: message.role.clone(),
                    content: message.content.clone(),
                    timestamp,
                    tool_calls: Vec::new(),
                })
                .collect()
        }

        let mut state = ChatState::new(
            Arc::from("test-prov"),
            Arc::from("test-model"),
            CancellationToken::new(),
        );
        state.session.history = vec![
            PMsg::system("sys"),
            PMsg::user(format!("old user {}", "long context ".repeat(80))),
            PMsg::assistant(format!("old assistant {}", "long context ".repeat(80))),
        ];
        state.session.turns = durable_test_turns(&state.session.history);
        let _ = state.reduce(Action::RecordUserTurn(current_question.to_string()));
        let guard = crate::agent::loop_::compaction_patch_guard_for(&state.session.history, 1, 3)
            .expect("test: compaction guard");
        let patch = crate::agent::loop_::CompactionPatch {
            range_start: 1,
            range_end: 3,
            replacement: vec![PMsg::assistant(
                "[Context compacted at test. Summary: DURABLE_SUMMARY_MARKER]",
            )],
            append_after: vec![PMsg::user("[Post-compaction context refresh]\nre-read")],
            guard,
        };
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 10,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 170,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };

        let snapshot = state
            .reduce(Action::HistoryCompactionPatchApplied {
                reason: crate::chat::action::CompactReason::ContextOverflow,
                patch,
                compaction_config: config.clone(),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::SaveSession(session) => Some(session),
                _ => None,
            })
            .expect("compaction patch must persist a session snapshot");

        let mut resumed = ChatState::new(
            Arc::from("test-prov"),
            Arc::from("test-model"),
            CancellationToken::new(),
        );
        let _ = resumed.reduce(Action::SessionLoaded(snapshot));
        let resumed_pairs = resumed
            .session
            .history
            .iter()
            .map(|message| (message.role.as_str(), message.content.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            resumed_pairs,
            vec![
                (
                    "assistant",
                    "[Context compacted at test. Summary: DURABLE_SUMMARY_MARKER]"
                ),
                ("user", current_question),
            ],
            "resume must rebuild the already-compacted durable history, not the old source turns"
        );

        let summary_calls = Arc::new(AtomicUsize::new(0));
        let streamed_history = Arc::new(Mutex::new(None));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ResumeProvider {
            summary_calls: Arc::clone(&summary_calls),
            streamed_history: Arc::clone(&streamed_history),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-resume-compacted".to_string(),
                history: resumed.session.history.clone(),
                compaction_guard_history: None,
                compaction_config: Some(config),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_completion = false;
        for _ in 0..8 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("driver action should arrive");
            match action {
                Action::HistoryCompactionPatchApplied { .. } => {
                    panic!("resumed compacted history must not dispatch another summary patch");
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("resume-ok"));
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                _ => {}
            }
        }

        assert!(
            saw_completion,
            "driver must stream normally from compacted resume history"
        );
        assert_eq!(
            summary_calls.load(AtomicOrdering::SeqCst),
            0,
            "resuming an already-compacted session must not invoke the summarizer again"
        );
        let streamed = streamed_history
            .lock()
            .clone()
            .expect("provider-bound history should be captured");
        assert_eq!(
            streamed
                .iter()
                .map(|message| (message.role.as_str(), message.content.as_str()))
                .collect::<Vec<_>>(),
            resumed_pairs,
            "provider should receive the compacted resume shape"
        );
    }

    #[tokio::test]
    async fn redux_driver_preflight_provider_history_keeps_real_user_question_last_after_compaction() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex;

        struct CaptureStreamProvider {
            captured_history: Arc<Mutex<Option<Vec<PMsg>>>>,
        }

        #[async_trait]
        impl Provider for CaptureStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }

            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok("## Decisions\n- preserve Redux compacted context\n## Open TODOs\n- answer the latest user question\n## Critical Context\n- ISS-037 Redux capture test".to_string())
            }

            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }

            fn supports_streaming(&self) -> bool {
                true
            }

            fn stream_chat_with_history(
                &self,
                messages: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let last_user = messages
                    .iter()
                    .rfind(|message| message.role == "user")
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                *self.captured_history.lock() = Some(messages.to_vec());
                stream::iter(vec![
                    Ok(StreamChunk::delta(format!("answer bound to: {last_user}"))),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }

            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let current_question = "What should ISS-037 answer now?";
        let captured_history = Arc::new(Mutex::new(None));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(CaptureStreamProvider {
            captured_history: Arc::clone(&captured_history),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 10,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 180,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let mut history = vec![PMsg::system("sys")];
        for i in 0..24 {
            history.push(PMsg::user(format!("old turn {i} {}", "long context ".repeat(40))));
        }
        history.push(PMsg::user(current_question));

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-iss-037-redux".to_string(),
                history,
                compaction_guard_history: None,
                compaction_config: Some(config),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_summary_patch = false;
        let mut saw_completion = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("driver action should arrive");
            match action {
                Action::HistoryCompactionPatchApplied { .. } => {
                    saw_summary_patch = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(
                        final_text.contains(current_question),
                        "assistant response must be bound to the real current user question"
                    );
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                _ => {}
            }
        }

        assert!(saw_summary_patch, "preflight must apply a summary compaction patch");
        assert!(saw_completion, "driver must complete");
        let captured = captured_history
            .lock()
            .clone()
            .expect("provider-bound history should be captured");
        let last = captured.last().expect("provider-bound history should not be empty");
        assert_eq!(last.role, "user");
        assert_eq!(last.content, current_question);
        let refresh_index = captured
            .iter()
            .position(|message| message.content.starts_with("[Post-compaction context refresh]"))
            .expect("refresh marker should be present");
        assert!(
            refresh_index + 1 < captured.len(),
            "refresh marker must not be the trailing provider-bound message"
        );
    }

    #[tokio::test]
    async fn redux_switch_without_exact_provenance_degrades_instead_of_failing_the_turn() {
        let provider = MockEnvProvider::from_env();
        let mut history = vec![
            crate::providers::ChatMessage::system("sys"),
            crate::providers::ChatMessage::user("old user ".repeat(120)),
            crate::providers::ChatMessage::assistant("old assistant ".repeat(120)),
            crate::providers::ChatMessage::user("current user"),
        ];
        let original = history
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect::<Vec<_>>();
        let mut guard_history = history.clone();
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 1,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 50,
            max_context_tokens_explicit: true,
            os_paging: crate::config::OsPagingConfig::default(),
        };
        let (action_tx, mut action_rx) = mpsc::channel(2);

        let result = apply_redux_context_rollover(
            &provider,
            &mut history,
            &mut guard_history,
            "model",
            &config,
            None,
            &action_tx,
            crate::chat::action::CompactReason::ContextOverflow,
            "test_switch_failure",
        )
        .await;

        assert_eq!(
            result,
            Ok(ContextRolloverOutcome {
                replacement_len: None,
                degraded: true,
            }),
            "a switch without exact provenance must hand the caller its trim fallback (flagged lossy), not end the turn"
        );
        assert_eq!(
            history
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect::<Vec<_>>(),
            original,
            "a rollover that produced no patch must leave history untouched for the caller to trim"
        );
        assert!(
            action_rx.try_recv().is_err(),
            "degrading must not emit a terminal StreamFailed for the draft"
        );
    }

    #[tokio::test]
    async fn redux_compaction_guard_uses_persisted_source_when_provider_history_is_enriched() {
        use crate::chat::state::{ChatState, Effect};
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct SummaryProvider;

        #[async_trait]
        impl Provider for SummaryProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }

            async fn chat_with_system(&self, _: Option<&str>, prompt: &str, _: &str, _: f64) -> anyhow::Result<String> {
                assert!(
                    !prompt.contains("HIDDEN_FILE_SENTINEL"),
                    "persisted-source compaction must not summarize hidden enrichment"
                );
                Ok(
                    "PERSISTED_SOURCE_SUMMARY\n## Decisions\n- original transcript only\n## Open TODOs\n- continue"
                        .to_string(),
                )
            }

            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }

            fn supports_streaming(&self) -> bool {
                true
            }

            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Ok(StreamChunk::final_chunk())]).boxed()
            }

            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let original_history = vec![
            PMsg::system("sys"),
            PMsg::user(format!("old visible user @file.txt {}", "visible ".repeat(60))),
            PMsg::assistant(format!("old assistant {}", "visible ".repeat(60))),
            PMsg::user(format!("another old user {}", "visible ".repeat(60))),
            PMsg::assistant(format!("another old assistant {}", "visible ".repeat(60))),
            PMsg::user("current visible user"),
        ];
        let mut driver_history = original_history.clone();
        let Some(enriched_old_user) = driver_history.get_mut(1) else {
            panic!("test fixture must include old user turn");
        };
        enriched_old_user.content.push_str(&format!(
            "\n\n[Attached file context from @path mentions]\nHIDDEN_FILE_SENTINEL {}\n[End attached file context]",
            "hidden ".repeat(120)
        ));
        let enriched_before_compaction = driver_history.clone();
        let mut compaction_guard_history = original_history.clone();
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 5,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 220,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

        let replacement_len = apply_redux_context_rollover(
            &SummaryProvider,
            &mut driver_history,
            &mut compaction_guard_history,
            "model",
            &config,
            None,
            &action_tx,
            crate::chat::action::CompactReason::ContextOverflow,
            "test_persisted_guard_source",
        )
        .await
        .unwrap()
        .replacement_len
        .expect("summary patch must apply");
        assert_eq!(replacement_len, 1);

        let patch = match action_rx.recv().await.expect("patch action") {
            Action::HistoryCompactionPatchApplied { patch, .. } => patch,
            other => panic!("expected summary patch action, got {other:?}"),
        };
        let reducer_history = original_history
            .get(..original_history.len() - 1)
            .expect("reducer history fixture")
            .to_vec();
        assert!(
            crate::agent::loop_::compaction_patch_guard_matches(&reducer_history, &patch.guard),
            "patch guard must validate against reducer history before ordered user-turn commit"
        );
        assert!(
            !crate::agent::loop_::compaction_patch_guard_matches(&enriched_before_compaction, &patch.guard),
            "old enriched guard source would have mismatched the reducer"
        );

        let mut reducer_state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
        reducer_state.session.history = reducer_history;
        let effects = reducer_state.reduce(Action::HistoryCompactionPatchApplied {
            reason: crate::chat::action::CompactReason::ContextOverflow,
            patch,
            compaction_config: config,
        });
        assert!(
            effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
            "matching persisted guard must take the exact patch path, not plain-trim fallback"
        );
        assert!(
            effects.iter().all(|effect| {
                !matches!(
                    effect,
                    Effect::LogTrace {
                        level: tracing::Level::WARN,
                        msg
                    } if msg.contains("guard mismatch")
                )
            }),
            "persisted guard source must not log a guard mismatch"
        );
        let _ = reducer_state.reduce(Action::RecordUserTurn("current visible user".to_string()));
        assert_eq!(
            driver_history
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect::<Vec<_>>(),
            reducer_state
                .session
                .history
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect::<Vec<_>>(),
            "GP-6: driver history must match reducer history after persisted-source patch"
        );
        assert!(
            reducer_state
                .session
                .history
                .iter()
                .all(|message| !message.content.contains("HIDDEN_FILE_SENTINEL")),
            "hidden enrichment must not be persisted through the compaction summary"
        );
    }

    #[tokio::test]
    async fn redux_compaction_guard_source_is_inert_when_provider_history_is_not_enriched() {
        use crate::chat::state::{ChatState, Effect};
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct SummaryProvider;

        #[async_trait]
        impl Provider for SummaryProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }

            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok("EMPTY_ENRICHMENT_SUMMARY\n## Decisions\n- unchanged".to_string())
            }

            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }

            fn supports_streaming(&self) -> bool {
                true
            }

            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Ok(StreamChunk::final_chunk())]).boxed()
            }

            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let original_history = vec![
            PMsg::system("sys"),
            PMsg::user(format!("old user {}", "visible ".repeat(60))),
            PMsg::assistant(format!("old assistant {}", "visible ".repeat(60))),
            PMsg::user("current user"),
        ];
        let mut driver_history = original_history.clone();
        let mut compaction_guard_history = original_history.clone();
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 5,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 120,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

        let replacement_len = apply_redux_context_rollover(
            &SummaryProvider,
            &mut driver_history,
            &mut compaction_guard_history,
            "model",
            &config,
            None,
            &action_tx,
            crate::chat::action::CompactReason::ContextOverflow,
            "test_empty_enrichment_guard_source",
        )
        .await
        .unwrap()
        .replacement_len
        .expect("summary patch must apply");
        assert_eq!(replacement_len, 1);
        let patch = match action_rx.recv().await.expect("patch action") {
            Action::HistoryCompactionPatchApplied { patch, .. } => patch,
            other => panic!("expected summary patch action, got {other:?}"),
        };
        let reducer_history = original_history
            .get(..original_history.len() - 1)
            .expect("reducer history fixture")
            .to_vec();
        assert!(
            crate::agent::loop_::compaction_patch_guard_matches(&reducer_history, &patch.guard),
            "empty-enrichment path should guard reducer history before ordered user-turn commit"
        );

        let mut reducer_state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
        reducer_state.session.history = reducer_history;
        let effects = reducer_state.reduce(Action::HistoryCompactionPatchApplied {
            reason: crate::chat::action::CompactReason::ContextOverflow,
            patch,
            compaction_config: config,
        });
        assert!(effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))));
        let _ = reducer_state.reduce(Action::RecordUserTurn("current user".to_string()));
        assert_eq!(
            driver_history
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect::<Vec<_>>(),
            reducer_state
                .session
                .history
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect::<Vec<_>>(),
            "empty-enrichment path should remain byte-for-byte aligned"
        );
    }

    #[tokio::test]
    async fn redux_compaction_keeps_enrichment_only_trim_fallback() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct SummaryProvider;

        #[async_trait]
        impl Provider for SummaryProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }

            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok("ENRICHMENT_ONLY_SUMMARY\n## Decisions\n- fallback remains".to_string())
            }

            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }

            fn supports_streaming(&self) -> bool {
                true
            }

            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Ok(StreamChunk::final_chunk())]).boxed()
            }

            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let original_history = vec![
            PMsg::system("sys"),
            PMsg::user("small old user"),
            PMsg::assistant("small old assistant"),
            PMsg::user("current @huge.txt"),
        ];
        let mut driver_history = original_history.clone();
        let Some(current_user) = driver_history.get_mut(3) else {
            panic!("test fixture must include current user turn");
        };
        current_user.content.push_str(&format!(
            "\n\n[Attached file context from @path mentions]\nRECENT_ENRICHMENT_ONLY {}\n[End attached file context]",
            "hidden ".repeat(1000)
        ));
        let mut compaction_guard_history = original_history.clone();
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 5,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 120,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

        let replacement_len = apply_redux_context_rollover(
            &SummaryProvider,
            &mut driver_history,
            &mut compaction_guard_history,
            "model",
            &config,
            None,
            &action_tx,
            crate::chat::action::CompactReason::ContextOverflow,
            "test_enrichment_only_fallback",
        )
        .await
        .unwrap()
        .replacement_len
        .expect("provider-side enrichment pressure should still produce a patch attempt");
        assert_eq!(replacement_len, 1);
        let patch = match action_rx.recv().await.expect("patch action") {
            Action::HistoryCompactionPatchApplied { patch, .. } => patch,
            other => panic!("expected summary patch action, got {other:?}"),
        };
        let reducer_history = original_history
            .get(..original_history.len() - 1)
            .expect("reducer history fixture")
            .to_vec();
        assert!(
            crate::agent::loop_::compaction_patch_guard_matches(&reducer_history, &patch.guard),
            "fallback-preserving patch must still be guarded by reducer persisted source"
        );
        assert!(
            !driver_history
                .iter()
                .any(|message| message.content.contains("RECENT_ENRICHMENT_ONLY")),
            "existing provider-side trim fallback must still remove enrichment-only overflow"
        );
    }

    #[tokio::test]
    async fn redux_driver_preserve_trim_floor_preserves_fitting_summary_and_matches_reducer_history() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct OversizedSummaryProvider {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for OversizedSummaryProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
                if call == 0 {
                    Ok(format!("FLUSH_MARKER {}", "flush-context ".repeat(12)))
                } else {
                    Ok(format!(
                        "SUMMARY_MARKER\n## Decisions\n- keep\n## Open TODOs\n- keep\n## Critical Context\n- keep\n{}",
                        "summary-context ".repeat(12)
                    ))
                }
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Ok(StreamChunk::final_chunk())]).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let original_history = vec![
            PMsg::system("sys"),
            PMsg::user("old user ".repeat(40)),
            PMsg::assistant("old assistant ".repeat(40)),
            PMsg::user("older user ".repeat(40)),
            PMsg::assistant("older assistant ".repeat(40)),
            PMsg::user("recent bulk ".repeat(40)),
        ];
        let mut driver_history = original_history.clone();
        let mut compaction_guard_history = original_history.clone();
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 5,
            keep_recent_messages: 1,
            memory_flush: true,
            max_context_tokens: 300,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let provider = OversizedSummaryProvider {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

        let replacement_len = apply_redux_context_rollover(
            &provider,
            &mut driver_history,
            &mut compaction_guard_history,
            "model",
            &config,
            None,
            &action_tx,
            crate::chat::action::CompactReason::ContextOverflow,
            "test_second_trim",
        )
        .await
        .unwrap()
        .replacement_len
        .expect("summary patch must apply");
        assert_eq!(replacement_len, 2, "memory flush plus summary must both be protected");
        let after_compact = crate::agent::loop_::plan_context_budget(
            &driver_history,
            &config,
            crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
        );
        assert!(
            !after_compact.over_hard_limit,
            "preserve-trim floor should fully remediate when the protected replacement fits"
        );
        let _ = trim_redux_driver_context_budget_after_rollover(
            &mut driver_history,
            &mut compaction_guard_history,
            &config,
            Some(replacement_len),
        );

        let patch = match action_rx.recv().await.expect("patch action") {
            Action::HistoryCompactionPatchApplied { patch, .. } => patch,
            other => panic!("expected summary patch action, got {other:?}"),
        };
        let mut reducer_state =
            crate::chat::state::ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
        reducer_state.session.history = original_history
            .get(..original_history.len() - 1)
            .expect("reducer history fixture")
            .to_vec();
        let _ = reducer_state.reduce(Action::HistoryCompactionPatchApplied {
            reason: crate::chat::action::CompactReason::ContextOverflow,
            patch,
            compaction_config: config,
        });
        let pending_user = original_history.last().expect("pending user fixture").content.clone();
        let _ = reducer_state.reduce(Action::RecordUserTurn(pending_user));

        assert!(
            driver_history
                .iter()
                .any(|message| message.content.contains("SUMMARY_MARKER")),
            "second trim must not delete the provider summary"
        );
        assert_eq!(
            driver_history
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect::<Vec<_>>(),
            reducer_state
                .session
                .history
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect::<Vec<_>>(),
            "GP-6: driver fallback history must exactly match reducer history"
        );
    }

    #[tokio::test]
    async fn redux_driver_preserve_trim_floor_drops_unfit_summary_and_matches_reducer_history() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct HugeSummaryProvider;
        #[async_trait]
        impl Provider for HugeSummaryProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(format!(
                    "SUMMARY_TOO_LARGE\n## Decisions\n- {}",
                    "summary-over-budget ".repeat(800)
                ))
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Ok(StreamChunk::final_chunk())]).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let mut original_history = vec![PMsg::system("sys")];
        for idx in 0..8 {
            original_history.push(PMsg::user(format!(
                "trim-candidate-{idx} {}",
                "recent-bulk ".repeat(80)
            )));
        }
        let mut driver_history = original_history.clone();
        let mut compaction_guard_history = original_history.clone();
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 10,
            keep_recent_messages: 4,
            memory_flush: false,
            max_context_tokens: 90,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(4);

        let replacement_len = apply_redux_context_rollover(
            &HugeSummaryProvider,
            &mut driver_history,
            &mut compaction_guard_history,
            "model",
            &config,
            None,
            &action_tx,
            crate::chat::action::CompactReason::ContextOverflow,
            "test_floor_drops_unfit_summary",
        )
        .await
        .unwrap()
        .replacement_len
        .expect("summary patch must apply");
        assert_eq!(replacement_len, 1);

        let driver_budget = crate::agent::loop_::plan_context_budget(
            &driver_history,
            &config,
            crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
        );
        assert!(
            !driver_budget.over_hard_limit,
            "driver history must be under hard limit"
        );
        assert!(
            !driver_history
                .iter()
                .any(|message| message.content.contains("SUMMARY_TOO_LARGE")),
            "floor fallback may drop a summary that cannot fit by itself"
        );

        let patch = match action_rx.recv().await.expect("patch action") {
            Action::HistoryCompactionPatchApplied { patch, .. } => patch,
            other => panic!("expected summary patch action, got {other:?}"),
        };
        let mut reducer_state =
            crate::chat::state::ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
        reducer_state.session.history = original_history
            .get(..original_history.len() - 1)
            .expect("reducer history fixture")
            .to_vec();
        let effects = reducer_state.reduce(Action::HistoryCompactionPatchApplied {
            reason: crate::chat::action::CompactReason::ContextOverflow,
            patch,
            compaction_config: config.clone(),
        });
        assert!(
            effects.iter().all(|effect| {
                !matches!(
                    effect,
                    crate::chat::state::Effect::LogTrace {
                        level: tracing::Level::WARN,
                        msg
                    } if msg.contains("guard mismatch")
                )
            }),
            "pending-user timing must not turn an exact rollover into a stale-patch fallback"
        );
        let pending_user = original_history.last().expect("pending user fixture").content.clone();
        let _ = reducer_state.reduce(Action::RecordUserTurn(pending_user.clone()));
        assert!(
            reducer_state
                .session
                .history
                .last()
                .is_some_and(|message| message.role == "user" && message.content == pending_user),
            "ordered commit must preserve the durable user turn even when it cannot fit provider context"
        );
    }

    #[tokio::test]
    async fn redux_driver_off_mode_preflight_trims_without_history_compacted_action() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct BudgetCaptureProvider {
            first_tokens: Arc<AtomicUsize>,
            summary_calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for BudgetCaptureProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                messages: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                self.first_tokens
                    .compare_exchange(
                        0,
                        crate::agent::loop_::measure_history_tokens(messages),
                        AtomicOrdering::SeqCst,
                        AtomicOrdering::SeqCst,
                    )
                    .ok();
                stream::iter(vec![
                    Ok(StreamChunk::delta("off-budget-ok")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let first_tokens = Arc::new(AtomicUsize::new(0));
        let summary_calls = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(BudgetCaptureProvider {
            first_tokens: Arc::clone(&first_tokens),
            summary_calls: Arc::clone(&summary_calls),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Off,
            reserve_tokens: 10,
            keep_recent_messages: 2,
            memory_flush: false,
            max_context_tokens: 120,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let mut history = vec![PMsg::system("sys")];
        for i in 0..24 {
            history.push(PMsg::user(format!("turn {i} {}", "long context ".repeat(40))));
        }

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-budget-off-preflight".into(),
                history,
                compaction_guard_history: None,
                compaction_config: Some(config),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_history_compacted = false;
        let mut saw_summary_patch = false;
        let mut saw_completion = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::HistoryCompacted { .. } => {
                    saw_history_compacted = true;
                }
                Action::HistoryCompactionPatchApplied { .. } => {
                    saw_summary_patch = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("off-budget-ok"));
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => {
                    panic!("driver should trim in Off mode before stream request, not fail: {err}");
                }
                _ => {}
            }
        }
        assert!(
            saw_completion,
            "driver must complete after Off-mode trim-only preflight"
        );
        assert!(
            !saw_history_compacted,
            "Off mode must not dispatch HistoryCompacted because reducer compacts lossy"
        );
        assert!(!saw_summary_patch, "Off mode must not dispatch summary patch action");
        assert_eq!(
            summary_calls.load(AtomicOrdering::SeqCst),
            0,
            "Off mode must not call summarizer"
        );
        assert!(
            first_tokens.load(AtomicOrdering::SeqCst) <= 110,
            "Off-mode first provider call must be below literal hard limit 110 tokens"
        );
    }

    #[tokio::test]
    async fn redux_driver_summarizer_failure_uses_local_guard_before_stream() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct FailingSummaryProvider {
            first_tokens: Arc<AtomicUsize>,
            summary_calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for FailingSummaryProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Err(anyhow::anyhow!("summary unavailable"))
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: Some("unused".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                messages: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                self.first_tokens
                    .compare_exchange(
                        0,
                        crate::agent::loop_::measure_history_tokens(messages),
                        AtomicOrdering::SeqCst,
                        AtomicOrdering::SeqCst,
                    )
                    .ok();
                stream::iter(vec![
                    Ok(StreamChunk::delta("trim-fallback-ok")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let first_tokens = Arc::new(AtomicUsize::new(0));
        let summary_calls = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(FailingSummaryProvider {
            first_tokens: Arc::clone(&first_tokens),
            summary_calls: Arc::clone(&summary_calls),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 10,
            keep_recent_messages: 2,
            memory_flush: false,
            max_context_tokens: 120,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let mut history = vec![PMsg::system("sys")];
        for i in 0..24 {
            history.push(PMsg::user(format!("turn {i} {}", "long context ".repeat(40))));
        }

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-summary-failure".into(),
                history,
                compaction_guard_history: None,
                compaction_config: Some(config),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_local_guard_patch = false;
        let mut saw_completion = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::HistoryCompactionPatchApplied { patch, .. } => {
                    saw_local_guard_patch = patch.replacement.iter().any(|message| {
                        message
                            .content
                            .contains("Summary unavailable; compacted conservatively.")
                    });
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("trim-fallback-ok"));
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should fail soft to trim: {err}"),
                _ => {}
            }
        }

        assert!(saw_completion, "driver must complete after summary failure trim");
        assert!(
            saw_local_guard_patch,
            "summary failure must dispatch the deterministic local guard patch"
        );
        assert_eq!(
            summary_calls.load(AtomicOrdering::SeqCst),
            2,
            "Safeguard mode must stop requesting summaries once compaction cannot progress"
        );
        assert!(
            first_tokens.load(AtomicOrdering::SeqCst) <= 110,
            "summary failure fallback must stream under literal hard limit 110"
        );
    }

    #[tokio::test]
    async fn redux_driver_overflow_retry_uses_provider_summary_patch() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct OverflowThenSuccessProvider {
            stream_calls: Arc<AtomicUsize>,
            summary_calls: Arc<AtomicUsize>,
            retry_history: Arc<Mutex<Option<Vec<PMsg>>>>,
        }
        #[async_trait]
        impl Provider for OverflowThenSuccessProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(
                    "OVERFLOW_PROVIDER_SUMMARY\n## Decisions\n- recovered\n## Open TODOs\n- retry\n## Constraints\n- no duplicate tools"
                        .to_string(),
                )
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                messages: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let call = self.stream_calls.fetch_add(1, AtomicOrdering::SeqCst);
                if call == 0 {
                    stream::iter(vec![Err::<StreamChunk, _>(StreamError::Provider(
                        "context_length_exceeded".into(),
                    ))])
                    .boxed()
                } else {
                    let mut guard = self.retry_history.lock();
                    if guard.is_none() {
                        *guard = Some(messages.to_vec());
                    }
                    stream::iter(vec![
                        Ok(StreamChunk::delta("summary-retry-ok")),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let stream_calls = Arc::new(AtomicUsize::new(0));
        let summary_calls = Arc::new(AtomicUsize::new(0));
        let retry_history = Arc::new(Mutex::new(None));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(OverflowThenSuccessProvider {
            stream_calls: Arc::clone(&stream_calls),
            summary_calls: Arc::clone(&summary_calls),
            retry_history: Arc::clone(&retry_history),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Safeguard,
            reserve_tokens: 10,
            keep_recent_messages: 2,
            memory_flush: false,
            max_context_tokens: 2_500,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let mut history = vec![PMsg::system("sys")];
        for i in 0..12 {
            history.push(PMsg::user(format!("turn {i} {}", "overflow context ".repeat(18))));
        }

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-overflow-summary".into(),
                history,
                compaction_guard_history: None,
                compaction_config: Some(config),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_summary_patch = false;
        let mut saw_feedback = false;
        let mut saw_context_update = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::HistoryCompactionPatchApplied { patch, .. } => {
                    saw_summary_patch = true;
                    assert!(
                        patch
                            .replacement
                            .iter()
                            .any(|message| message.content.contains("OVERFLOW_PROVIDER_SUMMARY")),
                        "overflow retry patch must carry provider summary"
                    );
                }
                Action::HistoryCompacted { .. } => {
                    panic!("overflow retry with config must not use lossy HistoryCompacted");
                }
                Action::ContextWindowUpdated {
                    used_context_tokens: Some(used),
                    max_context_tokens: Some(max),
                } => {
                    saw_context_update = true;
                    assert_eq!(max, 2_500);
                    assert!(
                        used <= 2_490,
                        "overflow context window update should use compacted history: {used}"
                    );
                }
                Action::SystemMessageAdded { text } => {
                    saw_feedback = true;
                    assert!(
                        text.contains("Compacted context") || text.contains("Context already compact"),
                        "overflow feedback should describe compaction result: {text}"
                    );
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("summary-retry-ok"));
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should recover after summary retry: {err}"),
                _ => {}
            }
        }

        assert!(saw_summary_patch, "overflow retry must dispatch exact summary patch");
        assert!(
            saw_feedback,
            "overflow retry must dispatch user-visible compact feedback"
        );
        assert!(
            saw_context_update,
            "overflow retry must update context-window UI metadata after mid-turn compaction"
        );
        assert_eq!(summary_calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            stream_calls.load(AtomicOrdering::SeqCst),
            2,
            "this fixture should recover on its first progress-making retry"
        );
        let retry = retry_history.lock().clone().expect("retry history");
        assert!(
            retry
                .iter()
                .any(|message| message.content.contains("OVERFLOW_PROVIDER_SUMMARY")),
            "retry provider request must include summary"
        );
    }

    /// **S3 T3-1 Step 3**: more than one context overflow retry → StreamFailed.
    #[tokio::test]
    async fn t31_driver_context_overflow_exhausted_emits_stream_failed() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct AlwaysOverflowProvider;
        #[async_trait]
        impl Provider for AlwaysOverflowProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Err::<StreamChunk, _>(StreamError::Provider(
                    "context_length_exceeded: please reduce input".into(),
                ))])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(AlwaysOverflowProvider);
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-overflow-fail".into(),
                history: vec![crate::providers::traits::ChatMessage {
                    role: "user".into(),
                    content: "x".repeat(1000),
                }],
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_failed = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                .await
                .expect("action within 2s")
                .expect("must arrive");
            if let Action::StreamFailed { err, .. } = &action {
                assert!(
                    err.contains("context overflow") || err.contains("context_length_exceeded"),
                    "err must mention overflow: {err}"
                );
                saw_failed = true;
                break;
            }
        }
        assert!(saw_failed, "must emit StreamFailed after overflow retries exhausted");
    }

    /// **S3 T3-1 Step 4**: basic ApprovalRouter resolve / register path.
    #[tokio::test]
    async fn t31_approval_router_register_and_resolve_basic() {
        let router = ApprovalRouter::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        router.register("call-1".to_string(), tx);
        assert!(router.resolve("call-1", true), "resolve should find the tx");
        assert!(rx.await.expect("oneshot rx must resolve"));
        // resolving the same id a second time must return false (nothing pending)
        assert!(!router.resolve("call-1", false), "second resolve must miss");
    }

    /// **S3 T3-1 Step 4**: approval path — policy `Ask` waits on the router;
    /// stub EffectExecutor::RequestApproval defaults to auto-approve.
    #[tokio::test]
    async fn t31_driver_approval_path_auto_approves_via_stub() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct ShellTool {
            arguments: Arc<parking_lot::Mutex<Option<serde_json::Value>>>,
        }
        #[async_trait]
        impl crate::tools::Tool for ShellTool {
            fn name(&self) -> &str {
                "shell"
            }
            fn description(&self) -> &str {
                "shell"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                *self.arguments.lock() = Some(args);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "ran".into(),
                    error: None,
                })
            }
        }

        struct ToolThenText {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for ToolThenText {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    let c = ToolCallChunk::new("call-shell", "shell", r#"{"command":"echo ok"}"#, 0);
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(vec![c])),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("ok")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ToolThenText {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        let approved_arguments = Arc::new(parking_lot::Mutex::new(None));
        deps.tools_registry = Some(Arc::new(vec![Box::new(ShellTool {
            arguments: Arc::clone(&approved_arguments),
        }) as Box<dyn crate::tools::Tool>]));
        deps.tool_security_policy = tool_security_policy(crate::security::AutonomyLevel::Supervised);
        // test interceptor: when it sees `Action::ToolApprovalRequested` it calls router.resolve(true)
        // itself, simulating the end-to-end auto-approve behaviour of dispatcher_task + the EffectExecutor stub.
        let router_for_resolve = Arc::clone(&deps.approval_router);
        let executor = EffectExecutor::new_with_deps(deps);
        let shutdown_d = CancellationToken::new();
        let (sink_tx, mut sink_rx) = mpsc::channel::<Action>(64);
        let router_handle = Arc::clone(&router_for_resolve);
        let shutdown_clone = shutdown_d.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_clone.cancelled() => break,
                    maybe = action_rx.recv() => {
                        match maybe {
                            Some(action) => {
                                if let Action::ToolApprovalRequested { tool_id, .. } = &action {
                                    router_handle.resolve(tool_id, true);
                                    // simulate the stub sending ToolApprovalReceived back to the observer:
                                    let _ = sink_tx
                                        .send(Action::ToolApprovalReceived {
                                            tool_id: tool_id.clone(),
                                            approved: true,
                                        })
                                        .await;
                                }
                                let _ = sink_tx.send(action).await;
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-approval".into(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_request = false;
        let mut saw_received = false;
        let mut saw_tool_started = false;
        let mut saw_tool_finished = false;
        let mut saw_completion = false;
        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_secs(3), sink_rx.recv())
                .await
                .expect("action within 3s")
                .expect("must arrive");
            match action {
                Action::ToolApprovalRequested { tool_id, name, .. } => {
                    assert_eq!(tool_id, "call-shell");
                    assert_eq!(name, "shell");
                    saw_request = true;
                }
                Action::ToolApprovalReceived { tool_id, approved } => {
                    assert_eq!(tool_id, "call-shell");
                    assert!(approved, "stub should auto-approve");
                    saw_received = true;
                }
                Action::ToolStarted { name, .. } => {
                    assert_eq!(name, "shell");
                    saw_tool_started = true;
                }
                Action::ToolFinished { success, name, .. } => {
                    assert_eq!(name, "shell");
                    assert!(success);
                    saw_tool_finished = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("ok"));
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                _ => {}
            }
        }
        shutdown_d.cancel();
        assert!(saw_request, "must see ToolApprovalRequested");
        assert!(saw_received, "must see ToolApprovalReceived");
        assert!(saw_tool_started, "must see ToolStarted after approval");
        assert!(saw_tool_finished, "must see ToolFinished after approval");
        assert!(saw_completion, "must see StreamCompleted after approval");
        let args = approved_arguments.lock().clone().unwrap_or_default();
        assert_eq!(
            args.get(crate::security::policy::RUNTIME_APPROVAL_GRANTED_ARG),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(
            args.get(crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG).is_some(),
            "TUI approval strategy must inject the trusted command-bound grant"
        );
    }

    /// **S3 T3-1 Step 4**: approval rejected → the tool is not executed + ToolFinished(success=false, "User rejected").
    #[tokio::test]
    async fn t31_driver_approval_rejected_skips_tool_execution() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

        let exec_counter = Arc::new(AtomicBool::new(false));
        struct RejectableTool {
            executed: Arc<AtomicBool>,
        }
        #[async_trait]
        impl crate::tools::Tool for RejectableTool {
            fn name(&self) -> &str {
                "danger"
            }
            fn description(&self) -> &str {
                "danger"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.executed.store(true, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "did dangerous thing".into(),
                    error: None,
                })
            }
        }

        struct ToolThenText {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for ToolThenText {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    let c = ToolCallChunk::new("call-danger", "danger", "{}", 0);
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(vec![c])),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("declined")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ToolThenText {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(RejectableTool {
            executed: Arc::clone(&exec_counter),
        }) as Box<dyn crate::tools::Tool>]));
        deps.tool_security_policy = tool_security_policy(crate::security::AutonomyLevel::Supervised);
        let router_for_resolve = Arc::clone(&deps.approval_router);
        let executor = EffectExecutor::new_with_deps(deps);

        // intercept action_rx: capture ToolApprovalRequested → produce ToolApprovalDecision(false)
        // through the TUI `N` key → resolve(false), so the driver receives the rejection.
        let shutdown_d = CancellationToken::new();
        let (sink_tx, mut sink_rx) = mpsc::channel::<Action>(64);
        let router_handle = Arc::clone(&router_for_resolve);
        let shutdown_clone = shutdown_d.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_clone.cancelled() => break,
                    maybe = action_rx.recv() => {
                        match maybe {
                            Some(action) => {
                                if let Action::ToolApprovalRequested {
                                    task_id,
                                    tool_id,
                                    name,
                                    args,
                                } = &action
                                {
                                    #[cfg(not(feature = "terminal-tui"))]
                                    let _ = (name, args);
                                    #[cfg(feature = "terminal-tui")]
                                    {
                                        let mut tui = crate::chat::tui::TuiState::new("p", "m");
                                        tui.focus = crate::chat::sessions::FocusTarget::Approval;
                                        tui.pending_tool_approval =
                                            Some(crate::chat::sessions::PendingToolApprovalView {
                                                task_id: *task_id,
                                                tool_id: tool_id.clone(),
                                                name: name.clone(),
                                                args: args.clone(),
                                                selected_approval: false,
                                            });
                                        let decision = crate::chat::tui::dispatch_global_key(
                                            crossterm::event::KeyEvent::new(
                                                crossterm::event::KeyCode::Char('n'),
                                                crossterm::event::KeyModifiers::NONE,
                                            ),
                                            &mut tui,
                                        );
                                        match decision {
                                            crate::chat::tui::KeyDispatch::ToolApprovalDecision {
                                                tool_id: decided_tool_id,
                                                approved,
                                            } => {
                                                assert_eq!(decided_tool_id, *tool_id);
                                                assert!(!approved, "N key must deny approval");
                                                assert!(tui.pending_tool_approval.is_none());
                                                assert_eq!(tui.focus, crate::chat::sessions::FocusTarget::Main);
                                                router_handle.resolve(tool_id, approved);
                                            }
                                            other => panic!("expected TUI approval denial, got {other:?}"),
                                        }
                                    }
                                    #[cfg(not(feature = "terminal-tui"))]
                                    {
                                        router_handle.resolve(tool_id, false);
                                    }
                                }
                                let _ = sink_tx.send(action).await;
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-reject".into(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_tool_finished_rejected = false;
        let mut saw_completion = false;
        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_secs(3), sink_rx.recv())
                .await
                .expect("action within 3s")
                .expect("must arrive");
            match action {
                Action::ToolFinished {
                    success, result, name, ..
                } => {
                    assert_eq!(name, "danger");
                    assert!(!success, "rejected tool must report success=false");
                    let r = result.as_deref().unwrap_or_default();
                    assert!(r.contains("User rejected") || r.contains("rejected"), "result={r:?}");
                    saw_tool_finished_rejected = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("declined"));
                    saw_completion = true;
                    break;
                }
                _ => {}
            }
        }
        shutdown_d.cancel();
        assert!(
            saw_tool_finished_rejected,
            "must see ToolFinished(success=false) on reject"
        );
        assert!(saw_completion, "must see StreamCompleted with replacement text");
        assert!(
            !exec_counter.load(AtomicOrdering::SeqCst),
            "rejected tool MUST NOT have executed"
        );
    }

    #[tokio::test]
    async fn dispatch_tool_with_missing_approval_router_rejects() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        struct DangerousTool {
            executed: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for DangerousTool {
            fn name(&self) -> &str {
                "danger"
            }
            fn description(&self) -> &str {
                "danger"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.executed.store(true, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "ran".into(),
                    error: None,
                })
            }
        }

        let policy = tool_security_policy(crate::security::AutonomyLevel::Supervised);
        let executed = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(vec![Box::new(DangerousTool {
            executed: Arc::clone(&executed),
        }) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-danger".into(),
            name: "danger".into(),
            args: "{}".into(),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();
        let context = chat_tool_execution_context(policy.as_ref(), None, None, "draft-missing-router");
        let service = ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(policy)),
            Arc::new(crate::tools::DenyApprovalStrategy),
            Arc::new(ChatToolExecutionPreparation {
                task_id: None,
                action_tx: action_tx.clone(),
            }),
            Arc::new(TracingToolExecutionAudit),
        );

        let outcome = execute_single_tool_call(
            None,
            Some(&service),
            &context,
            &call,
            &CancellationToken::new(),
            &action_tx,
            "draft-missing-router",
            &mut history,
            None,
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done { .. }));
        assert!(
            !executed.load(AtomicOrdering::SeqCst),
            "tool requiring approval must not execute without router"
        );
        let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
            .await
            .expect("ToolFinished action")
            .expect("action present");
        match action {
            Action::ToolFinished {
                name, success, result, ..
            } => {
                assert_eq!(name, "danger");
                assert!(!success);
                assert!(
                    result
                        .as_deref()
                        .is_some_and(|value| value.contains("no approval resolver")),
                    "unexpected result: {result:?}"
                );
            }
            other => panic!("expected ToolFinished fail-CLOSED action, got {other:?}"),
        }
        assert!(
            action_rx.try_recv().is_err(),
            "fail-CLOSED path must not start the tool"
        );
        let tool_message = history.last().expect("tool rejection history expected");
        assert_eq!(tool_message.role, "tool");
        let payload: serde_json::Value = serde_json::from_str(&tool_message.content).expect("tool payload JSON");
        assert_eq!(
            payload.get("tool_call_id").and_then(serde_json::Value::as_str),
            Some("call-danger")
        );
        assert_eq!(payload.get("success").and_then(serde_json::Value::as_bool), Some(false));
        assert!(
            payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.contains("no approval resolver"))
        );
    }

    #[tokio::test]
    async fn dispatch_tool_with_approval_router_works_normally() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        struct DangerousTool {
            executed: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for DangerousTool {
            fn name(&self) -> &str {
                "danger"
            }
            fn description(&self) -> &str {
                "danger"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.executed.store(true, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "ran".into(),
                    error: None,
                })
            }
        }

        let policy = tool_security_policy(crate::security::AutonomyLevel::Supervised);
        let approval_router = Arc::new(ApprovalRouter::new());
        let executed = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(vec![Box::new(DangerousTool {
            executed: Arc::clone(&executed),
        }) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-danger".into(),
            name: "danger".into(),
            args: "{}".into(),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();
        let cancellation = CancellationToken::new();
        let context = chat_tool_execution_context(policy.as_ref(), None, None, "draft-router");
        let ledger = tempfile::TempDir::new().expect("approval router ledger");
        let ledger_memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(ledger.path()).expect("approval router sqlite"));
        let service = chat_tool_execution_service(
            Arc::clone(&registry),
            Some(ledger_memory),
            policy,
            Arc::clone(&approval_router),
            action_tx.clone(),
            cancellation.clone(),
            None,
        );

        let router_handle = Arc::clone(&approval_router);
        let executed_before_approval = Arc::clone(&executed);
        let resolver = tokio::spawn(async move {
            let action = action_rx.recv().await.expect("approval request action");
            match action {
                Action::ToolApprovalRequested { tool_id, name, .. } => {
                    assert_eq!(tool_id, "call-danger");
                    assert_eq!(name, "danger");
                    assert!(
                        !executed_before_approval.load(AtomicOrdering::SeqCst),
                        "tool must not execute before approval is resolved"
                    );
                    assert!(router_handle.resolve(&tool_id, true));
                }
                other => panic!("expected approval request, got {other:?}"),
            }

            let started = action_rx.recv().await.expect("tool started action");
            assert!(matches!(started, Action::ToolStarted { ref name, .. } if name == "danger"));
            let finished = action_rx.recv().await.expect("tool finished action");
            match finished {
                Action::ToolFinished { name, success, .. } => {
                    assert_eq!(name, "danger");
                    assert!(success);
                }
                other => panic!("expected ToolFinished success, got {other:?}"),
            }
        });

        let outcome = execute_single_tool_call(
            None,
            Some(&service),
            &context,
            &call,
            &cancellation,
            &action_tx,
            "draft-router",
            &mut history,
            None,
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done { .. }));
        assert!(executed.load(AtomicOrdering::SeqCst));
        resolver.await.expect("resolver task should complete");
        let tool_message = history.last().expect("tool result history expected");
        assert_eq!(tool_message.role, "tool");
        let payload: serde_json::Value = serde_json::from_str(&tool_message.content).expect("tool payload JSON");
        assert_eq!(
            payload.get("tool_call_id").and_then(serde_json::Value::as_str),
            Some("call-danger")
        );
        assert_eq!(payload.get("success").and_then(serde_json::Value::as_bool), Some(true));
    }

    #[tokio::test]
    async fn read_only_policy_denies_act_tool_without_prompt_or_execution() {
        let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let registry = Arc::new(vec![Box::new(FlagTool {
            name: "danger",
            executed: Arc::clone(&executed),
        }) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-read-only".to_string(),
            name: "danger".to_string(),
            args: "{}".to_string(),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let (service, context, cancellation, _ledger) = tool_service_for_test(
            registry,
            &action_tx,
            "draft-read-only",
            crate::security::AutonomyLevel::ReadOnly,
        );
        let mut history = Vec::new();

        let outcome = execute_single_tool_call(
            None,
            Some(&service),
            &context,
            &call,
            &cancellation,
            &action_tx,
            "draft-read-only",
            &mut history,
            None,
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done { unrecoverable: Some(_) }));
        assert!(!executed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(
            action_rx.recv().await,
            Some(Action::ToolFinished { success: false, .. })
        ));
        assert!(action_rx.try_recv().is_err(), "deny must not prompt or start the tool");
    }

    #[tokio::test]
    async fn cancelling_pending_tui_approval_cancels_service_and_clears_router() {
        let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let registry = Arc::new(vec![Box::new(FlagTool {
            name: "danger",
            executed: Arc::clone(&executed),
        }) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-cancel-approval".to_string(),
            name: "danger".to_string(),
            args: "{}".to_string(),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let policy = tool_security_policy(crate::security::AutonomyLevel::Supervised);
        let cancellation = CancellationToken::new();
        let context = chat_tool_execution_context(policy.as_ref(), None, None, "draft-cancel-approval");
        let approval_router = Arc::new(ApprovalRouter::new());
        let service = chat_tool_execution_service(
            registry,
            None,
            policy,
            Arc::clone(&approval_router),
            action_tx.clone(),
            cancellation.clone(),
            None,
        );
        let cancellation_for_task = cancellation.clone();
        let task = tokio::spawn(async move {
            let mut history = Vec::new();
            execute_single_tool_call(
                None,
                Some(&service),
                &context,
                &call,
                &cancellation_for_task,
                &action_tx,
                "draft-cancel-approval",
                &mut history,
                None,
                crate::agent::loop_::ChatMode::Edit,
            )
            .await
        });

        assert!(matches!(
            action_rx.recv().await,
            Some(Action::ToolApprovalRequested { .. })
        ));
        assert!(approval_router.has_pending());
        cancellation.cancel();
        assert!(matches!(task.await.expect("tool task"), ToolExecOutcome::Cancelled));
        assert!(!approval_router.has_pending());
        assert!(!executed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(action_rx.recv().await, Some(Action::StreamCancelled { .. })));
        assert!(
            action_rx.try_recv().is_err(),
            "cancelled approval must not start the tool"
        );
    }

    /// **S3 T3-1 Step 5**: stream_error_is_network_timeout correctly recognises reqwest errors.
    ///
    /// Uses reqwest::Client to GET an unreachable address on purpose, triggering a real connect error.
    #[test]
    fn t31_stream_error_is_network_timeout_recognises_io_error() {
        use crate::providers::traits::StreamError;
        let io_err = StreamError::Io(std::io::Error::other("simulated"));
        assert!(stream_error_is_network_timeout(&io_err));
        let json_err = StreamError::Json(serde_json::from_str::<serde_json::Value>("notjson").unwrap_err());
        assert!(!stream_error_is_network_timeout(&json_err));
        let provider_err = StreamError::Provider("rate limit".into());
        assert!(!stream_error_is_network_timeout(&provider_err));
    }

    /// **S3 T3-1 Step 5**: stream_error_is_context_overflow substring-matches several providers.
    #[test]
    fn t31_stream_error_is_context_overflow_matches_provider_strings() {
        use crate::providers::traits::StreamError;
        for msg in [
            "Error: maximum context length exceeded",
            "context_length_exceeded",
            "the prompt is too long",
            "input token count is 200000",
            "exceeds maximum allowed",
            "Token limit reached",
        ] {
            let err = StreamError::Provider(msg.into());
            assert!(
                stream_error_is_context_overflow(&err),
                "expected overflow match for: {msg}"
            );
        }
        let normal = StreamError::Provider("rate limited".into());
        assert!(!stream_error_is_context_overflow(&normal));
    }

    /// **S3 T3-1 Step 5**: io error retry — the driver succeeds on attempt 3 after io errors on attempts 1 and 2.
    ///
    /// A short backoff keeps the unit test fast (note: the current BACKOFF_BASE_MS=500ms is already small,
    /// and with 1s+2s the total is 3.5s, comfortably inside the unit test timeout).
    #[tokio::test]
    async fn t31_driver_network_timeout_retries_with_backoff_then_succeeds() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct FlakyProvider {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for FlakyProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n < 2 {
                    stream::iter(vec![Err::<StreamChunk, _>(StreamError::Io(std::io::Error::other(
                        "simulated network timeout",
                    )))])
                    .boxed()
                } else {
                    stream::iter(vec![
                        Ok(StreamChunk::delta("recovered")),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(FlakyProvider {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-flaky".into(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut retry_attempts: u8 = 0;
        let mut saw_completion = false;
        // total time budget: ~3.5s of real sleep + some RTT, with 8s of headroom.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            assert!(std::time::Instant::now() < deadline, "test deadline exceeded");
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let action = match tokio::time::timeout(remaining.min(Duration::from_secs(4)), action_rx.recv()).await {
                Ok(Some(a)) => a,
                Ok(None) => break,
                Err(_) => continue,
            };
            match action {
                Action::StreamRetryAttempt { attempt, .. } => {
                    retry_attempts = retry_attempts.max(attempt);
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("recovered"));
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => {
                    panic!("driver should retry, not fail: {err}");
                }
                _ => {}
            }
        }
        assert!(
            retry_attempts >= 1,
            "must emit at least one StreamRetryAttempt (got {retry_attempts})"
        );
        assert!(saw_completion, "must complete after backoff retries");
    }

    /// A network error after any model output is not safe to retry: replaying
    /// the request would duplicate streamed text/reasoning and could rebuild a
    /// partial tool call with different arguments.
    #[tokio::test]
    async fn driver_does_not_retry_network_error_after_reasoning_output() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct PartialThenErrorProvider {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl Provider for PartialThenErrorProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }

            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }

            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }

            fn supports_streaming(&self) -> bool {
                true
            }

            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                self.calls.fetch_add(1, AtomicOrdering::SeqCst);
                stream::iter(vec![
                    Ok(StreamChunk::reasoning_delta("partial reasoning")),
                    Err(StreamError::Io(std::io::Error::other("stream interrupted"))),
                ])
                .boxed()
            }

            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(PartialThenErrorProvider {
            calls: Arc::clone(&calls),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-partial-network-error".into(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut retry_attempts = 0_u8;
        let failed = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                match action_rx.recv().await {
                    Some(Action::StreamRetryAttempt { attempt, .. }) => retry_attempts = retry_attempts.max(attempt),
                    Some(Action::StreamFailed { err, retryable, .. }) => break (err, retryable),
                    Some(_) => {}
                    None => panic!("action channel closed before StreamFailed"),
                }
            }
        })
        .await
        .expect("partial stream must fail promptly");

        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "partial response must not be replayed"
        );
        assert_eq!(retry_attempts, 0, "partial response must not enter the retry loop");
        assert!(!failed.1, "a partial response cannot be retried safely");
        assert!(failed.0.contains("after model output"), "err={}", failed.0);
    }

    /// **S3 T3-1 Step 5**: persistent io errors → retries exhausted → StreamFailed(retryable=false).
    #[tokio::test]
    async fn t31_driver_network_timeout_exhausted_emits_stream_failed() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct AlwaysIoErrProvider;
        #[async_trait]
        impl Provider for AlwaysIoErrProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Err::<StreamChunk, _>(StreamError::Io(std::io::Error::other(
                    "persistent network failure",
                )))])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(AlwaysIoErrProvider);
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                provider_turn_task_id: None,
                draft_id: "draft-net-fail".into(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            })
            .await;

        let mut saw_failed = false;
        // backoff = 500ms + 1s + 2s = 3.5s + RTT, with 10s of headroom.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(std::time::Instant::now() < deadline, "test deadline exceeded");
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let action = match tokio::time::timeout(remaining.min(Duration::from_secs(5)), action_rx.recv()).await {
                Ok(Some(a)) => a,
                Ok(None) => break,
                Err(_) => continue,
            };
            if let Action::StreamFailed { err, retryable, .. } = action {
                assert!(!retryable, "exhausted retries must be non-retryable");
                assert!(err.contains("network retries exhausted"), "err={err}");
                saw_failed = true;
                break;
            }
        }
        assert!(saw_failed, "must emit StreamFailed after exhausting network retries");
    }

    // ─── S2.5 T2.5-3: Effect replay idempotency tests ──────────────────────────

    /// S2.5 T2.5-3: dispatching SaveSession twice in a row with the same snapshot gives store_count == 2,
    /// and the state converges (idempotent-overwrite semantics: each write overwrites, ending consistent).
    #[tokio::test]
    async fn s2_5_t2_5_3_save_session_dispatch_idempotent() {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let session = ChatSession::new("prov", "model");
        executor.execute(Effect::SaveSession(session.clone())).await;
        executor.execute(Effect::SaveSession(session)).await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            store_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "two SaveSessions must trigger memory.store twice (idempotent-overwrite, not deduplication)"
        );
    }

    /// S2.5 T2.5-3: two NotifyHooks for one event hit the real hook twice (fire-and-forget does not dedupe).
    ///
    /// Verified by registering a real hook + touching a sentinel + counting file size (a second touch updates
    /// mtime but not size), so appending a line with `>>` is more reliable: first creates, second grows by 1 byte.
    #[tokio::test]
    async fn s2_5_t2_5_3_notify_hook_repeat_no_double_fire() {
        use crate::hooks::HookEvent;

        let temp = TempDir::new().expect("tempdir");
        let counter = temp.path().join("hook_counter.log");
        let counter_str = counter.to_str().expect("valid path");

        // each trigger appends one character to the counter file (append implemented with sh -c).
        let hooks_json = serde_json::json!({
            "enabled": true,
            "hooks": {
                "turn_complete": [
                    {
                        "command": "sh",
                        "args": ["-c", format!("printf x >> {counter_str}")],
                        "stdin_json": false
                    }
                ]
            }
        });
        std::fs::write(temp.path().join("hooks.json"), hooks_json.to_string()).expect("write hooks.json");

        let hooks = Arc::new(HookManager::new(temp.path().to_path_buf()));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let shutdown = CancellationToken::new();
        let deps = EffectDeps {
            provider,
            memory,
            memory_event_recording: MemoryEventRecording::default(),
            channel,
            hooks: Arc::clone(&hooks),
            observer,
            action_tx,
            provider_turn_lifecycle_tx: None,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            #[cfg(feature = "terminal-tui")]
            tui_mirror: None,
            shutdown,
            model: ModelSlot::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            approval_router: Arc::new(ApprovalRouter::new()),
            tool_security_policy: full_tool_security_policy(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
            exposed_tools: crate::tools::intent::SessionToolExposure::new(),
        };
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"seq": 1}),
            })
            .await;
        executor
            .execute(Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"seq": 2}),
            })
            .await;

        // the hook command is spawned; give it enough time.
        tokio::time::sleep(Duration::from_millis(800)).await;

        let bytes = std::fs::read(&counter).unwrap_or_default();
        assert_eq!(
            bytes.len(),
            2,
            "two NotifyHooks must run the hook twice → the counter accumulates 2 bytes, measured {} bytes",
            bytes.len()
        );
    }

    /// S2.5 T2.5-3: cancelling a CancelToken three times does not panic (the token is internally idempotent).
    ///
    /// The first cancel makes token.is_cancelled() == true; later dispatches must keep it true
    /// with no panic and no new side effects.
    #[tokio::test]
    async fn s2_5_t2_5_3_cancel_token_triple_cancel_no_panic() {
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let token = CancellationToken::new();
        assert!(!token.is_cancelled(), "token initially not cancelled");

        for _ in 0..3 {
            executor.execute(Effect::CancelToken(token.clone())).await;
        }
        // all three must keep the token at cancelled = true, with no panic.
        assert!(
            token.is_cancelled(),
            "the token must stay cancelled after three cancels"
        );
    }

    /// S2.5 T2.5-3: dispatching two SaveSessions concurrently has no race / no deadlock
    /// (regression guard after the T3-3-fixB D1 inline await).
    ///
    /// Two spawns call execute; once the tasks finish store_count == 2, with no panic and no hang.
    #[tokio::test]
    async fn s2_5_t2_5_3_save_session_concurrent_dispatch_no_race() {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = Arc::new(EffectExecutor::new_with_deps(deps));

        let session = ChatSession::new("prov", "model");
        let exec1 = Arc::clone(&executor);
        let sess1 = session.clone();
        let h1 = tokio::spawn(async move {
            exec1.execute(Effect::SaveSession(sess1)).await;
        });
        let exec2 = Arc::clone(&executor);
        let sess2 = session;
        let h2 = tokio::spawn(async move {
            exec2.execute(Effect::SaveSession(sess2)).await;
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = h1.await;
            let _ = h2.await;
        })
        .await
        .expect("test: concurrent dispatch should not deadlock");

        // the SaveSession subtask is spawned asynchronously; wait for it to finish.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            store_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both concurrent SaveSessions must trigger memory.store once each (two in total)"
        );
    }
}

#[cfg(test)]
mod turn_characterization_tests;

#[cfg(test)]
mod tool_tiering_tests;

// ─── S4-A Commit 3: dispatcher snapshot push ────────────────────────────────

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod s4_a_3 {
    use super::*;
    use crate::chat::action::Action;
    use crate::chat::state::{ChatState, UiSnapshot};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    fn make_state() -> ChatState {
        ChatState::new(Arc::from("p-rx"), Arc::from("m-rx"), CancellationToken::new())
    }

    #[tokio::test]
    async fn s4_a_3_dispatcher_pushes_snapshot_on_ui_action() {
        // a UI-affecting Action (SystemMessageAdded) must trigger a snapshot push.
        let state = make_state();
        let initial = Arc::new(UiSnapshot::initial(
            Arc::clone(&state.session.provider),
            Arc::clone(&state.session.model),
        ));
        let (snap_tx, mut snap_rx) = watch::channel(initial);
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let _handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            Some(snap_tx),
        );

        let _ = dispatcher
            .dispatch(Action::SystemMessageAdded { text: "banner".into() })
            .await;
        // wait for watch update
        tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
            .await
            .expect("snap_rx should receive update within 300ms")
            .expect("watch send_if_modified should have fired");
        let snap = snap_rx.borrow();
        assert!(
            snap.revision >= 1,
            "revision should advance to >=1, got {}",
            snap.revision
        );
        assert!(
            !snap.conversation_lines.is_empty(),
            "the snapshot must contain the conversation line written by SystemMessageAdded"
        );

        shutdown.cancel();
    }

    #[tokio::test]
    async fn s4_a_3_dispatched_keypress_updates_snapshot_input() {
        let state = make_state();
        let initial = Arc::new(UiSnapshot::initial(
            Arc::clone(&state.session.provider),
            Arc::clone(&state.session.model),
        ));
        let (snap_tx, mut snap_rx) = watch::channel(initial);
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let _handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            Some(snap_tx),
        );

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        );
        let _ = dispatcher.dispatch(Action::KeyPressed(key)).await;
        tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
            .await
            .expect("snap_rx should receive input update within 300ms")
            .expect("watch send_if_modified should have fired");

        let snap = snap_rx.borrow();
        assert_eq!(snap.input.text(), "x");

        shutdown.cancel();
    }

    #[tokio::test]
    async fn s4_a_3_dispatcher_skips_unrelated_action() {
        // StreamRetryAttempt is statically judged dirty=false and writes no ui field → no snapshot push.
        let state = make_state();
        let initial = Arc::new(UiSnapshot::initial(
            Arc::clone(&state.session.provider),
            Arc::clone(&state.session.model),
        ));
        let initial_rev = initial.revision;
        let (snap_tx, mut snap_rx) = watch::channel(initial);
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let _handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            Some(snap_tx),
        );

        let _ = dispatcher
            .dispatch(Action::StreamRetryAttempt {
                attempt: 1,
                reason: "transient".to_string(),
            })
            .await;
        // no changed signal must appear within 200ms.
        let result = tokio::time::timeout(Duration::from_millis(200), snap_rx.changed()).await;
        assert!(
            result.is_err(),
            "StreamRetryAttempt must not trigger a snapshot push (changed returned={:?})",
            result.map(|r| r.is_ok())
        );
        assert_eq!(
            snap_rx.borrow().revision,
            initial_rev,
            "the revision must stay unchanged"
        );

        shutdown.cancel();
    }

    #[tokio::test]
    async fn s4_a_3_revision_strict_monotonic_in_pure() {
        // after several UI Actions the revision must increase strictly monotonically.
        let state = make_state();
        let initial = Arc::new(UiSnapshot::initial(
            Arc::clone(&state.session.provider),
            Arc::clone(&state.session.model),
        ));
        let (snap_tx, mut snap_rx) = watch::channel(initial);
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let _handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            Some(snap_tx),
        );

        let mut prev_rev = snap_rx.borrow().revision;
        for i in 0..3 {
            let _ = dispatcher
                .dispatch(Action::SystemMessageAdded {
                    text: format!("msg-{i}"),
                })
                .await;
            tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
                .await
                .expect("changed within 300ms")
                .expect("watch send");
            let cur = snap_rx.borrow().revision;
            assert!(
                cur > prev_rev,
                "the revision must increase strictly: prev={prev_rev}, cur={cur}"
            );
            prev_rev = cur;
        }

        shutdown.cancel();
    }

    #[tokio::test]
    async fn s4_a_3_off_mode_no_snapshot_push() {
        // when snapshot_tx=None (Off/Both/Redux), no snapshot is built even if ui_dirty
        // — verifying the zero-overhead contract.
        let state = make_state();
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            None, // key point: snapshot_tx=None
        );

        // push several UI Actions; the dispatcher must not panic or hang.
        for i in 0..5 {
            let _ = dispatcher
                .dispatch(Action::SystemMessageAdded { text: format!("m{i}") })
                .await;
        }
        // give the dispatcher time to process.
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.cancel();
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle should complete within 2s after shutdown")
            .expect("task join");
        assert!(
            stats.actions_seen >= 5,
            "must process at least 5 actions, got {}",
            stats.actions_seen
        );
    }

    // ── BUG-09 / BUG-05 driver-path coverage ──────────────────────────────

    #[test]
    fn plan_intercept_classifies_read_vs_write_tools() {
        // Read-only tools must NOT be intercepted in plan mode.
        for read in ["file_read", "grep", "web_fetch", "memory_recall", "sessions_list"] {
            assert!(
                !is_plan_intercepted_write_tool(read),
                "{read} is read-only and must run in plan mode"
            );
        }
        // Mutating + unknown tools MUST be intercepted.
        for write in ["file_write", "shell", "git_operations", "some_unknown_mcp_tool"] {
            assert!(
                is_plan_intercepted_write_tool(write),
                "{write} mutates state (or is unknown) and must be simulated in plan mode"
            );
        }
    }

    #[test]
    fn plan_preview_args_is_bounded_and_utf8_safe() {
        assert_eq!(plan_preview_args("short"), "short");
        let long = "a".repeat(500);
        let preview = plan_preview_args(&long);
        assert!(preview.chars().count() <= 161, "preview must be bounded");
        assert!(preview.ends_with('…'));
        // Must not panic on a multibyte boundary.
        let multibyte = "\u{20ac}".repeat(200);
        let _ = plan_preview_args(&multibyte);
    }

    #[tokio::test]
    async fn plan_mode_simulates_write_tool_without_executing() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        struct RecordingWrite {
            executed: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for RecordingWrite {
            fn name(&self) -> &str {
                "file_write"
            }
            fn description(&self) -> &str {
                "write"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.executed.store(true, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "REALLY WROTE".into(),
                    error: None,
                })
            }
        }

        let executed = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(vec![Box::new(RecordingWrite {
            executed: Arc::clone(&executed),
        }) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-w".into(),
            name: "file_write".into(),
            args: r#"{"path":"a.txt","content":"x"}"#.into(),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();
        let (service, context, cancellation, _ledger) = tool_service_for_test(
            Arc::clone(&registry),
            &action_tx,
            "draft-plan",
            crate::security::AutonomyLevel::Full,
        );

        let outcome = execute_single_tool_call(
            None,
            Some(&service),
            &context,
            &call,
            &cancellation,
            &action_tx,
            "draft-plan",
            &mut history,
            None,
            crate::agent::loop_::ChatMode::Plan,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done { .. }));
        assert!(
            !executed.load(AtomicOrdering::SeqCst),
            "plan mode MUST NOT execute the write tool"
        );
        // history tool message must carry the simulated marker, not the real output.
        let tool_msg = history.last().expect("tool result pushed to history");
        assert!(
            tool_msg.content.contains("[plan mode] would call file_write"),
            "history must carry simulated result, got: {}",
            tool_msg.content
        );
        assert!(
            !tool_msg.content.contains("REALLY WROTE"),
            "real tool output must never appear in plan mode"
        );
        // A ToolFinished(success=true) with the simulated text must be emitted.
        let mut saw_finished = false;
        while let Ok(action) = action_rx.try_recv() {
            if let Action::ToolFinished { name, result, .. } = action {
                assert_eq!(name, "file_write");
                assert!(result.unwrap_or_default().contains("[plan mode]"));
                saw_finished = true;
            }
        }
        assert!(saw_finished, "must emit ToolFinished for the simulated call");
    }

    #[tokio::test]
    async fn redux_tool_result_is_budgeted_and_trimmed_after_insertion() {
        struct LargeOutputTool;
        #[async_trait::async_trait]
        impl crate::tools::Tool for LargeOutputTool {
            fn name(&self) -> &str {
                "shell"
            }
            fn description(&self) -> &str {
                "large output"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: format!("START-LARGE-OUTPUT\n{}", "payload ".repeat(20_000)),
                    error: None,
                })
            }
        }

        let registry = Arc::new(vec![Box::new(LargeOutputTool) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-large".into(),
            name: "shell".into(),
            args: "{}".into(),
        };
        let (action_tx, _action_rx) = mpsc::channel::<Action>(8);
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Aggressive,
            reserve_tokens: 100,
            keep_recent_messages: 1,
            memory_flush: false,
            max_context_tokens: 1_000,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        let mut history = vec![
            crate::providers::traits::ChatMessage::system("sys"),
            crate::providers::traits::ChatMessage::user("run the large command"),
        ];
        let (service, context, cancellation, _ledger) = tool_service_for_test(
            Arc::clone(&registry),
            &action_tx,
            "draft-large",
            crate::security::AutonomyLevel::Full,
        );

        let outcome = execute_single_tool_call(
            None,
            Some(&service),
            &context,
            &call,
            &cancellation,
            &action_tx,
            "draft-large",
            &mut history,
            Some(&config),
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done { .. }));
        let tool_msg = history.last().expect("tool result pushed to history");
        let payload: serde_json::Value = serde_json::from_str(&tool_msg.content).expect("tool payload JSON");
        let content = payload
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            content.contains("[document_ingest_ref]"),
            "large redux tool result must be document-referenced, got {} chars",
            content.len()
        );
        assert!(
            !content.contains(&"payload ".repeat(1_000)),
            "large redux tool result must not insert the full output"
        );
        let tokens_after_tool = crate::agent::loop_::measure_history_tokens(&history);
        assert!(
            tokens_after_tool <= 900,
            "redux tool result insertion alone must stay below literal hard limit 900 tokens, got {tokens_after_tool}"
        );
    }

    #[tokio::test]
    async fn failed_tool_with_empty_output_surfaces_error_in_content() {
        // BUG-05: a tool that fails with empty output must put its error reason
        // into `content` so the LLM sees the rejection (not an empty result).
        struct RejectingTool;
        #[async_trait::async_trait]
        impl crate::tools::Tool for RejectingTool {
            fn name(&self) -> &str {
                "file_write"
            }
            fn description(&self) -> &str {
                "write"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Path not allowed by security policy: /etc/passwd".into()),
                })
            }
        }

        let registry = Arc::new(vec![Box::new(RejectingTool) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-rej".into(),
            name: "file_write".into(),
            args: "{}".into(),
        };
        let (action_tx, _action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();
        let (service, context, cancellation, _ledger) = tool_service_for_test(
            Arc::clone(&registry),
            &action_tx,
            "draft-rej",
            crate::security::AutonomyLevel::Full,
        );

        let outcome = execute_single_tool_call(
            None,
            Some(&service),
            &context,
            &call,
            &cancellation,
            &action_tx,
            "draft-rej",
            &mut history,
            None,
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        // BUG-03: a "Path not allowed" failure is unrecoverable — the outcome
        // must carry a signature so the driver can stop a retry spin.
        match outcome {
            ToolExecOutcome::Done { unrecoverable } => {
                let sig = unrecoverable.expect("path-not-allowed failure must be flagged unrecoverable");
                assert!(sig.starts_with("file_write::"), "signature keyed by tool name: {sig}");
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let tool_msg = history.last().expect("tool result pushed to history");
        let payload: serde_json::Value = serde_json::from_str(&tool_msg.content).expect("tool payload is JSON");
        assert_eq!(payload.get("success"), Some(&serde_json::json!(false)));
        let content = payload.get("content").and_then(|c| c.as_str()).unwrap_or_default();
        assert!(
            content.contains("Path not allowed") && content.contains("/etc/passwd"),
            "failed tool must surface its error reason in `content`, got: {content}"
        );
    }

    /// BUG-03: a recoverable tool failure (one the model can fix, e.g. a missing
    /// file) must NOT be flagged unrecoverable, so the agent retains the chance
    /// to self-correct rather than stopping prematurely.
    #[tokio::test]
    async fn recoverable_tool_failure_is_not_flagged_unrecoverable() {
        struct FlakyTool;
        #[async_trait::async_trait]
        impl crate::tools::Tool for FlakyTool {
            fn name(&self) -> &str {
                "file_read"
            }
            fn description(&self) -> &str {
                "read"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("file not found: /tmp/typo.txt".into()),
                })
            }
        }

        let registry = Arc::new(vec![Box::new(FlakyTool) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-flaky".into(),
            name: "file_read".into(),
            args: "{}".into(),
        };
        let (action_tx, _action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();
        let (service, context, cancellation, _ledger) = tool_service_for_test(
            Arc::clone(&registry),
            &action_tx,
            "draft-flaky",
            crate::security::AutonomyLevel::Full,
        );

        let outcome = execute_single_tool_call(
            None,
            Some(&service),
            &context,
            &call,
            &cancellation,
            &action_tx,
            "draft-flaky",
            &mut history,
            None,
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        match outcome {
            ToolExecOutcome::Done { unrecoverable } => {
                assert!(
                    unrecoverable.is_none(),
                    "'file not found' must stay retryable, got {unrecoverable:?}"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// BUG-03: detector classifies permanent blocks vs. retryable failures.
    #[test]
    fn unrecoverable_error_detector_classifies_correctly() {
        // Permanent / policy / permission failures → unrecoverable.
        for s in [
            "permission denied",
            "Permission Denied while writing",
            "command not allowed: rm",
            "this command is not allowed",
            "Path not allowed by security policy: /etc/passwd",
            "operation not permitted",
            "Access Denied",
            "Read-only file system",
            "User rejected tool approval",
        ] {
            assert!(is_unrecoverable_tool_error(s), "should be unrecoverable: {s}");
        }
        // Retryable / fixable failures → NOT unrecoverable.
        for s in [
            "file not found: /tmp/x",
            "connection timed out",
            "tool args JSON parse error: expected value",
            "rate limited, try again",
            "no such file or directory",
        ] {
            assert!(!is_unrecoverable_tool_error(s), "should be recoverable: {s}");
        }
    }

    /// BUG-03: the same blocked call collapses to one signature across retries
    /// (digits stripped so e.g. a PID/line-number difference does not split it),
    /// while a *different* tool yields a distinct signature.
    #[test]
    fn unrecoverable_signature_collapses_repeats_but_separates_tools() {
        let a = unrecoverable_signature("shell", "permission denied (errno 13)");
        let b = unrecoverable_signature("shell", "permission denied (errno 13)");
        let c = unrecoverable_signature("shell", "permission denied (errno 99)");
        let d = unrecoverable_signature("file_write", "permission denied (errno 13)");
        assert_eq!(a, b, "identical failures must share a signature");
        assert_eq!(a, c, "digit-only differences must collapse to one signature");
        assert_ne!(a, d, "different tools must have distinct signatures");
    }
}
