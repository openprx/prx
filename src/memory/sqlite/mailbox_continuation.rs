//! The one definition of a mailbox continuation input.
//!
//! A continuation input is a `user` row whose `causation_event_id` names a
//! `prx_mailbox` user row (the adoption row) of the same workspace, owner and
//! session. The adoption row is the message's only history row; the woken
//! turn's own input anchors the run but is never projected into history.
use rusqlite::{Connection, params};

/// SQL predicate over a `message_events` row aliased `e`: true when `e` is a
/// continuation input. Usable inside any query that exposes that alias.
pub(super) const CONTINUATION_PREDICATE_E: &str = "(e.role='user' AND EXISTS(SELECT 1 FROM message_events adopt \
     WHERE adopt.event_id=e.causation_event_id AND adopt.workspace_id=e.workspace_id \
     AND adopt.owner_id=e.owner_id AND adopt.session_key=e.session_key \
     AND adopt.source='prx_mailbox' AND adopt.role='user'))";

/// Whether `event` is a continuation of an adopted mailbox message.
pub(super) fn is_continuation(conn: &Connection, event: &crate::memory::MessageEvent) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM message_events WHERE event_id=?1 AND workspace_id=?2 AND owner_id=?3 \
         AND session_key=?4 AND source='prx_mailbox' AND role='user')",
        params![
            event.causation_event_id,
            event.workspace_id,
            event.owner_id,
            event.session_key
        ],
        |row| row.get(0),
    )?)
}
