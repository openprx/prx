//! The session prefix row: the system-prompt prefix of a session (the memory
//! index and the session-open context hook contributions), recorded once and
//! replayed byte for byte on every later turn of that session.
//!
//! The row has role `tool` and event type `context.prefix.recorded`. Its
//! content is a fixed label; the index text lives only in the payload, so the
//! text never reaches the content of a message row. Replay, history loaders,
//! compaction, handoff and provenance skip the row by role or by event type
//! (see `replay.rs`, `provenance.rs` and the `Transcript` readers). It is never
//! a user or assistant turn, and it carries no run id, so it is never a run's
//! tool round.
//!
//! The payload is `{"schema_version":1,"sections":[{"source":..,"text":..}]}`.
//! `memory_index` is the memory index; a source `hook:<name>` is a context hook
//! contribution. [`render_prefix`] is the one function that turns the sections
//! into the text, used by the writer's caller and by the reader, so the bytes
//! sent on a later turn are the bytes recorded.

use crate::memory::branch_copy::SESSION_PREFIX_EVENT_TYPE;
use crate::memory::{Memory, MemoryFabric, MemoryPrincipal, MessageEventScope};

/// Content of the row: a label, never the index text.
const PREFIX_CONTENT: &str = "session prefix";
/// Schema version of the prefix row payload.
const PREFIX_SCHEMA_VERSION: u32 = 1;
/// Source of the memory index section.
pub(crate) const MEMORY_INDEX_SOURCE: &str = "memory_index";
/// Source prefix of a context hook section; the hook's name follows it.
const HOOK_SOURCE_PREFIX: &str = "hook:";

/// One section of the session prefix: where its text came from and the text.
pub(crate) struct PrefixSection {
    pub(crate) source: String,
    pub(crate) text: String,
}

/// The system-prompt text of a prefix. Sections with empty text are left out;
/// the rest are joined by a blank line. A `hook:<name>` section is labelled
/// `[Hook context: <name>]`; any other section is its text as it is.
pub(crate) fn render_prefix(sections: &[PrefixSection]) -> String {
    sections
        .iter()
        .filter(|section| !section.text.is_empty())
        .map(|section| match section.source.strip_prefix(HOOK_SOURCE_PREFIX) {
            Some(name) => format!("[Hook context: {name}]\n{}", section.text),
            None => section.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The sections of a prefix row payload. A payload that does not parse, or a
/// section without text, contributes nothing.
fn sections_of(raw: Option<&str>) -> Vec<PrefixSection> {
    let Some(payload) = raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok()) else {
        return Vec::new();
    };
    let Some(items) = payload.get("sections").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|section| {
            Some(PrefixSection {
                source: section
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                text: section.get("text")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

/// Read the newest prefix row of `session_key` through `principal` as
/// `(row id, rendered text)`. `None` when the session has no such row, or the
/// backend has no ordered event log (its fail-fast read is the normal no-prefix
/// case).
pub(crate) async fn newest_session_prefix(
    memory: &dyn Memory,
    principal: &MemoryPrincipal,
    session_key: &str,
) -> anyhow::Result<Option<(i64, String)>> {
    let rows = match memory
        .newest_session_events_of_type(principal, session_key, SESSION_PREFIX_EVENT_TYPE, 1)
        .await
    {
        Ok(rows) => rows,
        Err(error) if error.is::<crate::memory::erasure::Unsupported>() => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(event) = rows
        .into_iter()
        .find(|event| event.session_key.as_deref() == Some(session_key))
    else {
        return Ok(None);
    };
    let text = render_prefix(&sections_of(event.raw_payload_json.as_deref()));
    Ok(Some((event.id, text)))
}

/// Append the prefix row of a session and return its row id. `scope` must carry
/// no run id: the row belongs to the session, not to a turn. `sections` is the
/// rendered, redacted prefix; an empty slice writes the marker row with no
/// sections, which freezes "no index and no hook context" for the session. The
/// write is best effort, like the attachment row: a failed write is logged with
/// its error digest only and gives `None`, as does a write that stored nothing
/// (tool events not recorded).
pub(crate) async fn append_session_prefix(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    sections: &[PrefixSection],
) -> Option<i64> {
    let sections: Vec<serde_json::Value> = sections
        .iter()
        .map(|section| serde_json::json!({ "source": section.source, "text": section.text }))
        .collect();
    let payload = serde_json::json!({
        "schema_version": PREFIX_SCHEMA_VERSION,
        "sections": sections,
    });
    match fabric
        .record_tool_round_event(
            scope,
            SESSION_PREFIX_EVENT_TYPE,
            PREFIX_CONTENT,
            Some(payload.to_string()),
        )
        .await
    {
        // A synthetic event (tool events not recorded) carries id 0.
        Ok(event) if event.id > 0 => Some(event.id),
        Ok(_) => None,
        Err(error) => {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(error_len, error_sha256, "Failed to persist session prefix");
            None
        }
    }
}
