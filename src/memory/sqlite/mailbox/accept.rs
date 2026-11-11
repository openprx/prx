//! Authenticated new accept and receipt replay, in the contract lookup order.
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use uuid::Uuid;

use super::{
    SqliteMailbox,
    model::{Common, DeliveryMeta, ReceiptMeta, encode},
    pin::check_scope,
    receipt, storage_error,
    stream::{self, Role},
};
use crate::messaging::mailbox::{
    MAX_BYTES, MAX_HOLES, MAX_QUEUE, MAX_RECEIPTS, MAX_ROWS, MailboxAuthority, MailboxError, MailboxReceipt,
    MailboxResult, MailboxScope, REPLAY_SECONDS, ledger_key, validate_address, validate_body,
};
use crate::messaging::{MessageKind, MessageOrigin, PrxMessage, RemotePeerAttr};

impl SqliteMailbox {
    pub fn accept(&self, authority: &MailboxAuthority, mut message: PrxMessage) -> MailboxResult<MailboxReceipt> {
        authority.authorize(&message)?;
        if message.to.instance != self.local_instance {
            return Err(MailboxError::Unauthorized);
        }
        // Identity/origin/auth are runtime-owned, never trusted from the wire.
        message.from = authority.source.clone();
        message.origin = Some(authority.origin);
        message.auth = authority.auth.clone();
        validate_address(&message.from)?;
        validate_address(&message.to)?;
        let now = self.clock.now();
        if now < 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        let key = ledger_key(&authority.workspace_id, &message)?;
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        if let Some(saved) = receipt::by_key(&tx, &key)? {
            saved.matches(&tx, &authority.workspace_id, &message)?;
            return saved.result(&tx);
        }
        // Non-Data frames update sender status elsewhere, never become inbox
        // rows and never generate another acknowledgment.
        if message.kind != MessageKind::Data {
            return Err(MailboxError::InvalidMetadata);
        }
        message.validate().map_err(|_| MailboxError::InvalidMetadata)?;
        validate_body(&message.body.mime, &message.body.bytes)?;
        if message.seq == 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        receipt::reject_id_reuse(&tx, &authority.workspace_id, &message.to.owner, &message)?;
        let claims = self.verify_pin(authority, &message)?;
        check_scope(
            &tx,
            &claims.scope,
            &message.to,
            Some(&claims.canonical_session_key),
            true,
        )?;
        if message
            .exec
            .as_ref()
            .and_then(|exec| exec.run.as_deref())
            .is_some_and(|run| {
                Some(run)
                    != self
                        .memory
                        .mailbox_runtime
                        .execution(&message.to, &claims.scope)
                        .as_deref()
            })
        {
            return Err(MailboxError::StaleRun);
        }
        let mut stream = self.ensure_stream(&tx, &message, &claims.scope, Role::Recipient)?;
        if stream.retired {
            // A rebind refusal may have just retired it. Persist that terminal
            // transition, but never insert/charge the refused Data.
            tx.commit().map_err(storage_error)?;
            return Err(MailboxError::SeqStreamRetired);
        }
        if message.seq <= stream.progress.consume_cursor
            || stream.progress.seq_skipped.iter().any(|h| h.seq == message.seq)
        {
            return Err(MailboxError::SeqAlreadyConsumed);
        }
        let slot: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM prx_mailbox_messages WHERE stream_id=?1 AND state IN(0,1,2) AND seq=?2)",
                params![stream.id, message.seq.to_be_bytes().as_slice()],
                |r| r.get(0),
            )
            .map_err(storage_error)?;
        if slot {
            return Err(MailboxError::SeqConflict);
        }
        self.memory.mailbox_runtime.check_rate(authority, now)?;
        match incoming_quota(&tx, &claims.scope, &message, now) {
            Err(MailboxError::Quota(_)) => {
                self.gc_owner_rows(&tx, &claims.scope.workspace_id, &claims.scope.owner_id)?;
                stream = stream::load(&tx, stream.id)?;
                if stream.retired {
                    tx.commit().map_err(storage_error)?;
                    return Err(MailboxError::SeqStreamRetired);
                }
                incoming_quota(&tx, &claims.scope, &message, now)?;
            }
            result => result?,
        }
        reserve_gaps(&tx, &stream, &message)?;
        receipt::derive_chain(&tx, &authority.workspace_id, &claims.scope.owner_id, &mut message)?;
        let remote = if authority.origin == MessageOrigin::RemotePeer {
            let auth = authority.auth.as_ref().ok_or(MailboxError::Unauthorized)?;
            Some(RemotePeerAttr {
                issuer: auth.issuer.clone(),
                subject: auth.subject.clone(),
                message_id: message.id,
            })
        } else {
            authority.remote.clone()
        };
        let common = Common::from_message(&message, remote)?;
        common.validate(message.id)?;
        let meta = ReceiptMeta {
            common,
            acceptance_id: Uuid::new_v4(),
            wire_ts: message.ts,
            canonical_session_key: claims.canonical_session_key,
            accepted_at: now,
            delivered_at: None,
            consumed_at: None,
            updated_at: now,
            body_erased_at: None,
            adopted_message_event_id: None,
        };
        let expiry = now.checked_add(REPLAY_SECONDS).ok_or(MailboxError::InvalidMetadata)?;
        tx.execute("INSERT INTO prx_mailbox_receipts(ledger_key,stream_id,seq,workspace_id,owner_id,ma_scope_id,incarnation,generation,idempotency_key,message_id,state,expires_at,receipt_meta) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,?11,?12)",params![key,stream.id,message.seq.to_be_bytes().as_slice(),claims.scope.workspace_id,claims.scope.owner_id,claims.scope.scope_id.to_string(),claims.scope.incarnation.to_string(),claims.scope.generation,message.idempotency.as_str(),message.id.as_bytes().as_slice(),expiry,encode(&meta)?]).map_err(storage_error)?;
        let id = tx.last_insert_rowid();
        let delivery = DeliveryMeta {
            to_pin_token: message.to_pin.token,
            pin_expires_at: claims.expires_at,
        };
        tx.execute("INSERT INTO prx_mailbox_messages(id,stream_id,seq,workspace_id,owner_id,ma_scope_id,incarnation,generation,state,body_mime,body_bytes,delivery_meta) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,?11)",params![id,stream.id,message.seq.to_be_bytes().as_slice(),claims.scope.workspace_id,claims.scope.owner_id,claims.scope.scope_id.to_string(),claims.scope.incarnation.to_string(),claims.scope.generation,message.body.mime,message.body.bytes,encode(&delivery)?]).map_err(storage_error)?;
        stream::save_progress(&tx, &stream, now)?;
        let result = receipt::load(&tx, id)?.result(&tx)?;
        tx.commit().map_err(storage_error)?;
        self.memory.mailbox_runtime.record_rate(authority, now)?;
        super::crash_barrier("after_accept_commit")?;
        Ok(result)
    }
}

