//! Dormant mailbox contracts. Authority is runtime input, never deserialized
//! from a model/tool request. SQLite owns all durable mutations.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{CausalChain, DeliveryState, MessageAuth, MessageOrigin, PrxMessage, RemotePeerAttr, SessionAddr};
use crate::config::agent_name::AgentName;

pub const REPLAY_SECONDS: i64 = 604_800;
pub const GAP_SECONDS: i64 = 30;
pub const MAX_HOLES: usize = 1024;
pub const MAX_QUEUE: i64 = 64;
pub const MAX_ROWS: i64 = 4096;
pub const MAX_RECEIPTS: i64 = 16_384;
pub const MAX_BYTES: i64 = 64 * 1024 * 1024;

/// Stable refusals, shared by store, IPC and dormant send adapters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MailboxError {
    #[error("invalid_address")]
    InvalidAddress,
    #[error("invalid_metadata")]
    InvalidMetadata,
    #[error("invalid_mime")]
    InvalidMime,
    #[error("body_too_large")]
    BodyTooLarge,
    #[error("natural_key_collision")]
    NaturalKeyCollision,
    #[error("idempotency_conflict")]
    IdempotencyConflict,
    #[error("id_reuse")]
    IdReuse,
    #[error("seq_conflict")]
    SeqConflict,
    #[error("seq_gap")]
    SeqAlreadyConsumed,
    #[error("seq_stream_retired")]
    SeqStreamRetired,
    #[error("stale_incarnation")]
    StaleIncarnation,
    #[error("recipient_pin_expired")]
    PinExpired,
    #[error("recipient_pin_invalid")]
    InvalidPin,
    #[error("history_pending")]
    Fenced,
    #[error("stale_run")]
    StaleRun,
    #[error("unknown_parent")]
    UnknownParent,
    #[error("chain_depth_exceeded")]
    ChainDepth,
    #[error("queue_full")]
    QueueFull,
    #[error("quota_{0}")]
    Quota(&'static str),
    #[error("rate_limited")]
    RateLimited,
    #[error("remote_tree_closed")]
    RemoteTreeClosed,
    #[error("remote_tree_resource_exhausted: send")]
    RemoteSendExhausted,
    #[error("mailbox_role_busy")]
    RoleBusy,
    #[error("mailbox_receipt_expired")]
    ReceiptExpired,
    #[error("mailbox_corrupt_state")]
    CorruptState,
    #[error("mailbox_storage_error")]
    Storage,
    #[error("mailbox_unauthorized")]
    Unauthorized,
    #[error("mailbox_identity_immutable")]
    IdentityImmutable,
}

pub type MailboxResult<T> = Result<T, MailboxError>;

/// Injected clock. Tests advance it; they do not wait for retention/gap timers.
pub trait MailboxClock: Send + Sync {
    fn now(&self) -> i64;
}

pub struct SystemMailboxClock;
impl MailboxClock for SystemMailboxClock {
    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

/// Immutable scope snapshot. A snapshot is not authority until the store
/// checks its exact registry row, agent, current incarnation and fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailboxScope {
    pub workspace_id: String,
    pub owner_id: String,
    pub scope_id: Uuid,
    pub incarnation: Uuid,
    pub generation: i64,
}

/// Verified runtime sender and one authorized destination. No Deserialize.
#[derive(Clone)]
pub struct MailboxAuthority {
    pub(crate) workspace_id: String,
    pub(crate) source: SessionAddr,
    pub(crate) destination: SessionAddr,
    pub(crate) origin: MessageOrigin,
    pub(crate) auth: Option<MessageAuth>,
    pub(crate) remote: Option<RemotePeerAttr>,
}

impl MailboxAuthority {
    /// Local runtime caller. Cross-instance/owner delivery requires the
    /// authenticated peer path, which is registered only by activation.
    pub fn local(workspace_id: String, source: SessionAddr, destination: SessionAddr) -> MailboxResult<Self> {
        validate_address(&source)?;
        validate_address(&destination)?;
        bounded(&workspace_id, 4096)?;
        if source.instance != destination.instance || source.owner != destination.owner {
            return Err(MailboxError::Unauthorized);
        }
        Ok(Self {
            workspace_id,
            source,
            destination,
            origin: MessageOrigin::LocalModel,
            auth: None,
            remote: None,
        })
    }

    /// The caller must already have verified peer key possession and the
    /// grant for this exact recipient. This is not a wire parsing API.
    pub fn verified_peer(
        workspace_id: String,
        source: SessionAddr,
        destination: SessionAddr,
        auth: MessageAuth,
    ) -> MailboxResult<Self> {
        bounded(&workspace_id, 4096)?;
        validate_address(&source)?;
        validate_address(&destination)?;
        if !matches!(auth.method.as_str(), "jwt" | "mtls") {
            return Err(MailboxError::Unauthorized);
        }
        Ok(Self {
            workspace_id,
            source,
            destination,
            origin: MessageOrigin::RemotePeer,
            auth: Some(auth),
            remote: None,
        })
    }

