//! The tool-call protocol repair: the one pass that leaves a provider-bound
//! message list in a shape every provider accepts, whatever reduction step ran
//! before it.
//!
//! The invariants of a provider-bound list:
//!
//! 1. The first non-system message is a user message.
//! 2. An assistant message that carries tool calls is followed at once by the
//!    tool results for exactly those call ids, all of them, before any other
//!    role.
//! 3. No tool result stands without the assistant message that called it.
//! 4. An assistant message that carries tool calls follows a user message or
//!    tool results. Strict providers (the Gemini family, reached natively or
//!    through a gateway) answer a function call that follows anything else
//!    with "function call turn comes immediately after a user turn or after a
//!    function response turn".
//! 5. The live user message of the turn is never removed.
//!
//! Every reduction step removes from the front of the view, one message at a
//! time, and stops as soon as the request fits. Where it stops is arbitrary:
//! it can take a user message and leave the assistant tool-call message that
//! answered it as the first message, or cut a round from its results. The step
//! sites cut at round boundaries where they can ([`drop_leading_round_at`]);
//! this pass repairs what is left. It removes tool results without a call,
//! rounds whose results are missing, rounds that do not follow a user message
//! or tool results, and opening assistant text, but never a user message. The
//! summary or handoff a compaction wrote is assistant text that opens the view
//! by design and is kept.
//!
//! The scan is linear, allocates nothing when the list is valid, and a second
//! run over its own output changes nothing.

use super::handoff_note::is_context_handoff_message;
use crate::agent::turn_history::TurnHistory;
use crate::providers::ChatMessage;
use serde::Deserialize;
use std::ops::Range;

/// What a repair removed. All zero when the list was valid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Repair {
    /// Tool results without a call: standing alone, or answering a call the
    /// round does not hold.
    pub(crate) orphan_results: usize,
    /// Tool-call messages removed together with their results, because a
    /// result was missing or the round did not follow a user message or tool
    /// results.
    pub(crate) broken_rounds: usize,
    /// Assistant text that opened the view and was not a compaction note.
    pub(crate) leading_messages: usize,
    /// Messages removed in all.
    pub(crate) removed_messages: usize,
}

impl Repair {
    /// Whether the list needed no repair.
    pub(crate) const fn is_empty(&self) -> bool {
        self.removed_messages == 0
    }
}

#[derive(Deserialize)]
struct EnvelopeCalls {
    tool_calls: Option<Vec<CallId>>,
}

#[derive(Deserialize)]
struct CallId {
    id: Option<String>,
}

#[derive(Deserialize)]
struct ResultId {
    tool_call_id: Option<String>,
}

/// The call ids of an assistant message that carries native tool calls (an id
/// is `None` when the call has none), or `None` when it carries no call.
pub(crate) fn tool_call_ids(message: &ChatMessage) -> Option<Vec<Option<String>>> {
    if message.role != "assistant" || !message.content.starts_with('{') || !message.content.contains("\"tool_calls\"") {
        return None;
    }
    let calls = serde_json::from_str::<EnvelopeCalls>(&message.content).ok()?.tool_calls?;
    if calls.is_empty() {
        return None;
    }
    Some(calls.into_iter().map(|call| call.id).collect())
}

/// Whether `message` is an assistant message that carries native tool calls.
pub(crate) fn is_tool_call_envelope(message: &ChatMessage) -> bool {
    tool_call_ids(message).is_some()
}

fn result_call_id(message: &ChatMessage) -> Option<String> {
    if !message.content.starts_with('{') {
        return None;
    }
    serde_json::from_str::<ResultId>(&message.content).ok()?.tool_call_id
}

/// The first position at or after `from` that is not a tool result.
fn results_end(messages: &[ChatMessage], from: usize) -> usize {
    let mut end = from;
    while messages.get(end).is_some_and(|message| message.role == "tool") {
        end = end.saturating_add(1);
    }
    end
}

/// Remove the tool-call message at `from` together with the tool results that
/// follow it, as one unit, for as long as the message at `from` is one. A trim
/// that took the user message in front of a round calls this so the round does
/// not become the first message of the view without its question.
pub(crate) fn drop_leading_round_at(history: &mut Vec<ChatMessage>, from: usize) {
    while history.get(from).is_some_and(is_tool_call_envelope) {
        let end = results_end(history, from.saturating_add(1));
        history.drain(from..end);
    }
}

/// Assistant text a compaction wrote to stand at the head of the view: the
/// handoff note, the summary, the compaction note and the memory flush note.
fn is_compaction_note(message: &ChatMessage) -> bool {
    message.role == "assistant"
        && (is_context_handoff_message(message)
            || message.content.starts_with("[Compaction summary]")
            || message.content.starts_with("[Context compacted at")
            || message.content.starts_with("[Memory flush at"))
}

/// What the scan saw before the message it is looking at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Previous {
    /// Nothing but system messages.
    Start,
    User,
    ToolResult,
    Assistant,
    Other,
}

