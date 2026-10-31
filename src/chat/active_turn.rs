//! Operator commands handled while a provider turn is active (`/queue`, `/workers`, `/cost`) and worker reports.

use super::input_queue::is_local_operator_input;
#[cfg(feature = "terminal-tui")]
use super::input_queue::{
    QueuedInputMessage, classify_input_priority, enqueue_input_message_and_return_priority_with_scheduler,
    format_input_backlog_report, input_priority_from_text, truncate_chars_for_queued_notice,
};
use super::provider_turns::request_provider_turn_cancel;
#[cfg(feature = "terminal-tui")]
use super::provider_turns::{provider_worker_status, publish_provider_worker_status};
#[cfg(feature = "terminal-tui")]
use super::session_glue::{
    DaemonControl, ReapedSessionLogArchive, handle_local_session_command, start_daemon_session_command,
};
#[cfg(feature = "terminal-tui")]
use super::session_glue::{is_daemon_session_command, local_session_command};
#[cfg(feature = "terminal-tui")]
use super::{commands, session};
use crate::providers::ChatMessage;
#[cfg(feature = "terminal-tui")]
use crate::tools::Tool;
use anyhow::Result;
#[cfg(feature = "terminal-tui")]
use tokio::sync::mpsc;

pub(super) fn is_queue_command(input: &str) -> bool {
    matches!(input.trim(), "/queue" | "/queue status")
}

#[cfg(feature = "terminal-tui")]
fn is_cost_command(input: &str) -> bool {
    input.trim() == "/cost"
}

pub(super) fn is_workers_command(input: &str) -> bool {
    matches!(input.trim(), "/workers" | "/workers status")
        || !matches!(
            parse_workers_cancel_command(input),
            ProviderWorkerCancelCommand::NotCancel
        )
}

#[cfg(feature = "terminal-tui")]
pub(super) fn is_active_turn_local_command(input: &str) -> bool {
    let (_priority, content) = input_priority_from_text(input);
    is_queue_command(&content)
        || is_cost_command(&content)
        || is_workers_command(&content)
        || local_session_command(&content).is_some()
}

#[cfg(feature = "terminal-tui")]
pub(super) fn format_active_turn_local_notice(input: &str) -> String {
    let (_priority, content) = input_priority_from_text(input);
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = truncate_chars_for_queued_notice(&collapsed, 160);
    format!("local > {preview}")
}

