use crate::agent::context_budget::estimate_history_tokens;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::response_guard::strip_assistant_fabricated_tool_markers;
use crate::agent::turn_contracts::{
    CompactionPatch, CompactionPatchError, CompactionPatchGuard, POST_COMPACTION_REFRESH_MARKER,
};
use crate::memory::{
    CompactionRunInput, CompactionSourceEventRange, Memory, MemoryCategory, MemoryPrincipal, MemoryStoreMetadata,
    MemoryWriteContext, MessageEvent, SessionContextQuery,
};
use crate::providers::{ChatMessage, Provider};
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use uuid::Uuid;

/// Keep this many most-recent non-system messages after compaction.
pub(super) const COMPACTION_KEEP_RECENT_MESSAGES: usize = 20;

/// Safety cap for compaction source transcript passed to the summarizer.
const COMPACTION_MAX_SOURCE_CHARS: usize = 12_000;

/// Max characters retained in stored compaction summary.
/// 🟡 Behavior-limits Phase 1: raised 2000 -> 8000 (4x).
pub(super) const COMPACTION_MAX_SUMMARY_CHARS: usize = 8_000;

/// 🟡 Behavior-limits Phase 1: raised 800 -> 3200 (4x).
pub(super) const MEMORY_FLUSH_MAX_CHARS: usize = 3_200;

fn build_compaction_transcript(messages: &[ChatMessage]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        let role = msg.role.to_uppercase();
        let _ = writeln!(transcript, "{role}: {}", msg.content.trim());
    }

    if transcript.chars().count() > COMPACTION_MAX_SOURCE_CHARS {
        truncate_with_ellipsis(&transcript, COMPACTION_MAX_SOURCE_CHARS)
    } else {
        transcript
    }
}

fn bounded_compaction_projection(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut projected = Vec::new();
    let mut retained_chars = 0usize;
    for msg in messages {
        if retained_chars >= COMPACTION_MAX_SOURCE_CHARS {
            break;
        }
        let remaining = COMPACTION_MAX_SOURCE_CHARS.saturating_sub(retained_chars);
        let role_chars = msg.role.chars().count();
        let content_budget = remaining.saturating_sub(role_chars.saturating_add(4));
        if content_budget == 0 {
            break;
        }
        let content = if msg.content.chars().count() > content_budget {
            truncate_with_ellipsis(&msg.content, content_budget)
        } else {
            msg.content.clone()
        };
        retained_chars =
            retained_chars.saturating_add(role_chars.saturating_add(content.chars().count()).saturating_add(4));
        projected.push(ChatMessage {
            role: msg.role.clone(),
            content,
        });
    }
    projected
}

#[cfg(test)]
fn apply_compaction_summary(history: &mut Vec<ChatMessage>, start: usize, compact_end: usize, summary: &str) {
    let summary_msg = ChatMessage::assistant(format!("[Compaction summary]\n{}", summary.trim()));
    history.splice(start..compact_end, std::iter::once(summary_msg));
}

fn hash_compaction_messages(messages: &[ChatMessage]) -> String {
    let mut hasher = sha2::Sha256::new();
    for message in messages {
        hasher.update(message.role.as_bytes());
        hasher.update(b"\0");
        hasher.update(message.content.as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

#[must_use]
pub fn compaction_patch_guard_for(
    history: &[ChatMessage],
    range_start: usize,
    range_end: usize,
) -> Option<CompactionPatchGuard> {
    let replaced = history.get(range_start..range_end)?;
    Some(CompactionPatchGuard {
        history_len: history.len(),
        range_start,
        range_end,
        replaced_hash: hash_compaction_messages(replaced),
    })
}

#[must_use]
pub fn compaction_patch_guard_matches(history: &[ChatMessage], guard: &CompactionPatchGuard) -> bool {
    let Some(current) = compaction_patch_guard_for(history, guard.range_start, guard.range_end) else {
        return false;
    };
    current == *guard
}

/// Applies `patch` to `history` only when the patch's guard describes it.
/// The replaced range is the guard's range. Both the bounds and the guard are
/// checked before any mutation, so on error `history` is unchanged.
pub fn apply_compaction_patch_exact(
    history: &mut Vec<ChatMessage>,
    patch: &CompactionPatch,
) -> Result<(), CompactionPatchError> {
    let start = patch.range_start();
    let end = patch.range_end();
    let len = history.len();
    if start > end || end > len {
        return Err(CompactionPatchError::RangeOutOfBounds { start, end, len });
    }
    if !compaction_patch_guard_matches(history, &patch.guard) {
        return Err(CompactionPatchError::GuardMismatch {
            expected_len: patch.guard.history_len,
            actual_len: len,
        });
    }
    let append_index = start.saturating_add(patch.replacement.len());
    history.splice(start..end, patch.replacement.iter().cloned());
    history.splice(append_index..append_index, patch.append_after.iter().cloned());
    Ok(())
}

fn compaction_summary_fidelity_status(
    summary: &str,
    mode: crate::config::AgentCompactionMode,
    source_messages: &[ChatMessage],
) -> &'static str {
    if summary.trim().is_empty() {
        return "rejected_empty";
    }
    if matches!(mode, crate::config::AgentCompactionMode::Safeguard) {
        let required_headers = [
            "## Decisions",
            "## Open TODOs",
            "## Constraints",
            "## Pending",
            "## Exact",
            "## Progress",
            "## Critical Context",
        ];
        let found_count = required_headers
            .iter()
            .filter(|header| summary.contains(*header))
            .count();
        if found_count < 3 {
            return "accepted_retry_missing_headers";
        }
    }
    let identifiers = source_messages
        .iter()
        .flat_map(|message| message.content.split_whitespace())
        .filter(|word| {
            word.contains('/')
                || word.contains("://")
                || word.contains(':')
                || word.chars().filter(|ch| *ch == '-').count() >= 2
        })
        .take(8);
    let missing_identifiers = identifiers
        .filter(|identifier| !summary.contains(identifier.trim_matches(|ch: char| ch.is_ascii_punctuation())))
        .count();
    if missing_identifiers > 0 {
        "accepted_identifier_loss"
    } else {
        "accepted"
    }
}

pub(super) fn extract_document_ingest_refs(source_messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    let mut refs = Vec::new();
    for (index, message) in source_messages.iter().enumerate() {
        let mut remaining = message.content.as_str();
        while let Some(start) = remaining.find("[document_ingest_ref]") {
            let after_start = &remaining[start + "[document_ingest_ref]".len()..];
            let Some(end) = after_start.find("[/document_ingest_ref]") else {
                break;
            };
            let block = &after_start[..end];
            let mut item = serde_json::Map::new();
            item.insert("index".to_string(), serde_json::json!(index));
            item.insert("role".to_string(), serde_json::json!(message.role));
            for line in block.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let key = key.trim();
                let value = value.trim();
                if key.is_empty() || value.is_empty() {
                    continue;
                }
                let parsed_value = match key {
                    "bytes" | "inline_preview_bytes" | "chunks" => value
                        .parse::<u64>()
                        .map_or_else(|_| serde_json::json!(value), serde_json::Value::from),
                    _ => serde_json::json!(value),
                };
                item.insert(key.to_string(), parsed_value);
            }
            if item.contains_key("document_id") {
                refs.push(serde_json::Value::Object(item));
            }
            remaining = &after_start[end + "[/document_ingest_ref]".len()..];
        }
    }
    refs
}

#[derive(Debug)]
struct CompactionAuditSource {
    message_count: usize,
    token_estimate: usize,
    source_event_ids: Vec<String>,
    source_event_range: Option<CompactionSourceEventRange>,
    source_event_provenance_status: String,
    /// Cold-window messages covered by an assistant turn rather than by an
    /// event id of their own (tool round-trip envelopes and results).
    unpersisted_companion_count: usize,
    document_refs: Vec<serde_json::Value>,
}

impl Default for CompactionAuditSource {
    fn default() -> Self {
        Self {
            message_count: 0,
            token_estimate: 0,
            source_event_ids: Vec::new(),
            source_event_range: None,
            source_event_provenance_status: "unavailable".to_string(),
            unpersisted_companion_count: 0,
            document_refs: Vec::new(),
        }
    }
}

fn build_compaction_audit_source(source_messages: &[ChatMessage]) -> CompactionAuditSource {
    CompactionAuditSource {
        message_count: source_messages.len(),
        token_estimate: estimate_history_tokens(source_messages),
        source_event_ids: Vec::new(),
        source_event_range: None,
        source_event_provenance_status: "unavailable".to_string(),
        unpersisted_companion_count: 0,
        document_refs: extract_document_ingest_refs(source_messages),
    }
}

/// Provenance status used when every cold-window message is accounted for, but
/// some of them are tool round-trip parts covered by their assistant turn
/// rather than by an event id of their own.
const COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS: &str = "exact_with_tool_roundtrips";

/// Event type recorded when a `switch` rollover could not stay lossless and the
/// turn continued on a token-aware trim instead of failing.
pub(super) const CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE: &str = "context.compaction.degraded";

const CONTEXT_HANDOFF_START: &str = "[context_handoff]";

const CONTEXT_HANDOFF_END: &str = "[/context_handoff]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionEventProvenance {
    pub(crate) source_event_ids: Vec<String>,
    pub(crate) covered_range: CompactionSourceEventRange,
    /// Messages in the cold window that belong to a tool round-trip and have no
    /// durable event of their own. They are covered by the assistant turn they
    /// hang off, not by an entry in `source_event_ids`.
    pub(crate) companion_message_count: usize,
}

