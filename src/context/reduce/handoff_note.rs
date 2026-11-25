//! The context-handoff note: its wire form, its validation and its rendering
//! for a provider.

use super::provenance::COMPACTION_PROVENANCE_EXACT_WITH_TOOL_ROUNDTRIPS;
use super::roundtrips::{FOLDED_TOOL_NAME_LIMIT, FoldedToolRoundtrips, format_folded_tool_roundtrips_notice};
use crate::providers::ChatMessage;
use std::fmt::Write;

pub(crate) const CONTEXT_HANDOFF_START: &str = "[context_handoff]";

pub(crate) const CONTEXT_HANDOFF_END: &str = "[/context_handoff]";

pub(crate) fn is_context_handoff_message(message: &ChatMessage) -> bool {
    is_context_handoff_turn(&message.role, &message.content)
}

/// Whether a stored turn (`role`, `content`) is a context handoff.
pub(crate) fn is_context_handoff_turn(role: &str, content: &str) -> bool {
    role == "assistant" && content.starts_with(CONTEXT_HANDOFF_START)
}

/// Whether `message` is a context handoff the compaction code wrote, in
/// either form a provider view holds: the stored note, when it validates
/// ([`validated_context_handoff_note`]), or exactly the compact rendering
/// [`render_context_handoff_for_provider`] makes of a validated note (a chat
/// view). Model output that merely looks like either one, and any message
/// that is not an assistant message, is not.
pub(crate) fn is_validated_context_handoff_message(message: &ChatMessage) -> bool {
    is_context_handoff_message(message)
        && (is_compact_context_handoff(&message.content) || validated_context_handoff_note(&message.content).is_some())
}

