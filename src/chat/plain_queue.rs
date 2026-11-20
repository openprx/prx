//! Plain REPL (`--plain`) input queue: a line typed while a turn runs waits in
//! the chat backlog (the [`crate::chat::turn_scheduler::TurnScheduler`]
//! order) and starts when the running turn ends. Only lines typed at the
//! local terminal are acknowledged, bounded and dropped here; scheduled and
//! daemon-assigned input keeps its own rules.

use super::input_queue::{QueuedInputMessage, is_local_operator_input};
use super::session_glue::is_chat_quit_command;
use crate::chat::turn_scheduler::TurnScheduler;
use std::collections::VecDeque;

/// Most typed lines that may wait for the running turn.
const PLAIN_INPUT_QUEUE_LIMIT: usize = 8;
const QUEUE_FULL_NOTICE: &str = "Input queue is full; wait for the current turn.";

/// Ids of the backlog entries from `start` on: the lines one input batch added.
pub(super) fn lines_queued_since(backlog: &VecDeque<QueuedInputMessage>, start: usize) -> Vec<String> {
    backlog.iter().skip(start).map(|queued| queued.msg.id.clone()).collect()
}

/// Applies the plain queue rules to the lines `added` that still wait in the
/// backlog: a typed `/exit` or `/quit` drops every other typed line; any other
/// typed line is acknowledged with its position, or refused past the limit.
pub(super) fn settle_queued_lines(
    backlog: &mut VecDeque<QueuedInputMessage>,
    scheduler: &mut TurnScheduler,
    added: &[String],
) {
    let is_added_typed_line =
        |queued: &QueuedInputMessage| is_local_operator_input(&queued.msg) && added.contains(&queued.msg.id);
    if let Some(quit_id) = backlog
        .iter()
        .find(|queued| is_added_typed_line(queued) && is_chat_quit_command(queued.msg.content.trim()))
        .map(|queued| queued.msg.id.clone())
    {
        drop_typed_lines(backlog, scheduler, |queued| queued.msg.id != quit_id);
        return;
    }
    let mut index = 0;
    while let Some(queued) = backlog.get(index) {
        if !is_added_typed_line(queued) {
            index += 1;
            continue;
        }
        if index >= PLAIN_INPUT_QUEUE_LIMIT {
            if let Some(refused) = backlog.remove(index) {
                cancel_queued_task(scheduler, &refused);
            }
            eprintln!("{QUEUE_FULL_NOTICE}");
            continue;
        }
        index += 1;
        eprintln!("Queued (position {index}).");
    }
}

fn drop_typed_lines(
    backlog: &mut VecDeque<QueuedInputMessage>,
    scheduler: &mut TurnScheduler,
    should_drop: impl Fn(&QueuedInputMessage) -> bool,
) {
    let mut kept = VecDeque::with_capacity(backlog.len());
    for queued in backlog.drain(..) {
        if is_local_operator_input(&queued.msg) && should_drop(&queued) {
            cancel_queued_task(scheduler, &queued);
        } else {
            kept.push_back(queued);
        }
    }
    *backlog = kept;
}

/// The scheduler task of a line that leaves the backlog unrun is cancelled
/// before start, so queue status no longer counts it.
fn cancel_queued_task(scheduler: &mut TurnScheduler, queued: &QueuedInputMessage) {
    if let Some(id) = queued.turn_task_id
        && let Err(error) = scheduler.request_cancel(id)
    {
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            error = %crate::agent::diagnostics::error_class(&error),
            "TurnScheduler plain queue drop mirror failed"
        );
    }
}
