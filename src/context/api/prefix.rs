//! The session prefix: the memory index a session sends in its system prompt.
//!
//! The index is computed at the first turn of a session that has no prefix
//! row, recorded once (`context.prefix.recorded`) and then replayed byte for
//! byte on every later turn of that session, even when the memories changed
//! meanwhile. A prefix row is refreshed only when a context handoff
//! (`context.switch.created`) was recorded after it. A session whose index is
//! empty records a marker row with no sections; the marker freezes "no index"
//! for the session, so the index is read once per session and a later memory
//! write does not add one until a handoff.

use std::sync::Arc;

use crate::config::MemoryConfig;
use crate::context::sources::memory_index::{
    STORE_INDEX_COMPACT_MAX_CHARS, STORE_INDEX_FETCH, STORE_INDEX_MAX_CHARS, render_store_index,
};
use crate::context::transcript::{append_session_prefix, newest_session_prefix};
use crate::memory::branch_copy::CONTEXT_SWITCH_EVENT_TYPE;
use crate::memory::{Memory, MemoryFabric, MemoryPrincipal};
use crate::runtime::envelope::RuntimeEnvelope;

/// What a session prefix reads, borrowed from the turn's state.
pub(crate) struct SessionPrefixInput<'a> {
    pub(crate) memory: &'a Arc<dyn Memory>,
    pub(crate) envelope: &'a RuntimeEnvelope,
    pub(crate) memory_config: &'a MemoryConfig,
    /// `agent.compact_context`: the index is rendered with the compact cap.
    pub(crate) compact: bool,
}

/// The prefix text of the session: the frozen index, or a new one recorded
/// now. Empty when the session has no index to send. The text is what the
/// system prompt gets appended, so an empty string adds nothing.
///
/// A read that fails sends no prefix for this turn and records nothing; only
/// the error digest is logged, never the memory content.
pub(crate) async fn session_prefix(input: SessionPrefixInput<'_>) -> String {
    let SessionPrefixInput {
        memory,
        envelope,
        memory_config,
        compact,
    } = input;
    // Without a session there is no row to freeze the index in.
    if envelope.session_key.is_empty() {
        return String::new();
    }
    let principal = envelope.memory_principal();
    let session_key = envelope.session_key.as_str();
    let store: &dyn Memory = memory.as_ref();

    let recorded = match newest_session_prefix(store, &principal, session_key).await {
        Ok(recorded) => recorded,
        Err(error) => {
            log_read_failure(&error, "session prefix row could not be read");
            return String::new();
        }
    };
    let newest_switch = match newest_switch_id(store, &principal, session_key).await {
        Ok(switch) => switch,
        Err(error) => {
            log_read_failure(&error, "session handoff row could not be read");
            return String::new();
        }
    };
    if let Some((row_id, text)) = recorded
        && newest_switch.is_none_or(|switch| row_id > switch)
    {
        return text;
    }

    let entries = match store.newest_core_entries(&principal, STORE_INDEX_FETCH).await {
        Ok(entries) => entries,
        Err(error) => {
            log_read_failure(&error, "memory index could not be read");
            return String::new();
        }
    };
    let truncated = entries.len() >= STORE_INDEX_FETCH;
    let max_chars = if compact {
        STORE_INDEX_COMPACT_MAX_CHARS
    } else {
        STORE_INDEX_MAX_CHARS
    };
    // An empty index is recorded as a marker row too, so the store is read once per session.
    let text = render_store_index(&entries, max_chars, truncated)
        .map(|rendered| crate::security::redaction::redact_secrets(&rendered))
        .unwrap_or_default();

    let mut scope = envelope.message_scope();
    // The row belongs to the session, not to the turn that happens to open it.
    scope.run_id = None;
    scope.parent_run_id = None;
    let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(memory_config.event_recording_config());
    append_session_prefix(&fabric, scope, &text).await;
    text
}

/// The id of the newest handoff row of `session_key`, `None` when there is none
/// or the backend has no ordered event log.
async fn newest_switch_id(
    store: &dyn Memory,
    principal: &MemoryPrincipal,
    session_key: &str,
) -> anyhow::Result<Option<i64>> {
    match store
        .newest_session_events_of_type(principal, session_key, CONTEXT_SWITCH_EVENT_TYPE, 1)
        .await
    {
        Ok(rows) => Ok(rows
            .iter()
            .filter(|event| event.session_key.as_deref() == Some(session_key))
            .map(|event| event.id)
            .max()),
        Err(error) if error.is::<crate::memory::erasure::Unsupported>() => Ok(None),
        Err(error) => Err(error),
    }
}

fn log_read_failure(error: &anyhow::Error, what: &'static str) {
    let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(error);
    tracing::warn!(error_len, error_sha256, "{what}; this turn sends no memory index");
}