/// Whether a round's results answer exactly its calls. A call or a result
/// without an id cannot be matched: such a round is judged on having results
/// at all.
fn round_matches(messages: &[ChatMessage], results: &Range<usize>, calls: &[Option<String>]) -> RoundMatch {
    if results.is_empty() {
        return RoundMatch::Missing;
    }
    let mut result_ids: Vec<Option<String>> = Vec::with_capacity(results.len());
    for message in messages.get(results.clone()).unwrap_or_default() {
        result_ids.push(result_call_id(message));
    }
    if calls.iter().any(Option::is_none) || result_ids.iter().any(Option::is_none) {
        return RoundMatch::Unverifiable;
    }
    if calls.iter().all(|call| result_ids.contains(call)) {
        RoundMatch::Complete(result_ids)
    } else {
        RoundMatch::Missing
    }
}

enum RoundMatch {
    /// A result is missing.
    Missing,
    /// Ids are absent on a side; the round is left as it is.
    Unverifiable,
    /// Every call is answered; the ids of the results, in order.
    Complete(Vec<Option<String>>),
}

/// Scan `messages` and return the ranges to remove and what they are.
fn plan(messages: &[ChatMessage]) -> (Vec<Range<usize>>, Repair) {
    let last_user = messages.iter().rposition(|message| message.role == "user");
    let mut remove: Vec<Range<usize>> = Vec::new();
    let mut repair = Repair::default();
    let mut previous = Previous::Start;
    let mut index = 0usize;
    while let Some(message) = messages.get(index) {
        let next = index.saturating_add(1);
        match message.role.as_str() {
            "system" => index = next,
            "user" => {
                previous = Previous::User;
                index = next;
            }
            "tool" => {
                remove.push(index..next);
                repair.orphan_results = repair.orphan_results.saturating_add(1);
                index = next;
            }
            "assistant" => {
                let Some(calls) = tool_call_ids(message) else {
                    // Assistant text that opens the view and is not a
                    // compaction note: a user message follows, so the view
                    // can open on it.
                    if previous == Previous::Start
                        && last_user.is_some_and(|user| user > index)
                        && !is_compaction_note(message)
                    {
                        remove.push(index..next);
                        repair.leading_messages = repair.leading_messages.saturating_add(1);
                    } else {
                        previous = Previous::Assistant;
                    }
                    index = next;
                    continue;
                };
                let end = results_end(messages, next);
                let results = next..end;
                let answered = round_matches(messages, &results, &calls);
                let misplaced = matches!(previous, Previous::Start | Previous::Assistant)
                    && last_user.is_some_and(|user| user > index);
                if matches!(answered, RoundMatch::Missing) || misplaced {
                    remove.push(index..end);
                    repair.broken_rounds = repair.broken_rounds.saturating_add(1);
                } else {
                    if let RoundMatch::Complete(result_ids) = answered {
                        for (offset, id) in result_ids.iter().enumerate() {
                            if id.as_ref().is_some_and(|id| calls.iter().all(|call| call.as_ref() != Some(id))) {
                                let at = next.saturating_add(offset);
                                remove.push(at..at.saturating_add(1));
                                repair.orphan_results = repair.orphan_results.saturating_add(1);
                            }
                        }
                    }
                    previous = Previous::ToolResult;
                }
                index = end;
            }
            _ => {
                previous = Previous::Other;
                index = next;
            }
        }
    }
    repair.removed_messages = remove.iter().map(Range::len).sum();
    (remove, repair)
}

fn apply(messages: &mut Vec<ChatMessage>, remove: Vec<Range<usize>>) {
    for range in remove.into_iter().rev() {
        messages.drain(range);
    }
}

fn log(repair: &Repair, trigger: &'static str) {
    tracing::info!(
        trigger,
        orphan_results = repair.orphan_results,
        broken_rounds = repair.broken_rounds,
        leading_messages = repair.leading_messages,
        removed_messages = repair.removed_messages,
        "request repaired for the tool-call protocol"
    );
}

/// Repair `messages` for the tool-call protocol. For a list the caller owns
/// outright; a provider view that a [`TurnHistory`] tracks goes through
/// [`repair_view`].
pub(crate) fn repair_messages(messages: &mut Vec<ChatMessage>, trigger: &'static str) -> Repair {
    let (remove, repair) = plan(messages);
    if repair.is_empty() {
        return repair;
    }
    apply(messages, remove);
    log(&repair, trigger);
    repair
}

/// Repair the provider view `view` of a turn. The view is only read when it
/// is valid; a repair is a projection of this request and the canonical
/// history keeps every message.
pub(crate) fn repair_view(turn_history: &mut TurnHistory, view: &mut Vec<ChatMessage>, trigger: &'static str) -> Repair {
    let (remove, repair) = plan(view);
    if repair.is_empty() {
        return repair;
    }
    turn_history.reshape(view, |view| apply(view, remove));
    log(&repair, trigger);
    repair
}
