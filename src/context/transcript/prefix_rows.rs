//! The session prefix row: the memory index of a session, recorded once and
//! replayed byte for byte on every later turn of that session.
//!
//! The row has role `tool` and event type `context.prefix.recorded`. Its
//! content is a fixed label; the index text lives only in the payload, so the
//! text never reaches the content of a message row. Replay, history loaders,
//! compaction, handoff and provenance skip the row by role or by event type
//! (see `replay.rs`, `provenance.rs` and the `Transcript` readers). It is never
//! a user or assistant turn, and it carries no run id, so it is never a run's
//! tool round.

use crate::memory::branch_copy::SESSION_PREFIX_EVENT_TYPE;
use crate::memory::{Memory, MemoryFabric, MemoryPrincipal, MessageEventScope};

/// Content of the row: a label, never the index text.
const PREFIX_CONTENT: &str = "session prefix";
/// Schema version of the prefix row payload.
const PREFIX_SCHEMA_VERSION: u32 = 1;
/// Source named by the one section of the prefix row.
const PREFIX_SOURCE: &str = "memory_index";

/// Read the newest prefix row of `session_key` through `principal` as
/// `(row id, text)`. `None` when the session has no such row, or the backend
/// has no ordered event log (its fail-fast read is the normal no-prefix case).
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
    let text = event
        .raw_payload_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|payload| {
            payload
                .get("sections")?
                .as_array()?
                .first()?
                .get("text")?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default();
    Ok(Some((event.id, text)))
}

/// Append the prefix row of a session and return its row id. `scope` must carry
/// no run id: the row belongs to the session, not to a turn. `text` is the
/// rendered, redacted index; an empty `text` writes the marker row with no
/// sections, which freezes "no index" for the session. The write is best effort,
/// like the attachment row: a failed write is logged with its error digest only
/// and gives `None`, as does a write that stored nothing (tool events not recorded).
pub(crate) async fn append_session_prefix(fabric: &MemoryFabric, scope: MessageEventScope, text: &str) -> Option<i64> {
    let sections = if text.is_empty() {
        serde_json::json!([])
    } else {
        serde_json::json!([{ "source": PREFIX_SOURCE, "text": text }])
    };
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
