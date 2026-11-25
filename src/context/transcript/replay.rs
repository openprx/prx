//! Replay of earlier turns' tool work, and of the attachment a user message
//! carried when it was sent, from the durable log.
//!
//! Two kinds of row are read back, both written with role `tool` so the sync
//! stream never carries them:
//!
//! - `tool.call.recorded` / `tool.result.recorded` (see
//!   [`super::tool_rows`]): every call and result of a turn,
//!   paired by run, round and call id.
//! - `message.attachment.recorded` (written by [`super::tool_rows::append_attachment`]): the text
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
//! ([`History::fit`]). The stored rows are never touched.

use super::tool_rows::{ATTACHMENT_EVENT_TYPE, TOOL_CALL_EVENT_TYPE, TOOL_RESULT_EVENT_TYPE};
use crate::agent::context_budget::{PRE_TURN_FLUSH_THRESHOLD, estimate_history_tokens, plan_context_budget};
use crate::agent::history::build_native_assistant_history;
use crate::memory::{ConversationTurn, MemoryPrincipal, MessageEvent, SessionContextQuery};
use crate::providers::{ChatMessage, ToolCall};
use std::collections::{BTreeMap, HashMap, HashSet};

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
    /// The stored assistant replies, oldest first: text and run.
    assistant_runs: Vec<(String, String)>,
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if id.is_empty() { "call".to_owned() } else { id }
}

impl ReplayLog {
    /// Read the session's tool, attachment and message rows. A read that fails
    /// leaves the log empty, so the turn replays as it did before these rows.
    pub(super) async fn load(mem: &dyn crate::memory::Memory, principal: &MemoryPrincipal) -> Self {
        Self::load_rows(mem, principal, true).await
    }

    /// As [`Self::load`] without the user and assistant rows: enough to build
    /// [`Self::replay_of_run`].
    pub(super) async fn load_tool_rows(mem: &dyn crate::memory::Memory, principal: &MemoryPrincipal) -> Self {
        Self::load_rows(mem, principal, false).await
    }

