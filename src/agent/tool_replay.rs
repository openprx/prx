//! Replay of earlier turns' tool work, and of the attachment a user message
//! carried when it was sent, from the durable log.
//!
//! Two kinds of row are read back, both written with role `tool` so the sync
//! stream never carries them:
//!
//! - `tool.call.recorded` / `tool.result.recorded` (see
//!   [`crate::agent::tool_event_recording`]): every call and result of a turn,
//!   paired by run, round and call id.
//! - `message.attachment.recorded` (written by [`record_attachment`]): the text
//!   put before and after a user message when it was sent (recalled memory and
//!   the date line), so a later turn replays the message with the same bytes.
//!
//! A turn's tool rounds are replayed in the provider-neutral form the turn loop
//! uses for in-turn rounds (an assistant envelope carrying `tool_calls`, then
//! one `tool` message per call), between the user message and the assistant
//! reply of the run that made them. A call without a stored result is paired
//! with a stub result; a result without a stored call is dropped. A turn that
//! was interrupted before its reply was stored is never replayed.
//!
//! When the request would be over its budget, the oldest replayed results are
//! replaced by a one-line stub before anything else gives way
//! ([`ReplayedTurns::fit`]). The stored rows are never touched.

use crate::agent::context_budget::{PRE_TURN_FLUSH_THRESHOLD, estimate_history_tokens, plan_context_budget};
use crate::agent::history::build_native_assistant_history;
use crate::agent::tool_event_recording::{TOOL_CALL_EVENT_TYPE, TOOL_RESULT_EVENT_TYPE};
use crate::memory::{
    ConversationTurn, MemoryFabric, MemoryPrincipal, MessageEvent, MessageEventScope, SessionContextQuery,
};
use crate::providers::{ChatMessage, ToolCall};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Event type of the row that stores a user message's attachment.
pub(crate) const ATTACHMENT_EVENT_TYPE: &str = "message.attachment.recorded";

/// Rows read per query (the store's own page cap).
const REPLAY_ROW_LIMIT: usize = 500;

/// What was put around a user message when it was sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    /// Text placed before the user's text (recalled memory).
    #[serde(default)]
    pub prefix: String,
    /// Text placed after it (attached file text, the date line).
    #[serde(default)]
    pub tail: String,
}

impl Attachment {
    /// The message as it was sent: `text` between the prefix and the tail.
    pub(crate) fn compose(&self, text: &str) -> String {
        format!("{}{text}{}", self.prefix, self.tail)
    }
}

#[derive(Debug, Clone)]
struct StoredCall {
    call_id: String,
    tool: String,
    arguments: String,
}

#[derive(Debug, Clone)]
struct StoredResult {
    status: String,
    result_bytes: u64,
    document_id: Option<String>,
    content: String,
}

#[derive(Debug, Default)]
struct StoredRound {
    calls: BTreeMap<u64, StoredCall>,
    results: HashMap<String, StoredResult>,
}

/// The rows of one session that replay reads.
#[derive(Debug, Default)]
pub(crate) struct ReplayLog {
    /// Tool rounds by run id, in round order.
    rounds: HashMap<String, BTreeMap<u64, StoredRound>>,
    /// Attachments by the event id of the user message they belong to.
    attachments: HashMap<String, Attachment>,
    /// The run of each stored user or assistant message, by event id.
    run_of_event: HashMap<String, String>,
}

fn payload_of(event: &MessageEvent) -> Option<serde_json::Value> {
    event
        .raw_payload_json
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
}

fn text(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(serde_json::Value::as_str).map(str::to_owned)
}

/// A call id as every provider accepts it: ASCII letters, digits, `_` and `-`.
fn wire_id(call_id: &str) -> String {
    let id: String = call_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if id.is_empty() { "call".to_owned() } else { id }
}

