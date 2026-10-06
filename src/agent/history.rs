use crate::agent::tool_call_parser::unparseable_native_arguments;
use crate::agent::turn_contracts::is_loop_written_user_note;
use crate::providers::{ChatMessage, ToolCall};

/// Canonicalize request-local system instructions for providers whose chat
/// templates require one system message at the beginning of the transcript.
///
/// History remains untouched: callers run this on the prepared request copy.
/// That matters for corrective retries and OS-paging recall, both of which may
/// add request-local system context after ordinary conversation turns.
pub(super) fn coalesce_system_messages_at_front(messages: &mut Vec<ChatMessage>) {
    let system_count = messages.iter().filter(|message| message.role == "system").count();
    if system_count == 0 || system_count == 1 && messages.first().is_some_and(|message| message.role == "system") {
        return;
    }

    let mut system_parts = Vec::with_capacity(system_count);
    let mut conversation = Vec::with_capacity(messages.len().saturating_sub(system_count));
    for message in std::mem::take(messages) {
        if message.role == "system" {
            if !message.content.trim().is_empty() {
                system_parts.push(message.content);
            }
        } else {
            conversation.push(message);
        }
    }
    if !system_parts.is_empty() {
        messages.push(ChatMessage::system(system_parts.join("\n\n")));
    }
    messages.extend(conversation);
}

/// Legacy count-based trimming helpers retained only for their focused tests.
#[cfg(test)]
pub(super) const DEFAULT_MAX_HISTORY_MESSAGES: usize = 300;

/// Trim conversation history to prevent unbounded growth.
/// Preserves the system prompt (first message if role=system) and the most recent messages.
/// Drop `role="tool"` messages stranded at `from` after a trim removed the
/// assistant message that requested them.
///
/// Every lossy trim removes from a single position, so only that position can
/// strand a tool result; native providers reject a tool message that has no
/// matching `tool_calls` ahead of it, which would turn a survivable trim into
/// a failed turn.
pub(super) fn drop_orphan_tool_results_at(history: &mut Vec<ChatMessage>, from: usize) {
    while history.len() > from && history.get(from).is_some_and(|message| message.role == "tool") {
        history.remove(from);
    }
}

pub(super) fn history_non_system_start(history: &[ChatMessage]) -> usize {
    usize::from(history.first().is_some_and(|message| message.role == "system"))
}

fn trim_history(history: &mut Vec<ChatMessage>, max_history: usize) {
    // Nothing to trim if within limit
    let has_system = history.first().map_or(false, |m| m.role == "system");
    let non_system_count = if has_system { history.len() - 1 } else { history.len() };

    if non_system_count <= max_history {
        return;
    }

    let start: usize = if has_system { 1 } else { 0 };
    let to_remove = non_system_count - max_history;
    history.drain(start..start + to_remove);
    drop_orphan_tool_results_at(history, start);
}

/// Aggressive trim fallback: keep only the most recent non-system messages.
pub(super) fn apply_aggressive_trim(history: &mut Vec<ChatMessage>, keep_recent: usize) {
    trim_history(history, keep_recent.max(1));
}

/// Position of the latest real user turn, skipping internal bookkeeping
/// messages: the `[Tool results]` block a prompt-guided tool round feeds back,
/// the post-compaction refresh aside, and a recalled OS-paging page — matched
/// exactly ([`is_loop_written_user_note`]); a user turn cannot open with one,
/// because inbound text is neutralised on entry.
///
/// One predicate for "the user actually asked this", used both to build a
/// recall query and to identify the request the context budget must not delete.
pub(super) fn latest_user_query_index(history: &[ChatMessage]) -> Option<usize> {
    history
        .iter()
        .rposition(|m| m.role == "user" && !m.content.is_empty() && !is_loop_written_user_note(&m.content))
}

/// Extract the latest real user turn to use as the recall query, skipping
/// internal bookkeeping messages (tool results, compaction/paging breadcrumbs).
pub(super) fn latest_user_query(history: &[ChatMessage]) -> Option<&str> {
    latest_user_query_index(history)
        .and_then(|index| history.get(index))
        .map(|m| m.content.as_str())
}

/// Build assistant history entry in JSON format for native tool-call APIs.
/// `convert_messages` in the OpenRouter provider parses this JSON to reconstruct
/// the proper `NativeMessage` with structured `tool_calls`.
pub(super) fn build_native_assistant_history(
    text: &str,
    tool_calls: &[ToolCall],
    reasoning_content: Option<&str>,
) -> String {
    let calls_json: Vec<serde_json::Value> = tool_calls
        .iter()
        .map(|tc| {
            // A call refused for arguments that are not a complete JSON
            // object is replayed as `{}`: its tool result explains the
            // refusal, and a strict server may reject the raw arguments in
            // the follow-up request.
            let arguments = if unparseable_native_arguments(tc).is_some() {
                "{}"
            } else {
                tc.arguments.as_str()
            };
            serde_json::json!({
                "id": tc.id,
                "name": tc.name,
                "arguments": arguments,
            })
        })
        .collect();

    let content = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(text.trim().to_string())
    };

    let mut obj = serde_json::json!({
        "content": content,
        "tool_calls": calls_json,
    });
    if let Some(rc) = reasoning_content {
        if let Some(map) = obj.as_object_mut() {
            map.insert(
                "reasoning_content".to_string(),
                serde_json::Value::String(rc.to_string()),
            );
        }
    }
    obj.to_string()
}

#[path = "turn_tests/history.rs"]
mod tests;
