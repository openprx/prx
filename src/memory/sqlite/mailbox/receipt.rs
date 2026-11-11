//! Receipt identity is the durable replay/adoption checkpoint.
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use super::{
    model::{ReceiptMeta, decode, sequence, uuid},
    pin::{check_scope, scope_from_row},
    storage_error, stream,
};
use crate::messaging::mailbox::{MailboxError, MailboxReceipt, MailboxResult, MailboxScope, bounded};
use crate::messaging::{DeliveryState, IdempotencyKey, PrxMessage};

pub(super) struct Receipt {
    pub id: i64,
    pub ledger_key: Vec<u8>,
    pub stream_id: i64,
    pub seq: u64,
    pub scope: MailboxScope,
    pub idempotency: IdempotencyKey,
    pub message_id: Uuid,
    pub state: DeliveryState,
    pub expires_at: i64,
    pub adopted_turn_id: Option<i64>,
    pub meta: ReceiptMeta,
}

pub(super) const fn state(code: i64) -> MailboxResult<DeliveryState> {
    match code {
        0 => Ok(DeliveryState::Accepted),
        1 => Ok(DeliveryState::Delivered),
        2 => Ok(DeliveryState::Consumed),
        _ => Err(MailboxError::CorruptState),
    }
}
pub(super) const fn code(state: DeliveryState) -> i64 {
    match state {
        DeliveryState::Accepted => 0,
        DeliveryState::Delivered => 1,
        DeliveryState::Consumed => 2,
    }
}

pub(super) fn load(conn: &Connection, id: i64) -> MailboxResult<Receipt> {
    let raw=conn.query_row("SELECT ledger_key,stream_id,seq,workspace_id,owner_id,ma_scope_id,incarnation,generation,idempotency_key,message_id,state,expires_at,adopted_turn_id,receipt_meta FROM prx_mailbox_receipts WHERE id=?1",[id],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,i64>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,i64>(7)?,r.get::<_,String>(8)?,r.get::<_,Vec<u8>>(9)?,r.get::<_,i64>(10)?,r.get::<_,i64>(11)?,r.get::<_,Option<i64>>(12)?,r.get::<_,String>(13)?))).map_err(storage_error)?;
    let message_id = uuid(&raw.9)?;
    let meta: ReceiptMeta = decode(&raw.13)?;
    meta.common.validate(message_id)?;
    bounded(&meta.canonical_session_key, 512)?;
    let state = state(raw.10)?;
    let consumed = state == DeliveryState::Consumed;
    if consumed != raw.12.is_some()
        || consumed != meta.adopted_message_event_id.is_some()
        || consumed != meta.consumed_at.is_some()
        || (state != DeliveryState::Accepted) != meta.delivered_at.is_some()
        || raw.0.len() != 32
        || raw.11 < 0
        || meta.accepted_at < 0
        || meta.updated_at < 0
        || meta.delivered_at.is_some_and(|t| t < 0)
        || meta.consumed_at.is_some_and(|t| t < 0)
        || meta.body_erased_at.is_some_and(|t| t < 0)
        || raw.12.is_some_and(|id| id <= 0)
    {
        return Err(MailboxError::CorruptState);
    }
    let scope = scope_from_row(raw.3, raw.4, raw.5, raw.6, raw.7)?;
    let stream = stream::load(conn, raw.1)?;
    if stream.workspace_id != scope.workspace_id || stream.owner_id != scope.owner_id {
        return Err(MailboxError::CorruptState);
    }
    check_scope(conn, &scope, &stream.to, Some(&meta.canonical_session_key), false)?;
    let seq = sequence(&raw.2)?;
    if seq == 0 {
        return Err(MailboxError::CorruptState);
    }
    Ok(Receipt {
        id,
        ledger_key: raw.0,
        stream_id: raw.1,
        seq,
        scope,
        idempotency: IdempotencyKey::parse(&raw.8).map_err(|_| MailboxError::CorruptState)?,
        message_id,
        state,
        expires_at: raw.11,
        adopted_turn_id: raw.12,
        meta,
    })
}

pub(super) fn by_key(conn: &Connection, key: &[u8]) -> MailboxResult<Option<Receipt>> {
    let id = conn
        .query_row("SELECT id FROM prx_mailbox_receipts WHERE ledger_key=?1", [key], |r| {
            r.get::<_, i64>(0)
        })
        .optional()
        .map_err(storage_error)?;
    let saved = id.map(|id| load(conn, id)).transpose()?;
    if saved
        .as_ref()
        .is_some_and(|receipt| receipt.ledger_key.as_slice() != key)
    {
        return Err(MailboxError::CorruptState);
    }
    Ok(saved)
}
impl Receipt {
    pub fn matches(&self, conn: &Connection, workspace: &str, message: &PrxMessage) -> MailboxResult<()> {
        let stream = stream::load(conn, self.stream_id)?;
        if workspace != self.scope.workspace_id
            || message.to.owner != stream.to.owner
            || message.to.session != stream.to.session
            || message.to.agent != stream.to.agent
            || message.from.instance != stream.from.instance
            || self.idempotency != message.idempotency
        {
            return Err(MailboxError::NaturalKeyCollision);
        }
        if self.meta.common.request_digest != crate::messaging::request_digest(message) {
            return Err(MailboxError::IdempotencyConflict);
        }
        Ok(())
    }
    pub fn result(&self, conn: &Connection) -> MailboxResult<MailboxReceipt> {
        Ok(MailboxReceipt {
            row_id: self.id,
            message_id: self.message_id,
            stream_id: stream::load(conn, self.stream_id)?.wire_id,
            seq: self.seq,
            state: self.state,
            chain: self.meta.common.chain(self.message_id),
            remote: self.meta.common.remote_peer.clone(),
            adopted_turn_id: self.adopted_turn_id,
            adopted_message_event_id: self.meta.adopted_message_event_id,
        })
    }
}

