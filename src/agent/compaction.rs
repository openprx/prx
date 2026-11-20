use crate::agent::context_budget::{
    budgeted_tokens, estimate_history_tokens, estimate_history_tokens_with_source,
};
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
use std::collections::HashSet;
use std::fmt::Write;
use uuid::Uuid;

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

const COMPACTION_SOURCE_EVENT_WINDOW: usize = 500;

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

/// Whether a history message is the durable event it was persisted as: the
/// same text, the text redacted (every turn commit since D-054), or the text
/// redacted and capped (chat events written before D-054, which capped them).
fn source_message_matches_event(message: &ChatMessage, event: &MessageEvent) -> bool {
    message.role == event.role
        && (message.content == event.content
            || crate::security::redaction::redact_secrets(&message.content) == event.content
            || crate::chat::sanitize::sanitize_for_persistence(&message.content) == event.content)
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

pub(crate) async fn resolve_compaction_event_provenance(
    mem: &dyn Memory,
    principal: MemoryPrincipal,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
) -> anyhow::Result<Option<CompactionEventProvenance>> {
    if principal.session_key.is_none() || source_messages.is_empty() {
        return Ok(None);
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
        return Ok(None);
    }
    let source_messages = durable_messages.as_slice();

    let mut events = mem
        .load_recent_session_context(SessionContextQuery {
            principal,
            since_event_id: None,
            limit: COMPACTION_SOURCE_EVENT_WINDOW,
            include_roles: vec!["user".to_string(), "assistant".to_string()],
        })
        .await?;
    events.sort_by_key(|event| event.id);
    let message_events = events
        .iter()
        .filter(|event| event.event_type == "message.created")
        .collect::<Vec<_>>();
    let mut matched = Vec::new();
    let mut used_event_ids = HashSet::new();
    let mut source_index = 0usize;
    while source_index < source_messages.len() {
        let Some(message) = source_messages.get(source_index) else {
            return Ok(None);
        };
        if is_context_handoff_message(message) {
            let candidates = events
                .iter()
                .filter(|event| {
                    event.event_type == "context.switch.created"
                        && !used_event_ids.contains(&event.event_id)
                        && source_message_matches_event(message, event)
                })
                .collect::<Vec<_>>();
            let [event] = candidates.as_slice() else {
                return Ok(None);
            };
            used_event_ids.insert(event.event_id.clone());
            matched.push(*event);
            source_index = source_index.saturating_add(1);
            continue;
        }

        let Some(remaining_messages) = source_messages.get(source_index..) else {
            return Ok(None);
        };
        let segment_end = remaining_messages
            .iter()
            .position(|message| is_context_handoff_message(message))
            .map_or(source_messages.len(), |offset| source_index + offset);
        let Some(segment) = source_messages.get(source_index..segment_end) else {
            return Ok(None);
        };
        let matching_windows = message_events
            .windows(segment.len())
            .filter(|window| {
                window.iter().all(|event| !used_event_ids.contains(&event.event_id))
                    && segment
                        .iter()
                        .zip(window.iter())
                        .all(|(segment_message, event)| source_message_matches_event(segment_message, event))
            })
            .collect::<Vec<_>>();
        let [window] = matching_windows.as_slice() else {
            return Ok(None);
        };
        for event in *window {
            used_event_ids.insert(event.event_id.clone());
            matched.push(*event);
        }
        source_index = segment_end;
    }

    let Some(first) = matched.iter().min_by_key(|event| event.id) else {
        return Ok(None);
    };
    let Some(last) = matched.iter().max_by_key(|event| event.id) else {
        return Ok(None);
    };
    let source_event_ids = matched.iter().map(|event| event.event_id.clone()).collect::<Vec<_>>();
    Ok(Some(CompactionEventProvenance {
        covered_range: CompactionSourceEventRange {
            first_event_id: first.event_id.clone(),
            last_event_id: last.event_id.clone(),
            first_row_id: first.id,
            last_row_id: last.id,
            source_event_count: source_event_ids.len(),
        },
        source_event_ids,
        companion_message_count,
    }))
}

async fn attach_compaction_event_provenance(
    audit: &DocumentIngestRuntime,
    source_messages: &[ChatMessage],
    companions: RoundCompanions<'_>,
    source: &mut CompactionAuditSource,
) {
    match resolve_compaction_event_provenance(&*audit.memory, audit.memory_principal(), source_messages, companions)
        .await
    {
        Ok(Some(provenance)) => {
            source.source_event_ids = provenance.source_event_ids;
            source.source_event_range = Some(provenance.covered_range);
            source.unpersisted_companion_count = provenance.companion_message_count;
            source.source_event_provenance_status = if provenance.companion_message_count == 0 {
                "exact".to_string()
            } else {
                COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS.to_string()
            };
        }
        Ok(None) => {}
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
pub(crate) async fn persist_context_degradation_with_details(
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
    reason: &'static str,
    before_tokens: usize,
    after_tokens: usize,
    hard_limit: usize,
    // Counts of the fit's steps (`folded_rounds`, `removed_messages`), carried
    // into the payload next to the token figures.
    details: &[(&'static str, usize)],
) {
    let Some(audit) = audit else {
        return;
    };
    let mut detail = serde_json::json!({
        "trigger": trigger,
        "reason": reason,
        "before_used_tokens": before_tokens,
        "after_used_tokens": after_tokens,
        "hard_limit_tokens": hard_limit,
        "fallback": "token_aware_trim",
    });
    if let Some(object) = detail.as_object_mut() {
        for (key, value) in details {
            object.insert((*key).to_string(), serde_json::json!(value));
        }
    }
    // One event per (turn, reason) (§3.4). The claim and the durable key are both
    // scoped by reason, so an earlier `handoff_unavailable` never suppresses a
    // later `inflight_tool_rounds_folded`. The key is still derived from the turn,
    // not from a fresh id, so a retry from a second runtime built for the same
    // turn is rejected by the backend.
    if audit.degradation_recorded.lock().contains(&reason) {
        tracing::debug!(
            trigger,
            reason,
            "context compaction degradation already recorded for this turn"
        );
        return;
    }
    let degraded_event_id = Uuid::new_v4().to_string();
    let idempotency_key = format!("context-compaction-degraded:{}:{reason}", audit.turn_key);
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
        audit.degradation_recorded.lock().push(reason);
    }
}

/// [`persist_context_degradation_with_details`] without step counts.
pub(crate) async fn persist_context_switch_degradation(
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
    reason: &'static str,
    before_tokens: usize,
    after_tokens: usize,
    hard_limit: usize,
) {
    persist_context_degradation_with_details(
        audit,
        trigger,
        reason,
        before_tokens,
        after_tokens,
        hard_limit,
        &[],
    )
    .await;
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
    /// A `switch` whose handoff does not reclaim tokens (I1). Nothing was
    /// persisted, and the turn must not try another switch.
    NoProgress { before_tokens: usize, after_tokens: usize },
    /// An automatic `switch` this admitted turn already spent (§3.2): it
    /// succeeded or made no progress earlier in the turn. Nothing is built, and
    /// nothing is reported as a degradation; the turn fits its request instead.
    Suppressed,
}

/// The candidate canonical history a switch leaves: the cold window
/// `start..compact_end` replaced by `handoff`, with the turn's request restored
/// at the tail when the cold window took it, as the caller applies it. Built
/// with `take`/`skip`, so no index can panic.
fn switch_candidate_messages(
    source_history: &[ChatMessage],
    start: usize,
    compact_end: usize,
    handoff: &ChatMessage,
    trigger: &'static str,
) -> Vec<ChatMessage> {
    let mut candidate = Vec::with_capacity(source_history.len());
    candidate.extend(source_history.iter().take(start).cloned());
    candidate.push(handoff.clone());
    candidate.extend(source_history.iter().skip(compact_end).cloned());
    crate::agent::context_preflight::restore_current_turn_request(
        &mut candidate,
        crate::agent::context_preflight::capture_current_turn_request(source_history),
        trigger,
    );
    candidate
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
    match build_configurable_compaction_patch_with_source_history(history, &source, provider, model, config, audit, trigger)
        .await?
    {
        CompactionBuild::Patch(patch) => Ok(Some(patch)),
        CompactionBuild::NotNeeded => Ok(None),
        CompactionBuild::NoProgress {
            before_tokens,
            after_tokens,
        } => anyhow::bail!("context switch made no progress: {before_tokens} -> {after_tokens} tokens"),
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
    // §3.2: one successful automatic switch, or one NoProgress, per admitted
    // turn. Manual `/compact` is outside the budget and always builds.
    let automatic_switch = trigger != MANUAL_COMPACT_TRIGGER;
    if automatic_switch
        && matches!(config.mode, crate::config::AgentCompactionMode::Switch)
        && audit.is_some_and(DocumentIngestRuntime::automatic_switch_spent)
    {
        tracing::debug!(trigger, "context switch suppressed: this turn has already spent its switch budget");
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
            tracing::warn!(
                trigger,
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
        // Progress (I1): a switch must reclaim tokens or not happen. Both sides
        // use the budgeted estimator; the after side is the candidate canonical
        // history with the handoff in place of the cold window and the request
        // restored at the tail, exactly as the caller applies it.
        let handoff = ChatMessage::assistant(handoff_message);
        let before_tokens = budgeted_tokens(estimate_history_tokens_with_source(source_history));
        let after_tokens = budgeted_tokens(estimate_history_tokens_with_source(&switch_candidate_messages(
            source_history,
            start,
            compact_end,
            &handoff,
            trigger,
        )));
        if after_tokens >= before_tokens {
            tracing::warn!(
                trigger,
                before_tokens,
                after_tokens,
                "context switch skipped because the handoff does not reclaim tokens"
            );
            if automatic_switch {
                audit.mark_automatic_switch_spent();
            }
            return Ok(CompactionBuild::NoProgress {
                before_tokens,
                after_tokens,
            });
        }
        persist_context_switch(audit, trigger, source, &handoff_event_id, &handoff.content, &note_json).await?;
        if automatic_switch {
            audit.mark_automatic_switch_spent();
        }
        return Ok(CompactionBuild::Patch(CompactionPatch {
            replacement: vec![handoff],
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
        crate::config::AgentCompactionMode::Switch | crate::config::AgentCompactionMode::Off => return Ok(CompactionBuild::NotNeeded),
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
        CompactionBuild::NoProgress {
            before_tokens,
            after_tokens,
        } => {
            return Ok(CompactionApply::NoProgress {
                before_tokens,
                after_tokens,
            });
        }
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
    /// A switch that did not reclaim tokens; nothing was applied.
    NoProgress { before_tokens: usize, after_tokens: usize },
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