/// The texts a history message may have been persisted as: the same text,
/// the text redacted (every turn commit since D-054), or the text redacted and
/// capped (chat events written before D-054, which capped them).
fn persisted_forms(content: &str) -> [String; 3] {
    [
        content.to_string(),
        crate::security::redaction::redact_secrets(content),
        crate::chat::sanitize::sanitize_for_persistence(content),
    ]
}

/// Whether a history message is the durable event it was persisted as
/// ([`persisted_forms`]); the provenance pool indexes the same forms.
#[cfg(test)]
fn source_message_matches_event(message: &ChatMessage, event: &MessageEvent) -> bool {
    message.role == event.role && persisted_forms(&message.content).contains(&event.content)
}

pub(crate) fn is_context_handoff_message(message: &ChatMessage) -> bool {
    is_context_handoff_turn(&message.role, &message.content)
}

/// Whether a stored turn (`role`, `content`) is a context handoff.
pub(crate) fn is_context_handoff_turn(role: &str, content: &str) -> bool {
    role == "assistant" && content.starts_with(CONTEXT_HANDOFF_START)
}

/// The JSON note inside a context handoff message, if `content` is one.
#[must_use]
pub(crate) fn parse_context_handoff_note(content: &str) -> Option<serde_json::Value> {
    let json = content
        .strip_prefix(CONTEXT_HANDOFF_START)?
        .strip_suffix(CONTEXT_HANDOFF_END)?
        .trim();
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .filter(serde_json::Value::is_object)
}

/// The handoff message carrying `note_json`, as persisted and replayed.
#[must_use]
pub(crate) fn format_context_handoff_message(note_json: &str) -> String {
    format!("{CONTEXT_HANDOFF_START}\n{note_json}\n{CONTEXT_HANDOFF_END}")
}

/// Upper bound of a handoff message (note plus markers). The largest
/// supported note references [`MAX_REFERENCED_EVENTS`] event ids of at most
/// [`HANDOFF_NOTE_MAX_ID_BYTES`] bytes each and [`FOLDED_TOOL_NAME_LIMIT`]
/// tool names of at most [`HANDOFF_NOTE_MAX_TOOL_NAME_BYTES`] bytes (listed
/// and repeated in the notice): about 37 KiB. A handoff over this bound is
/// refused before it is applied; a validated note is never truncated, so a
/// persisted handoff always stays parseable.
///
/// [`MAX_REFERENCED_EVENTS`]: crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS
pub(crate) const MAX_HANDOFF_MESSAGE_BYTES: usize = 48 * 1024;

/// Longest event id, handoff id or trigger a validated note may carry.
pub(crate) const HANDOFF_NOTE_MAX_ID_BYTES: usize = 64;

/// Longest folded tool name a validated note may carry.
pub(crate) const HANDOFF_NOTE_MAX_TOOL_NAME_BYTES: usize = 128;

/// The exact schema of a note the compaction code writes; any other field,
/// at any level, makes the note invalid.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffNoteSchema {
    schema_version: u64,
    kind: String,
    strategy: String,
    handoff_event_id: String,
    parent_handoff_event_id: Option<String>,
    from_generation: u64,
    to_generation: u64,
    created_at: String,
    trigger: String,
    #[serde(rename = "source_message_count")]
    _source_message_count: u64,
    #[serde(rename = "source_token_estimate")]
    _source_token_estimate: u64,
    #[serde(rename = "unpersisted_companion_count")]
    _unpersisted_companion_count: u64,
    folded_tool_roundtrips: HandoffNoteFolded,
    folded_tool_roundtrips_notice: String,
    provenance_status: String,
    source_event_range: HandoffNoteRange,
    #[serde(rename = "retained_recent_message_count")]
    _retained_recent_message_count: u64,
    lookup: HandoffNoteLookup,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffNoteFolded {
    count: usize,
    tools: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffNoteRange {
    first_event_id: String,
    last_event_id: String,
    first_row_id: i64,
    last_row_id: i64,
    #[serde(rename = "source_event_count")]
    _source_event_count: u64,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffNoteLookup {
    tool: String,
    arguments: HandoffNoteLookupArguments,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffNoteLookupArguments {
    event_ids: Vec<String>,
    first_row_id: i64,
    last_row_id: i64,
}

/// An id, handoff id or trigger of a validated note: ASCII letters, digits,
/// `-`, `_`, `.` or `:`, at most [`HANDOFF_NOTE_MAX_ID_BYTES`] bytes.
fn is_handoff_note_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= HANDOFF_NOTE_MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn is_handoff_note_tool_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= HANDOFF_NOTE_MAX_TOOL_NAME_BYTES && !value.chars().any(char::is_control)
}

/// The note of `content` when it is a handoff the compaction code could have
/// written: the exact framing, at most [`MAX_HANDOFF_MESSAGE_BYTES`], the
/// exact schema ([`HandoffNoteSchema`], no other field) and every value in its
/// shape (constant kind, strategy and tool; consecutive generations; known
/// provenance status; RFC 3339 time; bounded identifiers and lookup range; the
/// notice the folded count and tools produce). `None` for anything else, in
/// particular for model output that merely looks like a handoff.
#[must_use]
pub(crate) fn validated_context_handoff_note(content: &str) -> Option<serde_json::Value> {
    if content.len() > MAX_HANDOFF_MESSAGE_BYTES {
        return None;
    }
    let json = content
        .strip_prefix(CONTEXT_HANDOFF_START)?
        .strip_prefix('\n')?
        .strip_suffix(CONTEXT_HANDOFF_END)?
        .strip_suffix('\n')?;
    let note: HandoffNoteSchema = serde_json::from_str(json).ok()?;
    let lookup = &note.lookup.arguments;
    let range = &note.source_event_range;
    let folded = FoldedToolRoundtrips {
        count: note.folded_tool_roundtrips.count,
        tools: note.folded_tool_roundtrips.tools,
    };
    let valid = note.schema_version == 1
        && note.kind == "context_handoff"
        && note.strategy == "hard_switch"
        && note.lookup.tool == crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME
        && note.from_generation.checked_add(1) == Some(note.to_generation)
        && matches!(
            note.provenance_status.as_str(),
            "exact" | COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS
        )
        && chrono::DateTime::parse_from_rfc3339(&note.created_at).is_ok()
        && is_handoff_note_identifier(&note.trigger)
        && is_handoff_note_identifier(&note.handoff_event_id)
        && note
            .parent_handoff_event_id
            .as_deref()
            .is_none_or(is_handoff_note_identifier)
        && is_handoff_note_identifier(&range.first_event_id)
        && is_handoff_note_identifier(&range.last_event_id)
        && !lookup.event_ids.is_empty()
        && lookup.event_ids.len() <= crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS
        && lookup.event_ids.iter().all(|id| is_handoff_note_identifier(id))
        && lookup.first_row_id <= lookup.last_row_id
        && lookup.last_row_id.saturating_sub(lookup.first_row_id)
            <= crate::tools::transcript_history_lookup::MAX_ROW_SPAN
        && (range.first_row_id, range.last_row_id) == (lookup.first_row_id, lookup.last_row_id)
        && folded.tools.len() <= FOLDED_TOOL_NAME_LIMIT
        && folded.tools.iter().all(|tool| is_handoff_note_tool_name(tool))
        && note.folded_tool_roundtrips_notice == format_folded_tool_roundtrips_notice(&folded);
    if !valid {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(json).ok()
}

/// True when `message` exists only inside a live tool round-trip and therefore
/// never became its own durable `message.created` event.
///
/// The loop appends three such shapes: the `role="tool"` result envelopes
/// (native mode), the `[Tool results]` user block (prompt mode), and the
/// assistant envelope carrying native `tool_calls`. They belong to the
/// assistant turn that requested them, so transcript provenance folds them into
/// that turn instead of refusing to resolve the whole cold window. Before this,
/// any agentic session was permanently unable to compact in `switch` mode.
pub(super) fn is_tool_roundtrip_companion(message: &ChatMessage) -> bool {
    match message.role.as_str() {
        "tool" => true,
        "user" => message
            .content
            .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX),
        "assistant" => {
            message.content.starts_with('{')
                && message.content.contains("\"tool_calls\"")
                && serde_json::from_str::<serde_json::Value>(&message.content).is_ok_and(|value| {
                    value
                        .get("tool_calls")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|calls| !calls.is_empty())
                })
        }
        _ => false,
    }
}

/// Which messages of a compaction source are tool round-trip companions
/// ([`is_tool_roundtrip_companion`]): folded into the assistant turn that
/// requested them rather than resolved to a transcript event of their own.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RoundCompanions<'a> {
    /// Every message is an acknowledged session turn: none is a companion,
    /// whatever its text.
    Acknowledged,
    /// One flag per message, from the canonical history's provenance
    /// ([`crate::agent::turn_history::CanonicalHistory::tool_round_companions`]).
    Marked(&'a [bool]),
}

impl RoundCompanions<'_> {
    fn is_companion(self, index: usize) -> bool {
        match self {
            Self::Acknowledged => false,
            Self::Marked(flags) => flags.get(index).copied().unwrap_or_default(),
        }
    }
}

/// True when a companion `message` is the *request* half of a tool round-trip: the assistant
/// envelope carrying native `tool_calls`, or the prompt-mode `[Tool results]`
/// block that stands in for one when the request itself is a durable message.
///
/// Counting request halves (rather than every companion) makes "round-trips" a
/// number the model and the user can reason about: one per tool batch the
/// window contains.
fn is_tool_roundtrip_request(message: &ChatMessage) -> bool {
    matches!(message.role.as_str(), "user" | "assistant")
}

