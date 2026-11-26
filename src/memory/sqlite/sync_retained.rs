//! Retention notices for the rf sync stream: the `retained:` projection, the
//! retention state row and the snapshot `retained_since` marker.
//!
//! A notice is a `turn.deleted` journal header whose source reference is
//! self-contained (`retained:<side>:<message_event_id>`), so projecting it
//! never reads the deleted rows.
use super::{
    mailbox_continuation::{CONTINUATION_PREDICATE_E, TASK_ECHO_PREDICATE_E},
    sync_journal::{Header, append, event_header},
};
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

/// Notice headers for the rf-visible messages among `event_ids` (rows about to
/// be deleted), in ascending `message_events.id`. The row predicate is the
/// snapshot `turn` inventory (`sync_snapshot::inventory`) without its
/// head/water bounds; a row that `event_header` cannot resolve gets no notice.
pub(super) fn notice_headers(conn: &Connection, event_ids: &[String]) -> anyhow::Result<Vec<Header>> {
    let ids = serde_json::to_string(event_ids)?;
    let mut q = conn.prepare(&format!(
        "SELECT x.event_id,x.role,x.event_type,x.idempotency_key FROM (
        SELECT e.id,e.event_id,e.owner_id,e.workspace_id,e.run_id,e.role,e.event_type,e.idempotency_key,
            CASE WHEN {CONTINUATION_PREDICATE_E} OR {TASK_ECHO_PREDICATE_E} THEN 1 ELSE 0 END AS continuation,
            CASE WHEN e.role='assistant' OR e.idempotency_key LIKE 'turn:%:assistant'
            THEN (SELECT f.event_id FROM message_events f WHERE f.owner_id=e.owner_id AND f.workspace_id=e.workspace_id
                AND f.run_id=e.run_id AND (
                    (f.event_type='turn.finalized' AND CASE WHEN json_valid(f.raw_payload_json)
                        THEN json_extract(f.raw_payload_json,'$.assistant_event_id') END=e.event_id)
                    OR (e.idempotency_key='turn:'||e.run_id||':assistant' AND f.idempotency_key='turn:'||e.run_id||':final'))
                ORDER BY f.id LIMIT 1) END AS final_marker
        FROM message_events e WHERE e.event_id IN(SELECT value FROM json_each(?1))) x
        WHERE (x.role='user' AND x.continuation=0)
            OR (x.event_type='message.deleted' AND (
                EXISTS(SELECT 1 FROM rf_message_attempts a WHERE a.owner_id=x.owner_id AND a.message_event_id=x.event_id)
                OR EXISTS(SELECT 1 FROM conversation_turns c WHERE c.message_event_id=x.event_id AND c.role='user')
                OR EXISTS(SELECT 1 FROM sync_journal j WHERE j.owner_id=x.owner_id AND j.kind='turn.appended' AND j.source_ref='event:'||x.event_id)))
            OR x.final_marker IS NOT NULL
        ORDER BY x.id"
    ))?;
    let rows = q.query_map([&ids], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut headers = Vec::new();
    for row in rows {
        let (event, role, event_type, key) = row?;
        let Some(mut h) = event_header(conn, &event, "turn.deleted")? else {
            continue;
        };
        // A tombstone has role `event`; its assistant key (`turn:<run>:assistant`) keeps the side.
        let reply = if event_type == "message.deleted" {
            key.as_deref()
                .is_some_and(|key| key.starts_with("turn:") && key.ends_with(":assistant"))
        } else {
            role != "user"
        };
        let side = if reply { "reply" } else { "message" };
        h.event_id = format!("{REF_PREFIX}{event}");
        h.source_ref = format!("{REF_PREFIX}{side}:{event}");
        h.timestamp = now.clone();
        headers.push(h);
    }
    Ok(headers)
}

/// Appends the notice headers inside the caller's transaction; returns how many were written.
pub(super) fn append_notices(conn: &Connection, headers: &[Header]) -> anyhow::Result<u64> {
    for h in headers {
        append(conn, h)?;
    }
    Ok(u64::try_from(headers.len())?)
}
