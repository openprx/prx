//! Chat input queue: priority classification, queueing and scheduler-ordered popping of operator input.

use super::SYNTHETIC_UI_COMMAND_SENDER;
use super::provider_turns::provider_turn_visible_admission;
use super::session_glue::is_chat_quit_command;
use crate::runtime::h2_entry::{ChatInput, TerminalInputProof};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum InputQueuePriority {
    Normal,
    Priority,
    Control,
}

pub(super) struct QueuedInputMessage {
    pub(super) priority: InputQueuePriority,
    pub(super) turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    pub(super) msg: crate::channels::traits::ChannelMessage,
    pub(super) terminal_origin: Option<TerminalInputProof>,
}

pub(super) struct DequeuedInputMessage {
    pub(super) priority: InputQueuePriority,
    pub(super) turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    pub(super) msg: crate::channels::traits::ChannelMessage,
    pub(super) terminal_origin: Option<TerminalInputProof>,
}

pub(super) fn is_local_operator_input(msg: &crate::channels::traits::ChannelMessage) -> bool {
    msg.channel == "terminal" && msg.sender == "user"
}

pub(super) fn is_synthetic_ui_command(msg: &crate::channels::traits::ChannelMessage) -> bool {
    msg.sender == SYNTHETIC_UI_COMMAND_SENDER && msg.channel == "terminal"
}

pub(super) fn classify_input_priority(msg: &crate::channels::traits::ChannelMessage) -> (InputQueuePriority, String) {
    if is_synthetic_ui_command(msg) {
        return (InputQueuePriority::Control, msg.content.clone());
    }
    input_priority_from_text(&msg.content)
}

pub(super) fn input_priority_from_text(input: &str) -> (InputQueuePriority, String) {
    let trimmed = input.trim();
    for prefix in ["/now ", "/priority ", "!! "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return (InputQueuePriority::Priority, rest.to_string());
            }
        }
    }
    (InputQueuePriority::Normal, input.to_string())
}

pub(super) fn enqueue_input_message_and_return_priority(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    input: impl Into<ChatInput>,
) -> InputQueuePriority {
    let ChatInput {
        message: mut msg,
        terminal,
    } = input.into();
    let original = msg.content.clone();
    let (priority, content) = classify_input_priority(&msg);
    msg.content = content;
    // Keep a failed proof invalid so admission refuses it, rather than minting
    // authority from a normalization error or silently broadening the call.
    let terminal_origin = terminal.map(|proof| {
        proof.routed(&msg.id, &original, &msg.content).unwrap_or_else(|_| {
            proof.invalidate_source();
            proof
        })
    });
    backlog.push_back(QueuedInputMessage {
        priority,
        turn_task_id: None,
        msg,
        terminal_origin,
    });
    priority
}

const fn turn_priority_from_input(priority: InputQueuePriority) -> crate::chat::turn_scheduler::TurnPriority {
    match priority {
        InputQueuePriority::Normal => crate::chat::turn_scheduler::TurnPriority::Normal,
        InputQueuePriority::Priority => crate::chat::turn_scheduler::TurnPriority::Priority,
        InputQueuePriority::Control => crate::chat::turn_scheduler::TurnPriority::Control,
    }
}

pub(super) fn enqueue_input_message_and_return_priority_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    input: impl Into<ChatInput>,
    history_base_len: usize,
) -> InputQueuePriority {
    let ChatInput {
        message: mut msg,
        terminal,
    } = input.into();
    let original = msg.content.clone();
    let (priority, content) = classify_input_priority(&msg);
    msg.content = content;
    // Keep a failed proof invalid so admission refuses it, rather than minting
    // authority from a normalization error or silently broadening the call.
    let terminal_origin = terminal.map(|proof| {
        proof.routed(&msg.id, &original, &msg.content).unwrap_or_else(|_| {
            proof.invalidate_source();
            proof
        })
    });
    let turn_task_id = scheduler.enqueue(
        msg.content.clone(),
        turn_priority_from_input(priority),
        history_base_len,
    );
    backlog.push_back(QueuedInputMessage {
        priority,
        turn_task_id: Some(turn_task_id),
        msg,
        terminal_origin,
    });
    priority
}

pub(super) fn enqueue_input_message_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    msg: impl Into<ChatInput>,
    history_base_len: usize,
) {
    let _ = enqueue_input_message_and_return_priority_with_scheduler(backlog, scheduler, msg, history_base_len);
}

pub(super) fn enqueue_input_message(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    msg: impl Into<ChatInput>,
) {
    let _ = enqueue_input_message_and_return_priority(backlog, msg);
}

pub(super) fn pop_next_queued_input_message(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
) -> Option<QueuedInputMessage> {
    let best_idx = next_queued_input_index(backlog)?;
    backlog.remove(best_idx)
}

fn next_queued_input_index(backlog: &std::collections::VecDeque<QueuedInputMessage>) -> Option<usize> {
    let mut best_idx = None;
    let mut best_priority = InputQueuePriority::Normal;
    for (idx, queued) in backlog.iter().enumerate() {
        if best_idx.is_none() || queued.priority > best_priority {
            best_idx = Some(idx);
            best_priority = queued.priority;
        }
    }
    best_idx
}

pub(super) fn pop_next_input_task_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
) -> Option<DequeuedInputMessage> {
    let queued = pop_next_queued_input_message(backlog)?;
    if let Some(id) = queued.turn_task_id
        && let Err(error) = scheduler.mark_dispatched_to_chat_loop(id)
    {
        tracing::warn!(
            task_id = id.get(),
            error = ?error,
            "TurnScheduler chat-loop dequeue mirror failed"
        );
    }
    Some(DequeuedInputMessage {
        priority: queued.priority,
        turn_task_id: queued.turn_task_id,
        msg: queued.msg,
        terminal_origin: queued.terminal_origin,
    })
}