/// Covering index: every historical scope costs a seek, but non-matching
/// message ids never materialize receipt payloads.
pub(super) fn reject_id_reuse(
    conn: &Connection,
    workspace: &str,
    owner: &str,
    message: &PrxMessage,
) -> MailboxResult<()> {
    let mut query=conn.prepare("SELECT id FROM prx_mailbox_receipts WHERE ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2) AND message_id=?3").map_err(storage_error)?;
    let ids = query
        .query_map(params![workspace, owner, message.id.as_bytes().as_slice()], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    for id in ids {
        let receipt = load(conn, id)?;
        if stream::load(conn, receipt.stream_id)?.to.instance == message.to.instance {
            return Err(MailboxError::IdReuse);
        }
    }
    Ok(())
}

pub(super) fn parent_chain(
    conn: &Connection,
    workspace: &str,
    owner: &str,
    id: Uuid,
) -> MailboxResult<(u8, Vec<Uuid>)> {
    let mut query=conn.prepare("SELECT r.id FROM prx_mailbox_receipts r WHERE r.ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2) AND r.message_id=?3").map_err(storage_error)?;
    let ids = query
        .query_map(params![workspace, owner, id.as_bytes().as_slice()], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    for row_id in ids {
        let receipt = load(conn, row_id)?;
        // Retained receipts alone are not an inbound parent lookup surface:
        // erase may have removed both its body and its transcript.
        let retained:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM prx_mailbox_messages WHERE id=?1) OR EXISTS(SELECT 1 FROM message_events WHERE event_id=?2 AND workspace_id=?3 AND owner_id=?4)",params![row_id,receipt.meta.adopted_message_event_id.map(|id|id.to_string()),workspace,owner],|r|r.get(0)).map_err(storage_error)?;
        if retained {
            return Ok((receipt.meta.common.hop_count, receipt.meta.common.ancestry));
        }
    }
    let parent = conn
        .query_row(
            "SELECT parent_meta FROM prx_mailbox_parents WHERE workspace_id=?1 AND owner_id=?2 AND message_id=?3",
            params![workspace, owner, id.as_bytes().as_slice()],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(storage_error)?;
    if let Some(json) = parent {
        let meta: super::model::ParentMeta = decode(&json)?;
        let unique: std::collections::HashSet<_> = meta.ancestry.iter().collect();
        if meta.request_digest.len() != 64
            || !meta
                .request_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || meta.created_at < 0
            || unique.len() != meta.ancestry.len()
            || meta.ancestry.len() != usize::from(meta.hop_count)
            || meta.hop_count > 8
            || meta.ancestry.contains(&id)
        {
            return Err(MailboxError::CorruptState);
        }
        return Ok((meta.hop_count, meta.ancestry));
    }
    let prefix = format!("prx_mailbox/{id}/");
    let mut query=conn.prepare("SELECT source_ref_json FROM message_events WHERE workspace_id=?1 AND idempotency_key>=?2 AND idempotency_key<?3 AND owner_id=?4 AND source='prx_mailbox'").map_err(storage_error)?;
    let events = query
        .query_map(params![workspace, prefix, format!("{prefix}~"), owner], |r| {
            r.get::<_, String>(0)
        })
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    if let Some(json) = events.first() {
        let event: super::model::EventRef = decode(json)?;
        if event.message_id != id || event.workspace_id != workspace || event.owner_id != owner {
            return Err(MailboxError::CorruptState);
        }
        event.common.validate(id)?;
        return Ok((event.common.hop_count, event.common.ancestry));
    }
    Err(MailboxError::UnknownParent)
}

pub(super) fn derive_chain(
    conn: &Connection,
    workspace: &str,
    owner: &str,
    message: &mut PrxMessage,
) -> MailboxResult<()> {
    let Some(parent) = message.in_reply_to else {
        message.hop_count = 0;
        message.ancestry.clear();
        return Ok(());
    };
    let (depth, mut ancestry) = parent_chain(conn, workspace, owner, parent)?;
    if depth >= 8 || ancestry.contains(&message.id) || parent == message.id {
        return Err(MailboxError::ChainDepth);
    }
    ancestry.push(parent);
    message.hop_count = depth.checked_add(1).ok_or(MailboxError::ChainDepth)?;
    message.ancestry = ancestry;
    Ok(())
}
