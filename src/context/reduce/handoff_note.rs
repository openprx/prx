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
    /// How the model's summary of the moved turns ended. Absent for a note
    /// written before summaries existed.
    #[serde(default)]
    summary_status: Option<HandoffSummaryStatus>,
    /// The summary itself, present exactly when `summary_status` is `written`.
    #[serde(default)]
    summary: Option<String>,
    /// Skills the session had loaded before the handoff; absent or empty means none.
    #[serde(default)]
    skills_in_use: Vec<String>,
}

/// The outcome of the summary a handoff note may carry. Only `written` carries
/// a summary; the other statuses say why none was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HandoffSummaryStatus {
    Written,
    Failed,
    Empty,
    OmittedSize,
}

/// Longest summary a validated note may carry, in bytes.
pub(crate) const HANDOFF_SUMMARY_MAX_BYTES: usize = 6_000;

/// Most skill names a validated note may carry.
pub(crate) const HANDOFF_SKILLS_MAX_COUNT: usize = 8;

/// Longest skill name a validated note may carry, in bytes.
pub(crate) const HANDOFF_SKILL_NAME_MAX_BYTES: usize = 128;

/// Markers a summary may not contain: a summary that carried one could be
/// read as a handoff or a document reference by the provider view.
const HANDOFF_SUMMARY_FORBIDDEN_MARKERS: [&str; 4] = [
    CONTEXT_HANDOFF_START,
    CONTEXT_HANDOFF_END,
    "[document_ingest_ref]",
    "[/document_ingest_ref]",
];

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

/// Whether a summary status and summary pair is one the writer may produce:
/// a summary exactly with status `written`, a text within the bound and free
/// of the reserved markers.
fn is_valid_handoff_summary(status: Option<HandoffSummaryStatus>, summary: Option<&str>) -> bool {
    match (status, summary) {
        (Some(HandoffSummaryStatus::Written), Some(text)) => {
            !text.is_empty()
                && text.len() <= HANDOFF_SUMMARY_MAX_BYTES
                && !HANDOFF_SUMMARY_FORBIDDEN_MARKERS
                    .iter()
                    .any(|marker| text.contains(*marker))
        }
        (Some(HandoffSummaryStatus::Written), None) => false,
        (Some(_), None) | (None, None) => true,
        (_, Some(_)) => false,
    }
}

/// Whether a list of skill names is one the writer may produce: at most
/// [`HANDOFF_SKILLS_MAX_COUNT`] names, each non-empty, bounded, without control
/// characters, `[` or `]`.
fn is_valid_handoff_skills(skills: &[String]) -> bool {
    skills.len() <= HANDOFF_SKILLS_MAX_COUNT
        && skills.iter().all(|skill| {
            !skill.is_empty()
                && skill.len() <= HANDOFF_SKILL_NAME_MAX_BYTES
                && !skill
                    .chars()
                    .any(|character| character.is_control() || character == '[' || character == ']')
        })
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
    /// The summary status; `None` for a note without the field.
    summary_status: Option<HandoffSummaryStatus>,
    /// The summary, present exactly when the status is `written`.
    summary: Option<String>,
    /// The skills loaded before the handoff, in note order.
    skills_in_use: Vec<String>,
}

impl ValidatedContextHandoff {
    /// The status of the summary of the moved turns, `None` when the note has none.
    pub(crate) fn summary_status(&self) -> Option<HandoffSummaryStatus> {
        self.summary_status
    }

    /// The summary of the moved turns, present exactly when the status is `written`.
    pub(crate) fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// The skills loaded before the handoff; empty when none were recorded.
    pub(crate) fn skills_in_use(&self) -> &[String] {
        &self.skills_in_use
    }
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
        && note.folded_tool_roundtrips_notice == format_folded_tool_roundtrips_notice(&folded)
        && is_valid_handoff_summary(note.summary_status, note.summary.as_deref())
        && is_valid_handoff_skills(&note.skills_in_use);
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
            summary_status: note.summary_status,
            summary: note.summary,
            skills_in_use: note.skills_in_use,
        },
        json,
    ))
}

