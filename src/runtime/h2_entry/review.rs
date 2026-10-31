//! The original OS terminal owner is distinct from an API principal's review.
use super::{ControlledH2Profile, EntryOriginRecord, VerifiedEntryOrigin};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(crate) struct LocalControlAuthority {
    profile: Arc<ControlledH2Profile>,
}
impl LocalControlAuthority {
    pub(super) fn from_producer(profile: Arc<ControlledH2Profile>) -> Result<Self> {
        ensure!(
            profile.kind == super::EntryKind::Gateway,
            "local review requires direct Gateway launch"
        );
        profile.ensure_live()?;
        Ok(Self { profile })
    }
    pub(crate) fn principal(&self) -> &str {
        self.profile.principal.as_deref().unwrap_or_default()
    }
    pub(crate) fn profile(&self) -> &Arc<ControlledH2Profile> {
        &self.profile
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attest(
        &self,
        origin: &EntryOriginRecord,
        pending_mac: [u8; 32],
        pending_id: &str,
        call_digest: [u8; 32],
        args: &serde_json::Value,
        capability_generation: [u8; 32],
        catalog_generation: [u8; 32],
        expires: chrono::DateTime<chrono::Utc>,
    ) -> Result<LocalOwnerReviewStamp> {
        self.verify_source(origin)?;
        ensure!(
            !pending_id.is_empty() && expires > chrono::Utc::now(),
            "local review pending identity expired"
        );
        let body = LocalReviewBody {
            version: 1,
            boot: self.profile.boot.clone(),
            profile: self.profile.digest.clone(),
            original: origin.clone(),
            pending_mac,
            pending_id: pending_id.into(),
            call_digest,
            args_sha256: super::arguments_digest(args)?,
            capability_generation,
            catalog_generation,
            expires,
        };
        let key = crate::decision::DecisionKey::load_or_create(self.profile.workspace())?;
        let mac = key.hmac(
            crate::decision::KeyDomain::ApprovalState,
            &[b"h2-local-owner-review-v1", &serde_json::to_vec(&body)?],
        );
        Ok(LocalOwnerReviewStamp { body, mac })
    }
    pub(crate) fn verify_source(&self, origin: &EntryOriginRecord) -> Result<()> {
        self.profile.ensure_live()?;
        ensure!(
            origin.authority_expires_at > chrono::Utc::now(),
            "original controlled input expired"
        );
        ensure!(
            origin.version == 1
                && origin.kind == "gateway"
                && origin.profile == self.profile.digest
                && origin.actor == self.principal()
                && origin.target_owner == self.profile.owner.as_str()
                && origin.workspace == self.profile.workspace.to_string_lossy(),
            "pending call is outside this local delegation"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalReviewBody {
    version: u32,
    boot: String,
    profile: String,
    original: EntryOriginRecord,
    pending_mac: [u8; 32],
    pending_id: String,
    call_digest: [u8; 32],
    args_sha256: String,
    capability_generation: [u8; 32],
    catalog_generation: [u8; 32],
    expires: chrono::DateTime<chrono::Utc>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalOwnerReviewStamp {
    body: LocalReviewBody,
    mac: [u8; 32],
}
impl LocalOwnerReviewStamp {
    pub(crate) fn verify_pending(
        &self,
        pending_mac: [u8; 32],
        origin: &EntryOriginRecord,
        pending_id: &str,
        call_digest: [u8; 32],
    ) -> Result<()> {
        ensure!(
            self.body.pending_mac == pending_mac
                && &self.body.original == origin
                && self.body.pending_id == pending_id
                && self.body.call_digest == call_digest,
            "local-owner attestation differs from the authenticated pending call"
        );
        Ok(())
    }
    fn verify_live(&self, origin: &VerifiedEntryOrigin) -> Result<()> {
        origin.ensure_live()?;
        let profile = origin.profile();
        let key = crate::decision::DecisionKey::load_or_create(profile.workspace())?;
        ensure!(
            key.verify_hmac(
                crate::decision::KeyDomain::ApprovalState,
                &[b"h2-local-owner-review-v1", &serde_json::to_vec(&self.body)?],
                &self.mac
            ) && self.body.version == 1
                && self.body.boot == profile.boot
                && self.body.profile == profile.digest
                && self.body.original.actor == origin.record().actor
                && self.body.original.target_owner == origin.record().target_owner
                && self.body.original.workspace == origin.record().workspace
                && self.body.expires > chrono::Utc::now(),
            "local-owner attestation is stale or unauthenticated"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalOwnerReview {
    origin: Arc<VerifiedEntryOrigin>,
    stamp: LocalOwnerReviewStamp,
}
impl LocalOwnerReview {
    pub(crate) fn from_signed(origin: Arc<VerifiedEntryOrigin>, stamp: LocalOwnerReviewStamp) -> Result<Self> {
        stamp.verify_live(&origin)?;
        Ok(Self { origin, stamp })
    }
    pub(super) fn verify(&self, origin: &VerifiedEntryOrigin, call: &super::EntryCall) -> Result<()> {
        self.stamp.verify_live(origin)?;
        ensure!(
            self.origin.as_ref() == origin
                && self.stamp.body.args_sha256 == call.args_sha256
                && hex::encode(self.stamp.body.capability_generation) == call.capability_generation
                && hex::encode(self.stamp.body.catalog_generation) == call.catalog_generation,
            "local-owner review differs from the executing call"
        );
        Ok(())
    }
}