    async fn load_rows(mem: &dyn crate::memory::Memory, principal: &MemoryPrincipal, with_messages: bool) -> Self {
        let query = |roles: &[&str]| SessionContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: REPLAY_ROW_LIMIT,
            include_roles: roles.iter().map(|role| (*role).to_owned()).collect(),
        };
        let tool_rows = mem.load_recent_session_context(query(&["tool"])).await;
        let message_rows = if with_messages {
            mem.load_recent_session_context(query(&["user", "assistant"])).await
        } else {
            Ok(Vec::new())
        };
        match (tool_rows, message_rows) {
            (Ok(tool_rows), Ok(message_rows)) => Self::from_rows(&tool_rows, &message_rows),
            (Err(error), _) | (_, Err(error)) => {
                tracing::warn!(
                    error = %crate::agent::diagnostics::error_class(&error),
                    "tool replay rows unavailable; history is replayed without them"
                );
                Self::default()
            }
        }
    }

    fn from_rows(tool_rows: &[MessageEvent], message_rows: &[MessageEvent]) -> Self {
        let mut log = Self::default();
        let mut ordered: Vec<&MessageEvent> = message_rows.iter().collect();
        ordered.sort_by_key(|event| event.id);
        for event in ordered {
            if event.event_type == "message.created"
                && let Some(run_id) = &event.run_id
            {
                log.run_of_event.insert(event.event_id.clone(), run_id.clone());
                if event.role == "assistant" {
                    log.assistant_runs.push((event.content.clone(), run_id.clone()));
                }
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

    /// The tool rounds of `run_id` in replay form, `None` when it made none.
    pub(crate) fn replay_of_run(&self, run_id: &str) -> Option<TurnReplay> {
        let mut replay = TurnReplay::default();
        for round in self.rounds.get(run_id)?.values() {
            replay.push_round(round);
        }
        (!replay.messages.is_empty()).then_some(replay)
    }

    /// The run of each reply in `replies` (oldest first), matched with the
    /// stored assistant messages from the newest back by their text. A reply
    /// that does not match its stored message, and every older one, gets
    /// `None`: a run is never guessed.
    pub(crate) fn runs_of_replies(&self, replies: &[&str]) -> Vec<Option<&str>> {
        let mut runs = vec![None; replies.len()];
        let pairs = runs
            .iter_mut()
            .rev()
            .zip(replies.iter().rev().zip(self.assistant_runs.iter().rev()));
        for (slot, (reply, (stored, run))) in pairs {
            if *reply != stored.as_str() {
                break;
            }
            *slot = Some(run.as_str());
        }
        runs
    }

    /// The stored turns as history messages, in order: each user message with
    /// its stored attachment, each assistant reply preceded by the tool rounds
    /// of its run. Event ids line up with the messages, `None` for a replayed
    /// tool message.
    pub(crate) fn expand_turns(&self, turns: Vec<ConversationTurn>) -> History {
        let mut out = History::default();
        let mut replayed_runs: HashSet<&str> = HashSet::new();
        for turn in turns {
            let event_id = turn.message_event_id.clone();
            // The rows are read newest first up to a cap, so the oldest turn
            // can be an assistant reply whose user message was cut: its tool
            // rounds would open the history without a question. The run is
            // marked replayed either way, so a later reply of the same run
            // does not replay the rounds out of their place.
            if turn.role == "assistant"
                && let Some(run_id) = event_id.as_deref().and_then(|id| self.run_of_event.get(id))
                && replayed_runs.insert(run_id.as_str())
                && !out.messages.is_empty()
                && let Some(replay) = self.replay_of_run(run_id)
            {
                out.push_replay(&replay);
            }
            let content = match (
                turn.role.as_str(),
                event_id.as_deref().and_then(|id| self.attachments.get(id)),
            ) {
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
pub struct ReplayedResult {
    /// Index of the result message in the messages that hold it.
    index: usize,
    wire_id: String,
    tool: String,
    status: String,
    bytes: u64,
    document_id: Option<String>,
}

/// The tool rounds of one run as history messages: for each round an
/// assistant envelope carrying the calls, then one `tool` message per call.
#[derive(Debug, Clone, Default)]
pub struct TurnReplay {
    pub(crate) messages: Vec<ChatMessage>,
    results: Vec<ReplayedResult>,
}

impl PartialEq for TurnReplay {
    fn eq(&self, other: &Self) -> bool {
        crate::agent::turn_history::same_messages(&self.messages, &other.messages)
    }
}

impl Eq for TurnReplay {}

/// The turns of a session as history messages, with their tool rounds.
#[derive(Debug, Default)]
pub(crate) struct History {
    pub messages: Vec<ChatMessage>,
    /// The stored event of each message, index for index.
    pub event_ids: Vec<Option<String>>,
    results: Vec<ReplayedResult>,
}

const MISSING_RESULT: &str = "[no result was stored for this call]";

fn result_message(wire_id: &str, content: &str) -> ChatMessage {
    ChatMessage::tool(serde_json::json!({ "tool_call_id": wire_id, "content": content }).to_string())
}

impl TurnReplay {
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
        }
    }
}

impl History {
    /// Append `replay` after the messages held so far.
    fn push_replay(&mut self, replay: &TurnReplay) {
        let base = self.messages.len();
        self.messages.extend(replay.messages.iter().cloned());
        self.event_ids.extend(replay.messages.iter().map(|_| None));
        self.results.extend(replay.results.iter().map(|result| ReplayedResult {
            index: result.index + base,
            ..result.clone()
        }));
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
        fit_results(
            &mut self.messages,
            &self.results,
            std::slice::from_ref(leading),
            std::slice::from_ref(request),
            config,
        )
    }
}

/// Replace the oldest of `results` (in `messages`) by a stub until
/// `before`, `messages` and `after` together fit under the compaction warning
/// threshold. Returns how many were stubbed.
fn fit_results(
    messages: &mut [ChatMessage],
    results: &[ReplayedResult],
    before: &[ChatMessage],
    after: &[ChatMessage],
    config: &crate::config::AgentCompactionConfig,
) -> usize {
    if results.is_empty() {
        return 0;
    }
    let mut whole = Vec::with_capacity(messages.len() + before.len() + after.len());
    whole.extend(before.iter().cloned());
    whole.extend(messages.iter().cloned());
    whole.extend(after.iter().cloned());
    let budget = plan_context_budget(&whole, config, PRE_TURN_FLUSH_THRESHOLD);
    if !budget.over_warning {
        return 0;
    }
    let mut excess = budget.used_tokens.saturating_sub(budget.warning_threshold_tokens);
    let mut stubbed = 0;
    for result in results {
        if excess == 0 {
            break;
        }
        let Some(message) = messages.get_mut(result.index) else {
            continue;
        };
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

/// Terminal chat: replace the oldest replayed results held in `view` by a
/// stub until `view` and the `request` that follows it fit under the warning
/// threshold. `replays` are the replayed rounds of the view's turns, oldest
/// first; a result is found in the view by its bytes, in order, and one that
/// is not found is left alone. Returns how many were stubbed.
pub(crate) fn stub_replayed_results(
    view: &mut [ChatMessage],
    replays: &[&TurnReplay],
    request: &ChatMessage,
    config: &crate::config::AgentCompactionConfig,
) -> usize {
    let mut found = Vec::new();
    let mut cursor = 0;
    for replay in replays {
        for result in &replay.results {
            let Some(original) = replay.messages.get(result.index) else {
                continue;
            };
            let Some(offset) = view.get(cursor..).and_then(|rest| {
                rest.iter()
                    .position(|m| m.role == "tool" && m.content == original.content)
            }) else {
                continue;
            };
            let index = cursor.saturating_add(offset);
            found.push(ReplayedResult {
                index,
                ..result.clone()
            });
            cursor = index.saturating_add(1);
        }
    }
    fit_results(view, &found, &[], std::slice::from_ref(request), config)
}

/// The one-line text a cleared result is replaced by; one wording on every
/// surface (see `context::reduce::clear`).
fn stub_text(result: &ReplayedResult) -> String {
    crate::context::reduce::clear::cleared_result_text(
        &result.tool,
        &result.status,
        result.bytes,
        result.document_id.as_deref(),
    )
}