/// Names of the tools called inside `message`, in call order.
fn tool_names_in_roundtrip_request(message: &ChatMessage) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content) else {
        return Vec::new();
    };
    let Some(calls) = value.get("tool_calls").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|call| {
            call.get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    call.get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_string)
        })
        .collect()
}

/// How many tool round-trips a switch handoff folds away, and which tools they
/// called.
///
/// A `switch` handoff is an event-id pointer, not a summary, and tool results
/// never became events of their own. Everything this counts is therefore
/// verbatim context that no lookup can bring back, which is precisely what the
/// note has to say out loud.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FoldedToolRoundtrips {
    pub(crate) count: usize,
    pub(crate) tools: Vec<String>,
}

/// Names listed in the handoff note. Beyond this the list stops being a hint and
/// starts being the payload it is meant to replace.
const FOLDED_TOOL_NAME_LIMIT: usize = 12;

pub(crate) fn folded_tool_roundtrips(
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
) -> FoldedToolRoundtrips {
    let mut count = 0usize;
    let mut tools: Vec<String> = Vec::new();
    for message in source_messages
        .iter()
        .enumerate()
        .filter(|(index, message)| companions.is_companion(*index) && is_tool_roundtrip_request(message))
        .map(|(_, message)| message)
    {
        count = count.saturating_add(1);
        for name in tool_names_in_roundtrip_request(message) {
            if !tools.iter().any(|existing| *existing == name) {
                tools.push(name);
            }
        }
    }
    tools.truncate(FOLDED_TOOL_NAME_LIMIT);
    FoldedToolRoundtrips { count, tools }
}

/// The one line the model and the user read to learn what the window lost.
pub(crate) fn format_folded_tool_roundtrips_notice(folded: &FoldedToolRoundtrips) -> String {
    if folded.count == 0 {
        return "no tool round-trips were folded into this handoff".to_string();
    }
    let tools = if folded.tools.is_empty() {
        "unknown".to_string()
    } else {
        folded.tools.join(", ")
    };
    format!(
        "{} tool round-trips (tools: {tools}) were folded into this handoff and are not recoverable in this window",
        folded.count
    )
}

/// Move the compaction boundary backwards so the retained hot window never
/// starts in the middle of a tool round-trip.
///
/// A `role="tool"` result whose requesting assistant message was compacted away
/// is an orphan every native provider rejects, so the boundary walks back until
/// the first retained message is a durable user/assistant turn.
fn compaction_boundary_without_orphan_tool_results(companions: &[bool], start: usize, compact_end: usize) -> usize {
    let mut boundary = compact_end;
    while boundary > start {
        let Some(first_retained) = companions.get(boundary) else {
            break;
        };
        if !*first_retained {
            break;
        }
        boundary = boundary.saturating_sub(1);
    }
    if boundary > start {
        return boundary;
    }
    // Walking back reached `start`: the whole requested cold window sits inside
    // one long tool round-trip chain. Giving up here returned "nothing to
    // compact" on every rollover, so `switch` silently degraded to the lossy
    // trim for the rest of the session. Walk forward to the first retained
    // message that can stand at the head of the hot window instead — the cold
    // window grows, but it still folds into one handoff.
    let mut forward = compact_end;
    while forward < companions.len() {
        if !companions.get(forward).copied().unwrap_or_default() {
            return forward;
        }
        forward = forward.saturating_add(1);
    }
    // Every message after `start` is a companion: there is no durable turn to
    // retain and nothing safe to cut.
    start
}

/// The newest context-handoff generation in `history`, 0 when none.
#[must_use]
pub(crate) fn context_handoff_generation(history: &[ChatMessage]) -> u64 {
    context_handoff_lineage(history).0
}

fn context_handoff_lineage(history: &[ChatMessage]) -> (u64, Option<String>) {
    history
        .iter()
        .rev()
        .find(|message| is_context_handoff_message(message))
        .and_then(|message| parse_context_handoff_note(&message.content))
        .map_or((0, None), |note| {
            (
                note.get("to_generation")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                note.get("handoff_event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            )
        })
}

/// Rows per page of the provenance pool read (D8; the SQLite query cap,
/// `MAX_CONVERSATION_QUERY_LIMIT`). A shorter page means the session's first
/// row has been read.
const POOL_PAGE_ROWS: usize = 500;

/// Most rows the provenance pool reads (D8: 40 pages). A session with more own
/// user/assistant rows has a pool whose completeness is unproven, so its
/// provenance is never exact (D9).
const POOL_MAX_ROWS: usize = 20_000;

/// Candidate checks one segment search may spend (D7). Running out leaves
/// uniqueness unproven: the result is unavailable, never a guess.
const TURN_MATCH_STEP_BUDGET: usize = 200_000;

/// Why a compaction source has no exact transcript provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProvenanceGap {
    /// The principal has no session key (or the source is empty).
    NoSessionKey,
    /// The durable remainder is empty or holds a role other than user/assistant.
    NonTranscriptSource,
    /// A handoff message does not match exactly one unused switch row.
    HandoffRowUnmatched,
    /// A run of durable messages has no admissible assignment of rows.
    TurnUnmatched,
    /// Uniqueness is not proven: two or more admissible assignments, an
    /// exhausted search budget, or a candidate pool that may be truncated.
    TurnAmbiguous,
}

impl ProvenanceGap {
    const ALL: [Self; 5] = [
        Self::NoSessionKey,
        Self::NonTranscriptSource,
        Self::HandoffRowUnmatched,
        Self::TurnUnmatched,
        Self::TurnAmbiguous,
    ];

    /// The provenance status recorded for this gap.
    pub(crate) const fn status(self) -> &'static str {
        match self {
            Self::NoSessionKey => "unavailable:no_session_key",
            Self::NonTranscriptSource => "unavailable:non_transcript_source",
            Self::HandoffRowUnmatched => "unavailable:handoff_row_unmatched",
            Self::TurnUnmatched => "unavailable:turn_unmatched",
            Self::TurnAmbiguous => "unavailable:turn_ambiguous",
        }
    }
}

/// The outcome of resolving a compaction source to its transcript rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProvenanceResolution {
    Exact(CompactionEventProvenance),
    Unavailable(ProvenanceGap),
}

/// Role of a pool row (only user and assistant rows are transcript rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolRole {
    User,
    Assistant,
}

