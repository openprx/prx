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
}

impl RecoveryMacDomain {
    pub(crate) const fn bytes(self) -> &'static [u8] {
        match self {
            Self::CapsuleV3 => b"prx-recovery-capsule-v3",
            Self::InputDigestV3 => b"prx-recovery-input-digest-v3",
            Self::StateV3 => b"prx-recovery-state-v3",
            Self::QuarantineKeyV3 => b"prx-recovery-quarantine-key-v3",
            Self::QuarantineV3 => b"prx-recovery-quarantine-v3",
        }
    }
}

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
}
