//! Version-directed authentication: derived slots always follow their capsule.
use super::IngressRecorder;
use crate::agent::terminal::TurnTerminalStatus;
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::memory::ingress_recovery::{IngressAttestationError, IngressCapsule};
use crate::memory::recovery_authority::{RecoveryMacDomain, RecoveryTag};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[error("legacy ingress authority unavailable; outcome remains unconfirmed")]
pub(crate) struct LegacyUnconfirmed;

const SETTLED_FACT_KEYS: [&str; 14] = [
    "v",
    "terminal_id",
    "claim_event_id",
    "assistant_event_id",
    "usage_event_id",
    "final_event_id",
    "semantic_mac_v3",
    "status",
    "surface",
    "parent_authority_id",
    "pricing",
    "usage_digest",
    "key_id",
    "skill_observation",
];

const SKILL_OBSERVATION_KEYS: [&str; 3] = ["status", "awaiting_approval", "verified"];

/// Closed JSON of a settled terminal receipt. 3a produces this; 2a parses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettledTerminalFacts {
    pub(crate) v: u32,
    pub(crate) terminal_id: String,
    pub(crate) claim_event_id: String,
    pub(crate) assistant_event_id: Option<String>,
    pub(crate) usage_event_id: Option<String>,
    pub(crate) final_event_id: String,
    pub(crate) semantic_mac_v3: String,
    pub(crate) status: String,
    pub(crate) surface: String,
    pub(crate) parent_authority_id: Option<String>,
    pub(crate) pricing: String,
    pub(crate) usage_digest: String,
    pub(crate) key_id: String,
    pub(crate) skill_observation: SkillObservation,
}

/// Skill-lifecycle observation bound into settled facts. Unrecorded: awaiting
/// and verified are both false (`recovered_signals` with no decision/trace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillObservation {
    pub(crate) status: TurnTerminalStatus,
    pub(crate) awaiting_approval: bool,
    pub(crate) verified: bool,
}

impl SettledTerminalFacts {
    /// Parse the closed schema: an unknown or missing key refuses.
    pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| IngressAttestationError::Malformed)?;
        let object = value.as_object().ok_or(IngressAttestationError::Malformed)?;
        if object.keys().any(|key| !SETTLED_FACT_KEYS.contains(&key.as_str())) {
            return Err(IngressAttestationError::UnknownKey.into());
        }
        if SETTLED_FACT_KEYS.iter().any(|key| !object.contains_key(*key)) {
            return Err(IngressAttestationError::Malformed.into());
        }
        let observation = object
            .get("skill_observation")
            .and_then(serde_json::Value::as_object)
            .ok_or(IngressAttestationError::Malformed)?;
        if observation
            .keys()
            .any(|key| !SKILL_OBSERVATION_KEYS.contains(&key.as_str()))
        {
            return Err(IngressAttestationError::UnknownKey.into());
        }
        if SKILL_OBSERVATION_KEYS.iter().any(|key| !observation.contains_key(*key)) {
            return Err(IngressAttestationError::Malformed.into());
        }
        let facts: Self = serde_json::from_value(value).map_err(|_| IngressAttestationError::Malformed)?;
        if facts.v != 1 {
            return Err(IngressAttestationError::Mismatch("v").into());
        }
        Ok(facts)
    }
}

impl IngressRecorder {
    pub(super) fn legacy_key(&self) -> anyhow::Result<Arc<DecisionKey>> {
        DecisionKey::load_existing(&self.workspace).map_err(|_| LegacyUnconfirmed.into())
    }

    pub(super) fn slot_mac(
        &self,
        capsule: &IngressCapsule,
        domain: RecoveryMacDomain,
        legacy: &[&[u8]],
        bytes: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        if capsule.version == 3 {
            let repository = self
                .memory
                .recovery_repository()
                .context("recovery repository unavailable")?;
            let id = capsule.key_id.as_deref().context("missing recovery key id")?;
            return Ok(repository.recovery_mac(Some(id), domain, bytes)?.tag.to_vec());
        }
        Ok(self.legacy_key()?.hmac(KeyDomain::IngressRecovery, legacy).to_vec())
    }

    pub(super) fn verify_slot(
        &self,
        capsule: &IngressCapsule,
        domain: RecoveryMacDomain,
        legacy: &[&[u8]],
        bytes: &[u8],
        tag: &[u8],
    ) -> anyhow::Result<bool> {
        if capsule.version == 3 {
            let repository = self
                .memory
                .recovery_repository()
                .context("recovery repository unavailable")?;
            let tag = RecoveryTag {
                key_id: capsule.key_id.clone().context("missing recovery key id")?,
                tag: tag.try_into().context("invalid recovery tag length")?,
            };
            // An unusable key is not a bad tag: propagate it so the candidate is
            // left pending instead of being quarantined.
            return match repository.verify_recovery_mac(&tag, domain, bytes) {
                Ok(()) => Ok(true),
                Err(error) if crate::memory::recovery_authority::is_key_unavailable(&error) => Err(error),
                Err(_) => Ok(false),
            };
        }
        Ok(self.legacy_key()?.verify_hmac(KeyDomain::IngressRecovery, legacy, tag))
    }
}
