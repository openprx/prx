//! Provider-turn lifecycle: completion events, ordered commit and finalization of per-turn contexts.

use super::active_turn::rollback_cancelled_turn_history;
use super::chat_events::{finalize_chat_non_success_turn, finalize_chat_success_turn};
use super::input_queue::publish_main_queue_status;
use super::session_glue::{surface_session_message, surface_turn_elapsed_message};
#[cfg(feature = "terminal-tui")]
use super::tui;
use super::{decision_source, dispatcher, session};
use crate::agent::turn_contracts::ToolCallNotification;
use crate::channels::Channel;
use crate::channels::TerminalChannel;
use crate::channels::sanitize_channel_response;
use crate::config::Config;
use crate::llm::route_decision::ProviderExecutionOutcome;
use crate::llm::route_decision::RouteDecision;
use crate::memory::MemoryFabric;
use crate::memory::MessageEventScope;
use crate::providers::ChatMessage;
use crate::tools::Tool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Queue depth for provider-turn lifecycle events (`HandleAttached`, `Started`,
/// `Exited`) travelling from the effect executor back to the chat loop.
///
/// Sized as a backpressure valve, not a concurrency limit. Each turn emits
/// exactly three of these small, `Copy`-sized records, so this depth absorbs
/// roughly 1300 turns' worth of lifecycle traffic — far more than the chat loop
/// can fall behind by in practice, since it drains the queue at several points
/// in every iteration. If it ever does fill, the emitting turn parks on
/// `send().await` instead of the queue growing without bound; slowing turn
/// startup is the correct outcome, because the worker registry driving aborts
/// and completion routing must not fall arbitrarily far behind reality.
pub(crate) const PROVIDER_TURN_LIFECYCLE_CHANNEL_CAPACITY: usize = 4096;

#[derive(Debug, Clone)]
pub(super) struct ProviderTurnCompletionEvent {
    pub(super) task_id: crate::chat::turn_scheduler::TurnTaskId,
    // Only the TUI completion waiter fills these and only the TUI finalizer
    // reads them.
    #[cfg(feature = "terminal-tui")]
    pub(super) outcome: Option<dispatcher::TurnOutcomeKind>,
    #[cfg(feature = "terminal-tui")]
    pub(super) usage: crate::llm::route_decision::TokenUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProviderTurnCompletionContext {
    history_len_before_assistant: usize,
}

#[cfg(feature = "terminal-tui")]
pub(super) struct PerTurnContext {
    pub(super) cancellation: CancellationToken,
    pub(super) turn_binding: Option<Arc<decision_source::ChatTurnBinding>>,
    /// The disposition captured before provider execution can seal the
    /// decision context. `Some` identifies the fixed-terminal path; `None`
    /// identifies a provider-backed completion.
    pub(super) fixed_reply: Option<String>,
    pub(super) task_id: crate::chat::turn_scheduler::TurnTaskId,
    pub(super) draft_id: String,
    pub(super) delta_tx: Option<mpsc::Sender<String>>,
    pub(super) tool_event_tx: Option<mpsc::Sender<ToolCallNotification>>,
    pub(super) draft_updater: Option<tokio::task::JoinHandle<()>>,
    pub(super) tool_event_forwarder: Option<tokio::task::JoinHandle<()>>,
    pub(super) user_input: String,
    pub(super) turn_run_id: String,
    pub(super) route_scope: MessageEventScope,
    pub(super) route_decision: RouteDecision,
    pub(super) provider_started_at: chrono::DateTime<chrono::Utc>,
    pub(super) provider_name: String,
    pub(super) model_name: String,
    pub(super) history_len_before_user_turn: usize,
    pub(super) history_user_message: ChatMessage,
    /// Set when this turn was started by daemon-assigned work, and carried all
    /// the way to the commit because that is where the reply exists.
    ///
    /// A visible turn is *launched* by the main loop and finalized somewhere
    /// else entirely, so a guard left behind in the loop body would report the
    /// moment the turn started — an answer of "no reply" to work that was about
    /// to produce one. Moving it into the context ties the report to the turn
    /// rather than to the iteration that started it.
    pub(super) assignment: Option<crate::chat::sessions::assignment::AssignmentTurnGuard>,
}

#[cfg(feature = "terminal-tui")]
pub(super) struct PendingOrderedProviderTurnCommit {
    pub(super) context: PerTurnContext,
    pub(super) terminal_plan: ProviderTurnTerminalPlan,
}

#[derive(Debug)]
#[cfg(feature = "terminal-tui")]
pub(super) struct ResolvedProviderTurnCompletion {
    pub(super) outcome: Option<dispatcher::TurnOutcomeKind>,
    pub(super) usage: crate::llm::route_decision::TokenUsage,
}

#[derive(Debug, Clone)]
#[cfg(feature = "terminal-tui")]
pub(super) enum ProviderTurnTerminalPlan {
    Completed {
        final_text: String,
        reasoning: String,
        recorded_response: String,
        empty_response: bool,
        usage: crate::llm::route_decision::TokenUsage,
        history_commit_len: usize,
        final_text_chars: usize,
        recorded_response_chars: usize,
        summary: &'static str,
    },
    Failed {
        err: String,
        history_commit_len: usize,
        summary: String,
    },
    Cancelled {
        summary: &'static str,
    },
}

#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone)]
pub(super) struct ProviderTurnFinalizerEvent {
    pub(super) task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    pub(super) plan: ProviderTurnTerminalPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "terminal-tui")]
pub(super) struct ProviderTurnFinalizerResult {
    pub(super) task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    pub(super) terminal_status: &'static str,
    pub(super) finalized: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ProviderTurnVisibleAdmission {
    pub(super) active_workers: usize,
    pub(super) foreground_active: usize,
    pub(super) detached_active: usize,
    pub(super) effective_max_visible_turns: usize,
    pub(super) can_start_visible: bool,
}

#[cfg(feature = "terminal-tui")]
pub(super) enum ProviderTurnTerminalGate<'a> {
    Completed {
        history_commit_len: usize,
        final_text_chars: usize,
        recorded_response_chars: usize,
        usage: &'a crate::llm::route_decision::TokenUsage,
        summary: &'static str,
    },
    Failed {
        history_commit_len: usize,
        summary: String,
    },
    Cancelled {
        summary: &'static str,
    },
}

pub(super) fn provider_worker_status(
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
) -> crate::chat::action::ProviderWorkerStatus {
    use crate::chat::action::{ProviderWorkerRowKind, ProviderWorkerRowState, ProviderWorkerStatusRow};

    let mut status = crate::chat::action::ProviderWorkerStatus::default();
    for worker in workers.snapshot() {
        if worker.finalized_payload_ready {
            status.finalized_payloads = status.finalized_payloads.saturating_add(1);
            status.finalized_total_tokens = status
                .finalized_total_tokens
                .saturating_add(worker.finalized_total_tokens.unwrap_or_default());
        }
        let row_state = match worker.state {
            crate::chat::turn_worker::ProviderTurnWorkerState::Running => ProviderWorkerRowState::Running,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling => ProviderWorkerRowState::Cancelling,
            crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_) => {
                ProviderWorkerRowState::AwaitingCommit
            }
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed => ProviderWorkerRowState::Committed,
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled => ProviderWorkerRowState::Cancelled,
            crate::chat::turn_worker::ProviderTurnWorkerState::Failed => ProviderWorkerRowState::Failed,
        };
        if !worker.state.is_terminal() || worker.finalized_payload_ready {
            let row_kind = match worker.kind {
                crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited => {
                    ProviderWorkerRowKind::ForegroundAwaited
                }
                crate::chat::turn_worker::ProviderTurnWorkerKind::Detached => ProviderWorkerRowKind::Detached,
            };
            status.rows.push(ProviderWorkerStatusRow {
                task_id: worker.task_id.get(),
                sequence: worker.sequence,
                kind: row_kind,
                state: row_state,
                started_at_ms: worker.started_at_ms,
                finalized_total_tokens: worker.finalized_total_tokens,
                completion_ready: worker.completion_ready,
                recent_tool_call: None,
            });
        }
        match worker.state {
            crate::chat::turn_worker::ProviderTurnWorkerState::Running => {
                status.running = status.running.saturating_add(1);
                status.oldest_started_at_ms = Some(
                    status
                        .oldest_started_at_ms
                        .map_or(worker.started_at_ms, |current| current.min(worker.started_at_ms)),
                );
            }
            crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling => {
                status.cancelling = status.cancelling.saturating_add(1);
                status.oldest_started_at_ms = Some(
                    status
                        .oldest_started_at_ms
                        .map_or(worker.started_at_ms, |current| current.min(worker.started_at_ms)),
                );
            }
            crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_) => {
                status.awaiting_commit = status.awaiting_commit.saturating_add(1);
            }
            crate::chat::turn_worker::ProviderTurnWorkerState::Committed
            | crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled
            | crate::chat::turn_worker::ProviderTurnWorkerState::Failed => {}
        }
    }
    status
}

