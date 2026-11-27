//! The session prefix: the memory index a session sends in its system prompt.
//!
//! The index is computed at the first turn of a session that has no prefix
//! row, recorded once (`context.prefix.recorded`) and then replayed byte for
//! byte on every later turn of that session, even when the memories changed
//! meanwhile. A prefix row is refreshed only when a context handoff
//! (`context.switch.created`) was recorded after it. A session whose index is
//! empty records a marker row with no sections; the marker freezes "no index"
//! for the session, so the index is read once per session and a later memory
//! write does not add one until a handoff. A group-shared channel turn has no
//! index and no row: it renders only its `session_opened` hook blocks, every
//! turn.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::MemoryConfig;
use crate::context::sources::memory_index::{
    STORE_INDEX_COMPACT_MAX_CHARS, STORE_INDEX_FETCH, STORE_INDEX_MAX_CHARS, render_store_index,
};
use crate::context::transcript::{
    HOOK_SOURCE_PREFIX, MEMORY_INDEX_SOURCE, PrefixSection, append_hook_contributions, append_session_prefix,
    newest_session_prefix, render_prefix,
};
use crate::hooks::context::{ContextPoint, HookContribution};
use crate::hooks::{HookIdentity, HookManager};
use crate::memory::branch_copy::CONTEXT_SWITCH_EVENT_TYPE;
use crate::memory::{Memory, MemoryEntry, MemoryFabric, MemoryPrincipal};
use crate::runtime::envelope::{RuntimeEnvelope, RuntimeSource};

/// What a session prefix reads, borrowed from the turn's state.
pub(crate) struct SessionPrefixInput<'a> {
    pub(crate) memory: &'a Arc<dyn Memory>,
    pub(crate) envelope: &'a RuntimeEnvelope,
    pub(crate) memory_config: &'a MemoryConfig,
    /// `agent.compact_context`: the index is rendered with the compact cap.
    pub(crate) compact: bool,
    /// The surface's hook manager for the `session_opened` context hooks.
    /// `None` runs none (child turns).
    pub(crate) hooks: Option<&'a HookManager>,
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
        hooks,
    } = input;
    // Without a session there is no row to freeze the index in.
    if envelope.session_key.is_empty() {
        return String::new();
    }
    // A group-shared channel chat reads and records no prefix row: its memory
    // index stays out of the prompt, and the legacy session key it may share
    // with a direct chat must not carry a row from one into the other.
    if super::tail::is_group_shared_channel(envelope) {
        return group_hook_prefix(memory, envelope, memory_config, hooks).await;
    }
    // Delegate children share one session key per chat sender, so a frozen row
    // would show the index of the first delegate call to every later one. They
    // render the index fresh and neither read nor record a prefix row.
    if envelope.source == RuntimeSource::Delegate {
        return delegate_prefix(memory, envelope, compact).await;
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
    let resumed = recorded.is_some();
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

    let mut sections = Vec::new();
    if !text.is_empty() {
        sections.push(PrefixSection {
            source: MEMORY_INDEX_SOURCE.to_owned(),
            text,
        });
    }
    let mut scope = envelope.message_scope();
    // The row belongs to the session, not to the turn that happens to open it.
    scope.run_id = None;
    scope.parent_run_id = None;
    let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(memory_config.event_recording_config());
    // The `session_opened` hooks run on this render path only. When the row is
    // not stored below (tool events not recorded, or a backend without an event
    // log), the next turn renders again and the hooks run again: their output is
    // not frozen for the session in that case.
    if let Some(hooks) = hooks {
        let contributions = session_opened_contributions(hooks, envelope, resumed).await;
        append_hook_contributions(&fabric, &scope, &contributions).await;
        sections.extend(contributions.into_iter().map(|contribution| PrefixSection {
            source: format!("{HOOK_SOURCE_PREFIX}{}", contribution.hook),
            text: contribution.text,
        }));
    }
    let rendered = render_prefix(&sections);
    if let Some(row_id) = append_session_prefix(&fabric, scope, &sections).await
        && !entries.is_empty()
    {
        verify_recorded(store, &principal, &entries, row_id).await;
    }
    rendered
}