pub(super) fn pop_next_visible_input_task_with_scheduler(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    target_kind: crate::chat::turn_worker::ProviderTurnWorkerKind,
    max_concurrent_visible_turns: usize,
) -> Option<DequeuedInputMessage> {
    if !provider_turn_visible_admission(workers, target_kind, max_concurrent_visible_turns).can_start_visible {
        return None;
    }
    if workers.snapshot().iter().any(|worker| !worker.state.is_terminal())
        && next_queued_input_index(backlog)
            .and_then(|index| backlog.get(index))
            .is_some_and(|queued| is_persistence_dependent_command(&queued.msg.content))
    {
        // Provider concurrency may admit another ordinary user turn, but
        // commands that inspect, mutate, switch, export, or terminate the
        // foreground session must observe the preceding turn's durable state.
        // Leave the command at the queue head until the ordered
        // StreamCompleted -> SaveSession acknowledgement finalizes the worker.
        return None;
    }
    pop_next_input_task_with_scheduler(backlog, scheduler)
}

pub(super) fn is_persistence_dependent_command(input: &str) -> bool {
    let (_priority, content) = input_priority_from_text(input);
    let command = content.trim();
    is_chat_quit_command(command)
        || command == "/apply"
        || command == "/cost"
        || command == "/export"
        || command == "/new"
        || command == "/clear"
        || command == "/resume"
        || command.starts_with("/resume ")
        || command == "/branch"
        || command.starts_with("/branch ")
        || command == "/rewind"
        || command.starts_with("/rewind ")
}

pub(super) fn requeue_post_route_admission_rejected_input(
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    defer_visible_input_pop_once: &mut bool,
    input: DequeuedInputMessage,
) {
    backlog.push_front(QueuedInputMessage {
        priority: input.priority,
        turn_task_id: input.turn_task_id,
        msg: input.msg,
        terminal_origin: input.terminal_origin,
    });
    *defer_visible_input_pop_once = true;
}

pub(super) fn publish_main_queue_status(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
) {
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::MainQueueStatusUpdated {
            status: scheduler.status().main_queue_status(),
        },
        "chat.main_queue_status_updated",
    );
}

/// A refused input never reaches provider start, but must release its scheduler slot.
pub(super) fn cancel_dequeued_input_task(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> anyhow::Result<()> {
    scheduler
        .request_cancel(id)
        .map_err(|error| anyhow::anyhow!("Chat refused input could not release its queue task: {error:?}"))
}

#[cfg(test)]
mod refusal_tests {
    use super::*;
    use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskState};

    #[test]
    fn refused_input_releases_queued_and_dispatched_scheduler_tasks() {
        for dispatched in [false, true] {
            let mut scheduler = TurnScheduler::default();
            let id = scheduler.enqueue("/resume-decision prx_consumed", TurnPriority::Normal, 2);
            if dispatched {
                scheduler.mark_dispatched_to_chat_loop(id).unwrap();
            }
            cancel_dequeued_input_task(&mut scheduler, id).unwrap();
            let task = scheduler.task(id).unwrap();
            assert_eq!(task.state, TurnTaskState::Cancelled);
            assert_eq!(task.history_commit_len, Some(2));
            assert_eq!(scheduler.status().terminal, 1);
            assert_eq!(scheduler.status().queued, 0);
        }
    }
}

pub(super) fn format_input_backlog_report(
    backlog: &std::collections::VecDeque<QueuedInputMessage>,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    max_preview: usize,
) -> String {
    let status = scheduler.status();
    let mut lines = vec![format!(
        "Main queue: {} queued ({} priority), {} running.",
        status.queued, status.priority_queued, status.running
    )];
    if backlog.is_empty() {
        lines.push("Queue is empty.".to_string());
        return lines.join("\n");
    }
    for (idx, queued) in backlog.iter().take(max_preview).enumerate() {
        let label = match queued.priority {
            InputQueuePriority::Normal => "normal",
            InputQueuePriority::Priority => "priority",
            InputQueuePriority::Control => "control",
        };
        let preview = truncate_chars_for_queued_notice(
            &queued.msg.content.split_whitespace().collect::<Vec<_>>().join(" "),
            120,
        );
        lines.push(format!("{}. [{}] {}", idx + 1, label, preview));
    }
    let hidden = backlog.len().saturating_sub(max_preview);
    if hidden > 0 {
        lines.push(format!("... {hidden} more queued."));
    }
    lines.join("\n")
}

pub(super) fn drain_available_input_messages<M: Into<ChatInput>>(
    input_rx: &mut mpsc::Receiver<M>,
    backlog: &mut std::collections::VecDeque<QueuedInputMessage>,
    mut scheduler: Option<&mut crate::chat::turn_scheduler::TurnScheduler>,
    history_base_len: usize,
) {
    while let Ok(msg) = input_rx.try_recv() {
        if let Some(scheduler) = scheduler.as_deref_mut() {
            enqueue_input_message_with_scheduler(backlog, scheduler, msg, history_base_len);
        } else {
            enqueue_input_message(backlog, msg);
        }
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn format_queued_input_notice(input: &str, priority: InputQueuePriority) -> String {
    let (_, text) = input_priority_from_text(input);
    let label = match priority {
        InputQueuePriority::Normal => "queued",
        InputQueuePriority::Priority => "priority queued",
        InputQueuePriority::Control => "control queued",
    };
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = truncate_chars_for_queued_notice(&collapsed, 160);
    format!("{label} > {preview}")
}

pub(super) fn truncate_chars_for_queued_notice(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = input.chars().take(keep).collect();
    out.push('…');
    out
}