/// Position of the newest validated context handoff in `messages`: the one
/// handoff a front-first trim of a provider view keeps until nothing else it
/// may remove is left.
#[must_use]
pub(crate) fn newest_validated_context_handoff_index(messages: &[ChatMessage]) -> Option<usize> {
    messages.iter().rposition(is_validated_context_handoff_message)
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
pub(crate) struct HandoffNoteSchema {
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
pub(crate) struct HandoffNoteFolded {
    count: usize,
    tools: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffNoteRange {
    first_event_id: String,
    last_event_id: String,
    first_row_id: i64,
    last_row_id: i64,
    #[serde(rename = "source_event_count")]
    _source_event_count: u64,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffNoteLookup {
    tool: String,
    arguments: HandoffNoteLookupArguments,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffNoteLookupArguments {
    event_ids: Vec<String>,
    first_row_id: i64,
    last_row_id: i64,
}

/// A handoff message in the exact form the switch writes, referencing
/// `event_ids` (stored rows `first_row_id..`), for tests that need a validated
/// handoff in a provider view without running a switch over a store.
#[cfg(test)]
pub(crate) fn handoff_message_for_test(handoff_event_id: &str, event_ids: &[&str], first_row_id: i64) -> String {
    let folded = FoldedToolRoundtrips {
        count: 0,
        tools: Vec::new(),
    };
    let last_row_id = first_row_id.saturating_add(i64::try_from(event_ids.len()).unwrap_or(i64::MAX));
    let note = serde_json::json!({
        "schema_version": 1,
        "kind": "context_handoff",
        "strategy": "hard_switch",
        "handoff_event_id": handoff_event_id,
        "parent_handoff_event_id": null,
        "from_generation": 0,
        "to_generation": 1,
        "created_at": "2026-11-20T00:00:00+00:00",
        "trigger": "pre_provider_call",
        "source_message_count": event_ids.len(),
        "source_token_estimate": 0,
        "unpersisted_companion_count": 0,
        "folded_tool_roundtrips": {
            "count": folded.count,
            "tools": folded.tools,
        },
        "folded_tool_roundtrips_notice": format_folded_tool_roundtrips_notice(&folded),
        "provenance_status": "exact",
        "source_event_range": {
            "first_event_id": event_ids.first().copied().unwrap_or_default(),
            "last_event_id": event_ids.last().copied().unwrap_or_default(),
            "first_row_id": first_row_id,
            "last_row_id": last_row_id,
            "source_event_count": event_ids.len(),
        },
        "retained_recent_message_count": 2,
        "lookup": {
            "tool": crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME,
            "arguments": {
                "event_ids": event_ids,
                "first_row_id": first_row_id,
                "last_row_id": last_row_id,
            }
        }
    });
    format_context_handoff_message(&note.to_string())
}

/// An id, handoff id or trigger of a validated note: ASCII letters, digits,
/// `-`, `_`, `.` or `:`, at most [`HANDOFF_NOTE_MAX_ID_BYTES`] bytes.
pub(crate) fn is_handoff_note_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= HANDOFF_NOTE_MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

pub(crate) fn is_handoff_note_tool_name(value: &str) -> bool {
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
    let (_, json) = validate_context_handoff(content)?;
    serde_json::from_str::<serde_json::Value>(json).ok()
}

/// The values of a validated handoff note ([`validated_context_handoff_note`])
/// that its provider rendering and the lookup by handoff id read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedContextHandoff {
    /// The note's own identity, the argument of a lookup.
    pub(crate) handoff_event_id: String,
    /// The transcript events the handoff moved out of the window, in order.
    pub(crate) event_ids: Vec<String>,
    /// The row range the referenced events lie in.
    pub(crate) first_row_id: i64,
    pub(crate) last_row_id: i64,
    /// Tool round-trips folded into the handoff (they have no events).
    pub(crate) folded_roundtrips: usize,
}

/// [`validated_context_handoff_note`] as typed values; `None` exactly when the
/// note does not validate.
#[must_use]
pub(crate) fn validated_context_handoff(content: &str) -> Option<ValidatedContextHandoff> {
    validate_context_handoff(content).map(|(handoff, _)| handoff)
}

/// The typed values and the JSON text of a note that passes every check of
/// [`validated_context_handoff_note`].
pub(crate) fn validate_context_handoff(content: &str) -> Option<(ValidatedContextHandoff, &str)> {
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
    let HandoffNoteLookupArguments {
        event_ids,
        first_row_id,
        last_row_id,
    } = note.lookup.arguments;
    Some((
        ValidatedContextHandoff {
            handoff_event_id: note.handoff_event_id,
            event_ids,
            first_row_id,
            last_row_id,
            folded_roundtrips: folded.count,
        },
        json,
    ))
}

/// The system-prompt paragraph of a chat session that states what a handoff
/// in its window means and how earlier turns are reached. Its bytes never
/// change between turns, so the cacheable prompt prefix stays stable.
pub(crate) const CONTEXT_HANDOFF_PROMPT_RULE: &str = "A message that starts with [context_handoff] means earlier turns of this session were moved out of this window; their user and assistant messages still exist and can be read back, but the tool calls and tool results of those turns cannot be recovered. Never treat the oldest visible message as the start of the session. Before answering a question about anything that may have happened earlier than the visible window, call transcript_history_lookup with that handoff's handoff_event_id and read the result; if the result contains another [context_handoff], look that one up too.";

/// Whether `messages` hold a context handoff, in its stored or its rendered
/// form: both start with the handoff marker.
#[must_use]
pub(crate) fn holds_context_handoff(messages: &[ChatMessage]) -> bool {
    messages.iter().any(is_context_handoff_message)
}

/// The one renderer of a stored message in a provider view: a context handoff
/// (`role` assistant, the handoff marker) is replaced by its compact form,
/// built from the validated note; every other message is borrowed unchanged.
/// A handoff whose note does not validate is replayed as stored. The stored
/// note, the canonical history and every saved turn keep the stored bytes;
/// the rendering of one note never changes between turns or restarts.
pub(crate) fn render_context_handoff_for_provider<'a>(role: &str, content: &'a str) -> std::borrow::Cow<'a, str> {
    if !is_context_handoff_turn(role, content) {
        return std::borrow::Cow::Borrowed(content);
    }
    let Some(handoff) = validated_context_handoff(content) else {
        tracing::warn!("context handoff note does not validate; the provider view replays it as stored");
        return std::borrow::Cow::Borrowed(content);
    };
    std::borrow::Cow::Owned(format_compact_context_handoff(&handoff))
}

/// `[context_handoff] handoff_event_id=<id> — <n> earlier user and assistant messages were moved
/// out of this window. They are recoverable with
/// transcript_history_lookup(handoff_event_id="<id>"). The tool calls and
/// tool results of the replaced turns are not recoverable.`, followed by
/// ` <k> tool round-trips were folded.` only when `k` is not zero.
pub(crate) fn format_compact_context_handoff(handoff: &ValidatedContextHandoff) -> String {
    format_compact_context_handoff_parts(
        &handoff.handoff_event_id,
        handoff.event_ids.len(),
        handoff.folded_roundtrips,
    )
}

/// The compact rendering of a note with handoff id `id`, `messages`
/// referenced events and `folded` folded tool round-trips.
pub(crate) fn format_compact_context_handoff_parts(id: &str, messages: usize, folded: usize) -> String {
    let mut rendered = format!(
        "{CONTEXT_HANDOFF_START}{COMPACT_HANDOFF_ID_FIELD}{id} — {messages} earlier user and assistant messages were moved out of this window. They are recoverable with {}(handoff_event_id=\"{id}\"). The tool calls and tool results of the replaced turns are not recoverable.",
        crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME,
    );
    if folded > 0 {
        let _ = write!(rendered, " {folded}{COMPACT_HANDOFF_FOLDED_SUFFIX}");
    }
    rendered
}

/// What follows the handoff marker in a compact rendering, up to the id.
pub(crate) const COMPACT_HANDOFF_ID_FIELD: &str = " handoff_event_id=";

/// The sentence after the folded round-trip count of a compact rendering.
pub(crate) const COMPACT_HANDOFF_FOLDED_SUFFIX: &str = " tool round-trips were folded.";

/// Whether `content` is exactly a compact rendering
/// ([`format_compact_context_handoff_parts`]) of a validated note: a
/// well-formed handoff id, a positive message count, an optional positive
/// folded count, and every other byte as the renderer writes it. The parts
/// are read back and the rendering rebuilt from them, so the renderer stays
/// the one definition of the shape.
pub(crate) fn is_compact_context_handoff(content: &str) -> bool {
    let Some(rest) = content
        .strip_prefix(CONTEXT_HANDOFF_START)
        .and_then(|rest| rest.strip_prefix(COMPACT_HANDOFF_ID_FIELD))
    else {
        return false;
    };
    let Some((id, rest)) = rest.split_once(" — ") else {
        return false;
    };
    let Some(messages) = rest
        .split_once(' ')
        .and_then(|(count, _)| count.parse::<usize>().ok())
        .filter(|count| *count > 0)
    else {
        return false;
    };
    let folded = content
        .strip_suffix(COMPACT_HANDOFF_FOLDED_SUFFIX)
        .and_then(|head| head.rsplit_once(' '))
        .and_then(|(_, count)| count.parse::<usize>().ok())
        .unwrap_or(0);
    is_handoff_note_identifier(id) && format_compact_context_handoff_parts(id, messages, folded) == content
}

/// The newest context-handoff generation in `history`, 0 when none.
#[must_use]
pub(crate) fn context_handoff_generation(history: &[ChatMessage]) -> u64 {
    context_handoff_lineage(history).0
}

pub(crate) fn context_handoff_lineage(history: &[ChatMessage]) -> (u64, Option<String>) {
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
