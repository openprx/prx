//! Content-free diagnostics at the exact failing authorization boundary.
use super::MailboxError;
#[derive(Debug, Clone, Copy)]
pub(crate) enum UnauthorizedReason {
    CallerMissing,
    WorkerSenderUnavailable,
    LocalInstanceMismatch,
    LocalOwnerMismatch,
    PeerAuthenticationMethod,
    DestinationMismatch,
    SenderOwnerMismatch,
    SenderIdentityMismatch,
    SenderWorkspaceMismatch,
    ReceiverAgentMismatch,
    ReceiverUnknown,
    ReceiverAgentInactive,
    InboundDisabledOrConfigInvalid,
    PeerAbsent,
    MessageOriginMissing,
    PeerAuthenticationMissing,
    PinScopeMismatch,
    OutboxScopeMismatch,
    PublishScopeMismatch,
    StreamScopeMismatch,
    AdoptScopeMismatch,
    DeliverScopeMismatch,
    WorkerScopeMismatch,
    WorkerSenderMismatch,
}
impl UnauthorizedReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CallerMissing => "caller_missing",
            Self::WorkerSenderUnavailable => "worker_sender_unavailable",
            Self::LocalInstanceMismatch => "local_instance_mismatch",
            Self::LocalOwnerMismatch => "local_owner_mismatch",
            Self::PeerAuthenticationMethod => "peer_authentication_method",
            Self::DestinationMismatch => "destination_mismatch",
            Self::SenderOwnerMismatch => "sender_owner_mismatch",
            Self::SenderIdentityMismatch => "sender_identity_mismatch",
            Self::SenderWorkspaceMismatch => "sender_workspace_mismatch",
            Self::ReceiverAgentMismatch => "receiver_agent_mismatch",
            Self::ReceiverUnknown => "receiver_unknown",
            Self::ReceiverAgentInactive => "receiver_agent_inactive",
            Self::InboundDisabledOrConfigInvalid => "inbound_disabled_or_config_invalid",
            Self::PeerAbsent => "peer_absent",
            Self::MessageOriginMissing => "message_origin_missing",
            Self::PeerAuthenticationMissing => "peer_authentication_missing",
            Self::PinScopeMismatch => "pin_scope_mismatch",
            Self::OutboxScopeMismatch => "outbox_scope_mismatch",
            Self::PublishScopeMismatch => "publish_scope_mismatch",
            Self::StreamScopeMismatch => "stream_scope_mismatch",
            Self::AdoptScopeMismatch => "adopt_scope_mismatch",
            Self::DeliverScopeMismatch => "deliver_scope_mismatch",
            Self::WorkerScopeMismatch => "worker_scope_mismatch",
            Self::WorkerSenderMismatch => "worker_sender_mismatch",
        }
    }
}
pub(crate) fn unauthorized(reason: UnauthorizedReason) -> MailboxError {
    tracing::warn!(reason = reason.as_str(), "mailbox authorization refused");
    MailboxError::Unauthorized
}