    /// Outgoing authority is constructed by the local runtime after its send
    /// policy/peer resolver authorized this destination; never from tool JSON.
    pub fn outbound(workspace_id: String, source: SessionAddr, destination: SessionAddr) -> MailboxResult<Self> {
        bounded(&workspace_id, 4096)?;
        validate_address(&source)?;
        validate_address(&destination)?;
        if source.instance == destination.instance && source.owner != destination.owner {
            return Err(MailboxError::Unauthorized);
        }
        Ok(Self {
            workspace_id,
            source,
            destination,
            origin: MessageOrigin::LocalModel,
            auth: None,
            remote: None,
        })
    }

    /// Local descendants inherit attribution only from sealed runtime input.
    #[must_use]
    pub fn inherit_remote(mut self, inherited: &super::InheritedProvenance) -> Self {
        if self.origin != MessageOrigin::RemotePeer {
            self.remote = Some(inherited.remote.clone());
        }
        self
    }

    pub(crate) fn authorize(&self, message: &PrxMessage) -> MailboxResult<()> {
        if message.to != self.destination {
            return Err(MailboxError::Unauthorized);
        }
        Ok(())
    }
}

/// Durable accept/adopt result; the wire UUID is distinct from SQLite rowids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxReceipt {
    pub row_id: i64,
    pub message_id: Uuid,
    pub stream_id: Uuid,
    pub seq: u64,
    pub state: DeliveryState,
    pub chain: CausalChain,
    pub remote: Option<RemotePeerAttr>,
    pub adopted_turn_id: Option<i64>,
    pub adopted_message_event_id: Option<Uuid>,
}

/// Publishing returns status only. Payloads leave the store exclusively
/// through scoped pending-outbox leases, never through a status replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxPublished {
    pub row_id: i64,
    pub message_id: Uuid,
    pub stream_id: Uuid,
    pub seq: u64,
    pub state: MailboxSendState,
    pub ack_state: Option<DeliveryState>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxSendState {
    Pending,
    Sent,
    Acked,
    Failed,
}

pub(crate) fn bounded(value: &str, max: usize) -> MailboxResult<()> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        return Err(MailboxError::InvalidAddress);
    }
    Ok(())
}

pub(crate) fn agent_key(agent: &str) -> MailboxResult<String> {
    AgentName::parse(agent).map_err(|_| MailboxError::InvalidAddress)?;
    Ok(if agent == "main" {
        "m".to_owned()
    } else {
        format!("n:{agent}")
    })
}

pub(crate) fn validate_address(address: &SessionAddr) -> MailboxResult<()> {
    for field in [&address.instance, &address.owner, &address.session] {
        bounded(field, 512)?;
    }
    agent_key(&address.agent)?;
    Ok(())
}

pub(crate) fn validate_body(mime: &str, bytes: &[u8]) -> MailboxResult<()> {
    if mime.is_empty() || mime.len() > 255 || !mime.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(MailboxError::InvalidMime);
    }
    if mime.len().saturating_add(bytes.len()) > super::MAX_BODY_BYTES {
        return Err(MailboxError::BodyTooLarge);
    }
    Ok(())
}

pub(crate) fn natural_key(domain: &[u8], fields: &[&str]) -> MailboxResult<Vec<u8>> {
    let mut hash = Sha256::new();
    hash.update(domain);
    for value in fields {
        let length = u64::try_from(value.len()).map_err(|_| MailboxError::InvalidMetadata)?;
        hash.update(length.to_be_bytes());
        hash.update(value.as_bytes());
    }
    Ok(hash.finalize().to_vec())
}

pub(crate) fn ledger_key(workspace: &str, message: &PrxMessage) -> MailboxResult<Vec<u8>> {
    natural_key(
        b"prx-mailbox-ledger-v1",
        &[
            workspace,
            &message.to.owner,
            &message.to.session,
            &message.to.agent,
            &message.from.instance,
            message.idempotency.as_str(),
        ],
    )
}

pub(crate) fn outbox_key(workspace: &str, message: &PrxMessage) -> MailboxResult<Vec<u8>> {
    natural_key(
        b"prx-mailbox-outbox-v1",
        &[
            workspace,
            &message.from.owner,
            &message.from.session,
            &message.from.agent,
            &message.to.instance,
            message.idempotency.as_str(),
        ],
    )
}