pub(super) fn publish_provider_worker_status(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
) {
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ProviderWorkerStatusUpdated {
            status: provider_worker_status(workers),
        },
        "chat.provider_worker_status_updated",
    );
}

pub(super) fn provider_turn_visible_admission(
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    target_kind: crate::chat::turn_worker::ProviderTurnWorkerKind,
    max_concurrent_visible_turns: usize,
) -> ProviderTurnVisibleAdmission {
    let effective_max_visible_turns = max_concurrent_visible_turns.max(1);
    let active_rows: Vec<_> = workers
        .snapshot()
        .into_iter()
        .filter(|worker| {
            matches!(
                worker.state,
                crate::chat::turn_worker::ProviderTurnWorkerState::Running
                    | crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling
                    | crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_)
            )
        })
        .collect();
    let active_workers = active_rows.len();
    let foreground_active = active_rows
        .iter()
        .filter(|worker| worker.kind == crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited)
        .count();
    let detached_active = active_rows
        .iter()
        .filter(|worker| worker.kind == crate::chat::turn_worker::ProviderTurnWorkerKind::Detached)
        .count();
    let can_start_visible = match target_kind {
        crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited => active_workers == 0,
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached => {
            foreground_active == 0 && detached_active < effective_max_visible_turns
        }
    };
    ProviderTurnVisibleAdmission {
        active_workers,
        foreground_active,
        detached_active,
        effective_max_visible_turns,
        can_start_visible,
    }
}

fn drain_provider_turn_lifecycle_events(
    rx: &mut mpsc::Receiver<dispatcher::ProviderTurnLifecycleEvent>,
    events_open: &mut bool,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
) -> bool {
    if !*events_open {
        return false;
    }

    let mut drained = false;
    loop {
        match rx.try_recv() {
            Ok(event) => {
                drained = true;
                record_provider_turn_lifecycle_event(workers, event);
            }
            Err(mpsc::error::TryRecvError::Empty) => return drained,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                *events_open = false;
                return drained;
            }
        }
    }
}

pub(super) fn record_provider_turn_lifecycle_event(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    event: dispatcher::ProviderTurnLifecycleEvent,
) {
    let (task_id, lease_id, kind, result) = match event {
        dispatcher::ProviderTurnLifecycleEvent::HandleAttached {
            task_id,
            lease_id,
            abort_handle,
        } => {
            let result = workers.attach_execution_handle(task_id, lease_id, abort_handle);
            (task_id, lease_id, "handle_attached", result)
        }
        dispatcher::ProviderTurnLifecycleEvent::Started { task_id, lease_id } => {
            let result = workers.record_execution_started(task_id, lease_id);
            (task_id, lease_id, "started", result)
        }
        dispatcher::ProviderTurnLifecycleEvent::Exited { task_id, lease_id } => {
            let result = workers.record_execution_exited(task_id, lease_id);
            (task_id, lease_id, "exited", result)
        }
    };
    if let Err(error) = result {
        tracing::warn!(
            task_id = task_id.get(),
            lease_id,
            kind,
            error = ?error,
            "ProviderTurnWorkerRegistry provider execution lifecycle record failed"
        );
    }
}

fn record_provider_turn_completion_ready(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
) {
    if let Err(error) = workers.record_completion_ready(task_id) {
        tracing::warn!(
            task_id = task_id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider completion-ready gate failed"
        );
    }
}

