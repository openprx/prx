//! Reconstruct the mailbox projection from consume cursors and scoped events.
use super::{
    model::{EventRef, decode, encode},
    storage_error, stream,
};
use crate::memory::child_sessions::{ChildMailboxTurn, ChildSessionContext};
use crate::messaging::mailbox::{MailboxError, MailboxResult, MailboxScope};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;
use uuid::Uuid;

pub(in crate::memory::sqlite) fn projection(
    conn: &Connection,
    context: &ChildSessionContext,
    scope: &MailboxScope,
    canonical_key: &str,
) -> MailboxResult<(BTreeMap<i64, u64>, Vec<ChildMailboxTurn>)> {
    let mut query = conn
        .prepare("SELECT stream_id FROM prx_mailbox_seq WHERE recipient_ma_scope_id=?1 AND to_addr=?2")
        .map_err(storage_error)?;
    let ids = query
        .query_map(params![scope.scope_id.to_string(), encode(&context.addr)?], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    let mut cursors = BTreeMap::new();
    let mut identities = BTreeMap::new();
    for id in ids {
        let stream = stream::load(conn, id)?;
        if stream.recipient.as_ref() != Some(scope) {
            return Err(MailboxError::CorruptState);
        }
        cursors.insert(stream.id, stream.progress.consume_cursor);
        identities.insert(stream.id, (stream.wire_id, stream.from.instance));
    }
    let mut query=conn.prepare("SELECT event_id,content,created_at,source_ref_json FROM message_events WHERE workspace_id=?1 AND session_key=?2 AND owner_id=?3 AND ma_scope_id=?4 AND source='prx_mailbox' ORDER BY id").map_err(storage_error)?;
    let events = query
        .query_map(
            params![
                context.workspace_id,
                canonical_key,
                context.addr.owner,
                scope.scope_id.to_string()
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    let mut turns = Vec::new();
    for (event_id, content, timestamp, json) in events {
        let source: EventRef = decode(&json)?;
        source.common.validate(source.message_id)?;
        if source.scope != *scope
            || source.workspace_id != context.workspace_id
            || source.owner_id != context.addr.owner
            || source.session_id != context.addr.session
            || source.agent != context.addr.agent
        {
            return Err(MailboxError::CorruptState);
        }
        if identities.get(&source.local_stream_id) != Some(&(source.wire_stream_id, source.from_instance.clone())) {
            return Err(MailboxError::CorruptState);
        }
        if cursors
            .get(&source.local_stream_id)
            .is_some_and(|cursor| source.seq <= *cursor)
        {
            turns.push(ChildMailboxTurn {
                event_id: Uuid::parse_str(&event_id).map_err(|_| MailboxError::CorruptState)?,
                content,
                timestamp: DateTime::parse_from_rfc3339(&timestamp)
                    .map_err(|_| MailboxError::CorruptState)?
                    .with_timezone(&chrono::Utc),
            });
        }
    }
    Ok((cursors, turns))
}
use chrono::DateTime;
