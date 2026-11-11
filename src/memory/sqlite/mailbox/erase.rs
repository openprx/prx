//! Mailbox-family erasure: persist fence, join delivery/IPC, remove payload,
//! verify independent coverage. The global MA eraser owns its final receipt.
use super::{
    SqliteMailbox,
    gc::require_scope,
    model::{OutboxMeta, decode, encode},
    receipt, storage_error, stream,
};
use crate::messaging::mailbox::{MailboxError, MailboxResult, MailboxScope, REPLAY_SECONDS};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxErasure {
    pub erase_id: Uuid,
    pub remaining: i64,
}
impl SqliteMailbox {
    /// `drain` cancels the scope's queue/worker/TUI caches and waits for their
    /// acknowledgments. It runs outside the writer; leases cover selected
    /// bodies until the queue/IPC owner actually releases them.
    pub fn erase_scope(
        &self,
        scope: &MailboxScope,
        drain: &dyn Fn() -> MailboxResult<()>,
    ) -> MailboxResult<MailboxErasure> {
        let now = self.clock.now();
        if now < 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        let erase_id = {
            let writer = self.memory.pool.write();
            let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
            require_scope(&tx, scope)?;
            let old = tx
                .query_row(
                    "SELECT erase_id FROM agent_erase_fences WHERE scope_id=?1",
                    [scope.scope_id.to_string()],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(storage_error)?;
            let id = if let Some(id) = old {
                Uuid::parse_str(&id).map_err(|_| MailboxError::CorruptState)?
            } else {
                let id = Uuid::new_v4();
                let next = scope.generation.checked_add(1).ok_or(MailboxError::InvalidMetadata)?;
                tx.execute("INSERT INTO agent_erase_fences(scope_id,erase_id,state,next_generation) VALUES (?1,?2,'draining',?3)",params![scope.scope_id.to_string(),id.to_string(),next]).map_err(storage_error)?;
                id
            };
            tx.commit().map_err(storage_error)?;
            id
        };
        drain()?;
        self.memory.mailbox_runtime.drain_and_join(scope.scope_id);
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        let mut query=tx.prepare("SELECT stream_id FROM prx_mailbox_seq WHERE recipient_ma_scope_id=?1 UNION SELECT stream_id FROM prx_mailbox_receipts WHERE ma_scope_id=?1 AND state<>2").map_err(storage_error)?;
        let ids = query
            .query_map([scope.scope_id.to_string()], |r| r.get::<_, i64>(0))
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for id in ids {
            let mut saved = stream::load(&tx, id)?;
            if !saved.retired {
                stream::retire(&tx, &mut saved, now)?;
            }
        }
        let mut query = tx
            .prepare("SELECT id FROM prx_mailbox_receipts WHERE ma_scope_id=?1")
            .map_err(storage_error)?;
        let ids = query
            .query_map([scope.scope_id.to_string()], |r| r.get::<_, i64>(0))
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for id in ids {
            let mut saved = receipt::load(&tx, id)?;
            if saved.scope != *scope {
                return Err(MailboxError::CorruptState);
            }
            saved.meta.body_erased_at.get_or_insert(now);
            tx.execute(
                "UPDATE prx_mailbox_receipts SET receipt_meta=?1 WHERE id=?2",
                params![encode(&saved.meta)?, id],
            )
            .map_err(storage_error)?;
            tx.execute("INSERT OR IGNORE INTO agent_erase_manifest(erase_id,store,record_key) VALUES (?1,'prx_mailbox_messages',?2)",params![erase_id.to_string(),id.to_string()]).map_err(storage_error)?;
        }
        tx.execute(
            "DELETE FROM prx_mailbox_messages WHERE ma_scope_id=?1",
            [scope.scope_id.to_string()],
        )
        .map_err(storage_error)?;
        let mut query = tx
            .prepare("SELECT id,envelope_meta,state FROM prx_mailbox_outbox WHERE ma_scope_id=?1")
            .map_err(storage_error)?;
        let outboxes = query
            .query_map([scope.scope_id.to_string()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for (id, json, state) in outboxes {
            let mut meta: OutboxMeta = decode(&json)?;
            meta.body_erased_at.get_or_insert(now);
            meta.body_mime = None;
            meta.to_pin_token = None;
            meta.updated_at = now;
            let terminal = *meta.terminal_at.get_or_insert(now);
            let expiry = terminal.checked_add(REPLAY_SECONDS).ok_or(MailboxError::CorruptState)?;
            let state = if state < 2 {
                meta.last_error = Some("history_pending".into());
                3
            } else {
                state
            };
            tx.execute(
                "UPDATE prx_mailbox_outbox SET body_bytes=NULL,envelope_meta=?1,state=?2,expires_at=?3 WHERE id=?4",
                params![encode(&meta)?, state, expiry, id],
            )
            .map_err(storage_error)?;
            tx.execute("INSERT OR IGNORE INTO agent_erase_manifest(erase_id,store,record_key) VALUES (?1,'prx_mailbox_outbox',?2)",params![erase_id.to_string(),id.to_string()]).map_err(storage_error)?;
        }
        // Scoped mirrors and child JSON are payload, unlike receipts/parents.
        for table in ["conversation_turns", "message_events", "child_sessions"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE ma_scope_id=?1"),
                [scope.scope_id.to_string()],
            )
            .map_err(storage_error)?;
        }
        let remaining = verify_coverage(&tx, scope)?;
        tx.commit().map_err(storage_error)?;
        if remaining != 0 {
            return Err(MailboxError::Fenced);
        }
        // Do not claim the entire 39-store MA family is erased. Its controller
        // consumes this verified adapter report before writing a global receipt.
        Ok(MailboxErasure { erase_id, remaining })
    }
}

fn verify_coverage(conn: &rusqlite::Connection, scope: &MailboxScope) -> MailboxResult<i64> {
    let mut remaining = 0i64;
    for (table, extra) in [
        ("prx_mailbox_messages", ""),
        ("prx_mailbox_outbox", "AND body_bytes IS NOT NULL"),
        ("conversation_turns", ""),
        ("message_events", ""),
        ("child_sessions", ""),
    ] {
        remaining = remaining.saturating_add(
            conn.query_row(
                &format!("SELECT count(*) FROM {table} WHERE ma_scope_id=?1 {extra}"),
                [scope.scope_id.to_string()],
                |r| r.get::<_, i64>(0),
            )
            .map_err(storage_error)?,
        );
    }
    // Never mistake NULL/orphan linkage for a completed erase. A legacy child
    // cannot prove a workspace, so it conservatively blocks this adapter.
    let children:i64=conn.query_row("SELECT count(*) FROM child_sessions c WHERE c.workspace_id IS NULL OR c.owner_id IS NULL OR c.instance_id IS NULL OR c.session_id IS NULL OR c.agent_key IS NULL OR c.ma_scope_id IS NULL OR c.incarnation IS NULL OR c.generation IS NULL OR NOT EXISTS(SELECT 1 FROM agent_scopes a WHERE a.scope_id=c.ma_scope_id AND a.workspace_id=c.workspace_id AND a.owner_id=c.owner_id AND a.incarnation=c.incarnation AND a.generation=c.generation)",[],|r|r.get(0)).map_err(storage_error)?;
    remaining = remaining.saturating_add(children);
    for table in ["prx_mailbox_messages", "prx_mailbox_outbox"] {
        let body = if table == "prx_mailbox_outbox" {
            "AND b.body_bytes IS NOT NULL"
        } else {
            ""
        };
        let orphan:i64=conn.query_row(&format!("SELECT count(*) FROM {table} b WHERE b.workspace_id=?1 AND b.owner_id=?2 {body} AND NOT EXISTS(SELECT 1 FROM agent_scopes a WHERE a.scope_id=b.ma_scope_id AND a.workspace_id=b.workspace_id AND a.owner_id=b.owner_id AND a.incarnation=b.incarnation AND a.generation=b.generation)"),params![scope.workspace_id,scope.owner_id],|r|r.get(0)).map_err(storage_error)?;
        remaining = remaining.saturating_add(orphan);
    }
    Ok(remaining)
}
