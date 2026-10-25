use crate::agent::context_budget::{PRE_TURN_FLUSH_THRESHOLD, plan_context_budget};
use crate::agent::diagnostics::TOOL_PARSE_LOG_PREVIEW_CHARS;
use crate::agent::document_ingest::{TOOL_OUTPUT_DOCUMENT_CHUNK_TOKENS, tool_output_document_id};
use crate::agent::tool_call_parser::ParsedToolCall;
use crate::providers::{ChatMessage, ToolCall};
use crate::util::truncate_with_ellipsis;
use std::fmt::Write;

/// Facts returned by the execution service, kept separate from model text.
#[derive(Debug, Clone)]
pub(super) struct ObservedToolResult {
    pub content: String,
    pub status: crate::tools::ToolExecutionStatus,
    pub executed: bool,
    pub backend_name: Option<String>,
}

impl ObservedToolResult {
    pub(super) const fn not_executed(content: String, status: crate::tools::ToolExecutionStatus) -> Self {
        Self {
            content,
            status,
            executed: false,
            backend_name: None,
        }
    }

    pub(super) fn succeeded(&self) -> bool {
        self.status == crate::tools::ToolExecutionStatus::Succeeded
    }
}

/// Maximum inline characters allowed for a single tool result before it is
/// represented as a document-ingest reference in model history.
/// 🟡 Behavior-limits Phase 1: raised 8K -> 48K (6x) so medium tool outputs stay
/// directly visible to the agent instead of needing a second search.
pub(super) const MAX_TOOL_RESULT_INLINE_CHARS: usize = 48_000;

/// Browser and other MCP results commonly contain accessibility trees or
/// structured payloads much larger than the decision-relevant part. Keep those
/// calls responsive while retaining the full output in the document layer
/// referenced by the compacted history entry.
const MAX_MCP_TOOL_RESULT_INLINE_CHARS: usize = 16_000;

/// Smallest inline preview a tool result keeps when the window it has to fit
/// in is already spent. Sized off `TOOL_PARSE_LOG_PREVIEW_CHARS` so the two
/// "smallest useful look at a tool payload" figures stay one number.
const MIN_TOOL_RESULT_INLINE_CHARS: usize = TOOL_PARSE_LOG_PREVIEW_CHARS;

pub(crate) fn tool_result_inline_budget_for_history(
    tool_name: &str,
    history: &[ChatMessage],
    config: Option<&crate::config::AgentCompactionConfig>,
) -> usize {
    let absolute_cap = if tool_name == "mcp_call" || tool_name.starts_with("mcp__") {
        MAX_MCP_TOOL_RESULT_INLINE_CHARS
    } else {
        MAX_TOOL_RESULT_INLINE_CHARS
    };
    let Some(config) = config else {
        return absolute_cap;
    };
    let budget = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
    let remaining_tokens = budget.available_input_tokens.saturating_sub(budget.used_tokens);
    // Token-derived character guard. The final safety condition is enforced by
    // remeasuring history after insertion, so this conversion is deliberately
    // conservative and bounded by the absolute resident-memory guard.
    //
    // Floored, because a history that is already over its window leaves zero
    // remaining tokens: without the floor every tool result of that round
    // collapses into a bare reference with an empty preview, which is the one
    // state a model cannot act on. The floor is the same preview this file
    // already calls the smallest useful look at a tool payload.
    let token_derived_chars = remaining_tokens.saturating_mul(3).saturating_div(4);
    token_derived_chars.max(MIN_TOOL_RESULT_INLINE_CHARS).min(absolute_cap)
}

/// Compact a large tool result before inserting it into LLM history.
///
/// This is the Phase-0 document-ingest spike: the full document backend is not
/// required here, but model history receives a stable document/vector anchor
/// instead of a large raw blob, marked with the document backend's `status`.
/// Later phases can persist the same `document_id` and `vector_anchor` in the
/// document/vector layer without changing the history contract.
pub(super) fn compact_tool_result_for_history_with_status(
    tool_name: &str,
    content: &str,
    max_inline_chars: usize,
    status: &str,
) -> String {
    if content.len() <= max_inline_chars {
        return content.to_string();
    }

    let boundary = content
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max_inline_chars)
        .last()
        .unwrap_or(0);
    let preview = &content[..boundary];
    let (document_id, sha256) = tool_output_document_id(tool_name, content);
    let chunk_count = crate::memory::chunker::chunk_markdown(content, TOOL_OUTPUT_DOCUMENT_CHUNK_TOKENS)
        .len()
        .max(1);
    format!(
        "{preview}\n\n[document_ingest_ref]\n\
document_id: {document_id}\n\
source: tool_output\n\
tool: {tool_name}\n\
content_sha256: {sha256}\n\
bytes: {}\n\
inline_preview_bytes: {boundary}\n\
chunks: {chunk_count}\n\
vector_anchor: vector://{document_id}\n\
status: {status}\n\
[/document_ingest_ref]",
        content.len()
    )
}

