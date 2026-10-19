pub(crate) mod wire;
mod worker_constraints;

use crate::causal_tree::branch::{CapabilityRef, CatalogGeneration};
use crate::causal_tree::ids::DecisionId;
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::security::AutonomyLevel;
use std::collections::BTreeSet;
use std::sync::Arc;
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorityEnvelope {
    pub owner: String,
    pub workspace: String,
    pub level: AutonomyLevel,
    pub max_cost_cents: u32,
    pub remaining_depth: u8,
    pub delegation: Option<Arc<wire::WorkerDelegationAuthority>>,
}

#[derive(Clone)]
pub(crate) struct CapabilityConstraints {
    pub decision_id: DecisionId,
    pub allowed: BTreeSet<CapabilityRef>,
    pub authority: AuthorityEnvelope,
    pub catalog_generation: CatalogGeneration,
    pub key: Arc<DecisionKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildCapabilityManifest {
    pub parent_decision_id: DecisionId,
    pub allowed: BTreeSet<CapabilityRef>,
    pub authority: AuthorityEnvelope,
    pub catalog_generation: CatalogGeneration,
    pub seal: [u8; 32],
}

/// Immutable verified input to a child decision. Serialization is intentionally
/// unavailable: a transport manifest must be authenticated again on admission.
#[derive(Clone)]
pub(crate) struct VerifiedChildScope {
    manifest: ChildCapabilityManifest,
    parent: Arc<CapabilityConstraints>,
}

impl std::fmt::Debug for VerifiedChildScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedChildScope").finish_non_exhaustive()
    }
}

impl VerifiedChildScope {
    pub(crate) fn new(
        manifest: ChildCapabilityManifest,
        parent: &CapabilityConstraints,
    ) -> Result<Self, ChildManifestError> {
        manifest.verify(parent)?;
        Ok(Self {
            manifest,
            parent: Arc::new(parent.clone()),
        })
    }

    /// Re-seal an already verified scope for an authenticated process channel.
    /// This transfers the existing authority without deriving another child,
    /// widening the capability set, or consuming depth a second time.
    pub(crate) fn rebind_process_key(&self, key: Arc<DecisionKey>) -> Result<Self, ChildManifestError> {
        self.manifest.verify(&self.parent)?;
        let mut parent = self.parent.as_ref().clone();
        parent.key = key;
        let mut manifest = self.manifest.clone();
        manifest.seal = manifest.compute_seal(&parent.key);
        Self::new(manifest, &parent)
    }

    pub(crate) fn gate_inputs(&self) -> (&ChildCapabilityManifest, &CapabilityConstraints) {
        (&self.manifest, &self.parent)
    }

