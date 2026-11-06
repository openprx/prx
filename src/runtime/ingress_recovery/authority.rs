//! Version-directed authentication: derived slots always follow their capsule.
use super::IngressRecorder;
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::memory::ingress_recovery::IngressCapsule;
use crate::memory::recovery_authority::{RecoveryMacDomain, RecoveryTag};
use anyhow::Context;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[error("legacy ingress authority unavailable; outcome remains unconfirmed")]
pub(crate) struct LegacyUnconfirmed;

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