pub(super) fn route_provider_completion_event(
    lifecycle_rx: &mut mpsc::Receiver<dispatcher::ProviderTurnLifecycleEvent>,
    lifecycle_events_open: &mut bool,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    pending: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionEvent>,
    current_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    completion: ProviderTurnCompletionEvent,
) -> Option<ProviderTurnCompletionEvent> {
    if drain_provider_turn_lifecycle_events(lifecycle_rx, lifecycle_events_open, workers) {
        tracing::trace!(
            task_id = completion.task_id.get(),
            "drained provider lifecycle events before routing completion"
        );
    }
    let task_id = completion.task_id;
    record_provider_turn_completion_ready(workers, task_id);
    if Some(task_id) == current_task_id {
        return Some(completion);
    }
    if pending.insert(task_id, completion).is_some() {
        tracing::warn!(
            task_id = task_id.get(),
            "replaced duplicate pending provider completion event"
        );
    }
    None
}

pub(super) fn route_provider_completion_event_and_publish(
    lifecycle_rx: &mut mpsc::Receiver<dispatcher::ProviderTurnLifecycleEvent>,
    lifecycle_events_open: &mut bool,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    pending: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionEvent>,
    current_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    completion: ProviderTurnCompletionEvent,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
) -> Option<ProviderTurnCompletionEvent> {
    let route = route_provider_completion_event(
        lifecycle_rx,
        lifecycle_events_open,
        workers,
        pending,
        current_task_id,
        completion,
    );
    publish_provider_worker_status(chat_dispatcher, workers);
    route
}

pub(super) fn record_provider_turn_completion_context(
    contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionContext>,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_len_before_assistant: usize,
) {
    if let Some(task_id) = task_id
        && contexts
            .insert(
                task_id,
                ProviderTurnCompletionContext {
                    history_len_before_assistant,
                },
            )
            .is_some()
    {
        tracing::warn!(
            task_id = task_id.get(),
            "replaced duplicate provider turn completion context"
        );
    }
}

pub(super) fn take_provider_turn_completion_history_len(
    contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, ProviderTurnCompletionContext>,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    fallback_history_len: usize,
) -> usize {
    task_id
        .and_then(|task_id| contexts.remove(&task_id))
        .map_or(fallback_history_len, |context| context.history_len_before_assistant)
}

fn provider_turn_finalized_payload_from_usage(
    history_commit_len: usize,
    final_text_chars: usize,
    recorded_response_chars: usize,
    usage: &crate::llm::route_decision::TokenUsage,
) -> crate::chat::turn_worker::ProviderTurnFinalizedPayload {
    let prompt_tokens = usage.prompt_tokens.map_or(0, u64::from);
    let completion_tokens = usage.completion_tokens.map_or(0, u64::from);
    let total_tokens = usage
        .total_tokens
        .map_or_else(|| prompt_tokens.saturating_add(completion_tokens), u64::from);
    crate::chat::turn_worker::ProviderTurnFinalizedPayload {
        history_commit_len,
        final_text_chars,
        recorded_response_chars,
        total_tokens,
        prompt_tokens,
        completion_tokens,
    }
}

fn record_provider_turn_finalized_payload(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    payload: crate::chat::turn_worker::ProviderTurnFinalizedPayload,
) -> bool {
    let Some(id) = id else {
        tracing::warn!("ProviderTurnWorkerRegistry missing provider task for finalized payload");
        return false;
    };
    if let Err(error) = workers.record_finalized_payload(id, payload) {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider finalized payload record failed"
        );
        return false;
    }
    true
}

pub(super) fn gate_completed_provider_turn_finalization(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    final_text_chars: usize,
    recorded_response_chars: usize,
    usage: &crate::llm::route_decision::TokenUsage,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    let finalized_payload = provider_turn_finalized_payload_from_usage(
        history_commit_len,
        final_text_chars,
        recorded_response_chars,
        usage,
    );
    if !record_provider_turn_finalized_payload(workers, id, finalized_payload) {
        return Vec::new();
    }
    mark_provider_turn_completed(scheduler, coordinator, workers, id, history_commit_len, summary)
}

pub(super) fn gate_failed_provider_turn_finalization(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    summary: impl Into<String>,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    mark_provider_turn_failed(scheduler, coordinator, workers, id, history_commit_len, summary)
}

pub(super) fn gate_cancelled_provider_turn_finalization(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    mark_provider_turn_cancelled(scheduler, coordinator, workers, id, summary)
}

