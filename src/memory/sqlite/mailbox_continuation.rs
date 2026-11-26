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

/// SQL predicate over a `message_events` row aliased `e`: true when `e` is a
/// child task segment input that restates the task already recorded by the
/// spawn request row (same session, role and content, and the spawn row's run
/// is the segment's `task_run_id`). History hides it the same way
/// (`child_turns::history`), so the journal and snapshot must not publish it.
/// A payload that is not valid JSON is not an echo (no `malformed JSON` error).
pub(super) const TASK_ECHO_PREDICATE_E: &str = "(e.role='user' AND EXISTS(SELECT 1 FROM message_events p \
     WHERE p.workspace_id=e.workspace_id AND p.owner_id=e.owner_id AND p.id<e.id \
     AND p.session_key=e.session_key AND p.role=e.role AND p.content_hash=e.content_hash \
     AND CASE WHEN json_valid(e.raw_payload_json) THEN json_extract(e.raw_payload_json,'$.task_run_id') END=p.run_id))";

/// Whether the stored row `event_id` is a task echo (see [`TASK_ECHO_PREDICATE_E`]).
pub(super) fn is_task_echo(conn: &Connection, event_id: &str) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM message_events e WHERE e.event_id=?1 AND {TASK_ECHO_PREDICATE_E})"),
        params![event_id],
        |row| row.get(0),
    )?)
}
