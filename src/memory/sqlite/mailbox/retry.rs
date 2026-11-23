//! Durable outbox scheduling and first-pin binding. No payload leaves unleased.
use super::{
    SqliteMailbox,
    model::{OutboxMeta, ParentMeta, decode, encode},
    pin, storage_error,
};
use crate::messaging::{
    PrxMessage, RecipientPin, SessionAddr,
    mailbox::{MailboxError, MailboxResult, MailboxScope},
};
use rusqlite::{Transaction, TransactionBehavior, params};
impl SqliteMailbox {
    pub(crate) fn outbox_sources(&self) -> MailboxResult<Vec<(MailboxScope, SessionAddr)>> {
        let writer = self.memory.pool.write();
        let mut query = writer.prepare("SELECT DISTINCT a.workspace_id,a.owner_id,a.scope_id,a.incarnation,a.generation,s.from_addr FROM prx_mailbox_seq s JOIN agent_scopes a ON s.sender_ma_scope_id=a.scope_id WHERE EXISTS(SELECT 1 FROM prx_mailbox_outbox o WHERE o.stream_id=s.stream_id AND o.state IN(0,1)) LIMIT 4096").map_err(storage_error)?;
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        rows.into_iter()
            .map(|(workspace, owner, scope, inc, generation, source)| {
                Ok((
                    pin::scope_from_row(workspace, owner, scope, inc, generation)?,
                    decode(&source)?,
                ))
            })
            .collect()
    }
    pub(crate) fn retry_outbox(
        &self,
        scope: &MailboxScope,
        source: &SessionAddr,
        message: &PrxMessage,
        next: Option<i64>,
        error: &str,
    ) -> MailboxResult<()> {
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        pin::check_scope(&tx, scope, source, None, true)?;
        let (id,raw):(i64,String)=tx.query_row("SELECT id,envelope_meta FROM prx_mailbox_outbox WHERE message_id=?1 AND ma_scope_id=?2 AND state IN(0,1)", params![message.id.as_bytes().as_slice(),scope.scope_id.to_string()], |r| Ok((r.get(0)?,r.get(1)?))).map_err(storage_error)?;
        let mut meta: OutboxMeta = decode(&raw)?;
        let now = self.clock.now();
        meta.updated_at = now;
        meta.last_error = Some(error.into());
        meta.next_attempt_at = next;
        meta.attempts = meta.attempts.saturating_add(1);
        let expired = now.saturating_sub(meta.created_at) >= 86400;
        let terminal = expired || next.is_none();
        if expired {
            meta.last_error = Some("expired".into());
        }
        if terminal {
            meta.terminal_at = Some(now);
        }
        tx.execute(
            "UPDATE prx_mailbox_outbox SET state=?1,envelope_meta=?2,expires_at=?3 WHERE id=?4",
            params![
                if terminal { 3 } else { 0 },
                encode(&meta)?,
                if terminal {
                    Some(now + crate::messaging::mailbox::REPLAY_SECONDS)
                } else {
                    None
                },
                id
            ],
        )
        .map_err(storage_error)?;
        tx.commit().map_err(storage_error)
    }
    pub(crate) fn outbox_retry(&self, scope: &MailboxScope, message: &PrxMessage) -> MailboxResult<(u32, i64)> {
        let writer = self.memory.pool.write();
        let raw: String = writer
            .query_row(
                "SELECT envelope_meta FROM prx_mailbox_outbox WHERE message_id=?1 AND ma_scope_id=?2",
                params![message.id.as_bytes().as_slice(), scope.scope_id.to_string()],
                |r| r.get(0),
            )
            .map_err(storage_error)?;
        let meta: OutboxMeta = decode(&raw)?;
        Ok((meta.attempts, meta.created_at))
    }
    pub(crate) fn refresh_outbox_pin(
        &self,
        scope: &MailboxScope,
        source: &SessionAddr,
        message: &mut PrxMessage,
        pin: RecipientPin,
        expires: i64,
    ) -> MailboxResult<()> {
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        pin::check_scope(&tx, scope, source, None, true)?;
        let (id,raw,stream_id):(i64,String,i64)=tx.query_row("SELECT id,envelope_meta,stream_id FROM prx_mailbox_outbox WHERE message_id=?1 AND ma_scope_id=?2 AND state IN(0,1)",params![message.id.as_bytes().as_slice(),scope.scope_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(storage_error)?;
        let mut meta: OutboxMeta = decode(&raw)?;
        if !meta.to_pin.incarnation.is_nil()
            && (meta.to_pin.incarnation != pin.incarnation || meta.to_pin.generation != pin.generation)
        {
            // Old payloads stay bound to their old incarnation. Future sends
            // get a new wire stream instead of reusing the receiver's tombstone.
            let mut stream = super::stream::load(&tx, stream_id)?;
            super::stream::retire(&tx, &mut stream, self.clock.now())?;
            tx.commit().map_err(storage_error)?;
            return Err(MailboxError::StaleIncarnation);
        }
        message.to_pin = pin;
        let digest = crate::messaging::request_digest(message);
        meta.to_pin.incarnation = message.to_pin.incarnation;
        meta.to_pin.generation = message.to_pin.generation;
        meta.to_pin_token = Some(message.to_pin.token.clone());
        meta.pin_expires_at = expires;
        meta.common.request_digest = digest.clone();
        tx.execute(
            "UPDATE prx_mailbox_outbox SET envelope_meta=?1 WHERE id=?2",
            params![encode(&meta)?, id],
        )
        .map_err(storage_error)?;
        let raw: String = tx
            .query_row("SELECT parent_meta FROM prx_mailbox_parents WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .map_err(storage_error)?;
        let mut parent: ParentMeta = decode(&raw)?;
        parent.request_digest = digest;
        tx.execute(
            "UPDATE prx_mailbox_parents SET parent_meta=?1 WHERE id=?2",
            params![encode(&parent)?, id],
        )
        .map_err(storage_error)?;
        tx.commit().map_err(storage_error)
    }
}

impl crate::memory::SqliteMemory {
    /// Owner-scoped send states without message payloads or recipient pins.
    pub(crate) fn mailbox_send_status(
        &self,
        workspace: &str,
        owner: &str,
        session: &str,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let reader = self.pool.read()?;
        let conn = reader.connection()?;
        let mut query=conn.prepare("SELECT o.message_id,o.state,o.envelope_meta,s.to_addr FROM prx_mailbox_outbox o JOIN prx_mailbox_seq s ON s.stream_id=o.stream_id JOIN agent_scopes a ON a.scope_id=o.ma_scope_id WHERE a.workspace_id=?1 AND a.owner_id=?2 AND s.from_addr IN(SELECT from_addr FROM prx_mailbox_seq WHERE json_extract(from_addr,'$.session')=?3) AND NOT EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id=a.scope_id) ORDER BY o.id DESC LIMIT 64")?;
        let rows = query
            .query_map(params![workspace, owner, session], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(id,state,raw,target)| {
            let meta:OutboxMeta=decode(&raw)?;
            let status=if meta.last_error.as_deref()==Some("expired") {"expired"} else {match state {0|1=>"queued",2=>"accepted",_=>"failed"}};
            Ok(serde_json::json!({"message_id":uuid::Uuid::from_slice(&id)?,"to":serde_json::from_str::<SessionAddr>(&target)?,"status":status,"error":meta.last_error}))
        }).collect()
    }
}