#[allow(clippy::option_if_let_else)]
#[cfg(feature = "terminal-tui")]
pub(super) fn resolve_provider_turn_completion(
    turn_signal: &dispatcher::TurnCompletionSignal,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    completion_event: Option<ProviderTurnCompletionEvent>,
) -> ResolvedProviderTurnCompletion {
    let outcome = if let Some(completion) = completion_event.as_ref() {
        completion.outcome.clone().or_else(|| turn_signal.consume_outcome())
    } else if let Some(id) = task_id {
        turn_signal
            .consume_turn_outcome(id)
            .or_else(|| turn_signal.consume_outcome())
    } else {
        turn_signal.consume_outcome()
    };
    let usage = if let Some(completion) = completion_event {
        if completion.usage.has_any_tokens() {
            completion.usage
        } else {
            turn_signal.consume_usage()
        }
    } else if let Some(id) = task_id {
        let keyed_usage = turn_signal.consume_turn_usage(id);
        if keyed_usage.has_any_tokens() {
            keyed_usage
        } else {
            turn_signal.consume_usage()
        }
    } else {
        turn_signal.consume_usage()
    };
    if let Some(id) = task_id {
        turn_signal.unregister_turn(id);
    }
    ResolvedProviderTurnCompletion { outcome, usage }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn provider_turn_terminal_plan_from_completion(
    resolved: ResolvedProviderTurnCompletion,
    history_len: usize,
    tools_registry: &[Box<dyn Tool>],
) -> ProviderTurnTerminalPlan {
    match resolved.outcome {
        Some(dispatcher::TurnOutcomeKind::Completed { final_text, reasoning }) => {
            let mut usage = resolved.usage;
            if !usage.has_any_tokens() {
                let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                usage = accumulator.finish_or_estimate_completion_chars(final_text.chars().count());
            }
            if crate::agent::response_guard::is_empty_assistant_response(&final_text, false) {
                ProviderTurnTerminalPlan::Completed {
                    final_text,
                    reasoning: String::new(),
                    recorded_response: String::new(),
                    empty_response: true,
                    usage,
                    history_commit_len: history_len,
                    final_text_chars: 0,
                    recorded_response_chars: 0,
                    summary: "redux driver completed with empty response",
                }
            } else {
                let recorded_response = sanitize_channel_response(&final_text, tools_registry);
                let final_text_chars = final_text.chars().count();
                let recorded_response_chars = recorded_response.chars().count();
                ProviderTurnTerminalPlan::Completed {
                    final_text,
                    reasoning,
                    final_text_chars,
                    recorded_response_chars,
                    recorded_response,
                    empty_response: false,
                    usage,
                    history_commit_len: history_len.saturating_add(1),
                    summary: "redux driver completed",
                }
            }
        }
        Some(dispatcher::TurnOutcomeKind::Failed { err, retryable: _ }) => ProviderTurnTerminalPlan::Failed {
            summary: format!("redux driver failed: {err}"),
            err,
            history_commit_len: history_len,
        },
        Some(dispatcher::TurnOutcomeKind::Cancelled) | None => ProviderTurnTerminalPlan::Cancelled {
            summary: "redux driver cancelled",
        },
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn gate_provider_turn_terminal_finalization(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    gate: ProviderTurnTerminalGate<'_>,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    match gate {
        ProviderTurnTerminalGate::Completed {
            history_commit_len,
            final_text_chars,
            recorded_response_chars,
            usage,
            summary,
        } => gate_completed_provider_turn_finalization(
            scheduler,
            coordinator,
            workers,
            id,
            history_commit_len,
            final_text_chars,
            recorded_response_chars,
            usage,
            summary,
        ),
        ProviderTurnTerminalGate::Failed {
            history_commit_len,
            summary,
        } => gate_failed_provider_turn_finalization(scheduler, coordinator, workers, id, history_commit_len, summary),
        ProviderTurnTerminalGate::Cancelled { summary } => {
            gate_cancelled_provider_turn_finalization(scheduler, coordinator, workers, id, summary)
        }
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn finalize_provider_turn_from_event(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    event: ProviderTurnFinalizerEvent,
) -> Vec<ProviderTurnFinalizerResult> {
    let task_id = event.task_id;
    let decisions = match event.plan {
        ProviderTurnTerminalPlan::Completed {
            usage,
            history_commit_len,
            final_text_chars,
            recorded_response_chars,
            summary,
            ..
        } => gate_provider_turn_terminal_finalization(
            scheduler,
            coordinator,
            workers,
            task_id,
            ProviderTurnTerminalGate::Completed {
                history_commit_len,
                final_text_chars,
                recorded_response_chars,
                usage: &usage,
                summary,
            },
        ),
        ProviderTurnTerminalPlan::Failed {
            history_commit_len,
            summary,
            ..
        } => gate_provider_turn_terminal_finalization(
            scheduler,
            coordinator,
            workers,
            task_id,
            ProviderTurnTerminalGate::Failed {
                history_commit_len,
                summary,
            },
        ),
        ProviderTurnTerminalPlan::Cancelled { summary } => gate_provider_turn_terminal_finalization(
            scheduler,
            coordinator,
            workers,
            task_id,
            ProviderTurnTerminalGate::Cancelled { summary },
        ),
    };
    if decisions.is_empty() {
        return vec![ProviderTurnFinalizerResult {
            task_id,
            terminal_status: "unknown",
            finalized: false,
        }];
    }
    decisions
        .iter()
        .map(provider_turn_finalizer_result_from_commit_decision)
        .collect()
}

#[cfg(feature = "terminal-tui")]
const fn provider_turn_finalizer_result_from_commit_decision(
    decision: &crate::chat::history_commit::HistoryCommitDecision,
) -> ProviderTurnFinalizerResult {
    match decision {
        crate::chat::history_commit::HistoryCommitDecision::Commit { task_id, .. } => ProviderTurnFinalizerResult {
            task_id: Some(*task_id),
            terminal_status: "completed",
            finalized: true,
        },
        crate::chat::history_commit::HistoryCommitDecision::Skip { task_id, status, .. } => {
            ProviderTurnFinalizerResult {
                task_id: Some(*task_id),
                terminal_status: match status {
                    crate::chat::history_commit::HistoryCommitStatus::Completed => "completed",
                    crate::chat::history_commit::HistoryCommitStatus::Cancelled => "cancelled",
                    crate::chat::history_commit::HistoryCommitStatus::Failed => "failed",
                },
                finalized: true,
            }
        }
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn enqueue_provider_turn_finalizer_event(
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    plan: ProviderTurnTerminalPlan,
) {
    queue.push_back(ProviderTurnFinalizerEvent { task_id, plan });
}

pub(super) fn dispatch_ordered_provider_turn_commit(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    draft_id: &str,
    user_input: &str,
    final_text: &str,
    reasoning: &str,
    empty_response: bool,
    record_user: bool,
) -> std::result::Result<(), String> {
    if record_user {
        let result = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordUserTurn(user_input.to_string()),
            "chat.ordered_record_user_turn",
        );
        if !matches!(result, dispatcher::DispatchResult::Sent) {
            return Err(format!("ordered history dispatch failed: {result:?}"));
        }
    }
    if !empty_response {
        let result = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordAssistantTurn {
                task_id: Some(task_id),
                content: final_text.to_string(),
            },
            "chat.ordered_record_assistant_turn",
        );
        if !matches!(result, dispatcher::DispatchResult::Sent) {
            return Err(format!("ordered history dispatch failed: {result:?}"));
        }
    }
    let result = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::StreamCompleted {
            draft_id: draft_id.to_string(),
            final_text: final_text.to_string(),
            reasoning: reasoning.to_string(),
        },
        "chat.ordered_stream_completed",
    );
    if !matches!(result, dispatcher::DispatchResult::Sent) {
        return Err(format!("ordered history dispatch failed: {result:?}"));
    }
    Ok(())
}

pub(super) async fn dispatch_ordered_provider_turn_commit_and_wait(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    turn_signal: &dispatcher::TurnCompletionSignal,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    draft_id: &str,
    user_input: &str,
    final_text: &str,
    reasoning: &str,
    empty_response: bool,
    record_user: bool,
) -> std::result::Result<(), String> {
    // The first task-scoped notification means the provider produced its
    // terminal payload. The ordered StreamCompleted below is a distinct phase:
    // the dispatcher only emits its second notification after every reducer
    // effect, including SaveSession, has completed. Arm the waiter before
    // dispatch so the latch-less Notify cannot be missed.
    // Provider completion consumption unregisters the first-phase slot. Open
    // a fresh slot for the same task/draft so the ordered reducer commit has
    // its own acknowledgement lifecycle.
    turn_signal.register_turn(task_id, draft_id.to_string());
    let Some(persistence_committed) = turn_signal.notified_for(task_id) else {
        tracing::error!(task_id = task_id.get(), "missing task-scoped persistence waiter");
        turn_signal.fail_history_commit("missing task-scoped persistence waiter");
        return Err("missing task-scoped persistence waiter".to_string());
    };
    if let Err(error) = dispatch_ordered_provider_turn_commit(
        chat_dispatcher,
        task_id,
        draft_id,
        user_input,
        final_text,
        reasoning,
        empty_response,
        record_user,
    ) {
        turn_signal.unregister_turn(task_id);
        turn_signal.fail_history_commit(error.clone());
        return Err(error);
    }
    let result = match tokio::time::timeout(Duration::from_secs(30), persistence_committed).await {
        Ok(()) => {
            // Consume the second terminal outcome so a later waiter cannot
            // mistake this persistence acknowledgement for a provider result.
            let outcome = turn_signal.consume_turn_outcome(task_id);
            turn_signal.unregister_turn(task_id);
            match outcome {
                Some(dispatcher::TurnOutcomeKind::Completed { .. }) => Ok(()),
                Some(dispatcher::TurnOutcomeKind::Failed { err, .. }) => Err(err),
                Some(dispatcher::TurnOutcomeKind::Cancelled) => {
                    Err("ordered session persistence was cancelled".to_string())
                }
                None => Err("ordered session persistence acknowledged without an outcome".to_string()),
            }
        }
        Err(_) => {
            tracing::error!(
                task_id = task_id.get(),
                "timed out waiting for ordered session persistence"
            );
            turn_signal.unregister_turn(task_id);
            Err("timed out waiting for ordered session persistence".to_string())
        }
    };
    if let Err(error) = &result {
        turn_signal.fail_history_commit(error.clone());
    }
    result
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
async fn commit_completed_provider_turn(
    pending_commit: PendingOrderedProviderTurnCommit,
    history: &mut Vec<ChatMessage>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    provider_turn_workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    turn_signal: &dispatcher::TurnCompletionSignal,
) -> bool {
    let mut pending = pending_commit.context;
    let ProviderTurnTerminalPlan::Completed {
        final_text,
        reasoning,
        recorded_response,
        empty_response,
        usage: redux_tokens_used,
        history_commit_len,
        ..
    } = pending_commit.terminal_plan
    else {
        tracing::warn!(
            task_id = pending.task_id.get(),
            "ordered provider turn commit received non-completed terminal plan"
        );
        return false;
    };

    let provider_turn_task_id = Some(pending.task_id);
    // Provider trace capture seals execution authority before this ordered
    // finalizer runs. Re-reading the live disposition here would therefore
    // reject every successful provider-backed turn. Use the immutable
    // pre-execution classification carried with the turn instead.
    let fixed_reply = pending.fixed_reply.take();
    let (provider_outcome, terminal_usage) = if let Some(reply) = fixed_reply {
        if empty_response || final_text != reply || recorded_response != reply || !reasoning.is_empty() {
            tracing::error!("Chat fixed reply differs from its decision");
            return false;
        }
        let Some(binding) = pending.turn_binding.as_ref() else {
            return false;
        };
        match binding
            .close(
                memory_fabric,
                history_commit_len,
                pending.provider_started_at,
                &pending.cancellation,
            )
            .await
        {
            Ok(receipt) => (None, receipt.usage_settlement),
            Err(error) => {
                tracing::error!(error = %error, "Chat fixed terminal commit failed");
                return false;
            }
        }
    } else {
        let provider_outcome = if let Some(binding) = pending.turn_binding.as_ref() {
            match binding.provider_outcome(&pending.route_decision, pending.provider_started_at) {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::error!(error = %error, "Chat completion has no authenticated provider trace");
                    return false;
                }
            }
        } else {
            ProviderExecutionOutcome::success_for_decision_with_usage(
                &pending.route_decision,
                pending.provider_started_at,
                redux_tokens_used.clone(),
            )
        };
        let terminal_usage = match finalize_chat_success_turn(
            pending.turn_binding.as_deref(),
            memory_fabric,
            chat_session_key,
            &pending.turn_run_id,
            &pending.provider_name,
            &pending.model_name,
            pending.route_scope.clone(),
            provider_outcome.clone(),
            (!empty_response).then(|| recorded_response.clone()),
            history_commit_len,
            if empty_response {
                "redux driver completed with empty response"
            } else {
                "redux driver completed"
            },
            &config.cost,
            &config.workspace_dir,
        )
        .await
        {
            Ok(receipt) => receipt.usage_settlement,
            Err(error) => {
                tracing::error!(error = %error, "Chat terminal persistence is unconfirmed");
                return false;
            }
        };
        (Some(provider_outcome), terminal_usage)
    };
    let finished_at = provider_outcome
        .as_ref()
        .map_or_else(chrono::Utc::now, |outcome| outcome.finished_at);
    if empty_response {
        if let Err(e) = terminal.cancel_draft("user", &pending.draft_id).await {
            tracing::debug!(error = %e, "Redux driver: cancel empty-response draft failed");
        }
        let persistence_result = dispatch_ordered_provider_turn_commit_and_wait(
            chat_dispatcher,
            turn_signal,
            pending.task_id,
            &pending.draft_id,
            &pending.user_input,
            &final_text,
            &reasoning,
            empty_response,
            true,
        )
        .await;
        if let Err(error) = persistence_result {
            surface_session_message(
                chat_dispatcher,
                sessions_redraw_handle,
                &format!("Session persistence failed; dependent commands remain blocked: {error}"),
            );
            return false;
        }
        if let Some(guard) = pending.assignment.as_mut() {
            guard.failed("The turn finished without reply text.");
        }
        history.push(pending.history_user_message.clone());
        chat_session.add_user_turn(&pending.user_input);
        if let Some(record) = terminal_usage.and_then(|record| chat_session.record_usage_settlement(record)) {
            record_provider_turn_usage(turn_scheduler, provider_turn_task_id, &record);
            chat_mirror.lock().token_usage_summary = chat_session.token_usage_summary();
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::ProviderUsageRecorded {
                    task_id: provider_turn_task_id,
                    usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                    record,
                },
                "chat.provider_usage_recorded_empty_response",
            );
            if let Some(tx) = redraw_tx_for_main {
                let _ = tx.try_send(());
            }
        }
        surface_turn_elapsed_message(
            chat_dispatcher,
            sessions_redraw_handle,
            "completed",
            pending.provider_started_at,
            finished_at,
        );
        publish_main_queue_status(chat_dispatcher, turn_scheduler);
        publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        return true;
    }

    let persistence_result = dispatch_ordered_provider_turn_commit_and_wait(
        chat_dispatcher,
        turn_signal,
        pending.task_id,
        &pending.draft_id,
        &pending.user_input,
        &final_text,
        &reasoning,
        empty_response,
        true,
    )
    .await;
    if let Err(error) = persistence_result {
        surface_session_message(
            chat_dispatcher,
            sessions_redraw_handle,
            &format!("Session persistence failed; dependent commands remain blocked: {error}"),
        );
        return false;
    }
    history.push(pending.history_user_message.clone());
    history.push(ChatMessage::assistant(final_text.clone()));
    if let Err(e) = terminal.finalize_draft("user", &pending.draft_id, &final_text).await {
        tracing::warn!(error = %e, "Redux driver: finalize_draft failed");
    }
    if let Some(provider_outcome) = provider_outcome.as_ref() {
        let attempts_count = u8::try_from(provider_outcome.attempts.len()).unwrap_or(u8::MAX);
        crate::runtime::control_ladder::append_provider_outcome_trace(
            std::path::Path::new(&config.workspace_dir),
            &provider_outcome.decision_id,
            &provider_outcome.final_provider,
            &provider_outcome.final_model,
            attempts_count,
            "success",
        );
    }
    chat_session.add_user_turn(&pending.user_input);
    chat_session.add_assistant_turn(&recorded_response, Vec::new());
    if let Some(guard) = pending.assignment.as_mut() {
        guard.completed(&recorded_response);
    }
    if let Some(record) = terminal_usage.and_then(|record| chat_session.record_usage_settlement(record)) {
        record_provider_turn_usage(turn_scheduler, provider_turn_task_id, &record);
        chat_mirror.lock().token_usage_summary = chat_session.token_usage_summary();
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::ProviderUsageRecorded {
                task_id: provider_turn_task_id,
                usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                record,
            },
            "chat.provider_usage_recorded",
        );
        if let Some(tx) = redraw_tx_for_main {
            let _ = tx.try_send(());
        }
    }
    surface_turn_elapsed_message(
        chat_dispatcher,
        sessions_redraw_handle,
        "completed",
        pending.provider_started_at,
        finished_at,
    );
    publish_main_queue_status(chat_dispatcher, turn_scheduler);
    publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
    true
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_ready_ordered_provider_turn_commits(
    results: Vec<ProviderTurnFinalizerResult>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    history: &mut Vec<ChatMessage>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    provider_turn_workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    turn_signal: &dispatcher::TurnCompletionSignal,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) -> bool {
    let mut applied = false;
    for result in results {
        if turn_signal.ensure_history_commits_healthy().is_err() {
            break;
        }
        if !result.finalized || result.terminal_status != "completed" {
            continue;
        }
        let Some(task_id) = result.task_id else {
            continue;
        };
        let Some(pending_commit) = pending_ordered_provider_turn_commits.remove(&task_id) else {
            tracing::debug!(
                task_id = task_id.get(),
                "ordered provider turn commit ready before local payload was registered"
            );
            continue;
        };
        let committed = commit_completed_provider_turn(
            pending_commit,
            history,
            turn_scheduler,
            provider_turn_workers,
            chat_mirror,
            chat_dispatcher,
            terminal,
            memory_fabric,
            chat_session_key,
            chat_session,
            config,
            sessions_redraw_handle,
            redraw_tx_for_main,
            turn_signal,
        )
        .await;
        if !committed {
            turn_signal.fail_history_commit(format!("ordered turn {} was not committed", task_id.get()));
            if plain_mode {
                *plain_mode_turn_failed = true;
            }
            break;
        }
        applied = true;
    }
    applied
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
async fn finalize_per_turn_context(
    mut pending: PerTurnContext,
    completion_event: Option<ProviderTurnCompletionEvent>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    turn_signal: &dispatcher::TurnCompletionSignal,
    provider_turn_completion_contexts: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    >,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    provider_turn_finalizer_events: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    history_commit_coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) {
    let provider_turn_task_id = Some(pending.task_id);
    let resolved_completion = resolve_provider_turn_completion(turn_signal, provider_turn_task_id, completion_event);
    let completion_history_len = take_provider_turn_completion_history_len(
        provider_turn_completion_contexts,
        provider_turn_task_id,
        history.len(),
    );
    let terminal_plan =
        provider_turn_terminal_plan_from_completion(resolved_completion, completion_history_len, tools_registry);
    tracing::debug!(
        task_id = pending.task_id.get(),
        draft_id = %pending.draft_id,
        turn_run_id = %pending.turn_run_id,
        provider_name = %crate::providers::redact_endpoint_userinfo(&pending.provider_name),
        model_name = %pending.model_name,
        history_len_before_user_turn = pending.history_len_before_user_turn,
        "finalizing provider turn context"
    );
    enqueue_provider_turn_finalizer_event(
        provider_turn_finalizer_events,
        provider_turn_task_id,
        terminal_plan.clone(),
    );
    let finalizer_results = drain_provider_turn_finalizer_events_and_publish(
        turn_scheduler,
        history_commit_coordinator,
        provider_turn_workers,
        provider_turn_finalizer_events,
        chat_dispatcher,
    );

    drop(pending.delta_tx.take());
    drop(pending.tool_event_tx.take());
    if let Some(handle) = pending.draft_updater.take() {
        let _ = handle.await;
    }
    if let Some(handle) = pending.tool_event_forwarder.take() {
        let _ = handle.await;
    }

    match terminal_plan {
        plan @ ProviderTurnTerminalPlan::Completed { .. } => {
            let task_id = pending.task_id;
            if pending_ordered_provider_turn_commits
                .insert(
                    task_id,
                    PendingOrderedProviderTurnCommit {
                        context: pending,
                        terminal_plan: plan,
                    },
                )
                .is_some()
            {
                tracing::warn!(
                    task_id = task_id.get(),
                    "replaced pending ordered provider turn commit payload"
                );
            }
        }
        ProviderTurnTerminalPlan::Failed { err, .. } => {
            if let Some(guard) = pending.assignment.as_mut() {
                guard.failed(&format!("The turn this chat ran for the assignment failed: {err}"));
            }
            if let Err(error) = finalize_chat_non_success_turn(
                pending.turn_binding.as_deref(),
                memory_fabric,
                &pending.turn_run_id,
                pending.route_scope.clone(),
                crate::agent::terminal::TurnTerminalStatus::Failed,
                None,
                pending.provider_started_at,
                &format!("redux driver failed: {err}"),
                &config.cost,
                &config.workspace_dir,
            )
            .await
            {
                if pending
                    .turn_binding
                    .as_ref()
                    .is_some_and(|binding| binding.is_decided())
                {
                    *plain_mode_turn_failed = true;
                }
                tracing::warn!(error = %error, "Failed to commit ordered failed Chat terminal event");
            }
            let interactive_tui_active = redraw_tx_for_main.is_some();
            if !interactive_tui_active {
                let _ = terminal.cancel_draft("user", &pending.draft_id).await;
                eprintln!("\nError: {err}\n");
            }
            if plain_mode {
                *plain_mode_turn_failed = true;
            }
            surface_turn_elapsed_message(
                chat_dispatcher,
                sessions_redraw_handle,
                "failed",
                pending.provider_started_at,
                chrono::Utc::now(),
            );
            publish_main_queue_status(chat_dispatcher, turn_scheduler);
            publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        }
        ProviderTurnTerminalPlan::Cancelled { .. } => {
            // Deliberately not marked: the guard's own default already says the
            // turn ended before it answered, which is exactly what happened —
            // and `cancelled` is the daemon's verdict, not a client's to claim.
            if let Err(error) = finalize_chat_non_success_turn(
                pending.turn_binding.as_deref(),
                memory_fabric,
                &pending.turn_run_id,
                pending.route_scope.clone(),
                crate::agent::terminal::TurnTerminalStatus::Cancelled,
                None,
                pending.provider_started_at,
                "redux driver cancelled",
                &config.cost,
                &config.workspace_dir,
            )
            .await
            {
                if pending
                    .turn_binding
                    .as_ref()
                    .is_some_and(|binding| binding.is_decided())
                {
                    *plain_mode_turn_failed = true;
                }
                tracing::warn!(error = %error, "Failed to commit ordered cancelled Chat terminal event");
            }
            let _ = terminal.cancel_draft("user", &pending.draft_id).await;
            rollback_cancelled_turn_history(history, pending.history_len_before_user_turn);
            publish_main_queue_status(chat_dispatcher, turn_scheduler);
            publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        }
    }
    let _ = apply_ready_ordered_provider_turn_commits(
        finalizer_results,
        pending_ordered_provider_turn_commits,
        history,
        turn_scheduler,
        provider_turn_workers,
        chat_mirror,
        chat_dispatcher,
        terminal,
        memory_fabric,
        chat_session_key,
        chat_session,
        config,
        sessions_redraw_handle,
        redraw_tx_for_main,
        turn_signal,
        plain_mode,
        plain_mode_turn_failed,
    )
    .await;
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_ready_per_turn_contexts(
    per_turn_contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, PerTurnContext>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    pending_provider_completion_events: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionEvent,
    >,
    turn_signal: &dispatcher::TurnCompletionSignal,
    provider_turn_completion_contexts: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    >,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    provider_turn_finalizer_events: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    history_commit_coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) -> bool {
    let ready_task_ids: Vec<_> = per_turn_contexts
        .keys()
        .copied()
        .filter(|task_id| pending_provider_completion_events.contains_key(task_id))
        .collect();
    if ready_task_ids.is_empty() {
        return false;
    }
    for task_id in ready_task_ids {
        if turn_signal.ensure_history_commits_healthy().is_err() {
            break;
        }
        let Some(pending) = per_turn_contexts.remove(&task_id) else {
            continue;
        };
        let completion = pending_provider_completion_events.remove(&task_id);
        finalize_per_turn_context(
            pending,
            completion,
            pending_ordered_provider_turn_commits,
            turn_signal,
            provider_turn_completion_contexts,
            history,
            tools_registry,
            provider_turn_finalizer_events,
            turn_scheduler,
            history_commit_coordinator,
            provider_turn_workers,
            chat_mirror,
            chat_dispatcher,
            terminal,
            memory_fabric,
            chat_session_key,
            chat_session,
            config,
            sessions_redraw_handle,
            redraw_tx_for_main,
            plain_mode,
            plain_mode_turn_failed,
        )
        .await;
    }
    true
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_all_per_turn_contexts_as_cancelled(
    per_turn_contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, PerTurnContext>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    turn_signal: &dispatcher::TurnCompletionSignal,
    provider_turn_completion_contexts: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    >,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    provider_turn_finalizer_events: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    history_commit_coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    chat_session: &mut session::ChatSession,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) {
    let pending_turns: Vec<_> = per_turn_contexts.drain().map(|(_, pending)| pending).collect();
    for pending in pending_turns {
        finalize_per_turn_context(
            pending,
            None,
            pending_ordered_provider_turn_commits,
            turn_signal,
            provider_turn_completion_contexts,
            history,
            tools_registry,
            provider_turn_finalizer_events,
            turn_scheduler,
            history_commit_coordinator,
            provider_turn_workers,
            chat_mirror,
            chat_dispatcher,
            terminal,
            memory_fabric,
            chat_session_key,
            chat_session,
            config,
            sessions_redraw_handle,
            redraw_tx_for_main,
            plain_mode,
            plain_mode_turn_failed,
        )
        .await;
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn drain_provider_turn_finalizer_events(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
) -> Vec<ProviderTurnFinalizerResult> {
    let mut results = Vec::new();
    while let Some(event) = queue.pop_front() {
        results.extend(finalize_provider_turn_from_event(
            scheduler,
            coordinator,
            workers,
            event,
        ));
    }
    results
}

#[cfg(feature = "terminal-tui")]
pub(super) fn drain_provider_turn_finalizer_events_and_publish(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
) -> Vec<ProviderTurnFinalizerResult> {
    let results = drain_provider_turn_finalizer_events(scheduler, coordinator, workers, queue);
    if !results.is_empty() {
        publish_main_queue_status(chat_dispatcher, scheduler);
        publish_provider_worker_status(chat_dispatcher, workers);
    }
    results
}

pub(super) fn start_provider_turn_task(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    existing_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    input: &str,
    history_base_len: usize,
) -> Option<crate::chat::turn_scheduler::TurnTaskId> {
    let id = existing_task_id.unwrap_or_else(|| {
        scheduler.enqueue(
            input.to_string(),
            crate::chat::turn_scheduler::TurnPriority::Normal,
            history_base_len,
        )
    });
    if let Err(error) = scheduler.start_task(id) {
        tracing::warn!(
            task_id = id.get(),
            reused_queued_task = existing_task_id.is_some(),
            error = ?error,
            "TurnScheduler provider task start failed"
        );
        return None;
    }
    Some(id)
}

pub(super) fn register_provider_history_commit_task(
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) {
    if let Some(id) = id
        && let Some(task) = scheduler.task(id)
        && let Err(error) = coordinator.register_task(task)
    {
        tracing::warn!(
            task_id = id.get(),
            sequence = task.sequence,
            error = ?error,
            "HistoryCommitCoordinator provider task registration failed"
        );
    }
}

pub(super) fn register_provider_turn_worker(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    kind: crate::chat::turn_worker::ProviderTurnWorkerKind,
) {
    if let Some(id) = id
        && let Some(task) = scheduler.task(id)
        && let Err(error) = workers.start_from_task(task, kind)
    {
        tracing::warn!(
            task_id = id.get(),
            sequence = task.sequence,
            kind = ?kind,
            error = ?error,
            "ProviderTurnWorkerRegistry provider worker registration failed"
        );
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn spawn_provider_turn_completion_waiter(
    turn_signal: dispatcher::TurnCompletionSignal,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    tx: mpsc::Sender<ProviderTurnCompletionEvent>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let notified = turn_signal.notified_for(task_id);
        match notified {
            Some(notified) => {
                tokio::select! {
                    () = notified => {}
                    () = shutdown.cancelled() => {}
                }
            }
            None => {
                tokio::select! {
                    () = turn_signal.notified() => {}
                    () = shutdown.cancelled() => {}
                }
            }
        }
        let event = ProviderTurnCompletionEvent {
            task_id,
            outcome: turn_signal.consume_turn_outcome(task_id),
            usage: turn_signal.consume_turn_usage(task_id),
        };
        if tx.send(event).await.is_err() {
            tracing::debug!(
                task_id = task_id.get(),
                "provider turn completion waiter could not deliver event"
            );
        }
    });
}

pub(super) fn request_provider_turn_cancel(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    reason: &'static str,
) {
    if let Some(id) = id {
        if let Err(error) = scheduler.request_cancel(id) {
            tracing::warn!(
                task_id = id.get(),
                reason,
                error = ?error,
                "TurnScheduler provider task cancel request failed"
            );
        }
        if let Err(error) = workers.request_cancel(id) {
            tracing::warn!(
                task_id = id.get(),
                reason,
                error = ?error,
                "ProviderTurnWorkerRegistry provider worker cancel request failed"
            );
        }
    }
}

pub(super) fn record_provider_turn_usage(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    record: &crate::chat::session::MainSessionTokenUsageRecord,
) {
    if let Some(id) = id
        && let Err(error) = scheduler.record_usage(id, record)
    {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "TurnScheduler provider usage ledger record failed"
        );
    }
}

pub(super) fn mark_provider_turn_completed(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_completed(id, history_commit_len, summary) {
            Ok(()) => {
                if record_provider_worker_completed(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = id.get(),
                    error = ?error,
                    "TurnScheduler provider task completion failed"
                );
            }
        }
    }
    Vec::new()
}

fn mark_provider_turn_failed(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    summary: impl Into<String>,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_failed(id, history_commit_len, summary) {
            Ok(()) => {
                if record_provider_worker_failed(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = id.get(),
                    error = ?error,
                    "TurnScheduler provider task failure failed"
                );
            }
        }
    }
    Vec::new()
}

fn mark_provider_turn_cancelled(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_cancelled(id, summary) {
            Ok(()) => {
                if record_provider_worker_cancelled(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = id.get(),
                    error = ?error,
                    "TurnScheduler provider task cancellation failed"
                );
            }
        }
    }
    Vec::new()
}

fn record_provider_worker_completed(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_completed(id) {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider worker completion failed"
        );
        return false;
    }
    true
}

fn record_provider_worker_failed(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_failed(id) {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider worker failure failed"
        );
        return false;
    }
    true
}

fn record_provider_worker_cancelled(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_cancelled(id) {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "ProviderTurnWorkerRegistry provider worker cancellation failed"
        );
        return false;
    }
    true
}

