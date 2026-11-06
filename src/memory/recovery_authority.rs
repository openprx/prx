//! Repository-owned recovery authentication. No execution authority is granted.
//! Blocking methods must run on the repository's blocking worker. Callers must
//! verify legacy/new-format evidence before submitting exact event snapshots.

use serde::{Deserialize, Serialize};

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

/// Supported only by durable repositories; ordinary memory operations never
/// initialize this capability. Rotation is an operator operation after drain.
pub trait RecoveryRepository: Send + Sync {
    fn activate_recovery_authority(&self) -> anyhow::Result<String>;
    fn rotate_recovery_authority(&self) -> anyhow::Result<String>;
    fn recovery_event_evidence(&self, chain: &RecoveryChain, event_id: &str) -> anyhow::Result<RecoveryEvidence>;
    fn record_recovery_pending(&self, publication: &RecoveryPublication) -> anyhow::Result<RecoveryReceipt>;
    fn settle_recovery(&self, publication: &RecoveryPublication) -> anyhow::Result<RecoveryReceipt>;
    fn read_recovery_receipt(&self, chain: &RecoveryChain) -> anyhow::Result<Option<RecoveryReceipt>>;
}
