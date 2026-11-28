//! Mailbox-family erasure: persist fence, join delivery/IPC, remove payload,
//! verify independent coverage. The global MA eraser owns its final receipt.
use super::{
    SqliteMailbox,
    gc::require_scope,
    model::{OutboxMeta, ReceiptMeta, decode, encode},
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
        deadline: std::time::Duration,
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
        if !self.memory.mailbox_runtime.drain_and_join(scope.scope_id, deadline) {
            return Err(MailboxError::Fenced);
        }
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
            erase_outbox_body(&tx, id, &json, state, now)?;
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
        // The content-free write log names targets and runs of this scope.
        tx.execute(
            "DELETE FROM memory_write_log WHERE agent_partition=?1",
            [scope.scope_id.to_string()],
        )
        .map_err(storage_error)?;
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

/// Erase the body of one outbox row: payload NULL, replay window from now, a
/// row not yet delivered (state below 2) closed as `history_pending`.
fn erase_outbox_body(
    tx: &rusqlite::Connection,
    id: i64,
    envelope_meta_json: &str,
    state: i64,
    now: i64,
) -> MailboxResult<()> {
    let mut meta: OutboxMeta = decode(envelope_meta_json)?;
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
    Ok(())
}

/// Erase the mailbox bodies of the messages adopted as the conversation rows in
/// `event_ids_json` (a JSON array of event ids): the receiver's stored body, the
/// receipt's `body_erased_at`, and the body in the sender's outbox rows of the
/// same message id. The link is `adopted_message_event_id` in the receipt meta,
/// which survives the tombstone of the adopted row. A receipt or outbox row whose
/// metadata does not decode is skipped. Returns the number of receipts handled.
pub(in crate::memory::sqlite) fn erase_adopted_bodies(
    tx: &rusqlite::Connection,
    workspace_id: &str,
    owner_id: &str,
    event_ids_json: &str,
    now: i64,
) -> anyhow::Result<usize> {
    let receipts = {
        let mut query = tx.prepare(
            "SELECT id, message_id, receipt_meta FROM prx_mailbox_receipts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND json_valid(receipt_meta)
               AND json_extract(receipt_meta, '$.adopted_message_event_id') IN (SELECT value FROM json_each(?3))",
        )?;
        query
            .query_map(params![workspace_id, owner_id, event_ids_json], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut handled = 0usize;
    for (id, message_id, meta_json) in receipts {
        let Ok(mut meta) = decode::<ReceiptMeta>(&meta_json) else {
            continue;
        };
        tx.execute("DELETE FROM prx_mailbox_messages WHERE id = ?1", [id])?;
        meta.body_erased_at.get_or_insert(now);
        let Ok(encoded) = encode(&meta) else {
            continue;
        };
        tx.execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta = ?1 WHERE id = ?2",
            params![encoded, id],
        )?;
        let outboxes = {
            let mut query = tx.prepare(
                "SELECT id, envelope_meta, state FROM prx_mailbox_outbox
                 WHERE workspace_id = ?1 AND owner_id = ?2 AND message_id = ?3",
            )?;
            query
                .query_map(params![workspace_id, owner_id, message_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (outbox_id, json, state) in outboxes {
            match erase_outbox_body(tx, outbox_id, &json, state, now) {
                Ok(()) | Err(MailboxError::InvalidMetadata | MailboxError::CorruptState) => {}
                Err(error) => return Err(error.into()),
            }
        }
        handled += 1;
    }
    Ok(handled)
}

/// Erase the messages that the runs in `runs_json` (a JSON array of run ids)
/// sent and that no one has adopted yet: receipts still accepted or delivered
/// whose recorded `sender_run_id` is one of them. Each slot is closed the way a
/// stale slot is (`receipt::terminal_stale_slot`: the body row goes, the receipt
/// gets `body_erased_at`, the slot becomes a skipped hole so later messages of
/// the stream still flow, and the sender's outbox slot is nacked), then the
/// body of the sender's outbox row of the same message in the same stream is
/// erased. A later adopt of the receipt is refused as stale, so no conversation
/// row and no woken turn appears. A receipt or outbox row whose metadata does
/// not decode is skipped. Returns the number of receipts handled.
pub(in crate::memory::sqlite) fn erase_pending_from_runs(
    tx: &rusqlite::Connection,
    workspace_id: &str,
    owner_id: &str,
    runs_json: &str,
    now: i64,
) -> anyhow::Result<usize> {
    let ids = {
        let mut query = tx.prepare(
            "SELECT id FROM prx_mailbox_receipts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND state IN (0, 1)
               AND sender_run_id IN (SELECT value FROM json_each(?3)) ORDER BY id",
        )?;
        query
            .query_map(params![workspace_id, owner_id, runs_json], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut handled = 0usize;
    for id in ids {
        let Ok(mut saved) = receipt::load(tx, id) else {
            continue;
        };
        if saved.meta.body_erased_at.is_some() {
            continue;
        }
        let closed = match stream::load(tx, saved.stream_id) {
            Ok(mut slot) => receipt::terminal_stale_slot(tx, &mut slot, &mut saved, now),
            Err(error) => Err(error),
        };
        if closed.is_err() {
            // The slot could not be closed as a hole (for example the stream's
            // hole table is full): the body still goes.
            saved.meta.body_erased_at.get_or_insert(now);
            saved.meta.updated_at = now;
            tx.execute("DELETE FROM prx_mailbox_messages WHERE id = ?1", [id])?;
            tx.execute(
                "UPDATE prx_mailbox_receipts SET receipt_meta = ?1 WHERE id = ?2",
                params![encode(&saved.meta)?, id],
            )?;
        }
        let outboxes = {
            let mut query = tx.prepare(
                "SELECT id, envelope_meta, state FROM prx_mailbox_outbox
                 WHERE workspace_id = ?1 AND owner_id = ?2 AND message_id = ?3 AND stream_id = ?4",
            )?;
            query
                .query_map(
                    params![
                        workspace_id,
                        owner_id,
                        saved.message_id.as_bytes().as_slice(),
                        saved.stream_id
                    ],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (outbox_id, json, state) in outboxes {
            match erase_outbox_body(tx, outbox_id, &json, state, now) {
                Ok(()) | Err(MailboxError::InvalidMetadata | MailboxError::CorruptState) => {}
                Err(error) => return Err(error.into()),
            }
        }
        handled += 1;
    }
    Ok(handled)
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