impl PoolRole {
    fn of(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

/// One own `message.created` row of the session whose text matches at least
/// one durable source message. Its content is not kept.
#[derive(Debug, Clone)]
struct PoolRow {
    id: i64,
    event_id: String,
    role: PoolRole,
    /// The row's run id when it is non-empty after trim.
    run_id: Option<String>,
    /// Indices of the durable source messages this row matches
    /// ([`persisted_forms`] of the same role).
    matches: Vec<usize>,
    /// Position among every own user/assistant `message.created` row read, in
    /// id order: the store adjacency the contiguous seed is computed over.
    store_rank: usize,
}

/// One own `context.switch.created` row: the handoff messages it matches and
/// what its note references. Its content is not kept.
#[derive(Debug, Clone)]
struct PoolSwitchRow {
    id: i64,
    event_id: String,
    matches: Vec<usize>,
    lookup_event_ids: Vec<String>,
    parent_handoff_event_id: Option<String>,
}

/// Every own transcript row of the session that could be a candidate, read to
/// the session's first row (§3.1).
#[derive(Debug, Default)]
struct CandidatePool {
    /// Matching `message.created` rows, sorted by id.
    messages: Vec<PoolRow>,
    switches: Vec<PoolSwitchRow>,
    /// Ids of every own user/assistant `message.created` row read.
    message_ids: Vec<i64>,
}

/// The accepted persisted forms of every durable source message
/// ([`persisted_forms`]: raw, redacted, sanitized), keyed by role
/// and form, to the indices of the messages that have them.
struct SourceForms {
    by_role: HashMap<String, HashMap<String, Vec<usize>>>,
}

impl SourceForms {
    fn new(messages: &[&ChatMessage]) -> Self {
        let mut by_role: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        for (index, message) in messages.iter().enumerate() {
            let forms = by_role.entry(message.role.clone()).or_default();
            for form in persisted_forms(&message.content) {
                let indices = forms.entry(form).or_default();
                if indices.last() != Some(&index) {
                    indices.push(index);
                }
            }
        }
        Self { by_role }
    }

    fn matches(&self, role: &str, content: &str) -> Vec<usize> {
        self.by_role
            .get(role)
            .and_then(|forms| forms.get(content))
            .cloned()
            .unwrap_or_default()
    }
}

/// The lookup event ids and parent of a stored handoff note.
fn switch_note_references(content: &str) -> (Vec<String>, Option<String>) {
    let Some(note) = parse_context_handoff_note(content) else {
        return (Vec::new(), None);
    };
    let lookup_event_ids = note
        .pointer("/lookup/arguments/event_ids")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let parent = note
        .get("parent_handoff_event_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    (lookup_event_ids, parent)
}

impl CandidatePool {
    /// Keep a compact record of every own transcript row of `page`; drop
    /// every row's content.
    fn absorb(&mut self, page: Vec<MessageEvent>, session_key: Option<&str>, forms: &SourceForms) {
        for event in page {
            // Defence in depth: only the session's own rows (canonical key).
            if event.session_key.as_deref() != session_key {
                continue;
            }
            let Some(role) = PoolRole::of(&event.role) else {
                continue;
            };
            match event.event_type.as_str() {
                "message.created" => {
                    self.message_ids.push(event.id);
                    let matches = forms.matches(&event.role, &event.content);
                    if !matches.is_empty() {
                        self.messages.push(PoolRow {
                            id: event.id,
                            event_id: event.event_id,
                            role,
                            run_id: event.run_id.filter(|run| !run.trim().is_empty()),
                            matches,
                            store_rank: 0,
                        });
                    }
                }
                "context.switch.created" => {
                    let (lookup_event_ids, parent_handoff_event_id) = switch_note_references(&event.content);
                    self.switches.push(PoolSwitchRow {
                        id: event.id,
                        matches: forms.matches(&event.role, &event.content),
                        event_id: event.event_id,
                        lookup_event_ids,
                        parent_handoff_event_id,
                    });
                }
                _ => {}
            }
        }
    }

    /// Sort the rows by id and rank every matching row in store order.
    fn finish(&mut self) {
        self.message_ids.sort_unstable();
        self.message_ids.dedup();
        self.messages.sort_by_key(|row| row.id);
        self.messages.dedup_by_key(|row| row.id);
        for row in &mut self.messages {
            row.store_rank = self
                .message_ids
                .binary_search(&row.id)
                .unwrap_or_else(|insertion| insertion);
        }
    }
}

/// Whether a paging `error` leaves the pool unproven rather than failing the
/// read: the fail-fast error of a backend without the method, or a page the
/// backend could not return whole ([`crate::memory::traits::SessionPageIncomplete`]).
fn is_unproven_page_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::memory::erasure::Unsupported>()
        .is_some_and(|unsupported| unsupported.0 == "fabric operation")
        || error
            .downcast_ref::<crate::memory::traits::SessionPageIncomplete>()
            .is_some()
}

/// Every own user/assistant row of the session (§3.1): the newest page, then
/// older pages until a short page proves the session's first row was read.
/// Every page, the newest included, is read through
/// `load_session_context_before`, which reports rows it would have to drop.
/// `Ok(Err(TurnAmbiguous))` when completeness cannot be proven: the rows read
/// reached [`POOL_MAX_ROWS`], the backend cannot page, or a page is incomplete.
async fn load_complete_pool(
    mem: &dyn Memory,
    principal: &MemoryPrincipal,
    forms: &SourceForms,
) -> Result<Result<CandidatePool, ProvenanceGap>> {
    load_pool_within(mem, principal, forms, POOL_MAX_ROWS).await
}

async fn load_pool_within(
    mem: &dyn Memory,
    principal: &MemoryPrincipal,
    forms: &SourceForms,
    max_rows: usize,
) -> Result<Result<CandidatePool, ProvenanceGap>> {
    let query = || SessionContextQuery {
        principal: principal.clone(),
        since_event_id: None,
        limit: POOL_PAGE_ROWS,
        include_roles: vec!["user".to_string(), "assistant".to_string()],
    };
    let mut pool = CandidatePool::default();
    let mut rows_read = 0usize;
    let mut oldest: Option<i64> = None;
    // The newest page is "below" the largest possible id.
    let mut page = match mem.load_session_context_before(query(), i64::MAX).await {
        Ok(page) => page,
        Err(error) if is_unproven_page_error(&error) => return Ok(Err(ProvenanceGap::TurnAmbiguous)),
        Err(error) => return Err(error),
    };
    loop {
        let page_len = page.len();
        rows_read = rows_read.saturating_add(page_len);
        if let Some(page_oldest) = page.iter().map(|event| event.id).min() {
            oldest = Some(oldest.map_or(page_oldest, |seen| seen.min(page_oldest)));
        }
        pool.absorb(page, principal.session_key.as_deref(), forms);
        if page_len < POOL_PAGE_ROWS {
            break;
        }
        if rows_read >= max_rows {
            return Ok(Err(ProvenanceGap::TurnAmbiguous));
        }
        let Some(before_id) = oldest else {
            return Ok(Err(ProvenanceGap::TurnAmbiguous));
        };
        page = match mem.load_session_context_before(query(), before_id).await {
            Ok(page) => page,
            Err(error) if is_unproven_page_error(&error) => return Ok(Err(ProvenanceGap::TurnAmbiguous)),
            Err(error) => return Err(error),
        };
    }
    pool.finish();
    Ok(Ok(pool))
}

/// The group structure of a segment: for each message its role, the user that
/// opened its turn group and the nearest earlier assistant.
struct SegmentShape {
    roles: Vec<PoolRole>,
    group_user: Vec<Option<usize>>,
    previous_assistant: Vec<Option<usize>>,
}

impl SegmentShape {
    fn new(segment: &[&ChatMessage]) -> Option<Self> {
        let mut shape = Self {
            roles: Vec::with_capacity(segment.len()),
            group_user: Vec::with_capacity(segment.len()),
            previous_assistant: Vec::with_capacity(segment.len()),
        };
        let mut current_user = None;
        let mut last_assistant = None;
        for (index, message) in segment.iter().enumerate() {
            let role = PoolRole::of(&message.role)?;
            match role {
                PoolRole::User => {
                    current_user = Some(index);
                    shape.group_user.push(None);
                    shape.previous_assistant.push(None);
                }
                PoolRole::Assistant => {
                    shape.group_user.push(current_user);
                    shape.previous_assistant.push(last_assistant);
                    last_assistant = Some(index);
                }
            }
            shape.roles.push(role);
        }
        Some(shape)
    }

    /// Whether `row` may stand for message `index` given the rows `chosen`
    /// for the messages before it (A1, A3, A4, A5; A2 holds by construction).
    fn admits(&self, index: usize, row_index: usize, pool: &[PoolRow], chosen: &[usize], taken: &[bool]) -> bool {
        let Some(row) = pool.get(row_index) else {
            return false;
        };
        // (A1) one-to-one.
        if taken.get(row_index).copied().unwrap_or(true) {
            return false;
        }
        if self.roles.get(index) != Some(&PoolRole::Assistant) {
            return true;
        }
        let chosen_row = |message: usize| chosen.get(message).and_then(|&chosen_index| pool.get(chosen_index));
        // (A3) commit order among assistant rows.
        if let Some(previous) = self.previous_assistant.get(index).copied().flatten()
            && chosen_row(previous).is_none_or(|previous_row| previous_row.id >= row.id)
        {
            return false;
        }
        if let Some(user) = self.group_user.get(index).copied().flatten() {
            let Some(user_row) = chosen_row(user) else {
                return false;
            };
            // (A4) request before reply.
            if user_row.id >= row.id {
                return false;
            }
            // (A5) run consistency where both rows carry a run.
            if let (Some(user_run), Some(reply_run)) = (user_row.run_id.as_deref(), row.run_id.as_deref())
                && user_run != reply_run
            {
                return false;
            }
        }
        true
    }
}

/// The contiguous store window for the segment (today's rule), as one pool
/// index per message: a search seed only, never a justification.
fn contiguous_window(candidates: &[Vec<usize>], pool: &[PoolRow]) -> Option<Vec<usize>> {
    let first = candidates.first()?;
    'start: for &start in first {
        let start_rank = pool.get(start)?.store_rank;
        let mut window = Vec::with_capacity(candidates.len());
        window.push(start);
        for (offset, list) in candidates.iter().enumerate().skip(1) {
            let rank = start_rank.saturating_add(offset);
            let found = list.binary_search_by(|&row_index| {
                pool.get(row_index)
                    .map_or(std::cmp::Ordering::Less, |row| row.store_rank.cmp(&rank))
            });
            let Ok(position) = found else {
                continue 'start;
            };
            window.push(*list.get(position)?);
        }
        return Some(window);
    }
    None
}

/// The unique admissible rows for `segment` (durable messages between
/// handoffs, in history order; `first_index` is the index of its first message
/// in the durable source). `Err` names why uniqueness is not proven.
fn match_segment<'e>(
    segment: &[&ChatMessage],
    first_index: usize,
    pool: &'e [PoolRow],
    used_event_ids: &HashSet<String>,
) -> Result<Vec<&'e PoolRow>, ProvenanceGap> {
    match_segment_within(segment, first_index, pool, used_event_ids, TURN_MATCH_STEP_BUDGET)
}

/// [`match_segment`] with an explicit step `budget`: counts admissible
/// assignments by an iterative depth-first search, capped at 2.
fn match_segment_within<'e>(
    segment: &[&ChatMessage],
    first_index: usize,
    pool: &'e [PoolRow],
    used_event_ids: &HashSet<String>,
    budget: usize,
) -> Result<Vec<&'e PoolRow>, ProvenanceGap> {
    let message_count = segment.len();
    if message_count > crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS {
        return Err(ProvenanceGap::TurnUnmatched);
    }
    if message_count == 0 {
        return Ok(Vec::new());
    }
    let shape = SegmentShape::new(segment).ok_or(ProvenanceGap::TurnUnmatched)?;
    // (A2) candidates per message, ascending id (the pool is sorted by id).
    let mut candidates = vec![Vec::new(); message_count];
    for (row_index, row) in pool.iter().enumerate() {
        if used_event_ids.contains(&row.event_id) {
            continue;
        }
        for &message in &row.matches {
            let Some(local) = message.checked_sub(first_index) else {
                continue;
            };
            if let (Some(list), Some(role)) = (candidates.get_mut(local), shape.roles.get(local))
                && *role == row.role
            {
                list.push(row_index);
            }
        }
    }
    // Contiguous first: the window's row leads each candidate list.
    if let Some(window) = contiguous_window(&candidates, pool) {
        for (list, seed) in candidates.iter_mut().zip(window) {
            list.retain(|&row_index| row_index != seed);
            list.insert(0, seed);
        }
    }

    let mut next = vec![0usize; message_count];
    let mut chosen: Vec<usize> = Vec::with_capacity(message_count);
    let mut taken = vec![false; pool.len()];
    let mut steps = 0usize;
    let mut found: Option<Vec<usize>> = None;
    loop {
        let depth = chosen.len();
        let (Some(list), Some(position)) = (candidates.get(depth), next.get(depth).copied()) else {
            return Err(ProvenanceGap::TurnUnmatched);
        };
        if let Some(&row_index) = list.get(position) {
            if let Some(slot) = next.get_mut(depth) {
                *slot = position.saturating_add(1);
            }
            steps = steps.saturating_add(1);
            if steps > budget {
                return Err(ProvenanceGap::TurnAmbiguous);
            }
            if !shape.admits(depth, row_index, pool, &chosen, &taken) {
                continue;
            }
            if depth.saturating_add(1) == message_count {
                if found.is_some() {
                    return Err(ProvenanceGap::TurnAmbiguous);
                }
                let mut assignment = chosen.clone();
                assignment.push(row_index);
                found = Some(assignment);
                continue;
            }
            if let Some(flag) = taken.get_mut(row_index) {
                *flag = true;
            }
            chosen.push(row_index);
            if let Some(slot) = next.get_mut(depth.saturating_add(1)) {
                *slot = 0;
            }
            continue;
        }
        // Every candidate of this message was tried: backtrack.
        let Some(previous) = chosen.pop() else {
            break;
        };
        if let Some(flag) = taken.get_mut(previous) {
            *flag = false;
        }
    }
    let assignment = found.ok_or(ProvenanceGap::TurnUnmatched)?;
    let rows = assignment
        .iter()
        .filter_map(|&row_index| pool.get(row_index))
        .collect::<Vec<_>>();
    if rows.len() == message_count {
        Ok(rows)
    } else {
        Err(ProvenanceGap::TurnUnmatched)
    }
}

