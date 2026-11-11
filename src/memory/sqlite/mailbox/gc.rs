//! Bounded per-scope retention; scope-prefix indexes serve erase and expiry.
use super::{
    SqliteMailbox,
    model::{OutboxMeta, decode},
    receipt, storage_error, stream,
};
use crate::messaging::DeliveryState;
use crate::messaging::mailbox::{MailboxError, MailboxResult, MailboxScope, REPLAY_SECONDS};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MailboxGc {
    pub bodies: usize,
    pub receipts: usize,
    pub outboxes: usize,
    pub parents: usize,
    pub retired_streams: usize,
}

pub(super) fn require_scope(conn: &Connection, scope: &MailboxScope) -> MailboxResult<()> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM agent_scopes WHERE scope_id=?1 AND workspace_id=?2 AND owner_id=?3 AND incarnation=?4 AND generation=?5)",params![scope.scope_id.to_string(),scope.workspace_id,scope.owner_id,scope.incarnation.to_string(),scope.generation],|r|r.get(0)).map_err(storage_error)?;
    if !exists {
        return Err(MailboxError::StaleIncarnation);
    }
    Ok(())
}

impl SqliteMailbox {
    /// Called only under quota pressure, inside the caller's one transaction.
    /// Historical scopes are retained; enumerate them with the existing MA
    /// owner prefix and let each scope/expiry index identify eligible rows.
    pub(super) fn gc_owner_rows(&self, tx: &Connection, workspace: &str, owner: &str) -> MailboxResult<()> {
        let mut query = tx
            .prepare("SELECT scope_id,incarnation,generation FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2")
            .map_err(storage_error)?;
        let scopes = query
            .query_map(params![workspace, owner], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for (id, inc, generation) in scopes {
            let scope = super::pin::scope_from_row(workspace.to_owned(), owner.to_owned(), id, inc, generation)?;
            self.gc_scope_rows(tx, &scope)?;
            // The first pass can drop terminal outboxes; the next can release
            // their expired parents without rolling both back on quota refusal.
            self.gc_scope_rows(tx, &scope)?;
        }
        Ok(())
    }

    pub fn gc_scope(&self, scope: &MailboxScope) -> MailboxResult<MailboxGc> {
        let now = self.clock.now();
        if now < 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        let report = self.gc_scope_rows(&tx, scope)?;
        tx.commit().map_err(storage_error)?;
        Ok(report)
    }

    pub(super) fn gc_scope_rows(&self, tx: &Connection, scope: &MailboxScope) -> MailboxResult<MailboxGc> {
        let now = self.clock.now();
        require_scope(tx, scope)?;
        let mut report = MailboxGc::default();
        let mut query=tx.prepare("SELECT id FROM prx_mailbox_receipts WHERE ma_scope_id=?1 AND expires_at<=?2 AND (state=2 OR json_extract(receipt_meta,'$.body_erased_at') IS NOT NULL) ORDER BY expires_at,id LIMIT 512").map_err(storage_error)?;
        let ids = query
            .query_map(params![scope.scope_id.to_string(), now], |r| r.get::<_, i64>(0))
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for id in ids {
            let saved = receipt::load(tx, id)?;
            if saved.scope != *scope {
                return Err(MailboxError::CorruptState);
            }
            if saved.expires_at > now {
                continue;
            }
            let has_body: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM prx_mailbox_messages WHERE id=?1)",
                    [id],
                    |r| r.get(0),
                )
                .map_err(storage_error)?;
            if has_body {
                if saved.state != DeliveryState::Consumed {
                    continue;
                }
                let consumed = saved.meta.consumed_at.ok_or(MailboxError::CorruptState)?;
                if consumed.checked_add(REPLAY_SECONDS).ok_or(MailboxError::CorruptState)? > now {
                    continue;
                }
                // Body before receipt is a lifetime invariant, not a cascade.
                let count = tx
                    .execute(
                        "DELETE FROM prx_mailbox_messages WHERE id=?1 AND state=2 AND ma_scope_id=?2",
                        params![id, scope.scope_id.to_string()],
                    )
                    .map_err(storage_error)?;
                if count != 1 {
                    return Err(MailboxError::CorruptState);
                }
                report.bodies += count;
            } else if saved.state != DeliveryState::Consumed && saved.meta.body_erased_at.is_none() {
                return Err(MailboxError::CorruptState);
            }
            report.receipts += tx
                .execute("DELETE FROM prx_mailbox_receipts WHERE id=?1", [id])
                .map_err(storage_error)?;
        }
        // Evaluate parents before deleting terminal outboxes in this pass.
        // A parent still referenced at the pass boundary stays until the next
        // pass; parent-first rowid allocation makes that retention harmless.
        report.parents=tx.execute("DELETE FROM prx_mailbox_parents WHERE id IN(SELECT p.id FROM prx_mailbox_parents p WHERE p.ma_scope_id=?1 AND p.expires_at<=?2 AND NOT EXISTS(SELECT 1 FROM prx_mailbox_outbox o WHERE o.id=p.id) ORDER BY p.expires_at,p.id LIMIT 512)",params![scope.scope_id.to_string(),now]).map_err(storage_error)?;
        let mut query=tx.prepare("SELECT id,message_id,envelope_meta FROM prx_mailbox_outbox WHERE ma_scope_id=?1 AND expires_at<=?2 AND state IN(2,3) ORDER BY expires_at,id LIMIT 512").map_err(storage_error)?;
        let outboxes = query
            .query_map(params![scope.scope_id.to_string(), now], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2)?))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for (id, message_id, json) in outboxes {
            let meta: OutboxMeta = decode(&json)?;
            meta.common.validate(super::model::uuid(&message_id)?)?;
            let terminal = meta.terminal_at.ok_or(MailboxError::CorruptState)?;
            if terminal.checked_add(REPLAY_SECONDS).ok_or(MailboxError::CorruptState)? <= now {
                report.outboxes += tx
                    .execute("DELETE FROM prx_mailbox_outbox WHERE id=?1", [id])
                    .map_err(storage_error)?;
            }
        }
        let cutoff = now.saturating_sub(REPLAY_SECONDS);
        let mut query=tx.prepare("SELECT stream_id,updated_at FROM prx_mailbox_seq WHERE sender_ma_scope_id=?1 UNION SELECT stream_id,updated_at FROM prx_mailbox_seq WHERE recipient_ma_scope_id=?1").map_err(storage_error)?;
        let streams = query
            .query_map([scope.scope_id.to_string()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for (id, updated_at) in streams {
            let mut saved = stream::load(tx, id)?;
            saved
                .progress
                .seq_skipped
                .retain(|hole| hole.seq > saved.progress.consume_cursor || hole.skipped_at > cutoff);
            let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM prx_mailbox_messages WHERE stream_id=?1) OR EXISTS(SELECT 1 FROM prx_mailbox_outbox WHERE stream_id=?1) OR EXISTS(SELECT 1 FROM prx_mailbox_receipts WHERE ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?2 AND owner_id=?3) AND stream_id=?1)",params![id,scope.workspace_id,scope.owner_id],|r|r.get(0)).map_err(storage_error)?;
            if !saved.retired && !live && updated_at <= cutoff {
                stream::retire(tx, &mut saved, now)?;
                report.retired_streams += 1;
            } else {
                stream::save_progress(tx, &saved, updated_at)?;
            }
        }
        Ok(report)
    }
}
