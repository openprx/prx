//! Repository-owned recovery authentication. No execution authority is granted.
//! Blocking methods must run on the repository's blocking worker. Callers must
//! verify legacy/new-format evidence before submitting exact event snapshots.

use serde::{Deserialize, Serialize};

/// The key a v3 artifact needs is missing, retired or unusable. Recovery for
/// that artifact is unavailable; startup and chat continue, with no marker,
/// no replay and no regeneration.
#[derive(Debug, thiserror::Error)]
#[error("recovery key unavailable: {0}")]
pub struct RecoveryKeyUnavailable(pub String);

#[must_use]
pub fn is_key_unavailable(error: &anyhow::Error) -> bool {
    error.is::<RecoveryKeyUnavailable>()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryChain {
    pub workspace_id: String,
    pub owner_id: String,
    pub source: String,
    pub run_id: String,
}

/// Exact repository bytes, rechecked in the attestation writer transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEvidence {
    pub event_id: String,
    pub record: Vec<u8>,
}

/// Opaque, versioned verifier facts, including original capsule, lifetime,
/// generation, transport and lineage. Their interpretation belongs to AUTH-02.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPublication {
    pub chain: RecoveryChain,
    pub input: Option<RecoveryEvidence>,
    pub terminal: Option<RecoveryEvidence>,
    pub facts: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReceipt {
    pub version: u32,
    pub backend_namespace: String,
    pub key_id: String,
    pub state: String,
    pub facts: Vec<u8>,
    pub mac: Vec<u8>,
}

impl RecoveryReceipt {
    /// The opaque pending facts the receipt was published with. Call only on a
    /// receipt returned by `read_recovery_receipt`, which has authenticated it.
    pub fn pending_facts(&self) -> anyhow::Result<Vec<u8>> {
        #[derive(Deserialize)]
        struct Bound {
            pending: Vec<u8>,
        }
        Ok(serde_json::from_slice::<Bound>(&self.facts)?.pending)
    }

    /// The opaque settled facts the receipt was published with. Call only on a
    /// settled receipt returned by `read_recovery_receipt`.
    pub fn settled_facts(&self) -> anyhow::Result<Vec<u8>> {
        #[derive(Deserialize)]
        struct Bound {
            completion: Vec<u8>,
        }
        Ok(serde_json::from_slice::<Bound>(&self.facts)?.completion)
    }
}

/// Facts exceed the backend's publication or serialized receipt budget.
#[derive(Debug, thiserror::Error)]
#[error("recovery facts exceed size limit of {limit} bytes")]
pub struct RecoveryFactsTooLarge {
    pub limit: usize,
}

/// Repository-owned MAC; key material never crosses this interface.
#[derive(Clone, Debug)]
pub struct RecoveryTag {
    pub key_id: String,
    pub tag: [u8; 32],
}

#[derive(Clone, Copy, Debug)]
pub enum RecoveryMacDomain {
    CapsuleV3,
    InputDigestV3,
    StateV3,
    QuarantineKeyV3,
    QuarantineV3,
    TransportV3,
    InputKeyV3,
    ClaimV3,
    UsageDigestV1,
    OutboxV1,
    OutboxStateV1,
}

impl RecoveryMacDomain {
    pub(crate) const fn bytes(self) -> &'static [u8] {
        match self {
            Self::CapsuleV3 => b"prx-recovery-capsule-v3",
            Self::InputDigestV3 => b"prx-recovery-input-digest-v3",
            Self::StateV3 => b"prx-recovery-state-v3",
            Self::QuarantineKeyV3 => b"prx-recovery-quarantine-key-v3",
            Self::QuarantineV3 => b"prx-recovery-quarantine-v3",
            Self::TransportV3 => b"prx-recovery-transport-v3",
            Self::InputKeyV3 => b"prx-recovery-input-key-v3",
            Self::ClaimV3 => b"prx-recovery-claim-v3",
            Self::UsageDigestV1 => b"prx-recovery-usage-digest-v1",
            Self::OutboxV1 => b"prx-recovery-outbox-v1",
            Self::OutboxStateV1 => b"prx-recovery-outbox-state-v1",
        }
    }
}