/// The rows referenced by the adopted handoff chain: each matched switch row's
/// note, followed through `parent_handoff_event_id` to its ancestors in the
/// pool. A missing link claims nothing more.
fn claimed_event_ids(pool: &CandidatePool, adopted: &[&PoolSwitchRow]) -> HashSet<String> {
    let mut claimed = HashSet::new();
    let mut visited: HashSet<&str> = HashSet::new();
    for switch in adopted {
        let mut current = Some(*switch);
        while let Some(row) = current {
            if !visited.insert(row.event_id.as_str()) {
                break;
            }
            claimed.extend(row.lookup_event_ids.iter().cloned());
            current = row
                .parent_handoff_event_id
                .as_deref()
                .and_then(|parent| pool.switches.iter().find(|candidate| candidate.event_id == parent));
        }
    }
    claimed
}

/// The `(row id, event id)` of every durable message, in history order.
fn resolve_in_pool(messages: &[&ChatMessage], mut pool: CandidatePool) -> Result<Vec<(i64, String)>, ProvenanceGap> {
    // Handoffs first: each matches exactly one unused switch row.
    let mut used_event_ids: HashSet<String> = HashSet::new();
    let mut handoff_rows: Vec<Option<usize>> = vec![None; messages.len()];
    for (index, message) in messages.iter().enumerate() {
        if !is_context_handoff_message(message) {
            continue;
        }
        let candidates = pool
            .switches
            .iter()
            .enumerate()
            .filter(|(_, row)| row.matches.contains(&index) && !used_event_ids.contains(&row.event_id))
            .map(|(switch_index, _)| switch_index)
            .collect::<Vec<_>>();
        let [switch_index] = candidates.as_slice() else {
            return Err(ProvenanceGap::HandoffRowUnmatched);
        };
        let row = pool.switches.get(*switch_index).ok_or(ProvenanceGap::TurnUnmatched)?;
        used_event_ids.insert(row.event_id.clone());
        let slot = handoff_rows.get_mut(index).ok_or(ProvenanceGap::TurnUnmatched)?;
        *slot = Some(*switch_index);
    }
    let adopted = handoff_rows
        .iter()
        .flatten()
        .filter_map(|&switch_index| pool.switches.get(switch_index))
        .collect::<Vec<_>>();
    let claimed = claimed_event_ids(&pool, &adopted);
    pool.messages.retain(|row| !claimed.contains(&row.event_id));

    let mut matched = Vec::new();
    let mut source_index = 0usize;
    while source_index < messages.len() {
        if let Some(switch_index) = handoff_rows.get(source_index).copied().flatten() {
            let row = pool.switches.get(switch_index).ok_or(ProvenanceGap::TurnUnmatched)?;
            matched.push((row.id, row.event_id.clone()));
            source_index = source_index.saturating_add(1);
            continue;
        }
        let remaining = messages.get(source_index..).ok_or(ProvenanceGap::TurnUnmatched)?;
        let segment_end = remaining
            .iter()
            .position(|message| is_context_handoff_message(message))
            .map_or(messages.len(), |offset| source_index.saturating_add(offset));
        let segment = messages
            .get(source_index..segment_end)
            .ok_or(ProvenanceGap::TurnUnmatched)?;
        let rows = match_segment(segment, source_index, &pool.messages, &used_event_ids)?;
        for row in rows {
            used_event_ids.insert(row.event_id.clone());
            matched.push((row.id, row.event_id.clone()));
        }
        source_index = segment_end;
    }
    Ok(matched)
}

pub(crate) async fn resolve_compaction_event_provenance(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
) -> anyhow::Result<Option<CompactionEventProvenance>> {
    Ok(
        match resolve_compaction_event_provenance_detailed(mem, principal, source_messages, companions).await? {
            ProvenanceResolution::Exact(provenance) => Some(provenance),
            ProvenanceResolution::Unavailable(_) => None,
        },
    )
}

/// Resolve `source_messages` to the session's own transcript rows (§3): exact
/// only when every durable message has exactly one admissible row within a
/// pool proven complete; otherwise the precise gap.
pub(crate) async fn resolve_compaction_event_provenance_detailed(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
) -> anyhow::Result<ProvenanceResolution> {
    if principal.session_key.is_none() || source_messages.is_empty() {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::NoSessionKey));
    }

    // Tool round-trip messages are never persisted verbatim, so they cannot be
    // matched against transcript events. Fold them into the assistant turn that
    // produced them and resolve provenance over the durable remainder.
    let companion_message_count = (0..source_messages.len())
        .filter(|index| companions.is_companion(*index))
        .count();
    let durable_messages = source_messages
        .iter()
        .enumerate()
        .filter(|(index, _)| !companions.is_companion(*index))
        .map(|(_, message)| message)
        .collect::<Vec<_>>();
    if durable_messages.is_empty()
        || durable_messages
            .iter()
            .any(|message| !matches!(message.role.as_str(), "user" | "assistant"))
    {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::NonTranscriptSource));
    }

    // Only the session's own rows (canonical key): rows under the legacy key
    // (child or worker traffic) never take part.
    let principal = MemoryPrincipal {
        legacy_session_key: None,
        ..principal
    };
    let forms = SourceForms::new(&durable_messages);
    let pool = match load_complete_pool(mem, &principal, &forms).await? {
        Ok(pool) => pool,
        Err(gap) => return Ok(ProvenanceResolution::Unavailable(gap)),
    };
    let matched = match resolve_in_pool(&durable_messages, pool) {
        Ok(matched) => matched,
        Err(gap) => return Ok(ProvenanceResolution::Unavailable(gap)),
    };

    let Some((first_row_id, first_event_id)) = matched.iter().min_by_key(|(id, _)| *id) else {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnUnmatched));
    };
    let Some((last_row_id, last_event_id)) = matched.iter().max_by_key(|(id, _)| *id) else {
        return Ok(ProvenanceResolution::Unavailable(ProvenanceGap::TurnUnmatched));
    };
    let covered_range = CompactionSourceEventRange {
        first_event_id: first_event_id.clone(),
        last_event_id: last_event_id.clone(),
        first_row_id: *first_row_id,
        last_row_id: *last_row_id,
        source_event_count: matched.len(),
    };
    let source_event_ids = matched.into_iter().map(|(_, event_id)| event_id).collect::<Vec<_>>();
    Ok(ProvenanceResolution::Exact(CompactionEventProvenance {
        source_event_ids,
        covered_range,
        companion_message_count,
    }))
}

async fn attach_compaction_event_provenance(
    audit: &DocumentIngestRuntime,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
    source: &mut CompactionAuditSource,
) {
    match resolve_compaction_event_provenance_detailed(
        &*audit.memory,
        audit.memory_principal(),
        source_messages,
        companions,
    )
    .await
    {
        Ok(ProvenanceResolution::Exact(provenance)) => {
            source.source_event_ids = provenance.source_event_ids;
            source.source_event_range = Some(provenance.covered_range);
            source.unpersisted_companion_count = provenance.companion_message_count;
            source.source_event_provenance_status = if provenance.companion_message_count == 0 {
                "exact".to_string()
            } else {
                COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS.to_string()
            };
        }
        Ok(ProvenanceResolution::Unavailable(gap)) => {
            source.source_event_provenance_status = gap.status().to_string();
        }
        Err(error) => {
            source.source_event_provenance_status = "read_error".to_string();
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::debug!(
                error_len,
                error_sha256,
                "failed to resolve compaction MessageEvent provenance"
            );
        }
    }
}

/// The recorded provenance status as a static label: a known status, or
/// `"unavailable"` for anything else.
fn provenance_status_label(status: &str) -> &'static str {
    match status {
        "exact" => "exact",
        COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS => COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS,
        "read_error" => "read_error",
        other => ProvenanceGap::ALL
            .into_iter()
            .map(ProvenanceGap::status)
            .find(|gap_status| *gap_status == other)
            .unwrap_or("unavailable"),
    }
}