    pub(crate) fn validate_runtime(
        &self,
        key: &DecisionKey,
        owner: &str,
        workspace: &str,
        generation: CatalogGeneration,
    ) -> Result<(), ChildManifestError> {
        self.manifest.verify(&self.parent)?;
        if self.manifest.authority.owner != owner
            || self.manifest.authority.workspace != workspace
            || self.manifest.catalog_generation != generation
        {
            return Err(ChildManifestError::StaleReference);
        }
        let expected = self
            .parent
            .key
            .hmac(KeyDomain::StateFingerprint, &[b"child-runtime-key-v1"]);
        let actual = key.hmac(KeyDomain::StateFingerprint, &[b"child-runtime-key-v1"]);
        if !bool::from(expected.ct_eq(&actual)) {
            return Err(ChildManifestError::InvalidSeal);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChildManifestError {
    #[error("child capabilities are not a strict subset of the parent")]
    NotStrictSubset,
    #[error("child capability reference is stale")]
    StaleReference,
    #[error("child spawn depth is exhausted")]
    DepthExhausted,
    #[error("child authority exceeds the parent")]
    AuthorityWidened,
    #[error("child capability transport is malformed")]
    InvalidTransport,
    #[error("child capability seal is invalid")]
    InvalidSeal,
}

impl ChildCapabilityManifest {
    /// The caller has an explicit operator-permitted spawn request. This is a
    /// new child of the retained parent, never an expansion of an incoming child.
    pub(crate) fn for_recursive_spawn(
        parent: &CapabilityConstraints,
        requested: &[CapabilityRef],
        provider: &str,
        model: &str,
        max_depth: u8,
    ) -> Result<Self, ChildManifestError> {
        if max_depth == 0 {
            return Err(ChildManifestError::DepthExhausted);
        }
        if provider.trim().is_empty() || model.trim().is_empty() {
            return Err(ChildManifestError::InvalidTransport);
        }
        let mut manifest = Self::derive_strict_subset(parent, requested)?;
        manifest.authority.remaining_depth = manifest.authority.remaining_depth.min(max_depth);
        if manifest.authority.remaining_depth == 0 {
            return Err(ChildManifestError::DepthExhausted);
        }
        manifest.authority.delegation = Some(Arc::new(wire::WorkerDelegationAuthority {
            requested: true,
            provider: provider.to_owned(),
            model: model.to_owned(),
        }));
        manifest.seal = manifest.compute_seal(&parent.key);
        manifest.verify(parent)?;
        Ok(manifest)
    }

    /// Process transport may remove unavailable backends from this same child.
    /// Preserve the issued model pins and depth ceiling while narrowing its set.
    pub(crate) fn narrow_allowed(
        &self,
        parent: &CapabilityConstraints,
        requested: &[CapabilityRef],
    ) -> Result<Self, ChildManifestError> {
        self.verify(parent)?;
        if requested.iter().any(|reference| !self.allowed.contains(reference)) {
            return Err(ChildManifestError::NotStrictSubset);
        }
        let mut manifest = self.clone();
        manifest.allowed = requested.iter().cloned().collect();
        manifest.seal = manifest.compute_seal(&parent.key);
        manifest.verify(parent)?;
        Ok(manifest)
    }

    pub(crate) fn derive_strict_subset(
        parent: &CapabilityConstraints,
        requested: &[CapabilityRef],
    ) -> Result<Self, ChildManifestError> {
        if parent.authority.remaining_depth == 0 {
            return Err(ChildManifestError::DepthExhausted);
        }
        let allowed: BTreeSet<_> = requested.iter().cloned().collect();
        if allowed.len() >= parent.allowed.len() || !allowed.is_subset(&parent.allowed) {
            return Err(ChildManifestError::NotStrictSubset);
        }
        if allowed
            .iter()
            .any(|reference| reference.generation != parent.catalog_generation)
        {
            return Err(ChildManifestError::StaleReference);
        }
        let mut authority = parent.authority.clone();
        authority.remaining_depth -= 1;
        if let Some(delegation) = authority.delegation.as_ref().filter(|pins| pins.requested) {
            authority.delegation = Some(Arc::new(wire::WorkerDelegationAuthority {
                requested: false,
                provider: delegation.provider.clone(),
                model: delegation.model.clone(),
            }));
        }
        let mut manifest = Self {
            parent_decision_id: parent.decision_id,
            allowed,
            authority,
            catalog_generation: parent.catalog_generation,
            seal: [0; 32],
        };
        manifest.seal = manifest.compute_seal(&parent.key);
        Ok(manifest)
    }

    pub(crate) fn verify(&self, parent: &CapabilityConstraints) -> Result<(), ChildManifestError> {
        if self
            .authority
            .delegation
            .as_ref()
            .is_some_and(|delegation| delegation.provider.trim().is_empty() || delegation.model.trim().is_empty())
        {
            return Err(ChildManifestError::InvalidTransport);
        }
        if self.parent_decision_id != parent.decision_id {
            return Err(ChildManifestError::StaleReference);
        }
        if self.catalog_generation != parent.catalog_generation {
            return Err(ChildManifestError::StaleReference);
        }
        if self.allowed.len() >= parent.allowed.len() || !self.allowed.is_subset(&parent.allowed) {
            return Err(ChildManifestError::NotStrictSubset);
        }
        if self.authority.owner != parent.authority.owner
            || self.authority.workspace != parent.authority.workspace
            || level_rank(self.authority.level) > level_rank(parent.authority.level)
            || self.authority.max_cost_cents > parent.authority.max_cost_cents
            || self.authority.remaining_depth >= parent.authority.remaining_depth
            || parent.authority.delegation.as_ref().is_some_and(|pins| {
                self.authority
                    .delegation
                    .as_ref()
                    .is_none_or(|child| child.provider != pins.provider || child.model != pins.model)
            })
        {
            return Err(ChildManifestError::AuthorityWidened);
        }
        if !bool::from(self.seal.ct_eq(&self.compute_seal(&parent.key))) {
            return Err(ChildManifestError::InvalidSeal);
        }
        Ok(())
    }

    /// Decode transport data only against the authenticated parent context.
    /// An outer worker HMAC does not replace this exact subset/seal check.
    pub(crate) fn from_worker_manifest(
        wire: &wire::WorkerChildCapabilityManifest,
        parent: &CapabilityConstraints,
    ) -> Result<Self, ChildManifestError> {
        let invalid = || ChildManifestError::InvalidTransport;
        let parent_decision_id = DecisionId::parse(&wire.parent_decision_id).map_err(|_| invalid())?;
        if parent_decision_id.to_string() != wire.parent_decision_id
            || wire.owner.is_empty()
            || wire.workspace.is_empty()
            || wire.allowed.len() >= parent.allowed.len()
        {
            return Err(invalid());
        }
        let allowed = decode_worker_refs(&wire.allowed, CatalogGeneration(wire.catalog_generation))?;
        let manifest = Self {
            parent_decision_id,
            allowed,
            authority: AuthorityEnvelope {
                owner: wire.owner.clone(),
                workspace: wire.workspace.clone(),
                level: wire.level,
                max_cost_cents: wire.max_cost_cents,
                remaining_depth: wire.remaining_depth,
                delegation: wire.delegation.clone().map(Arc::new),
            },
            catalog_generation: CatalogGeneration(wire.catalog_generation),
            seal: wire.seal,
        };
        manifest.verify(parent)?;
        Ok(manifest)
    }

    pub(crate) fn to_worker_manifest(&self) -> wire::WorkerChildCapabilityManifest {
        use wire::{WorkerChildCapabilityManifest, WorkerChildCapabilityRef};
        WorkerChildCapabilityManifest {
            parent_decision_id: self.parent_decision_id.to_string(),
            allowed: self
                .allowed
                .iter()
                .map(|reference| WorkerChildCapabilityRef {
                    id: reference.id.as_str().to_owned(),
                    fingerprint: reference.fingerprint.0,
                    generation: reference.generation.0,
                })
                .collect(),
            owner: self.authority.owner.clone(),
            workspace: self.authority.workspace.clone(),
            level: self.authority.level,
            max_cost_cents: self.authority.max_cost_cents,
            remaining_depth: self.authority.remaining_depth,
            delegation: self.authority.delegation.as_deref().cloned(),
            catalog_generation: self.catalog_generation.0,
            seal: self.seal,
        }
    }

    fn compute_seal(&self, key: &DecisionKey) -> [u8; 32] {
        let mut encoded = Vec::new();
        append(&mut encoded, self.parent_decision_id.to_string().as_bytes());
        append(&mut encoded, &self.catalog_generation.0);
        append(&mut encoded, self.authority.owner.as_bytes());
        append(&mut encoded, self.authority.workspace.as_bytes());
        encoded.push(level_rank(self.authority.level));
        encoded.extend(self.authority.max_cost_cents.to_be_bytes());
        encoded.push(self.authority.remaining_depth);
        for reference in &self.allowed {
            append(&mut encoded, reference.id.as_str().as_bytes());
            append(&mut encoded, &reference.fingerprint.0);
            append(&mut encoded, &reference.generation.0);
        }
        if let Some(delegation) = &self.authority.delegation {
            append(&mut encoded, b"recursive-delegation-v1");
            encoded.push(u8::from(delegation.requested));
            append(&mut encoded, delegation.provider.as_bytes());
            append(&mut encoded, delegation.model.as_bytes());
        }
        key.hmac(
            KeyDomain::StateFingerprint,
            &[b"child-capability-manifest-v1", &encoded],
        )
    }
}

fn decode_worker_refs(
    items: &[wire::WorkerChildCapabilityRef],
    generation: CatalogGeneration,
) -> Result<BTreeSet<CapabilityRef>, ChildManifestError> {
    use crate::causal_tree::branch::{CapabilityId, CapabilityTarget, SchemaFingerprint};
    let invalid = || ChildManifestError::InvalidTransport;
    let mut allowed = BTreeSet::new();
    for item in items {
        let mut parts = item.id.split(':');
        let family = parts.next().ok_or_else(invalid)?;
        let family = CapabilityTarget::ALL
            .into_iter()
            .find(|target| target.as_str() == family)
            .ok_or_else(invalid)?;
        let namespace = parts.next().ok_or_else(invalid)?;
        let name = parts.next().ok_or_else(invalid)?;
        if parts.next().is_some() || item.generation != generation.0 {
            return Err(invalid());
        }
        let reference = CapabilityRef {
            id: CapabilityId::new(family, namespace, name).map_err(|_| invalid())?,
            fingerprint: SchemaFingerprint(item.fingerprint),
            generation: CatalogGeneration(item.generation),
        };
        if !allowed.insert(reference) {
            return Err(invalid());
        }
    }
    Ok(allowed)
}

fn append(out: &mut Vec<u8>, value: &[u8]) {
    out.extend(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    out.extend(value);
}

const fn level_rank(level: AutonomyLevel) -> u8 {
    match level {
        AutonomyLevel::ReadOnly => 0,
        AutonomyLevel::Supervised => 1,
        AutonomyLevel::Full => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_tree::branch::{CapabilityId, CapabilityTarget, SchemaFingerprint};

    #[test]
    fn child_manifest_is_a_sealed_strict_subset_with_narrower_depth() {
        let temp = tempfile::tempdir().unwrap();
        let generation = CatalogGeneration([7; 32]);
        let first = CapabilityRef {
            id: CapabilityId::new(CapabilityTarget::NativeTool, "native", "prx_read").unwrap(),
            fingerprint: SchemaFingerprint([1; 32]),
            generation,
        };
        let second = CapabilityRef {
            id: CapabilityId::new(CapabilityTarget::NativeTool, "native", "prx_write").unwrap(),
            fingerprint: SchemaFingerprint([2; 32]),
            generation,
        };
        let parent = CapabilityConstraints {
            decision_id: DecisionId::legacy(&[9; 32]),
            allowed: BTreeSet::from([first.clone(), second.clone()]),
            authority: AuthorityEnvelope {
                owner: "prx_user".to_string(),
                workspace: "prx_workspace".to_string(),
                level: AutonomyLevel::Supervised,
                max_cost_cents: 100,
                remaining_depth: 2,
                delegation: None,
            },
            catalog_generation: generation,
            key: DecisionKey::load_or_create(temp.path()).unwrap(),
        };
        let model_only = ChildCapabilityManifest::derive_strict_subset(&parent, &[]).unwrap();
        model_only.verify(&parent).unwrap();
        assert!(model_only.allowed.is_empty());
        assert_eq!(
            model_only.authority.remaining_depth,
            parent.authority.remaining_depth - 1
        );
        assert_eq!(model_only.authority.owner, parent.authority.owner);
        assert_eq!(model_only.authority.max_cost_cents, parent.authority.max_cost_cents);
        let leaf = CapabilityConstraints {
            decision_id: parent.decision_id,
            allowed: model_only.allowed.clone(),
            authority: model_only.authority,
            catalog_generation: parent.catalog_generation,
            key: Arc::clone(&parent.key),
        };
        assert!(matches!(
            ChildCapabilityManifest::derive_strict_subset(&leaf, &[]),
            Err(ChildManifestError::NotStrictSubset)
        ));
        let child = ChildCapabilityManifest::derive_strict_subset(&parent, std::slice::from_ref(&first)).unwrap();
        assert_eq!(child.authority.remaining_depth, 1);
        child.verify(&parent).unwrap();
        assert!(matches!(
            ChildCapabilityManifest::derive_strict_subset(&parent, &[first.clone(), second]),
            Err(ChildManifestError::NotStrictSubset)
        ));
        let mut forged = child.clone();
        forged.authority.level = AutonomyLevel::Full;
        assert!(matches!(
            forged.verify(&parent),
            Err(ChildManifestError::AuthorityWidened)
        ));
        for mutate in ["owner", "workspace", "budget", "depth"] {
            let mut forged = child.clone();
            match mutate {
                "owner" => forged.authority.owner = "other_owner".into(),
                "workspace" => forged.authority.workspace = "other_workspace".into(),
                "budget" => forged.authority.max_cost_cents = 101,
                "depth" => forged.authority.remaining_depth = 2,
                _ => continue,
            }
            assert!(
                matches!(forged.verify(&parent), Err(ChildManifestError::AuthorityWidened)),
                "{mutate}"
            );
        }
        let mut forged = child.clone();
        forged.allowed.insert(CapabilityRef {
            generation: CatalogGeneration([8; 32]),
            ..first
        });
        assert!(forged.verify(&parent).is_err());
        let mut forged = child;
        forged.seal = [0; 32];
        assert!(matches!(forged.verify(&parent), Err(ChildManifestError::InvalidSeal)));
    }
}

#[cfg(test)]
mod recursive_tests;