#[cfg(feature = "terminal-tui")]
pub(super) async fn active_turn_local_command_output(
    msg: &crate::channels::traits::ChannelMessage,
    backlog: &std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    chat_session: &session::ChatSession,
    provider_turn_workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
    tools_registry: &[Box<dyn Tool>],
    daemon: DaemonControl<'_>,
) -> Option<String> {
    let (_priority, content) = classify_input_priority(msg);
    if is_queue_command(&content) {
        return Some(format_input_backlog_report(backlog, scheduler, 8));
    }
    if is_cost_command(&content) {
        return Some(commands::format_cost_feedback(chat_session));
    }
    if is_workers_command(&content) {
        return Some(format_provider_worker_report(&provider_worker_status(
            provider_turn_workers,
        )));
    }
    if let Some(action) = local_session_command(&content) {
        if action.requires_local_operator() && !is_local_operator_input(msg) {
            return Some("This session command requires local terminal operator input.".to_owned());
        }
        if is_daemon_session_command(&action) {
            let local_fallback = if matches!(action, crate::chat::sessions::SessionCommand::DaemonSessions) {
                handle_local_session_command(
                    &crate::chat::sessions::SessionCommand::Sessions,
                    chat_sessions,
                    session_rings,
                    reaped_log_archive,
                    reap_policy,
                    tools_registry,
                )
                .await
            } else {
                None
            };
            return start_daemon_session_command(&action, local_fallback, &chat_sessions.daemon_requests(), daemon);
        }
        return handle_local_session_command(
            &action,
            chat_sessions,
            session_rings,
            reaped_log_archive,
            reap_policy,
            tools_registry,
        )
        .await;
    }
    None
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "terminal-tui")]
pub(super) async fn process_active_turn_input_message(
    msg: impl Into<crate::runtime::h2_entry::ChatInput>,
    emit_chat_output: &mut impl FnMut(&str),
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    chat_session: &session::ChatSession,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
    tools_registry: &[Box<dyn Tool>],
    daemon: DaemonControl<'_>,
) {
    let input = msg.into();
    let msg = &input.message;
    if let Some((output, signal_cancel)) =
        active_turn_workers_cancel_output(msg, scheduler, provider_turn_workers, provider_turn_task_id)
    {
        emit_chat_output(&output);
        publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
        dispatch_provider_worker_cancel_signal(chat_dispatcher, signal_cancel);
        return;
    }
    if let Some(output) = active_turn_local_command_output(
        &msg,
        backlog,
        scheduler,
        chat_session,
        provider_turn_workers,
        chat_sessions,
        session_rings,
        reaped_log_archive,
        reap_policy,
        tools_registry,
        daemon,
    )
    .await
    {
        emit_chat_output(&output);
        return;
    }
    // An assignment dispatched as `interrupt` ends the turn in flight first,
    // through the very same local path Ctrl+C uses. Two things this is not:
    // it is not a timeout — nothing measured a clock, somebody asked for it —
    // and it is not the daemon acting, which only recorded the disposition and
    // put the item at the head of its queue. `steer` deliberately does *not*
    // land here: it jumps the queue and leaves the running turn alone.
    if let Some(delivery) = crate::chat::sessions::assignment::delivered(&msg)
        && delivery.disposition.ends_the_turn_in_flight()
    {
        request_provider_turn_cancel(
            scheduler,
            provider_turn_workers,
            provider_turn_task_id,
            "daemon assignment interrupt",
        );
        dispatch_provider_worker_cancel_signal(chat_dispatcher, Some(ProviderWorkerCancelSignal::CancelRequested));
        publish_provider_worker_status(chat_dispatcher, provider_turn_workers);
    }
    // Everything else about an assignment is ordinary input: `queue` lands at
    // the back of the backlog, `steer` and `interrupt` at the front, because
    // their message carries the same `/now ` prefix a person types to do that.
    let _priority =
        enqueue_input_message_and_return_priority_with_scheduler(backlog, scheduler, input, chat_session.turns.len());
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "terminal-tui")]
pub(super) async fn process_active_turn_input_batch<M: Into<crate::runtime::h2_entry::ChatInput>>(
    msg: impl Into<crate::runtime::h2_entry::ChatInput>,
    emit_chat_output: &mut impl FnMut(&str),
    input_rx: &mut mpsc::Receiver<M>,
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    chat_session: &session::ChatSession,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    session_rings: &std::collections::HashMap<crate::chat::sessions::id::SessionId, crate::chat::sessions::SessionRing>,
    reaped_log_archive: &mut ReapedSessionLogArchive,
    reap_policy: &crate::chat::sessions::runtime::ReapPolicy,
    tools_registry: &[Box<dyn Tool>],
    daemon: DaemonControl<'_>,
) {
    process_active_turn_input_message(
        msg,
        emit_chat_output,
        backlog,
        scheduler,
        provider_turn_workers,
        provider_turn_task_id,
        chat_dispatcher,
        chat_session,
        chat_sessions,
        session_rings,
        reaped_log_archive,
        reap_policy,
        tools_registry,
        daemon,
    )
    .await;
    while let Ok(msg) = input_rx.try_recv() {
        process_active_turn_input_message(
            msg,
            emit_chat_output,
            backlog,
            scheduler,
            provider_turn_workers,
            provider_turn_task_id,
            chat_dispatcher,
            chat_session,
            chat_sessions,
            session_rings,
            reaped_log_archive,
            reap_policy,
            tools_registry,
            daemon,
        )
        .await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProviderWorkerCancelCommand {
    NotCancel,
    Invalid(String),
    Cancel { sequence: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderWorkerCancelSignal {
    CancelRequested,
    CancelProviderTurn {
        task_id: crate::chat::turn_scheduler::TurnTaskId,
    },
}

pub(super) fn parse_workers_cancel_command(input: &str) -> ProviderWorkerCancelCommand {
    let mut parts = input.split_whitespace();
    if parts.next() != Some("/workers") {
        return ProviderWorkerCancelCommand::NotCancel;
    }
    let Some(verb) = parts.next() else {
        return ProviderWorkerCancelCommand::NotCancel;
    };
    if !matches!(verb, "cancel" | "stop") {
        return ProviderWorkerCancelCommand::NotCancel;
    }
    let Some(target) = parts.next() else {
        return ProviderWorkerCancelCommand::Invalid("missing worker id".to_string());
    };
    if parts.next().is_some() {
        return ProviderWorkerCancelCommand::Invalid("expected exactly one worker id".to_string());
    }
    match parse_provider_worker_sequence(target) {
        Ok(sequence) => ProviderWorkerCancelCommand::Cancel { sequence },
        Err(error) => ProviderWorkerCancelCommand::Invalid(error),
    }
}

fn parse_provider_worker_sequence(target: &str) -> Result<u64, String> {
    let raw = target
        .strip_prefix("w#")
        .or_else(|| target.strip_prefix("W#"))
        .or_else(|| target.strip_prefix('#'))
        .unwrap_or(target);
    if raw.is_empty() {
        return Err("empty worker id".to_string());
    }
    let sequence = raw
        .parse::<u64>()
        .map_err(|_| format!("invalid worker id {target:?}"))?;
    if sequence == 0 {
        return Err("worker id must be greater than zero".to_string());
    }
    Ok(sequence)
}

#[cfg(feature = "terminal-tui")]
pub(super) fn active_turn_workers_cancel_output(
    msg: &crate::channels::traits::ChannelMessage,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) -> Option<(String, Option<ProviderWorkerCancelSignal>)> {
    let (_priority, content) = classify_input_priority(msg);
    provider_workers_cancel_output_for_input(msg, &content, scheduler, workers, provider_turn_task_id)
}

pub(super) fn provider_workers_cancel_output_for_input(
    source: &crate::channels::traits::ChannelMessage,
    input: &str,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) -> Option<(String, Option<ProviderWorkerCancelSignal>)> {
    match parse_workers_cancel_command(input) {
        ProviderWorkerCancelCommand::NotCancel => None,
        ProviderWorkerCancelCommand::Invalid(error) => Some((
            format!("Workers cancel failed: {error}.\nUsage: /workers cancel w#N"),
            None,
        )),
        ProviderWorkerCancelCommand::Cancel { sequence } => {
            if !is_local_operator_input(source) {
                return Some((
                    "Cancelling a provider worker requires local terminal operator input.".to_owned(),
                    None,
                ));
            }
            let Some(worker) = workers
                .snapshot()
                .into_iter()
                .find(|worker| worker.sequence == sequence)
            else {
                return Some((
                    format!("Workers cancel failed: provider worker w#{sequence} is not retained."),
                    None,
                ));
            };
            if !matches!(
                worker.state,
                crate::chat::turn_worker::ProviderTurnWorkerState::Running
                    | crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling
            ) {
                return Some((
                    format!(
                        "Workers cancel ignored: provider worker w#{sequence} is already {}.",
                        provider_turn_worker_state_label(worker.state)
                    ),
                    None,
                ));
            }
            request_provider_turn_cancel(
                scheduler,
                workers,
                Some(worker.task_id),
                "workers cancel command during active turn",
            );
            let state = workers
                .worker(worker.task_id)
                .map(|worker| provider_turn_worker_state_label(worker.state))
                .unwrap_or("unknown");
            Some((
                format!(
                    "Requested cancellation for provider worker w#{sequence} task={} kind={} state={state}.",
                    worker.task_id.get(),
                    provider_turn_worker_kind_label(worker.kind),
                ),
                Some(
                    if provider_turn_task_id == Some(worker.task_id)
                        && worker.kind == crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited
                    {
                        ProviderWorkerCancelSignal::CancelRequested
                    } else {
                        ProviderWorkerCancelSignal::CancelProviderTurn {
                            task_id: worker.task_id,
                        }
                    },
                ),
            ))
        }
    }
}

pub(super) fn dispatch_provider_worker_cancel_signal(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    signal_cancel: Option<ProviderWorkerCancelSignal>,
) {
    let Some(signal) = signal_cancel else {
        return;
    };
    let action = match signal {
        ProviderWorkerCancelSignal::CancelRequested => crate::chat::action::Action::CancelRequested,
        ProviderWorkerCancelSignal::CancelProviderTurn { task_id } => {
            crate::chat::action::Action::CancelProviderTurn { task_id }
        }
    };
    let _ = chat_dispatcher.dispatch_or_log(action, "chat.workers_cancel_provider_turn");
}

const fn provider_turn_worker_kind_label(kind: crate::chat::turn_worker::ProviderTurnWorkerKind) -> &'static str {
    match kind {
        crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited => "foreground_awaited",
        crate::chat::turn_worker::ProviderTurnWorkerKind::Detached => "detached",
    }
}

const fn provider_turn_worker_state_label(state: crate::chat::turn_worker::ProviderTurnWorkerState) -> &'static str {
    match state {
        crate::chat::turn_worker::ProviderTurnWorkerState::Running => "running",
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelling => "cancelling",
        crate::chat::turn_worker::ProviderTurnWorkerState::AwaitingCommit(_) => "awaiting_commit",
        crate::chat::turn_worker::ProviderTurnWorkerState::Committed => "committed",
        crate::chat::turn_worker::ProviderTurnWorkerState::Cancelled => "cancelled",
        crate::chat::turn_worker::ProviderTurnWorkerState::Failed => "failed",
    }
}

pub(super) fn format_provider_worker_report(status: &crate::chat::action::ProviderWorkerStatus) -> String {
    if status.running == 0
        && status.cancelling == 0
        && status.awaiting_commit == 0
        && status.finalized_payloads == 0
        && status.rows.is_empty()
    {
        return "No main provider workers are active.".to_string();
    }

    let mut lines = vec![format!(
        "Main provider workers: {} running, {} cancelling, {} awaiting commit, {} finalized payloads, {} finalized tokens.",
        status.running,
        status.cancelling,
        status.awaiting_commit,
        status.finalized_payloads,
        format_worker_tokens_compact(status.finalized_total_tokens),
    )];
    let active_rows: Vec<_> = status.rows.iter().filter(|row| row.is_active()).collect();
    if active_rows.is_empty() {
        lines.push("No active worker rows are currently retained.".to_string());
        return lines.join("\n");
    }
    for row in active_rows {
        lines.push(format_provider_worker_report_row(row));
    }
    lines.join("\n")
}

fn format_provider_worker_report_row(row: &crate::chat::action::ProviderWorkerStatusRow) -> String {
    use crate::chat::action::ProviderWorkerRowState;

    let state = match row.state {
        ProviderWorkerRowState::Running => "running",
        ProviderWorkerRowState::Cancelling => "cancelling",
        ProviderWorkerRowState::AwaitingCommit => "awaiting_commit",
        ProviderWorkerRowState::Committed => "committed",
        ProviderWorkerRowState::Cancelled => "cancelled",
        ProviderWorkerRowState::Failed => "failed",
    };
    let elapsed = format_provider_worker_elapsed(row.started_at_ms);
    let mut line = format!(
        "- w#{} task={} kind={} state={} completion={} elapsed={elapsed}",
        row.sequence,
        row.task_id,
        crate::chat::action::provider_worker_row_kind_label(row.kind),
        state,
        if row.completion_ready { "ready" } else { "pending" },
    );
    if let Some(tokens) = row.finalized_total_tokens.filter(|tokens| *tokens > 0) {
        line.push_str(" tokens=");
        line.push_str(&format_worker_tokens_compact(tokens));
    }
    line
}

fn format_provider_worker_elapsed(started_at_ms: i64) -> String {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let elapsed_ms = now_ms.saturating_sub(started_at_ms).max(0);
    let elapsed_secs = u64::try_from(elapsed_ms / 1000).unwrap_or_default();
    crate::chat::sessions::model::format_elapsed_compact(elapsed_secs)
}

pub(super) fn format_worker_tokens_compact(tokens: u64) -> String {
    if tokens >= 1_000 {
        let whole = tokens / 1_000;
        let decimal = (tokens % 1_000) / 100;
        if decimal == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{decimal}k")
        }
    } else {
        tokens.to_string()
    }
}

pub(super) fn rollback_cancelled_turn_history(history: &mut Vec<ChatMessage>, len_before_user_turn: usize) {
    history.truncate(len_before_user_turn);
}