/// Why the switch filter refused `source`: its provenance status when that is
/// not exact, else the bound that failed.
fn switch_refusal_reason(source: &CompactionAuditSource) -> &'static str {
    let status = provenance_status_label(&source.source_event_provenance_status);
    if !matches!(status, "exact" | COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS) {
        return status;
    }
    if source.source_event_ids.len() != source.message_count.saturating_sub(source.unpersisted_companion_count) {
        return "bound:count_mismatch";
    }
    if source.source_event_ids.len() > crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS {
        return "bound:too_many_events";
    }
    "bound:row_span"
}

async fn persist_context_switch(
    audit: &DocumentIngestRuntime,
    trigger: &'static str,
    source: &CompactionAuditSource,
    handoff_event_id: &str,
    handoff_message: &str,
    note_json: &str,
) -> Result<()> {
    let Some(range) = source.source_event_range.as_ref() else {
        return Ok(());
    };
    audit
        .memory
        .append_message_event(crate::memory::MessageEventInput {
            agent_partition: audit.agent_partition.clone(),
            event_id: Some(handoff_event_id.to_string()),
            idempotency_key: Some(format!("context-switch:{handoff_event_id}")),
            workspace_id: audit.workspace_id.clone(),
            owner_id: audit.owner_id.clone(),
            source: "context_switch".into(),
            channel: audit.channel.clone(),
            session_key: audit.session_key.clone(),
            parent_session_key: audit.legacy_session_key.clone(),
            run_id: None,
            parent_run_id: None,
            agent_id: audit.agent_id.clone(),
            persona_id: audit.persona_id.clone(),
            sender: Some("context_switch".to_string()),
            recipient: audit.sender.clone(),
            role: "assistant".to_string(),
            event_type: "context.switch.created".to_string(),
            subject: audit
                .session_key
                .as_ref()
                .map(|key| crate::memory::MessageEventSubject::Conversation(key.clone())),
            goal_id: None,
            causation_event_id: Some(range.last_event_id.clone()),
            correlation_id: audit.source_message_event_id.clone(),
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: audit.config_generation_id,
            config_source_revision: audit.config_source_revision.clone(),
            content: handoff_message.to_string(),
            raw_payload_json: Some(
                serde_json::json!({
                    "trigger": trigger,
                    "handoff_note": serde_json::from_str::<serde_json::Value>(note_json).unwrap_or_default()
                })
                .to_string(),
            ),
            visibility: audit.visibility.clone(),
        })
        .await?;
    Ok(())
}

/// Record that a `switch` rollover could not stay lossless and the turn
/// continued on a token-aware trim.
///
/// A hard switch that cannot resolve exact provenance used to abort the whole
/// turn with a non-retryable error, which made every session containing tool
/// calls permanently unusable once it crossed the hard limit. Degrading is
/// lossy, so it is recorded durably next to the compaction audit trail instead
/// of only being logged.
pub(crate) async fn persist_context_switch_degradation(
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
    reason: &'static str,
    before_tokens: usize,
    after_tokens: usize,
    hard_limit: usize,
) {
    let Some(audit) = audit else {
        return;
    };
    let detail = serde_json::json!({
        "trigger": trigger,
        "reason": reason,
        "before_used_tokens": before_tokens,
        "after_used_tokens": after_tokens,
        "hard_limit_tokens": hard_limit,
        "fallback": "token_aware_trim",
    });
    // One event per turn. The key is derived from the turn rather than from the
    // event id it is attached to: a fresh `Uuid` in both places made every write
    // a guaranteed insert and left the idempotency key unable to reject
    // anything, including a retry of this very call from a second runtime built
    // for the same turn.
    if audit.degradation_recorded.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::debug!(
            trigger,
            reason,
            "context switch degradation already recorded for this turn"
        );
        return;
    }
    let degraded_event_id = Uuid::new_v4().to_string();
    let idempotency_key = format!("context-compaction-degraded:{}", audit.turn_key);
    if let Err(error) = audit
        .memory
        .append_message_event(crate::memory::MessageEventInput {
            agent_partition: audit.agent_partition.clone(),
            event_id: Some(degraded_event_id.clone()),
            idempotency_key: Some(idempotency_key),
            workspace_id: audit.workspace_id.clone(),
            owner_id: audit.owner_id.clone(),
            source: "context_switch".into(),
            channel: audit.channel.clone(),
            session_key: audit.session_key.clone(),
            parent_session_key: audit.legacy_session_key.clone(),
            run_id: None,
            parent_run_id: None,
            agent_id: audit.agent_id.clone(),
            persona_id: audit.persona_id.clone(),
            sender: Some("context_switch".to_string()),
            recipient: audit.sender.clone(),
            role: "event".to_string(),
            event_type: CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE.to_string(),
            subject: audit
                .session_key
                .as_ref()
                .map(|key| crate::memory::MessageEventSubject::Conversation(key.clone())),
            goal_id: None,
            causation_event_id: None,
            correlation_id: audit.source_message_event_id.clone(),
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: audit.config_generation_id,
            config_source_revision: audit.config_source_revision.clone(),
            content: format!("context switch degraded to lossy trim ({reason})"),
            raw_payload_json: Some(detail.to_string()),
            visibility: audit.visibility.clone(),
        })
        .await
    {
        // Deliberately *not* claimed: the turn is marked as recorded only once
        // a write has actually landed, so a backend that was briefly
        // unavailable costs a retry on the next iteration instead of losing the
        // event for the rest of the turn. Duplicate writes are still impossible
        // — the backend is unique on `(workspace_id, idempotency_key)` and the
        // key is derived from the turn.
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::debug!(
            error_len,
            error_sha256,
            "failed to append context switch degradation event"
        );
    } else {
        audit
            .degradation_recorded
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn persist_compaction_audit(
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
    mode: &crate::config::AgentCompactionMode,
    source: Option<&CompactionAuditSource>,
    summary: &str,
    fidelity_status: &str,
) {
    let Some(audit) = audit else {
        return;
    };
    let empty_source = CompactionAuditSource::default();
    let source = source.unwrap_or(&empty_source);
    let run_id = Uuid::new_v4().to_string();
    let summary_memory_key = format!("compaction_summary_{}", run_id.replace('-', "_"));
    let metadata = MemoryStoreMetadata {
        workspace_id: Some(audit.workspace_id.clone()),
        owner_id: audit.owner_id.clone(),
        agent_id: audit.agent_id.clone(),
        persona_id: audit.persona_id.clone(),
        source_event_id: audit.source_message_event_id.clone(),
        source: Some("compaction_summary".to_string()),
        topic_id: None,
        channel: audit.channel.clone(),
    };
    // NOTE: `channel` is sourced from DocumentIngestRuntime so compaction
    // summaries are recall-scoped to the originating channel (FIX-P1-08).
    // Review M7: carry the turn partition so Named summary memories are stamped.
    let write_ctx = MemoryWriteContext {
        workspace_id: Some(audit.workspace_id.clone()),
        owner_id: audit.owner_id.clone(),
        session_key: audit.session_key.clone(),
        agent_partition: audit.agent_partition.clone(),
        channel: audit.channel.clone(),
        ..MemoryWriteContext::default()
    };
    if let Err(error) = audit
        .memory
        .store_with_context_and_metadata(
            &summary_memory_key,
            summary,
            MemoryCategory::Conversation,
            audit.session_key.as_deref(),
            Some(&write_ctx),
            metadata,
        )
        .await
    {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::debug!(error_len, error_sha256, "failed to persist compaction summary memory");
    }
    let summary_message_event_id = if let Some(range) = source.source_event_range.as_ref() {
        let raw_payload_json = serde_json::json!({
            "compaction_run_id": run_id,
            "trigger": trigger,
            "mode": format!("{mode:?}"),
            "fidelity_status": fidelity_status,
            "source_event_ids": source.source_event_ids,
            "covered_event_range": range
        })
        .to_string();
        match audit
            .memory
            .append_message_event(crate::memory::MessageEventInput {
                agent_partition: audit.agent_partition.clone(),
                event_id: None,
                idempotency_key: Some(format!("compaction:{run_id}:summary")),
                workspace_id: audit.workspace_id.clone(),
                owner_id: audit.owner_id.clone(),
                source: "compaction".into(),
                channel: audit.channel.clone(),
                session_key: audit.session_key.clone(),
                parent_session_key: None,
                run_id: Some(run_id.clone()),
                parent_run_id: None,
                agent_id: audit.agent_id.clone(),
                persona_id: audit.persona_id.clone(),
                sender: Some("compaction".to_string()),
                recipient: audit.sender.clone(),
                role: "event".to_string(),
                event_type: "compaction.summary.created".to_string(),
                subject: audit
                    .session_key
                    .as_ref()
                    .map(|session_key| crate::memory::MessageEventSubject::Conversation(session_key.clone())),
                goal_id: None,
                causation_event_id: Some(range.last_event_id.clone()),
                correlation_id: Some(run_id.clone()),
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: audit.config_generation_id,
                config_source_revision: audit.config_source_revision.clone(),
                content: crate::chat::sanitize::sanitize_for_persistence(summary),
                raw_payload_json: Some(raw_payload_json),
                visibility: audit.visibility.clone(),
            })
            .await
        {
            Ok(event) => Some(event.event_id),
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::debug!(
                    error_len,
                    error_sha256,
                    "failed to append compaction summary MessageEvent"
                );
                None
            }
        }
    } else {
        None
    };
    if let Err(error) = audit
        .memory
        .append_compaction_run(CompactionRunInput {
            agent_partition: audit.agent_partition.clone(),
            run_id: Some(run_id),
            workspace_id: audit.workspace_id.clone(),
            owner_id: audit.owner_id.clone(),
            session_key: audit.session_key.clone(),
            agent_id: audit.agent_id.clone(),
            persona_id: audit.persona_id.clone(),
            trigger: trigger.to_string(),
            mode: format!("{mode:?}"),
            source_message_count: source.message_count,
            source_token_estimate: source.token_estimate,
            summary: summary.to_string(),
            summary_memory_key: Some(summary_memory_key),
            source_event_ids_json: if source.source_event_ids.is_empty() {
                None
            } else {
                serde_json::to_string(&source.source_event_ids).ok()
            },
            source_event_range_json: source
                .source_event_range
                .as_ref()
                .and_then(|range| serde_json::to_string(range).ok()),
            source_document_refs_json: Some(
                serde_json::to_string(&source.document_refs).unwrap_or_else(|_| "[]".to_string()),
            ),
            fidelity_status: fidelity_status.to_string(),
            payload_json: Some(
                serde_json::json!({
                    "task_id": audit.task_id,
                    "topic_id": audit.topic_id,
                    "source_message_event_id": audit.source_message_event_id,
                    "source_event_provenance_status": source.source_event_provenance_status,
                    "summary_message_event_id": summary_message_event_id
                })
                .to_string(),
            ),
        })
        .await
    {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::debug!(error_len, error_sha256, "failed to append compaction run audit");
    }
}