/// Text substituted for a tool call that never got to run, or that was still
/// running when the turn was interrupted.
///
/// Every tool call the model asked for needs *some* result message, or the
/// assistant message carrying the `tool_calls` is left unanswered and the next
/// provider request is malformed. This marker fills the gap while saying
/// plainly that the call did not happen, so a model reading the transcript
/// re-issues that one call and leaves the calls that did happen alone.
pub(super) const INTERRUPTED_TOOL_RESULT: &str =
    "Error: this tool call was interrupted before it ran; it may be retried.";

/// Per-call tool output, captured the instant each call returns.
///
/// # Why this is not just the returned `Vec<String>`
///
/// A batch of tool calls is executed concurrently, so the results cannot be
/// written straight into `history` — that is a `&mut` borrow the caller holds.
/// Historically they were collected and written to history only after the
/// *whole* batch finished, which meant an interruption partway through a batch
/// left no trace in history of the calls that had already run and already had
/// side effects. A replay then re-issued the identical batch and ran them all
/// a second time.
///
/// So each call reports here the moment it completes. Whatever this holds is
/// exactly the set of calls that ran, which is exactly the set a replay must
/// not run again, and [`ExecutedToolRecords`] guarantees it reaches history
/// whether the batch finishes or is dropped mid-flight.
#[derive(Debug)]
pub(super) struct ToolResultRecorder {
    slots: parking_lot::Mutex<Vec<Option<String>>>,
}

impl ToolResultRecorder {
    pub(super) fn new(len: usize) -> Self {
        Self {
            slots: parking_lot::Mutex::new(vec![None; len]),
        }
    }

    pub(super) fn record(&self, index: usize, result: &str) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(index) {
            *slot = Some(result.to_string());
        }
    }

    /// Results captured so far, one entry per requested call.
    fn snapshot(&self) -> Vec<Option<String>> {
        self.slots.lock().clone()
    }
}

/// Guarantees that every tool call which actually ran is represented in
/// `history`, including when the turn is interrupted partway through the batch.
///
/// The guard owns the history borrow for the duration of tool execution and
/// post-processing. On the normal path [`Self::commit`] writes the fully
/// processed result messages — document-backed truncation and all — exactly as
/// before. On any other path (an error propagated with `?`, cancellation, or
/// the whole turn future being dropped by the stall detector) `Drop` writes a
/// plain-text record of the same calls instead. Either way the transcript that
/// survives says which calls ran, so a replay re-issues only the ones that did
/// not.
pub(super) struct ExecutedToolRecords<'a> {
    history: &'a mut Vec<ChatMessage>,
    recorder: &'a ToolResultRecorder,
    tool_calls: &'a [ParsedToolCall],
    native_tool_calls: &'a [ToolCall],
    armed: bool,
}

impl<'a> ExecutedToolRecords<'a> {
    pub(super) const fn new(
        history: &'a mut Vec<ChatMessage>,
        recorder: &'a ToolResultRecorder,
        tool_calls: &'a [ParsedToolCall],
        native_tool_calls: &'a [ToolCall],
    ) -> Self {
        Self {
            history,
            recorder,
            tool_calls,
            native_tool_calls,
            armed: true,
        }
    }

    /// History, still borrowed by the guard. Reads are needed while
    /// post-processing (the inline budget is measured against it).
    pub(super) const fn history(&self) -> &Vec<ChatMessage> {
        self.history
    }

    /// Write the fully processed result messages and stand down.
    pub(super) fn commit(mut self, messages: Vec<ChatMessage>) {
        self.armed = false;
        self.history.extend(messages);
    }
}

impl Drop for ExecutedToolRecords<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let slots = self.recorder.snapshot();
        if slots.iter().all(Option::is_none) {
            // Nothing ran, so there is nothing a replay could duplicate. The
            // unanswered assistant message is dropped with the rest of this
            // attempt's history rather than being papered over here.
            return;
        }
        let resolve = |index: usize| -> &str {
            slots
                .get(index)
                .and_then(Option::as_deref)
                .unwrap_or(INTERRUPTED_TOOL_RESULT)
        };
        if self.native_tool_calls.is_empty() {
            let mut tool_results = String::new();
            for (index, call) in self.tool_calls.iter().enumerate() {
                let _ = writeln!(
                    tool_results,
                    "<tool_result name=\"{}\">\n{}\n</tool_result>",
                    call.name,
                    truncate_with_ellipsis(resolve(index), MAX_TOOL_RESULT_INLINE_CHARS)
                );
            }
            self.history.push(ChatMessage::user(format!(
                "{}\n{tool_results}",
                crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
            )));
        } else {
            for (index, native_call) in self.native_tool_calls.iter().enumerate() {
                let tool_msg = serde_json::json!({
                    "tool_call_id": native_call.id,
                    "content": truncate_with_ellipsis(resolve(index), MAX_TOOL_RESULT_INLINE_CHARS),
                });
                self.history.push(ChatMessage::tool(tool_msg.to_string()));
            }
        }
        tracing::warn!(
            recorded = slots.iter().filter(|slot| slot.is_some()).count(),
            requested = self.tool_calls.len(),
            "turn interrupted mid tool batch; already-executed calls recorded in history so a replay cannot repeat them"
        );
    }
}

#[path = "turn_tests/tool_result_recording.rs"]
mod tests;