/// The system-prompt paragraph of a chat session that states what a handoff
/// in its window means and how earlier turns are reached. Its bytes never
/// change between turns, so the cacheable prompt prefix stays stable.
pub(crate) const CONTEXT_HANDOFF_PROMPT_RULE: &str = "A message that starts with [context_handoff] means earlier turns of this session were moved out of this window; their user and assistant messages still exist and can be read back, but the tool calls and tool results of those turns cannot be recovered. Never treat the oldest visible message as the start of the session. Before answering a question about anything that may have happened earlier than the visible window, call transcript_history_lookup with that handoff's handoff_event_id and read the result; if the result contains another [context_handoff], look that one up too. When a handoff carries a summary, use it to continue the work and look up the exact earlier messages before quoting them.";

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
    let mut rendered = format_compact_context_handoff_parts(
        &handoff.handoff_event_id,
        handoff.event_ids.len(),
        handoff.folded_roundtrips,
    );
    rendered.push_str(&format_compact_context_handoff_tail(handoff));
    rendered
}

/// What follows the first line of a compact rendering when the note carries
/// skills or a summary status: the skills line, then the summary block or the
/// no-summary line. Empty for a note without either field.
fn format_compact_context_handoff_tail(handoff: &ValidatedContextHandoff) -> String {
    let mut tail = String::new();
    let skills = handoff.skills_in_use();
    if !skills.is_empty() {
        let _ = write!(
            tail,
            "{COMPACT_HANDOFF_SKILLS_HEADER}{}. Call skill_read for each one before continuing its procedure.",
            skills.join(", ")
        );
    }
    match handoff.summary_status() {
        Some(HandoffSummaryStatus::Written) => {
            if let Some(summary) = handoff.summary() {
                let _ = write!(
                    tail,
                    "{COMPACT_HANDOFF_SUMMARY_HEADER} (use it to continue; look up the exact messages before quoting them):\n{summary}"
                );
            }
        }
        Some(_) => tail.push_str(COMPACT_HANDOFF_NO_SUMMARY),
        None => {}
    }
    tail
}

/// The line that opens the skills list of a compact rendering, through the
/// names.
pub(crate) const COMPACT_HANDOFF_SKILLS_HEADER: &str = "\nSkills loaded before the handoff: ";

/// The opening of the summary block of a compact rendering, through the
/// parenthesised instruction that the summary follows.
pub(crate) const COMPACT_HANDOFF_SUMMARY_HEADER: &str =
    "\n\nSummary of the moved turns, written by the model at the handoff";

/// The line a compact rendering carries when the summary status is not `written`.
pub(crate) const COMPACT_HANDOFF_NO_SUMMARY: &str = "\nNo summary was written for these turns.";

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

/// Whether `content` is a compact rendering
/// ([`format_compact_context_handoff`]) of a validated note: its first line is
/// exactly the compact rendering of the parts it reads back
/// ([`format_compact_context_handoff_parts`]), and any text after the first
/// line starts with one of the fixed headers the renderer writes for skills or
/// the summary status. Every other byte of the first line is as the renderer
/// writes it.
pub(crate) fn is_compact_context_handoff(content: &str) -> bool {
    let first_line = content.split_once('\n').map_or(content, |(first, _)| first);
    let Some(tail) = content.get(first_line.len()..) else {
        return false;
    };
    is_compact_context_handoff_line(first_line) && is_compact_context_handoff_tail(tail)
}

/// Whether `line` is exactly the first line of a compact rendering of a note
/// with a well-formed handoff id.
fn is_compact_context_handoff_line(line: &str) -> bool {
    let Some(rest) = line
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
    let folded = line
        .strip_suffix(COMPACT_HANDOFF_FOLDED_SUFFIX)
        .and_then(|head| head.rsplit_once(' '))
        .and_then(|(_, count)| count.parse::<usize>().ok())
        .unwrap_or(0);
    is_handoff_note_identifier(id) && format_compact_context_handoff_parts(id, messages, folded) == line
}

/// Whether the text after the first line of a compact rendering is empty or
/// opens with one of the headers the renderer writes after it.
fn is_compact_context_handoff_tail(tail: &str) -> bool {
    tail.is_empty()
        || [
            COMPACT_HANDOFF_SKILLS_HEADER,
            COMPACT_HANDOFF_SUMMARY_HEADER,
            COMPACT_HANDOFF_NO_SUMMARY,
        ]
        .iter()
        .any(|header| tail.starts_with(header))
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