fn compaction_trigger_limit(config: &crate::config::AgentCompactionConfig) -> Option<usize> {
    if config.mode == crate::config::AgentCompactionMode::Off {
        return None;
    }
    let max_tokens = config.max_context_tokens;
    let reserve = config.reserve_tokens;
    if max_tokens <= reserve {
        return Some(0);
    }
    Some(max_tokens - reserve)
}

/// Trigger name of an explicit `/compact` request from a chat front end.
pub(crate) const MANUAL_COMPACT_TRIGGER: &str = "chat_manual_compact";

/// Why a compaction runs, which decides whether the configured hard input
/// limit gates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompactionDemand {
    /// The history crossed the configured hard input limit (preflight).
    Threshold,
    /// The provider reported a context overflow. Authoritative even when the
    /// cl100k proxy says the request fits, so tokenizer differences can be
    /// repaired.
    ProviderOverflow,
    /// The user asked for `/compact`: every compactable message beyond
    /// `keep_recent_messages` is compacted, whatever the budget says.
    Manual,
}

impl CompactionDemand {
    fn from_trigger(trigger: &'static str) -> Self {
        if trigger == MANUAL_COMPACT_TRIGGER {
            Self::Manual
        } else if trigger.contains("overflow") {
            Self::ProviderOverflow
        } else {
            Self::Threshold
        }
    }

    const fn is_forced(self) -> bool {
        matches!(self, Self::ProviderOverflow | Self::Manual)
    }
}

/// What the builder decided for one compaction demand.
pub(crate) enum CompactionBuild {
    /// No patch: nothing to compact for this demand, or no exact handoff could
    /// be produced (the caller degrades as it always did).
    NotNeeded,
    /// A patch that replaces the cold window.
    Patch(CompactionPatch),
    /// An automatic `switch` this admitted turn already spent: one succeeded
    /// earlier in the turn. Nothing is built, and nothing is
    /// reported as a degradation; the turn fits its request instead.
    Suppressed,
}

