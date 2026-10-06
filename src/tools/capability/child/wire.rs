use serde::{Deserialize, Serialize};

/// Transport data only: authentication and parent-subset validation are required
/// before this data can authorize a child capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerChildCapabilityManifest {
    pub parent_decision_id: String,
    pub allowed: Vec<WorkerChildCapabilityRef>,
    pub owner: String,
    pub workspace: String,
    pub level: crate::security::AutonomyLevel,
    pub max_cost_cents: u32,
    pub remaining_depth: u8,
    pub catalog_generation: [u8; 32],
    pub seal: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerChildCapabilityRef {
    pub id: String,
    pub fingerprint: [u8; 32],
    pub generation: [u8; 32],
}

/// Parent constraints signed with the decision key, separately from the worker
/// transport signature. Never contains key material.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerParentConstraints {
    pub decision_id: String,
    pub allowed: Vec<WorkerChildCapabilityRef>,
    pub owner: String,
    pub workspace: String,
    pub level: crate::security::AutonomyLevel,
    pub max_cost_cents: u32,
    pub remaining_depth: u8,
    pub catalog_generation: [u8; 32],
    pub seal: [u8; 32],
}