fn incoming_quota(conn: &Connection, scope: &MailboxScope, message: &PrxMessage, now: i64) -> MailboxResult<()> {
    let (rows,bytes):(i64,i64)=conn.query_row("SELECT count(*),coalesce(sum(length(body_bytes)+length(CAST(body_mime AS BLOB))),0) FROM prx_mailbox_messages WHERE ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2)",params![scope.workspace_id,scope.owner_id],|r|Ok((r.get(0)?,r.get(1)?))).map_err(storage_error)?;
    if rows >= MAX_ROWS {
        return Err(MailboxError::Quota("rows"));
    }
    let added = i64::try_from(message.body.bytes.len().saturating_add(message.body.mime.len()))
        .map_err(|_| MailboxError::BodyTooLarge)?;
    if bytes.saturating_add(added) > MAX_BYTES {
        return Err(MailboxError::Quota("bodies"));
    }
    let receipts:i64=conn.query_row("SELECT count(*) FROM prx_mailbox_receipts WHERE ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2)",params![scope.workspace_id,scope.owner_id],|r|r.get(0)).map_err(storage_error)?;
    if receipts >= MAX_RECEIPTS {
        return Err(MailboxError::Quota("receipts"));
    }
    let target = encode(&message.to)?;
    let pending:i64=conn.query_row("SELECT count(*) FROM prx_mailbox_messages WHERE ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2) AND state IN(0,1) AND stream_id IN(SELECT stream_id FROM prx_mailbox_seq WHERE workspace_id=?1 AND owner_id=?2 AND to_addr=?3)",params![scope.workspace_id,scope.owner_id,target],|r|r.get(0)).map_err(storage_error)?;
    if pending >= MAX_QUEUE {
        return Err(MailboxError::QueueFull);
    }
    // The expiry suffix is accepted_at + the fixed replay window. This
    // injected-clock bucket reads only recent scope-index entries.
    let threshold = now.checked_add(REPLAY_SECONDS).ok_or(MailboxError::InvalidMetadata)?;
    let rate:i64=conn.query_row("SELECT count(*) FROM prx_mailbox_receipts WHERE ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2) AND expires_at>=?3 AND stream_id IN(SELECT stream_id FROM prx_mailbox_seq WHERE workspace_id=?1 AND owner_id=?2 AND to_addr=?4)",params![scope.workspace_id,scope.owner_id,threshold,target],|r|r.get(0)).map_err(storage_error)?;
    if rate >= 10 {
        return Err(MailboxError::RateLimited);
    }
    Ok(())
}

fn reserve_gaps(conn: &Connection, stream: &stream::Stream, message: &PrxMessage) -> MailboxResult<()> {
    let mut query = conn
        .prepare("SELECT seq FROM prx_mailbox_messages WHERE stream_id=?1 AND state IN(0,1,2) AND seq>?2 ORDER BY seq")
        .map_err(storage_error)?;
    let raw = query
        .query_map(
            params![stream.id, stream.progress.consume_cursor.to_be_bytes().as_slice()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    let mut present = raw
        .iter()
        .map(|b| super::model::sequence(b))
        .collect::<MailboxResult<std::collections::BTreeSet<_>>>()?;
    present.insert(message.seq);
    let max = present.last().copied().ok_or(MailboxError::CorruptState)?;
    let span = max
        .checked_sub(stream.progress.consume_cursor)
        .ok_or(MailboxError::CorruptState)?;
    let occupied = u64::try_from(present.len()).map_err(|_| MailboxError::CorruptState)?;
    let future_holes = span.saturating_sub(occupied);
    let retained = u64::try_from(
        stream
            .progress
            .seq_skipped
            .iter()
            .filter(|h| h.seq <= stream.progress.consume_cursor)
            .count(),
    )
    .map_err(|_| MailboxError::CorruptState)?;
    if future_holes.saturating_add(retained) > u64::try_from(MAX_HOLES).map_err(|_| MailboxError::CorruptState)? {
        return Err(MailboxError::Quota("seq"));
    }
    Ok(())
}
