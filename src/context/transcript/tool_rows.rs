//! Writers of the transcript's tool rows: tool calls, tool results, the
//! attachment of a user message and the load of a skill instruction.
//!
//! Each call the model makes is appended to `message_events` when the round
//! dispatches it, and each result when the call returns. A pair shares the
//! run (the row's `run_id`), the round and the call id in its payload, so a
//! reader can match them without relying on row order.
//!
//! The rows use their own event types (`tool.call.recorded`,
//! `tool.result.recorded`, `skill.load.recorded`) with role `tool`. They are not
//! `message.created`, so the next turn's history loaders, the handoff
//! provenance match and the sync projection all leave them alone. Whether
//! they are written is the `record_tool_events` setting of the fabric.
//!
//! Stored text is always the redacted form. A result keeps the inline text
//! the history keeps; above the inline limit the row carries the same
//! preview plus a document reference, and only when that document was
//! stored.

use super::replay::Attachment;
use super::sources::{ATTACHMENT_SCHEMA_VERSION, AttachmentSources};
use crate::agent::tool_call_parser::ParsedToolCall;
use crate::agent::tool_result_recording::{MAX_TOOL_RESULT_INLINE_CHARS, ObservedToolResult};
use crate::memory::{MemoryFabric, MessageEventScope};
use crate::providers::ToolCall;

pub(crate) const TOOL_CALL_EVENT_TYPE: &str = "tool.call.recorded";
pub(crate) const TOOL_RESULT_EVENT_TYPE: &str = "tool.result.recorded";
/// Event type of the row that stores a user message's attachment.
pub(crate) const ATTACHMENT_EVENT_TYPE: &str = "message.attachment.recorded";

/// Status `persist_tool_output_document` reports for a stored document.
const STORED_DOCUMENT_STATUS: &str = "persisted_document_backend";

/// The id a call is paired by: the provider's call id, or `<tool>#<index>`
/// for a call that has none (the same fallback the live notifications use).
pub(crate) fn call_id_for(tool: &str, index: usize, native: &[ToolCall]) -> String {
    native
        .get(index)
        .map(|call| call.id.clone())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| format!("{tool}#{index}"))
}

/// Cut `text` to at most `max_bytes` on a char boundary, saying so.
fn bounded(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let total = text.len();
    let shown = text.get(..end).unwrap_or_default();
    format!("{shown}\n[arguments truncated: {end} of {total} bytes shown]")
}

/// Where one provider round writes its tool rows.
pub(crate) struct RoundLog<'a> {
    fabric: &'a MemoryFabric,
    scope: &'a MessageEventScope,
    round: usize,
}

impl<'a> RoundLog<'a> {
    /// `None` when the turn has no durable event sink.
    pub(crate) fn new(fabric: Option<&'a MemoryFabric>, scope: &'a MessageEventScope, round: usize) -> Option<Self> {
        fabric.map(|fabric| Self { fabric, scope, round })
    }

    /// Append one `tool.call.recorded` row per call of the round.
    pub(crate) async fn append_calls(&self, calls: &[ParsedToolCall], native: &[ToolCall]) {
        record_tool_calls(self.fabric, self.scope, self.round, calls, native).await;
    }

    /// Append the `tool.result.recorded` row of one call.
    pub(crate) async fn append_result(
        &self,
        call_id: &str,
        tool: &str,
        outcome: &ObservedToolResult,
        inline_text: &str,
        document_status: &str,
    ) {
        record_tool_result(
            self.fabric,
            self.scope,
            self.round,
            (call_id, tool),
            outcome,
            inline_text,
            document_status,
        )
        .await;
        if let Some(load) = outcome.skill_load.as_ref() {
            super::skill_rows::append_skill_load(self.fabric, self.scope, self.round, call_id, load).await;
        }
    }
}

async fn record_tool_calls(
    fabric: &MemoryFabric,
    scope: &MessageEventScope,
    round: usize,
    calls: &[ParsedToolCall],
    native: &[ToolCall],
) {
    for (index, call) in calls.iter().enumerate() {
        let call_id = call_id_for(&call.name, index, native);
        let arguments = crate::agent::terminal::redact_persisted_json(call.arguments.clone());
        let payload = serde_json::json!({
            "schema_version": 1,
            "run_id": scope.run_id,
            "round": round,
            "index": index,
            "call_id": call_id,
            "tool": call.name,
            "arguments": arguments,
        });
        let content = bounded(arguments.to_string(), MAX_TOOL_RESULT_INLINE_CHARS);
        append(
            fabric,
            scope,
            TOOL_CALL_EVENT_TYPE,
            format!("{} {content}", call.name),
            payload,
        )
        .await;
    }
}

/// `inline_text` is the
/// compacted result the history carries (preview plus reference when the
/// output was stored); `document_status` is what the document store reported.
async fn record_tool_result(
    fabric: &MemoryFabric,
    scope: &MessageEventScope,
    round: usize,
    (call_id, tool): (&str, &str),
    outcome: &ObservedToolResult,
    inline_text: &str,
    document_status: &str,
) {
    let document = (document_status == STORED_DOCUMENT_STATUS).then(|| {
        let (document_id, sha256) = crate::agent::document_ingest::tool_output_document_id(tool, &outcome.content);
        serde_json::json!({ "document_id": document_id, "content_sha256": sha256, "source": "tool_output" })
    });
    let payload = serde_json::json!({
        "schema_version": 1,
        "run_id": scope.run_id,
        "round": round,
        "call_id": call_id,
        "tool": tool,
        "status": outcome.status,
        "executed": outcome.executed,
        "result_bytes": outcome.content.len(),
        "inline_bytes": inline_text.len(),
        "document": document,
    });
    let content = crate::security::redaction::redact_secrets(inline_text);
    append(fabric, scope, TOOL_RESULT_EVENT_TYPE, content, payload).await;
}

async fn append(
    fabric: &MemoryFabric,
    scope: &MessageEventScope,
    event_type: &str,
    content: String,
    payload: serde_json::Value,
) {
    if let Err(error) = fabric
        .record_tool_round_event(scope.clone(), event_type, content, Some(payload.to_string()))
        .await
    {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::warn!(error_len, error_sha256, event_type, "Failed to persist tool event");
    }
}

/// Append the attachment row of a user message: `prefix` and `tail` are the
/// text put before and after it and `sources` the ids that text was built from
/// (see [`AttachmentSources`]). `note` is the scheduled-results note contained
/// verbatim in the tail (empty when none). Nothing is written when tool events are not
/// recorded; the message is then replayed plain, as before.
pub(crate) async fn append_attachment(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    user_event_id: &str,
    attachment: &Attachment,
    sources: &AttachmentSources,
    note: &str,
) {
    let payload = serde_json::json!({
        "schema_version": ATTACHMENT_SCHEMA_VERSION,
        "run_id": scope.run_id,
        "user_event_id": user_event_id,
        "prefix": attachment.prefix,
        "tail": attachment.tail,
        "sources": sources,
        "note": note,
    });
    if let Err(error) = fabric
        .record_tool_round_event(
            scope,
            ATTACHMENT_EVENT_TYPE,
            format!("attachment of {user_event_id}"),
            Some(payload.to_string()),
        )
        .await
    {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::warn!(error_len, error_sha256, "Failed to persist message attachment");
    }
}
