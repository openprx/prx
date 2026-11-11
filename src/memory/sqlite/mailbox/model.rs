//! Typed compact records. All JSON is checked before a transaction can commit.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::messaging::mailbox::{MAX_HOLES, MailboxError, MailboxResult, MailboxScope};
use crate::messaging::{CausalChain, MessageAuth, MessageOrigin, PrxMessage, RemotePeerAttr};

pub(super) fn encode<T: Serialize>(value: &T) -> MailboxResult<String> {
    serde_json::to_string(value).map_err(|_| MailboxError::InvalidMetadata)
}
pub(super) fn decode<T: DeserializeOwned + Serialize>(json: &str) -> MailboxResult<T> {
    let value: T = serde_json::from_str(json).map_err(|_| MailboxError::InvalidMetadata)?;
    if encode(&value)? != json {
        return Err(MailboxError::InvalidMetadata);
    }
    Ok(value)
}
pub(super) fn uuid(bytes: &[u8]) -> MailboxResult<Uuid> {
    Uuid::from_slice(bytes).map_err(|_| MailboxError::CorruptState)
}
pub(super) fn sequence(bytes: &[u8]) -> MailboxResult<u64> {
    let array: [u8; 8] = bytes.try_into().map_err(|_| MailboxError::CorruptState)?;
    Ok(u64::from_be_bytes(array))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Common {
    pub request_digest: String,
    pub exec_run: Option<String>,
    pub in_reply_to: Option<Uuid>,
    pub hop_count: u8,
    pub ancestry: Vec<Uuid>,
    pub origin: MessageOrigin,
    pub auth: Option<MessageAuth>,
    pub remote_peer: Option<RemotePeerAttr>,
}
impl Common {
    pub fn from_message(message: &PrxMessage, remote_peer: Option<RemotePeerAttr>) -> MailboxResult<Self> {
        Ok(Self {
            request_digest: crate::messaging::request_digest(message),
            exec_run: message.exec.as_ref().and_then(|exec| exec.run.clone()),
            in_reply_to: message.in_reply_to,
            hop_count: message.hop_count,
            ancestry: message.ancestry.clone(),
            origin: message.origin.ok_or(MailboxError::Unauthorized)?,
            auth: message.auth.clone(),
            remote_peer,
        })
    }
    pub fn validate(&self, id: Uuid) -> MailboxResult<()> {
        if self.request_digest.len() != 64
            || !self
                .request_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(MailboxError::InvalidMetadata);
        }
        if self.ancestry.len() != usize::from(self.hop_count)
            || self.ancestry.len() > 8
            || self.in_reply_to != self.ancestry.last().copied()
            || self.ancestry.contains(&id)
        {
            return Err(MailboxError::InvalidMetadata);
        }
        let unique: std::collections::HashSet<_> = self.ancestry.iter().collect();
        if unique.len() != self.ancestry.len() || (self.origin == MessageOrigin::RemotePeer) != self.auth.is_some() {
            return Err(MailboxError::InvalidMetadata);
        }
        if let Some(auth) = &self.auth {
            if !matches!(auth.method.as_str(), "jwt" | "mtls") || self.remote_peer.is_none() {
                return Err(MailboxError::InvalidMetadata);
            }
        }
        Ok(())
    }
    pub fn chain(&self, id: Uuid) -> CausalChain {
        CausalChain {
            trigger: id,
            root: self.ancestry.first().copied().unwrap_or(id),
            depth: self.hop_count,
            ancestry: self.ancestry.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReceiptMeta {
    pub common: Common,
    pub acceptance_id: Uuid,
    pub wire_ts: i64,
    pub canonical_session_key: String,
    pub accepted_at: i64,
    pub delivered_at: Option<i64>,
    pub consumed_at: Option<i64>,
    pub updated_at: i64,
    pub body_erased_at: Option<i64>,
    pub adopted_message_event_id: Option<Uuid>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeliveryMeta {
    pub to_pin_token: String,
    pub pin_expires_at: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TargetPin {
    pub incarnation: Uuid,
    pub generation: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutboxMeta {
    pub common: Common,
    pub to_pin: TargetPin,
    pub wire_ts: i64,
    pub body_mime: Option<String>,
    pub to_pin_token: Option<String>,
    pub pin_expires_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub next_attempt_at: Option<i64>,
    pub terminal_at: Option<i64>,
    pub body_erased_at: Option<i64>,
    pub ack_state: Option<crate::messaging::DeliveryState>,
    pub last_error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ParentMeta {
    pub request_digest: String,
    pub hop_count: u8,
    pub ancestry: Vec<Uuid>,
    pub created_at: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Hole {
    pub seq: u64,
    pub skipped_at: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Progress {
    pub next_seq: u64,
    pub allocation_exhausted: bool,
    pub consume_cursor: u64,
    pub deliver_cursor: u64,
    pub seq_skipped: Vec<Hole>,
    pub gap_seq: Option<u64>,
    pub gap_started_at: Option<i64>,
    pub created_at: i64,
    pub retired_at: Option<i64>,
}
impl Progress {
    pub const fn new(now: i64) -> Self {
        Self {
            next_seq: 1,
            allocation_exhausted: false,
            consume_cursor: 0,
            deliver_cursor: 0,
            seq_skipped: Vec::new(),
            gap_seq: None,
            gap_started_at: None,
            created_at: now,
            retired_at: None,
        }
    }
    pub fn validate(&self, retired: bool) -> MailboxResult<()> {
        if self.next_seq == 0
            || self.consume_cursor > self.deliver_cursor
            || self.seq_skipped.len() > MAX_HOLES
            || self.gap_seq.is_some() != self.gap_started_at.is_some()
            || self.allocation_exhausted && self.next_seq != u64::MAX
            || retired != self.retired_at.is_some()
            || self.created_at < 0
            || self.retired_at.is_some_and(|t| t < 0)
        {
            return Err(MailboxError::CorruptState);
        }
        let mut last = 0;
        for hole in &self.seq_skipped {
            if hole.seq <= last || hole.skipped_at < 0 {
                return Err(MailboxError::CorruptState);
            }
            last = hole.seq;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventRef {
    pub workspace_id: String,
    pub owner_id: String,
    pub session_id: String,
    pub agent: String,
    pub from_instance: String,
    pub idempotency_key: String,
    pub message_id: Uuid,
    pub acceptance_id: Uuid,
    pub local_stream_id: i64,
    pub wire_stream_id: Uuid,
    pub seq: u64,
    pub scope: MailboxScope,
    pub common: Common,
}
