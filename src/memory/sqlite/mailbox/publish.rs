//! Durable sender allocation, shared parent/outbox ids and atomic send charge.
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{
    SqliteMailbox,
    model::{Common, OutboxMeta, ParentMeta, TargetPin, decode, encode, sequence, uuid},
    pin::check_scope,
    receipt, storage_error,
    stream::{self, Role},
};
use crate::memory::sqlite::remote_tree::{self, RemoteTreeCaps, RemoteTreeError, RemoteTreeId, RemoteTreeResource};
use crate::messaging::mailbox::{
    MAX_BYTES, MAX_ROWS, MailboxAuthority, MailboxError, MailboxPublished, MailboxResult, MailboxScope,
    MailboxSendState, outbox_key, validate_address, validate_body,
};
use crate::messaging::{InheritedProvenance, MessageKind, MessageOrigin, PrxMessage, stamp_outbound};

impl SqliteMailbox {
    /// The runtime resolves a recipient pin before calling. The receiver alone
    /// can verify remote pins; this sender preserves the exact supplied pin.
    pub fn publish(
        &self,
        authority: &MailboxAuthority,
        mut message: PrxMessage,
        sender: &MailboxScope,
        pin_expires_at: i64,
        inherited: Option<&InheritedProvenance>,
    ) -> MailboxResult<MailboxPublished> {
        authority.authorize(&message)?;
        if authority.source.instance != self.local_instance
            || authority.workspace_id != sender.workspace_id
            || authority.origin == MessageOrigin::RemotePeer
        {
            return Err(MailboxError::Unauthorized);
        }
        message.from = authority.source.clone();
        message.origin = Some(MessageOrigin::LocalModel);
        message.auth = None;
        validate_address(&message.from)?;
        validate_address(&message.to)?;
        validate_body(&message.body.mime, &message.body.bytes)?;
        if message.kind != MessageKind::Data || message.stream_id.is_none() {
            return Err(MailboxError::InvalidMetadata);
        }
        if let Some(inherited) = inherited {
            stamp_outbound(&inherited.chain, &mut message);
        }
        let key = outbox_key(&authority.workspace_id, &message)?;
        let now = self.clock.now();
        if now < 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        let existing=tx.query_row("SELECT id,stream_id,seq,idempotency_key,envelope_meta,message_id,state FROM prx_mailbox_outbox WHERE outbox_key=?1",[&key],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Vec<u8>>(5)?,r.get::<_,i64>(6)?))).optional().map_err(storage_error)?;
        if let Some((row_id, stream_id, seq, idempotency, json, message_id, status)) = existing {
            let stream = stream::load(&tx, stream_id)?;
            if stream.workspace_id != authority.workspace_id
                || stream.from.owner != message.from.owner
                || stream.from.session != message.from.session
                || stream.from.agent != message.from.agent
                || stream.to.instance != message.to.instance
                || idempotency != message.idempotency.as_str()
            {
                return Err(MailboxError::NaturalKeyCollision);
            }
            let meta: OutboxMeta = decode(&json)?;
            meta.common.validate(uuid(&message_id)?)?;
            // Allocation is sender-owned; a retry of its provisional request
            // must compare using the originally allocated seq, never charge it again.
            message.seq = sequence(&seq)?;
            if meta.common.request_digest != crate::messaging::request_digest(&message) {
                return Err(MailboxError::IdempotencyConflict);
            }
            let state = match status {
                0 => MailboxSendState::Pending,
                1 => MailboxSendState::Sent,
                2 => MailboxSendState::Acked,
                3 => MailboxSendState::Failed,
                _ => return Err(MailboxError::CorruptState),
            };
            return Ok(MailboxPublished {
                row_id,
                message_id: message.id,
                stream_id: stream.wire_id,
                seq: message.seq,
                state,
                ack_state: meta.ack_state,
                error: meta.last_error,
            });
        }
        check_scope(&tx, sender, &message.from, None, true)?;
        if pin_expires_at <= now || message.to_pin.token.is_empty() {
            return Err(MailboxError::PinExpired);
        }
        if message.to.instance == self.local_instance {
            let claims = self.verify_pin(authority, &message)?;
            check_scope(
                &tx,
                &claims.scope,
                &message.to,
                Some(&claims.canonical_session_key),
                true,
            )?;
        }
        let mut stream = self.ensure_stream(&tx, &message, sender, Role::Sender)?;
        if stream.retired {
            tx.commit().map_err(storage_error)?;
            return Err(MailboxError::SeqStreamRetired);
        }
        if stream.progress.allocation_exhausted {
            return Err(MailboxError::Quota("seq"));
        }
        let occupancy = || -> MailboxResult<(i64, i64)> {
            tx.query_row("SELECT (SELECT count(*) FROM prx_mailbox_seq WHERE workspace_id=?1 AND owner_id=?2)+(SELECT count(*) FROM prx_mailbox_parents WHERE workspace_id=?1 AND owner_id=?2),coalesce((SELECT sum(length(body_bytes)+length(CAST(json_extract(envelope_meta,'$.body_mime') AS BLOB))) FROM prx_mailbox_outbox WHERE ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2)),0)",params![sender.workspace_id,sender.owner_id],|r|Ok((r.get(0)?,r.get(1)?))).map_err(storage_error)
        };
        let added = i64::try_from(message.body.bytes.len().saturating_add(message.body.mime.len()))
            .map_err(|_| MailboxError::BodyTooLarge)?;
        let (mut metadata, mut bytes) = occupancy()?;
        if metadata >= MAX_ROWS || bytes.saturating_add(added) > MAX_BYTES {
            self.gc_owner_rows(&tx, &sender.workspace_id, &sender.owner_id)?;
            stream = stream::load(&tx, stream.id)?;
            if stream.retired {
                tx.commit().map_err(storage_error)?;
                return Err(MailboxError::SeqStreamRetired);
            }
            (metadata, bytes) = occupancy()?;
        }
        if metadata >= MAX_ROWS {
            return Err(MailboxError::Quota("streams"));
        }
        if bytes.saturating_add(added) > MAX_BYTES {
            return Err(MailboxError::Quota("outbox"));
        }
        message.seq = stream.progress.next_seq;
        receipt::derive_chain(&tx, &sender.workspace_id, &sender.owner_id, &mut message)?;
        let common = Common::from_message(&message, inherited.map(|p| p.remote.clone()))?;
        common.validate(message.id)?;
        if let Some(inherited) = inherited {
            let tree = RemoteTreeId {
                workspace_id: sender.workspace_id.clone(),
                owner_id: sender.owner_id.clone(),
                trigger_message_id: inherited.chain.trigger.to_string(),
            };
            let caps = RemoteTreeCaps {
                max_spawned: self.limits.max_spawned,
                max_scheduled: self.limits.max_scheduled,
                max_connections: self.limits.max_connections,
                max_sends: self.limits.max_sends,
            };
            remote_tree::charge_in_transaction(
                &tx,
                &tree,
                RemoteTreeResource::Send,
                "",
                Some(&sender.scope_id.to_string()),
                caps,
            )
            .map_err(|error| match error {
                RemoteTreeError::Closed => MailboxError::RemoteTreeClosed,
                RemoteTreeError::Exhausted { .. } => MailboxError::RemoteSendExhausted,
                RemoteTreeError::Storage => MailboxError::Storage,
            })?;
        }
        let parent = ParentMeta {
            request_digest: common.request_digest.clone(),
            hop_count: common.hop_count,
            ancestry: common.ancestry.clone(),
            created_at: now,
        };
        let expiry = now
            .checked_add(crate::messaging::mailbox::REPLAY_SECONDS)
            .ok_or(MailboxError::InvalidMetadata)?;
        // Allocate from parents FIRST. A retained highest parent still owns its
        // rowid even after its outbox is GC'd; outbox-first would alias it.
        tx.execute("INSERT INTO prx_mailbox_parents(stream_id,seq,workspace_id,owner_id,ma_scope_id,incarnation,generation,message_id,expires_at,parent_meta) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![stream.id,message.seq.to_be_bytes().as_slice(),sender.workspace_id,sender.owner_id,sender.scope_id.to_string(),sender.incarnation.to_string(),sender.generation,message.id.as_bytes().as_slice(),expiry,encode(&parent)?]).map_err(storage_error)?;
        let id = tx.last_insert_rowid();
        let meta = OutboxMeta {
            common,
            to_pin: TargetPin {
                incarnation: message.to_pin.incarnation,
                generation: message.to_pin.generation,
            },
            wire_ts: message.ts,
            body_mime: Some(message.body.mime.clone()),
            to_pin_token: Some(message.to_pin.token.clone()),
            pin_expires_at,
            created_at: now,
            updated_at: now,
            next_attempt_at: None,
            terminal_at: None,
            body_erased_at: None,
            ack_state: None,
            last_error: None,
        };
        tx.execute("INSERT INTO prx_mailbox_outbox(id,outbox_key,stream_id,seq,workspace_id,owner_id,ma_scope_id,incarnation,generation,idempotency_key,message_id,state,body_bytes,envelope_meta) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,?12,?13)",params![id,key,stream.id,message.seq.to_be_bytes().as_slice(),sender.workspace_id,sender.owner_id,sender.scope_id.to_string(),sender.incarnation.to_string(),sender.generation,message.idempotency.as_str(),message.id.as_bytes().as_slice(),message.body.bytes,encode(&meta)?]).map_err(storage_error)?;
        if let Some(next) = message.seq.checked_add(1) {
            stream.progress.next_seq = next;
        } else {
            stream.progress.allocation_exhausted = true;
        }
        stream::save_progress(&tx, &stream, now)?;
        tx.commit().map_err(storage_error)?;
        Ok(MailboxPublished {
            row_id: id,
            message_id: message.id,
            stream_id: stream.wire_id,
            seq: message.seq,
            state: MailboxSendState::Pending,
            ack_state: None,
            error: None,
        })
    }
}
