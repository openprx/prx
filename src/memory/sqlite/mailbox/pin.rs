//! Recipient pins are HMAC-bound to a scope snapshot, target and verified peer.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use super::{
    SqliteMailbox,
    model::{decode, encode},
    storage_error,
};
use crate::messaging::mailbox::{MailboxAuthority, MailboxError, MailboxResult, MailboxScope, agent_key, bounded};
use crate::messaging::{PrxMessage, RecipientPin, SessionAddr};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PinClaims {
    pub scope: MailboxScope,
    pub target: SessionAddr,
    pub peer_instance: String,
    pub peer_owner: String,
    pub canonical_session_key: String,
    pub expires_at: i64,
}

impl SqliteMailbox {
    /// Resolve only a known runtime target/scope. Never mint a scope or refresh
    /// an existing outbox token as part of delivery.
    pub fn recipient_pin(
        &self,
        authority: &MailboxAuthority,
        scope: &MailboxScope,
        canonical_session_key: &str,
    ) -> MailboxResult<RecipientPin> {
        if authority.destination.instance != self.local_instance || authority.workspace_id != scope.workspace_id {
            return Err(MailboxError::Unauthorized);
        }
        bounded(canonical_session_key, 512)?;
        let writer = self.memory.pool.write();
        check_scope(
            &writer,
            scope,
            &authority.destination,
            Some(canonical_session_key),
            true,
        )?;
        let claims = PinClaims {
            scope: scope.clone(),
            target: authority.destination.clone(),
            peer_instance: authority.source.instance.clone(),
            peer_owner: authority.source.owner.clone(),
            canonical_session_key: canonical_session_key.to_owned(),
            expires_at: self.clock.now().checked_add(300).ok_or(MailboxError::InvalidMetadata)?,
        };
        let payload = encode(&claims)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_ref()).map_err(|_| MailboxError::InvalidPin)?;
        mac.update(b"prx-mailbox-recipient-v1\0");
        mac.update(payload.as_bytes());
        let token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload.as_bytes()),
            hex::encode(mac.finalize().into_bytes())
        );
        Ok(RecipientPin {
            incarnation: scope.incarnation,
            generation: scope.generation,
            token,
        })
    }

    pub(super) fn verify_pin(&self, authority: &MailboxAuthority, message: &PrxMessage) -> MailboxResult<PinClaims> {
        let (payload, signature) = message.to_pin.token.split_once('.').ok_or(MailboxError::InvalidPin)?;
        let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|_| MailboxError::InvalidPin)?;
        let signature = hex::decode(signature).map_err(|_| MailboxError::InvalidPin)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_ref()).map_err(|_| MailboxError::InvalidPin)?;
        mac.update(b"prx-mailbox-recipient-v1\0");
        mac.update(&bytes);
        mac.verify_slice(&signature).map_err(|_| MailboxError::InvalidPin)?;
        let json = std::str::from_utf8(&bytes).map_err(|_| MailboxError::InvalidPin)?;
        let claims: PinClaims = decode(json).map_err(|_| MailboxError::InvalidPin)?;
        if claims.target != message.to
            || claims.scope.workspace_id != authority.workspace_id
            || claims.scope.owner_id != message.to.owner
            || claims.peer_instance != authority.source.instance
            || claims.peer_owner != authority.source.owner
            || claims.target.instance != self.local_instance
        {
            return Err(MailboxError::InvalidPin);
        }
        if claims.scope.incarnation != message.to_pin.incarnation
            || claims.scope.generation != message.to_pin.generation
        {
            return Err(MailboxError::StaleIncarnation);
        }
        if claims.expires_at <= self.clock.now() {
            return Err(MailboxError::PinExpired);
        }
        bounded(&claims.canonical_session_key, 512)?;
        Ok(claims)
    }
}

pub(in crate::memory::sqlite) fn check_scope(
    conn: &Connection,
    scope: &MailboxScope,
    address: &SessionAddr,
    canonical: Option<&str>,
    live: bool,
) -> MailboxResult<()> {
    bounded(&scope.workspace_id, 4096)?;
    bounded(&scope.owner_id, 512)?;
    if scope.owner_id != address.owner || scope.generation < 1 {
        return Err(MailboxError::StaleIncarnation);
    }
    let key = agent_key(&address.agent)?;
    let row=conn.query_row(
        "SELECT a.agent_key,a.session_key,i.state,(SELECT f.state FROM agent_erase_fences f WHERE f.scope_id=a.scope_id)
         FROM agent_scopes a JOIN agent_incarnations i ON i.workspace_id=a.workspace_id AND i.owner_id=a.owner_id AND i.agent_key=a.agent_key AND i.incarnation=a.incarnation
         WHERE a.scope_id=?1 AND a.workspace_id=?2 AND a.owner_id=?3 AND a.incarnation=?4 AND a.generation=?5",
        params![scope.scope_id.to_string(),scope.workspace_id,scope.owner_id,scope.incarnation.to_string(),scope.generation],
        |row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?))
    ).optional().map_err(storage_error)?.ok_or(MailboxError::StaleIncarnation)?;
    if row.0 != key || canonical.is_some_and(|expected| row.1.as_deref().is_some_and(|actual| actual != expected)) {
        return Err(MailboxError::StaleIncarnation);
    }
    if live {
        let latest:i64=conn.query_row("SELECT max(generation) FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2 AND agent_key=?3 AND incarnation=?4 AND session_key IS ?5",params![scope.workspace_id,scope.owner_id,key,scope.incarnation.to_string(),row.1],|r|r.get(0)).map_err(storage_error)?;
        if row.2 != "active" || latest != scope.generation || row.3.as_deref() == Some("erased") {
            return Err(MailboxError::StaleIncarnation);
        }
        if row.3.is_some() {
            return Err(MailboxError::Fenced);
        }
    }
    Ok(())
}

pub(in crate::memory::sqlite) fn scope_from_row(
    workspace: String,
    owner: String,
    id: String,
    incarnation: String,
    generation: i64,
) -> MailboxResult<MailboxScope> {
    let scope_id = Uuid::parse_str(&id).map_err(|_| MailboxError::CorruptState)?;
    let incarnation_uuid = Uuid::parse_str(&incarnation).map_err(|_| MailboxError::CorruptState)?;
    if scope_id.to_string() != id || incarnation_uuid.to_string() != incarnation || generation < 1 {
        return Err(MailboxError::CorruptState);
    }
    Ok(MailboxScope {
        workspace_id: workspace,
        owner_id: owner,
        scope_id,
        incarnation: incarnation_uuid,
        generation,
    })
}
