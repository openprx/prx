//! Retention notices for the rf sync stream: the `retained:` projection, the
//! retention state row and the snapshot `retained_since` marker.
//!
//! A notice is a `turn.deleted` journal header whose source reference is
//! self-contained (`retained:<side>:<message_event_id>`), so projecting it
//! never reads the deleted rows.
use super::sync_journal::Header;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

const REF_PREFIX: &str = "retained:";
const REASON: &str = "retention";

pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_retention_state(
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),last_run INTEGER NOT NULL,
        cutoff TEXT,deleted_total INTEGER NOT NULL DEFAULT 0) STRICT;",
    )?;
    Ok(())
}

/// Projected payload of a `retained:<side>:<id>` header; `None` for every
/// other source reference, which the caller's existing branches handle.
pub(super) fn payload(h: &Header) -> anyhow::Result<Option<Value>> {
    let Some(reference) = h.source_ref.strip_prefix(REF_PREFIX) else {
        return Ok(None);
    };
    let (side, message) = reference
        .split_once(':')
        .ok_or(crate::memory::sync::SyncError::InvalidSource)?;
    anyhow::ensure!(
        matches!(side, "message" | "reply") && !message.is_empty(),
        crate::memory::sync::SyncError::InvalidSource
    );
    let mut value = json!({"side":side,"message_id":message,"reason":REASON});
    if let (Some(run), Some(object)) = (&h.run_id, value.as_object_mut()) {
        object.insert("run_id".into(), json!(run));
    }
    Ok(Some(value))
}

/// The cutoff of the newest pass that deleted rows, if any pass has.
pub(super) fn retained_since(conn: &Connection) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT cutoff FROM message_retention_state WHERE singleton=1",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}