/// The prefix of a delegate call: the memory index rendered on every call.
/// Delegates get no `session_opened` hooks, as the only caller opens child
/// sessions without a hook manager. Nothing is read from or written to the
/// shared delegate session key, so no stale row can be replayed.
async fn delegate_prefix(memory: &Arc<dyn Memory>, envelope: &RuntimeEnvelope, compact: bool) -> String {
    let principal = envelope.memory_principal();
    let entries = match memory.newest_core_entries(&principal, STORE_INDEX_FETCH).await {
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
    let text = render_store_index(&entries, max_chars, truncated)
        .map(|rendered| crate::security::redaction::redact_secrets(&rendered))
        .unwrap_or_default();
    let mut sections = Vec::new();
    if !text.is_empty() {
        sections.push(PrefixSection {
            source: MEMORY_INDEX_SOURCE.to_owned(),
            text,
        });
    }
    render_prefix(&sections)
}

/// The prefix of a group-shared channel turn: only the `session_opened` hook
/// blocks, rendered on every turn. No prefix row is read or recorded, so a
/// row frozen by a direct chat on a shared session key never reaches a group
/// turn, and a group turn never freezes a row a direct chat would replay. The
/// hooks see `resumed: false`, since no row is consulted. Their contributions
/// are still logged like any other hook output.
async fn group_hook_prefix(
    memory: &Arc<dyn Memory>,
    envelope: &RuntimeEnvelope,
    memory_config: &MemoryConfig,
    hooks: Option<&HookManager>,
) -> String {
    let Some(hooks) = hooks else {
        return String::new();
    };
    let contributions = session_opened_contributions(hooks, envelope, false).await;
    if contributions.is_empty() {
        return String::new();
    }
    let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(memory_config.event_recording_config());
    let mut scope = envelope.message_scope();
    scope.run_id = None;
    scope.parent_run_id = None;
    append_hook_contributions(&fabric, &scope, &contributions).await;
    let sections: Vec<PrefixSection> = contributions
        .into_iter()
        .map(|contribution| PrefixSection {
            source: format!("{HOOK_SOURCE_PREFIX}{}", contribution.hook),
            text: contribution.text,
        })
        .collect();
    render_prefix(&sections)
}

/// The `session_opened` context contributions, taken once per prefix render
/// under the session's identity. `resumed` says a prefix row existed before
/// this render (a handoff or a memory change re-renders it).
///
/// Freezing holds only when the prefix row is stored. Without a stored row
/// (tool events not recorded, or a backend without an event log) every turn
/// renders the prefix again, so this hook runs on every turn and a varying
/// output changes the system prompt each turn.
async fn session_opened_contributions(
    hooks: &HookManager,
    envelope: &RuntimeEnvelope,
    resumed: bool,
) -> Vec<HookContribution> {
    let data = serde_json::json!({
        "surface": HookIdentity::origin_of(envelope.source),
        "resumed": resumed,
    });
    crate::hooks::identity_scope(
        HookIdentity::for_session(envelope),
        hooks.contribute(ContextPoint::SessionOpened, data),
    )
    .await
}

/// Check, after the row was recorded, that every memory it was rendered from is
/// still stored with the same content. A forget or a correction that ran in
/// between is not seen by the row's own transaction, so when a memory is gone
/// or changed the row is deleted. The text already rendered is still sent for
/// this turn; the next turn renders a fresh index. A failed re-read deletes the
/// row too, since the row cannot be shown to be current.
async fn verify_recorded(store: &dyn Memory, principal: &MemoryPrincipal, rendered: &[MemoryEntry], row_id: i64) {
    let current = match store.newest_core_entries(principal, STORE_INDEX_FETCH).await {
        Ok(now) => {
            let by_key: HashMap<&str, &str> = now
                .iter()
                .map(|entry| (entry.key.as_str(), entry.content.as_str()))
                .collect();
            rendered
                .iter()
                .all(|entry| by_key.get(entry.key.as_str()) == Some(&entry.content.as_str()))
        }
        Err(error) => {
            log_prefix_check_failure(&error, "memory index could not be re-read after recording");
            false
        }
    };
    if current {
        return;
    }
    if let Err(error) = store.delete_session_prefix_row(row_id).await {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::warn!(
            error_len,
            error_sha256,
            "session prefix row could not be dropped after a memory changed; a later turn may send the older index until the next handoff"
        );
    }
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

/// A failed check after a prefix row was recorded. The text is already sent
/// for this turn, so the message says what happens to the row instead.
fn log_prefix_check_failure(error: &anyhow::Error, what: &'static str) {
    let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(error);
    tracing::warn!(
        error_len,
        error_sha256,
        "{what}; the prefix row is dropped and the next turn renders a fresh index"
    );
}