/// Pricing stored on a recovery cost outbox row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPricing {
    Metered,
    Subscription,
    Unknown,
}

impl RecoveryPricing {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Metered => "metered",
            Self::Subscription => "subscription",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "metered" => Ok(Self::Metered),
            "subscription" => Ok(Self::Subscription),
            "unknown" => Ok(Self::Unknown),
            _ => anyhow::bail!("recovery cost outbox pricing is invalid"),
        }
    }
}

/// Durable projection state of one recovery cost outbox row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCostState {
    Pending,
    Acknowledged,
    Error,
    Blocked,
}

impl RecoveryCostState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Acknowledged => "acknowledged",
            Self::Error => "error",
            Self::Blocked => "blocked",
        }
    }

    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "acknowledged" => Ok(Self::Acknowledged),
            "error" => Ok(Self::Error),
            "blocked" => Ok(Self::Blocked),
            _ => anyhow::bail!("recovery cost outbox state is invalid"),
        }
    }
}

/// Closed cost intent sealed into the terminal commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryCostIntent {
    pub pricing: RecoveryPricing,
    pub facts: Vec<u8>,
}

/// Authenticated pending or error cost row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryCostRow {
    pub workspace_id: String,
    pub owner_id: String,
    pub settlement_id: String,
    pub version: i64,
    pub key_id: String,
    pub usage_digest: [u8; 32],
    pub pricing: RecoveryPricing,
    pub state: RecoveryCostState,
    pub facts: Vec<u8>,
}

/// Atomic terminal commit: claim, optional assistant/usage, final, attestation, outbox.
pub struct RecoveryTerminalCommit {
    pub events: Vec<super::traits::MessageEventInput>,
    pub ingress: Option<super::ingress_recovery::IngressRecoveryCommit>,
    pub publication: RecoveryPublication,
    pub outbox: RecoveryCostIntent,
}

/// Result of one atomic terminal commit. `Conflict` rolls the transaction back.
#[derive(Debug)]
pub enum RecoveryTerminalOutcome {
    Committed {
        receipt: RecoveryReceipt,
        events: Vec<super::traits::MessageEvent>,
    },
    Replayed {
        receipt: RecoveryReceipt,
        events: Vec<super::traits::MessageEvent>,
    },
    Conflict,
}

/// A terminal commit mixed inserted and existing events, or bound bytes differed.
#[derive(Debug, thiserror::Error)]
#[error("recovery terminal commit conflict")]
pub struct RecoveryTerminalConflict;

/// Supported only by durable repositories; ordinary memory operations never
/// initialize this capability. Rotation is an operator operation after drain.
pub trait RecoveryRepository: Send + Sync {
    fn activate_recovery_authority(&self) -> anyhow::Result<String>;
    fn recovery_mac(
        &self,
        key_id: Option<&str>,
        domain: RecoveryMacDomain,
        bytes: &[u8],
    ) -> anyhow::Result<RecoveryTag>;
    fn verify_recovery_mac(&self, tag: &RecoveryTag, domain: RecoveryMacDomain, bytes: &[u8]) -> anyhow::Result<()>;
    fn rotate_recovery_authority(&self) -> anyhow::Result<String>;
    fn recovery_event_evidence(&self, chain: &RecoveryChain, event_id: &str) -> anyhow::Result<RecoveryEvidence>;
    fn record_recovery_pending(&self, publication: &RecoveryPublication) -> anyhow::Result<RecoveryReceipt>;
    fn settle_recovery(&self, publication: &RecoveryPublication) -> anyhow::Result<RecoveryReceipt>;
    fn read_recovery_receipt(&self, chain: &RecoveryChain) -> anyhow::Result<Option<RecoveryReceipt>>;
    /// Retryable outbox rows (`pending` or `error`), authenticated, ascending by settlement id.
    fn pending_recovery_costs(
        &self,
        workspace_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<RecoveryCostRow>>;
    /// Write `state` and `state_mac` in one transaction. The outbox trigger enforces the transition.
    fn finish_recovery_cost(
        &self,
        workspace_id: &str,
        settlement_id: &str,
        usage_digest: &[u8; 32],
        state: RecoveryCostState,
    ) -> anyhow::Result<()>;
}