#[cfg(test)]
pub(crate) async fn build_configurable_compaction_patch(
    history: &[ChatMessage],
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<Option<CompactionPatch>> {
    let source = crate::agent::turn_history::CanonicalHistory::from_test_messages(history.to_vec());
    match build_configurable_compaction_patch_with_source_history(
        history, &source, provider, model, config, audit, trigger,
    )
    .await?
    {
        CompactionBuild::Patch(patch) => Ok(Some(patch)),
        CompactionBuild::NotNeeded => Ok(None),
        CompactionBuild::Suppressed => anyhow::bail!("context switch suppressed: the turn spent its switch budget"),
    }
}

pub(crate) async fn build_configurable_compaction_patch_with_source_history(
    budget_history: &[ChatMessage],
    source: &crate::agent::turn_history::CanonicalHistory,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<CompactionBuild> {
    let Some(limit) = compaction_trigger_limit(config) else {
        return Ok(CompactionBuild::NotNeeded);
    };
    if !CompactionDemand::from_trigger(trigger).is_forced() && estimate_history_tokens(budget_history) <= limit {
        return Ok(CompactionBuild::NotNeeded);
    }
    // One successful automatic switch per admitted turn.
    // Manual `/compact` is outside the budget and always builds.
    let automatic_switch = trigger != MANUAL_COMPACT_TRIGGER;
    if automatic_switch
        && matches!(config.mode, crate::config::AgentCompactionMode::Switch)
        && audit.is_some_and(DocumentIngestRuntime::automatic_switch_spent)
    {
        tracing::debug!(
            trigger,
            "context switch suppressed: this turn has already spent its switch budget"
        );
        return Ok(CompactionBuild::Suppressed);
    }

    let source_history = source.messages();
    // This turn's tool round traffic, by provenance (an acknowledged turn is
    // never folded, whatever its text).
    let companions = source.tool_round_companions();
    let has_system = source_history.first().is_some_and(|m| m.role == "system");
    let start = if has_system { 1 } else { 0 };
    let non_system_count = source_history.len().saturating_sub(start);
    if non_system_count <= 1 {
        return Ok(CompactionBuild::NotNeeded);
    }

    let requested_keep_recent = config.keep_recent_messages.max(1).min(non_system_count);
    let requested_compact_count = non_system_count.saturating_sub(requested_keep_recent);
    if requested_compact_count == 0 {
        return Ok(CompactionBuild::NotNeeded);
    }
    // Never cut inside a tool round-trip: an orphan `role="tool"` result whose
    // requesting assistant message was compacted away is rejected by every
    // native provider.
    let compact_end =
        compaction_boundary_without_orphan_tool_results(&companions, start, start + requested_compact_count);
    let compact_count = compact_end.saturating_sub(start);
    if compact_count == 0 {
        return Ok(CompactionBuild::NotNeeded);
    }
    let keep_recent = source_history.len().saturating_sub(compact_end);
    let compacted_source_messages = source_history.get(start..compact_end);
    let compacted_companions = RoundCompanions::Marked(companions.get(start..compact_end).unwrap_or_default());
    let mut audit_source = audit.and_then(|_| compacted_source_messages.map(build_compaction_audit_source));
    if let (Some(audit), Some(source_messages), Some(source)) =
        (audit, compacted_source_messages, audit_source.as_mut())
    {
        attach_compaction_event_provenance(audit, source_messages, compacted_companions, source).await;
    }
    let Some(guard) = compaction_patch_guard_for(source_history, start, compact_end) else {
        return Ok(CompactionBuild::NotNeeded);
    };
    let timestamp = chrono::Utc::now().to_rfc3339();

    if matches!(config.mode, crate::config::AgentCompactionMode::Switch) {
        let Some(audit) = audit else {
            tracing::warn!(
                trigger,
                "context switch skipped because durable transcript scope is unavailable"
            );
            return Ok(CompactionBuild::NotNeeded);
        };
        let Some(source) = audit_source.as_ref().filter(|source| {
            matches!(
                source.source_event_provenance_status.as_str(),
                "exact" | COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS
            ) && source.source_event_ids.len()
                == source.message_count.saturating_sub(source.unpersisted_companion_count)
                && source.source_event_ids.len() <= crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS
                && source.source_event_range.is_some()
                && source.source_event_range.as_ref().is_some_and(|range| {
                    range.last_row_id.saturating_sub(range.first_row_id)
                        <= crate::tools::transcript_history_lookup::MAX_ROW_SPAN
                })
        }) else {
            let status: &'static str = audit_source.as_ref().map_or("unavailable", switch_refusal_reason);
            tracing::warn!(
                trigger,
                provenance_status = %status,
                "context switch skipped because exact transcript provenance is unavailable"
            );
            return Ok(CompactionBuild::NotNeeded);
        };
        let Some(range) = source.source_event_range.as_ref() else {
            return Ok(CompactionBuild::NotNeeded);
        };
        let (from_generation, parent_handoff_event_id) = context_handoff_lineage(source_history);
        let to_generation = from_generation.saturating_add(1);
        let handoff_event_id = Uuid::new_v4().to_string();
        // The handoff replaces the cold window with event-id pointers, and tool
        // round-trips have no events of their own. Without this line the note
        // reads as an exact transfer while the tool output — usually the
        // substance of an agentic window — is gone with nothing naming it.
        let folded = folded_tool_roundtrips(compacted_source_messages.unwrap_or_default(), compacted_companions);
        let folded_notice = format_folded_tool_roundtrips_notice(&folded);
        let note = serde_json::json!({
            "schema_version": 1,
            "kind": "context_handoff",
            "strategy": "hard_switch",
            "handoff_event_id": handoff_event_id,
            "parent_handoff_event_id": parent_handoff_event_id,
            "from_generation": from_generation,
            "to_generation": to_generation,
            "created_at": timestamp,
            "trigger": trigger,
            "source_message_count": source.message_count,
            "source_token_estimate": source.token_estimate,
            "unpersisted_companion_count": source.unpersisted_companion_count,
            "folded_tool_roundtrips": {
                "count": folded.count,
                "tools": folded.tools,
            },
            "folded_tool_roundtrips_notice": folded_notice,
            "provenance_status": source.source_event_provenance_status,
            "source_event_range": range,
            "retained_recent_message_count": keep_recent,
            "lookup": {
                "tool": crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME,
                "arguments": {
                    "event_ids": source.source_event_ids,
                    "first_row_id": range.first_row_id,
                    "last_row_id": range.last_row_id
                }
            }
        });
        let note_json = serde_json::to_string(&note)?;
        let handoff_message = format_context_handoff_message(&note_json);
        // Every applied handoff is one a later load can validate, within its
        // size bound; anything else is refused before it is written.
        if validated_context_handoff_note(&handoff_message).is_none() {
            tracing::warn!(
                trigger,
                handoff_bytes = handoff_message.len(),
                limit = MAX_HANDOFF_MESSAGE_BYTES,
                referenced_events = source.source_event_ids.len(),
                "context switch skipped because the handoff note is oversized or does not validate"
            );
            return Ok(CompactionBuild::NotNeeded);
        }
        persist_context_switch(audit, trigger, source, &handoff_event_id, &handoff_message, &note_json).await?;
        if automatic_switch {
            audit.mark_automatic_switch_spent();
        }
        return Ok(CompactionBuild::Patch(CompactionPatch {
            replacement: vec![ChatMessage::assistant(handoff_message)],
            append_after: Vec::new(),
            guard,
        }));
    }

    let source_projection = source_history
        .get(start..compact_end)
        .map(bounded_compaction_projection)
        .unwrap_or_default();
    if source_projection.is_empty() {
        return Ok(CompactionBuild::NotNeeded);
    }

    // FU-02c-AUX-SENDS: compaction summarizer is exempt from admit_provider_send.
    // (a) tools is None (`chat_with_system`). (b) input is capped by
    // COMPACTION_MAX_SOURCE_CHARS and stored output by COMPACTION_MAX_SUMMARY_CHARS
    // / MEMORY_FLUSH_MAX_CHARS. (c) `compaction_aux_send_exemption_caps_input_and_output`
    // pins both literals. (d) the follow-up stays open for spend settlement.
    let mut replacement = Vec::new();
    if config.memory_flush {
        let flush_prompt = format!(
            "Write a concise memory flush message (max 6 bullets) capturing durable facts, decisions, and unresolved tasks from this history:\n\n{}",
            build_compaction_transcript(&source_projection)
        );
        let flush_note = provider
            .chat_with_system(
                Some("You write memory flush notes before context compaction."),
                &flush_prompt,
                model,
                0.1,
            )
            .await
            .unwrap_or_else(|_| "Memory flush fallback: key context retained.".to_string());
        replacement.push(ChatMessage::assistant(format!(
            "[Memory flush at {timestamp}: {}]",
            truncate_with_ellipsis(&flush_note, MEMORY_FLUSH_MAX_CHARS)
        )));
    }

    let summary = match config.mode {
        crate::config::AgentCompactionMode::Safeguard => {
            let transcript = build_compaction_transcript(&source_projection);
            let structured_prompt = format!(
                "Summarize this conversation into a structured context summary. \
                 You MUST include ALL of these sections (use ## headers):\n\
                 ## Decisions\n\
                 ## Open TODOs\n\
                 ## Constraints/Rules\n\
                 ## Pending user asks\n\
                 ## Exact identifiers\n\
                 Preserve all UUIDs, hashes, file paths, URLs, port numbers, and \
                 IP addresses EXACTLY as written. Never paraphrase identifiers.\n\n\
                 ## Progress\n\
                 ### Done\n\
                 ### In Progress\n\
                 ### Blocked\n\n\
                 ## Recent turns preserved verbatim\n\
                 Include the last 2-3 user/assistant exchanges word-for-word.\n\n\
                 ## Critical Context\n\
                 Key technical details that would be lost if forgotten.\n\n\
                 Conversation to summarize:\n{}",
                transcript
            );
            let raw = provider
                .chat_with_system(
                    Some("You are a context compaction summarizer."),
                    &structured_prompt,
                    model,
                    0.2,
                )
                .await
                .unwrap_or_else(|_| "Summary unavailable; compacted conservatively.".to_string());
            // Validate: require at least 3 of the expected ## headers.
            let required_headers = [
                "## Decisions",
                "## Open TODOs",
                "## Constraints",
                "## Pending",
                "## Exact",
                "## Progress",
                "## Critical Context",
            ];
            let found_count = required_headers.iter().filter(|h| raw.contains(*h)).count();
            let validated = if found_count < 3 {
                tracing::warn!(
                    found_count,
                    "Compaction summary missing required sections; retrying with explicit prompt"
                );
                let retry_prompt = format!(
                    "The previous summary was missing required sections. \
                     You MUST produce a summary with these exact ## headers: \
                     ## Decisions, ## Open TODOs, ## Constraints/Rules, ## Pending user asks, \
                     ## Exact identifiers, ## Progress, ## Critical Context. \
                     Do not skip any. Conversation:\n{}",
                    transcript
                );
                provider
                    .chat_with_system(
                        Some("You are a context compaction summarizer. Always include all required ## sections."),
                        &retry_prompt,
                        model,
                        0.1,
                    )
                    .await
                    .unwrap_or(raw)
            } else {
                raw
            };
            truncate_with_ellipsis(&validated, COMPACTION_MAX_SUMMARY_CHARS)
        }
        crate::config::AgentCompactionMode::Aggressive => {
            let mut summary = format!(
                "Aggressive compaction removed {} older messages; retained {} recent messages.",
                compact_count, keep_recent
            );
            if let Some(last_user) = source_projection.iter().rev().find(|m| m.role == "user") {
                summary.push_str(" Last preserved user intent: ");
                summary.push_str(&truncate_with_ellipsis(&last_user.content, 240));
            }
            truncate_with_ellipsis(&summary, COMPACTION_MAX_SUMMARY_CHARS)
        }
        crate::config::AgentCompactionMode::Switch | crate::config::AgentCompactionMode::Off => {
            return Ok(CompactionBuild::NotNeeded);
        }
    };
    // The summarizer reads the very turns that may still carry a fabricated
    // marker and is free to copy one into its output — the "recent turns
    // preserved verbatim" section asks it to. A summary is replayed on every
    // later turn and is persisted as compaction audit, so clean it in both
    // grains before it is measured, stored or inserted.
    let summary = strip_assistant_fabricated_tool_markers(&summary).into_owned();
    let original_source = source_history.get(start..compact_end).unwrap_or(&[]);
    let fidelity_status = compaction_summary_fidelity_status(&summary, config.mode, original_source);
    persist_compaction_audit(
        audit,
        trigger,
        &config.mode,
        audit_source.as_ref(),
        &summary,
        fidelity_status,
    )
    .await;

    replacement.insert(
        0,
        ChatMessage::assistant(format!("[Context compacted at {timestamp}. Summary: {summary}]")),
    );

    // P1-1: Inject post-compaction context refresh so the model knows critical
    // rules may have been summarized and should re-read workspace instructions.
    let append_after = vec![post_compaction_refresh_aside()];

    Ok(CompactionBuild::Patch(CompactionPatch {
        replacement,
        append_after,
        guard,
    }))
}

#[cfg(test)]
pub(super) async fn apply_configurable_compaction_with_replacement_len(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<Option<usize>> {
    let Some(patch) = build_configurable_compaction_patch(history, provider, model, config, audit, trigger).await?
    else {
        return Ok(None);
    };
    // Compacting history is real work done on the turn's behalf, so it counts
    // as progress even though it produces no user-visible output.
    crate::agent::idle::beat(crate::agent::idle::ProgressKind::Compaction);
    let replacement_len = patch.replacement.len();
    apply_compaction_patch_exact(history, &patch)?;
    Ok(Some(replacement_len))
}

/// [`apply_configurable_compaction_with_replacement_len`] over a `source`
/// history whose provider projection is `budget_history`: whether to compact
/// and how much is measured on the projection (what the provider receives,
/// enrichment included), the patch is built from and applied to `source`.
pub(super) async fn apply_configurable_compaction_over_view(
    source: &mut crate::agent::turn_history::CanonicalHistory,
    budget_history: &[ChatMessage],
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<CompactionApply> {
    let patch = match build_configurable_compaction_patch_with_source_history(
        budget_history,
        source,
        provider,
        model,
        config,
        audit,
        trigger,
    )
    .await?
    {
        CompactionBuild::Patch(patch) => patch,
        CompactionBuild::NotNeeded => return Ok(CompactionApply::NotNeeded),
        CompactionBuild::Suppressed => return Ok(CompactionApply::Suppressed),
    };
    crate::agent::idle::beat(crate::agent::idle::ProgressKind::Compaction);
    let replacement_len = patch.replacement.len();
    source.apply_patch_with_refresh_aside(&patch)?;
    Ok(CompactionApply::Applied(replacement_len))
}

/// What [`apply_configurable_compaction_over_view`] did to the canonical history.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CompactionApply {
    /// Nothing applied; see [`CompactionBuild::NotNeeded`].
    NotNeeded,
    /// Applied; the replacement's length in the canonical history.
    Applied(usize),
    /// An automatic switch the turn already spent; nothing was applied.
    Suppressed,
}

#[cfg(test)]
pub(super) async fn apply_configurable_compaction(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<bool> {
    Ok(
        apply_configurable_compaction_with_replacement_len(history, provider, model, config, audit, trigger)
            .await?
            .is_some(),
    )
}

/// The `user` aside summary compaction appends so the model knows critical
/// rules may have been summarized.
pub(super) fn post_compaction_refresh_aside() -> ChatMessage {
    ChatMessage::user(format!(
        "{POST_COMPACTION_REFRESH_MARKER}\n\
         Session was just compacted. Critical rules may have been summarized.\n\
         Re-read workspace instructions if available."
    ))
}

#[path = "turn_tests/compaction.rs"]
mod tests;