fn record_provider_history_commit_outcome(
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    let Some(task) = scheduler.task(id) else {
        tracing::warn!(
            task_id = id.get(),
            "HistoryCommitCoordinator missing scheduler task for provider outcome"
        );
        return Vec::new();
    };
    let Some(outcome) = crate::chat::history_commit::HistoryCommitOutcome::from_terminal_task(task) else {
        tracing::warn!(
            task_id = id.get(),
            state = ?task.state,
            "HistoryCommitCoordinator ignored non-terminal provider outcome"
        );
        return Vec::new();
    };
    if let Err(error) = coordinator.record_outcome(outcome) {
        tracing::warn!(
            task_id = id.get(),
            sequence = task.sequence,
            error = ?error,
            "HistoryCommitCoordinator provider outcome record failed"
        );
        return Vec::new();
    }
    let mut decisions = Vec::new();
    for decision in coordinator.drain_ready() {
        if let Err(error) = workers.apply_commit_decision(&decision) {
            tracing::warn!(
                task_id = id.get(),
                decision = ?decision,
                error = ?error,
                "ProviderTurnWorkerRegistry provider worker commit decision failed"
            );
        }
        tracing::trace!(
            task_id = id.get(),
            decision = ?decision,
            pending_tasks = coordinator.pending_tasks(),
            pending_outcomes = coordinator.pending_outcomes(),
            "HistoryCommitCoordinator provider outcome became ready"
        );
        decisions.push(decision);
    }
    decisions
}
