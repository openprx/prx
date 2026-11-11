//! Explicit dormant adapter used by tools and slash commands.
use super::{SqliteMailbox, storage_error};
use crate::messaging::mailbox::{MailboxError, MailboxPublished, MailboxResult, MailboxScope};
use crate::messaging::send::{MailboxResolver, MailboxSender};
use crate::messaging::{IdempotencyKey, InheritedProvenance, MessageBody, MessageKind, PrxMessage, SessionAddr};
use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;
use uuid::Uuid;

pub struct SqliteMailboxSender {
    store: SqliteMailbox,
    source: SessionAddr,
    scope: MailboxScope,
    resolver: Arc<dyn MailboxResolver>,
    inherited: Option<InheritedProvenance>,
}
impl SqliteMailboxSender {
    pub fn new(
        store: SqliteMailbox,
        source: SessionAddr,
        scope: MailboxScope,
        resolver: Arc<dyn MailboxResolver>,
        inherited: Option<InheritedProvenance>,
    ) -> MailboxResult<Self> {
        crate::messaging::mailbox::validate_address(&source)?;
        if source.instance != store.local_instance || scope.owner_id != source.owner {
            return Err(MailboxError::Unauthorized);
        }
        store.validate_scope(&scope, &source)?;
        Ok(Self {
            store,
            source,
            scope,
            resolver,
            inherited,
        })
    }
}
#[async_trait]
impl MailboxSender for SqliteMailboxSender {
    fn source(&self) -> &SessionAddr {
        &self.source
    }
    async fn send(&self, target: SessionAddr, text: String, mime: String) -> MailboxResult<MailboxPublished> {
        crate::messaging::mailbox::validate_body(&mime, text.as_bytes())?;
        if text.is_empty() {
            return Err(MailboxError::InvalidMetadata);
        }
        let resolved = self.resolver.resolve(&target).await?;
        if resolved.authority.source != self.source
            || resolved.authority.workspace_id != self.scope.workspace_id
            || resolved.authority.destination != target
        {
            return Err(MailboxError::Unauthorized);
        }
        let from = serde_json::to_string(&self.source).map_err(storage_error)?;
        let to = serde_json::to_string(&target).map_err(storage_error)?;
        // Streams survive reconstruction. Only a retired stream gets a new UUID.
        let wire:Option<Vec<u8>>=self.store.memory.pool.write().query_row(
            "SELECT wire_stream_id FROM prx_mailbox_seq WHERE workspace_id=?1 AND owner_id=?2 AND from_addr=?3 AND to_addr=?4 AND state=0 AND sender_ma_scope_id=?5 ORDER BY stream_id DESC LIMIT 1",
            params![self.scope.workspace_id,self.scope.owner_id,from,to,self.scope.scope_id.to_string()],|r|r.get(0)).optional().map_err(storage_error)?;
        let wire = wire
            .map(|bytes| Uuid::from_slice(&bytes).map_err(|_| MailboxError::CorruptState))
            .transpose()?
            .unwrap_or_else(Uuid::new_v4);
        let id = Uuid::new_v4();
        let message = PrxMessage {
            id,
            idempotency: IdempotencyKey::parse(&id.to_string()).map_err(|_| MailboxError::InvalidMetadata)?,
            stream_id: Some(wire),
            seq: 0,
            ts: self.store.clock.now(),
            from: self.source.clone(),
            to: target,
            to_pin: resolved.pin,
            in_reply_to: None,
            hop_count: 0,
            ancestry: Vec::new(),
            kind: MessageKind::Data,
            body: MessageBody {
                mime,
                bytes: text.into_bytes(),
            },
            origin: None,
            auth: None,
            exec: None,
            canonical_hash: None,
        };
        self.store.publish(
            &resolved.authority,
            message,
            &self.scope,
            resolved.pin_expires_at,
            self.inherited.as_ref(),
        )
    }
}