impl ReplayLog {
    /// Read the session's tool, attachment and message rows. A read that fails
    /// leaves the log empty, so the turn replays as it did before these rows.
    pub(crate) async fn load(mem: &dyn crate::memory::Memory, principal: &MemoryPrincipal) -> Self {
        let query = |roles: &[&str]| SessionContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: REPLAY_ROW_LIMIT,
            include_roles: roles.iter().map(|role| (*role).to_owned()).collect(),
        };
        let (tool_rows, message_rows) = match (
            mem.load_recent_session_context(query(&["tool"])).await,
            mem.load_recent_session_context(query(&["user", "assistant"])).await,
        ) {
            (Ok(tool_rows), Ok(message_rows)) => (tool_rows, message_rows),
            (Err(error), _) | (_, Err(error)) => {
                tracing::warn!(
                    error = %crate::agent::diagnostics::error_class(&error),
                    "tool replay rows unavailable; history is replayed without them"
                );
                return Self::default();
            }
        };
        Self::from_rows(&tool_rows, &message_rows)
    }

    fn from_rows(tool_rows: &[MessageEvent], message_rows: &[MessageEvent]) -> Self {
        let mut log = Self::default();
        for event in message_rows {
            if event.event_type == "message.created"
                && let Some(run_id) = &event.run_id
            {
                log.run_of_event.insert(event.event_id.clone(), run_id.clone());
            }
        }
        for event in tool_rows {
            let Some(payload) = payload_of(event) else { continue };
            match event.event_type.as_str() {
                TOOL_CALL_EVENT_TYPE => log.add_call(&payload),
                TOOL_RESULT_EVENT_TYPE => log.add_result(event, &payload),
                ATTACHMENT_EVENT_TYPE => {
                    if let Some(user_event_id) = text(&payload, "user_event_id") {
                        log.attachments.insert(
                            user_event_id,
                            Attachment {
                                prefix: text(&payload, "prefix").unwrap_or_default(),
                                tail: text(&payload, "tail").unwrap_or_default(),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        log
    }

    fn round_mut(&mut self, payload: &serde_json::Value) -> Option<&mut StoredRound> {
        let run_id = text(payload, "run_id")?;
        let round = payload.get("round").and_then(serde_json::Value::as_u64)?;
        Some(self.rounds.entry(run_id).or_default().entry(round).or_default())
    }

    fn add_call(&mut self, payload: &serde_json::Value) {
        let (Some(call_id), Some(tool), Some(index)) = (
            text(payload, "call_id"),
            text(payload, "tool"),
            payload.get("index").and_then(serde_json::Value::as_u64),
        ) else {
            return;
        };
        let arguments = payload
            .get("arguments")
            .map_or_else(|| "{}".to_owned(), serde_json::Value::to_string);
        if let Some(round) = self.round_mut(payload) {
            round.calls.insert(
                index,
                StoredCall {
                    call_id,
                    tool,
                    arguments,
                },
            );
        }
    }

    fn add_result(&mut self, event: &MessageEvent, payload: &serde_json::Value) {
        let Some(call_id) = text(payload, "call_id") else {
            return;
        };
        let result = StoredResult {
            status: text(payload, "status").unwrap_or_else(|| "unknown".to_owned()),
            result_bytes: payload
                .get("result_bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
            document_id: payload
                .get("document")
                .and_then(|document| text(document, "document_id")),
            content: event.content.clone(),
        };
        if let Some(round) = self.round_mut(payload) {
            round.results.insert(call_id, result);
        }
    }

    /// The stored turns as history messages, in order: each user message with
    /// its stored attachment, each assistant reply preceded by the tool rounds
    /// of its run. Event ids line up with the messages, `None` for a replayed
    /// tool message.
    pub(crate) fn expand_turns(&self, turns: Vec<ConversationTurn>) -> ReplayedTurns {
        let mut out = ReplayedTurns::default();
        let mut replayed_runs: HashSet<&str> = HashSet::new();
        for turn in turns {
            let event_id = turn.message_event_id.clone();
            if turn.role == "assistant"
                && let Some(run_id) = event_id.as_deref().and_then(|id| self.run_of_event.get(id))
                && replayed_runs.insert(run_id.as_str())
                && let Some(rounds) = self.rounds.get(run_id)
            {
                for round in rounds.values() {
                    out.push_round(round);
                }
            }
            let content = match (turn.role.as_str(), event_id.as_deref().and_then(|id| self.attachments.get(id))) {
                ("user", Some(attachment)) => format!("{}{}{}", attachment.prefix, turn.content, attachment.tail),
                _ => turn.content,
            };
            out.messages
                .push(crate::agent::turn_contracts::stored_turn_message(turn.role, content));
            out.event_ids.push(event_id);
        }
        out
    }
}

/// A replayed result message, kept so it can be turned into a stub.
#[derive(Debug, Clone)]
struct ReplayedResult {
    /// Index into [`ReplayedTurns::messages`].
    index: usize,
    wire_id: String,
    tool: String,
    status: String,
    bytes: u64,
    document_id: Option<String>,
}

/// The turns of a session as history messages, with their tool rounds.
#[derive(Debug, Default)]
pub(crate) struct ReplayedTurns {
    pub messages: Vec<ChatMessage>,
    /// The stored event of each message, index for index.
    pub event_ids: Vec<Option<String>>,
    results: Vec<ReplayedResult>,
}

const MISSING_RESULT: &str = "[no result was stored for this call]";

fn result_message(wire_id: &str, content: &str) -> ChatMessage {
    ChatMessage::tool(serde_json::json!({ "tool_call_id": wire_id, "content": content }).to_string())
}

impl ReplayedTurns {
    fn push_round(&mut self, round: &StoredRound) {
        if round.calls.is_empty() {
            return;
        }
        let calls: Vec<ToolCall> = round
            .calls
            .values()
            .map(|call| ToolCall {
                id: wire_id(&call.call_id),
                name: call.tool.clone(),
                arguments: call.arguments.clone(),
            })
            .collect();
        self.messages
            .push(ChatMessage::assistant(build_native_assistant_history("", &calls, None)));
        self.event_ids.push(None);
        for call in round.calls.values() {
            let id = wire_id(&call.call_id);
            let stored = round.results.get(&call.call_id);
            let content = stored.map_or(MISSING_RESULT, |result| result.content.as_str());
            self.results.push(ReplayedResult {
                index: self.messages.len(),
                wire_id: id.clone(),
                tool: call.tool.clone(),
                status: stored.map_or_else(|| "missing".to_owned(), |result| result.status.clone()),
                bytes: stored.map_or(0, |result| result.result_bytes),
                document_id: stored.and_then(|result| result.document_id.clone()),
            });
            self.messages.push(result_message(&id, content));
            self.event_ids.push(None);
        }
    }

    /// Replace the oldest replayed results by a stub, as many as it takes for
    /// the request (`leading`, these turns, `request`) to fit under the
    /// compaction warning threshold. Nothing else is dropped or changed.
    /// Returns how many results were stubbed.
    pub(crate) fn fit(
        &mut self,
        leading: &ChatMessage,
        request: &ChatMessage,
        config: &crate::config::AgentCompactionConfig,
    ) -> usize {
        if self.results.is_empty() {
            return 0;
        }
        let mut whole = Vec::with_capacity(self.messages.len() + 2);
        whole.push(leading.clone());
        whole.extend(self.messages.iter().cloned());
        whole.push(request.clone());
        let budget = plan_context_budget(&whole, config, PRE_TURN_FLUSH_THRESHOLD);
        if !budget.over_warning {
            return 0;
        }
        let mut excess = budget.used_tokens.saturating_sub(budget.warning_threshold_tokens);
        let mut stubbed = 0;
        for result in &self.results {
            if excess == 0 {
                break;
            }
            let Some(message) = self.messages.get_mut(result.index) else { continue };
            let stub = result_message(&result.wire_id, &stub_text(result));
            let saved = estimate_history_tokens(std::slice::from_ref(message))
                .saturating_sub(estimate_history_tokens(std::slice::from_ref(&stub)));
            if saved == 0 {
                continue;
            }
            *message = stub;
            excess = excess.saturating_sub(saved);
            stubbed += 1;
        }
        stubbed
    }
}

/// The one-line text a cleared result is replaced by.
fn stub_text(result: &ReplayedResult) -> String {
    let lookup = result.document_id.as_ref().map_or_else(
        || "the output is not kept in view; run the tool again if it is needed".to_owned(),
        |id| format!("full output stored as document {id}, read it with document_get_chunk"),
    );
    format!(
        "[tool result cleared: tool {}, status {}, {} bytes; {lookup}]",
        result.tool, result.status, result.bytes
    )
}

/// Append the attachment row of a user message: `prefix` and `tail` are the
/// text put before and after it. Nothing is written when tool events are not
/// recorded; the message is then replayed plain, as before.
pub(crate) async fn record_attachment(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    user_event_id: &str,
    attachment: &Attachment,
) {
    let payload = serde_json::json!({
        "schema_version": 1,
        "run_id": scope.run_id,
        "user_event_id": user_event_id,
        "prefix": attachment.prefix,
        "tail": attachment.tail,
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
