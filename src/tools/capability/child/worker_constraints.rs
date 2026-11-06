//! Exact parent constraints for cross-process subset verification.
use super::{
    Arc, AuthorityEnvelope, CapabilityConstraints, CatalogGeneration, ChildCapabilityManifest, ChildManifestError,
    DecisionKey, KeyDomain, ParentAuthorityId, decode_worker_refs, wire,
};
use wire::{WorkerChildCapabilityRef, WorkerParentConstraints};

fn signing_payload(wire: &WorkerParentConstraints) -> Result<Vec<u8>, ChildManifestError> {
    let mut payload = serde_json::to_vec(&(
        &wire.decision_id,
        &wire.allowed,
        &wire.owner,
        &wire.workspace,
        wire.level,
        wire.max_cost_cents,
        wire.remaining_depth,
        wire.catalog_generation,
    ))
    .map_err(|_| ChildManifestError::InvalidTransport)?;
    if let Some(delegation) = &wire.delegation {
        payload.extend_from_slice(b"\nrecursive-delegation-v1\n");
        payload.extend(serde_json::to_vec(delegation).map_err(|_| ChildManifestError::InvalidTransport)?);
    }
    Ok(payload)
}

impl CapabilityConstraints {
    pub(crate) fn to_worker_constraints(&self) -> Result<WorkerParentConstraints, ChildManifestError> {
        let mut wire = WorkerParentConstraints {
            decision_id: self.parent_authority_id.to_string(),
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
            seal: [0; 32],
        };
        wire.seal = self.key.hmac(
            KeyDomain::StateFingerprint,
            &[b"worker-parent-constraints-v1", &signing_payload(&wire)?],
        );
        Ok(wire)
    }

    pub(crate) fn from_worker_constraints(
        wire: &WorkerParentConstraints,
        key: Arc<DecisionKey>,
    ) -> Result<Self, ChildManifestError> {
        if !key.verify_hmac(
            KeyDomain::StateFingerprint,
            &[b"worker-parent-constraints-v1", &signing_payload(wire)?],
            &wire.seal,
        ) {
            return Err(ChildManifestError::InvalidSeal);
        }
        let parent_authority_id =
            ParentAuthorityId::parse(&wire.decision_id).map_err(|_| ChildManifestError::InvalidTransport)?;
        if parent_authority_id.to_string() != wire.decision_id || wire.owner.is_empty() || wire.workspace.is_empty() {
            return Err(ChildManifestError::InvalidTransport);
        }
        let catalog_generation = CatalogGeneration(wire.catalog_generation);
        let allowed = decode_worker_refs(&wire.allowed, catalog_generation)?;
        Ok(Self {
            parent_authority_id,
            allowed,
            catalog_generation,
            key,
            authority: AuthorityEnvelope {
                owner: wire.owner.clone(),
                workspace: wire.workspace.clone(),
                level: wire.level,
                max_cost_cents: wire.max_cost_cents,
                remaining_depth: wire.remaining_depth,
                delegation: wire.delegation.clone().map(Arc::new),
            },
        })
    }
}

impl ChildCapabilityManifest {
    /// Decode transport data only against the authenticated parent context.
    /// An outer worker HMAC does not replace this exact subset/seal check.
    pub(crate) fn from_worker_manifest(
        wire: &wire::WorkerChildCapabilityManifest,
        parent: &CapabilityConstraints,
    ) -> Result<Self, ChildManifestError> {
        let invalid = || ChildManifestError::InvalidTransport;
        let parent_authority_id = ParentAuthorityId::parse(&wire.parent_decision_id).map_err(|_| invalid())?;
        if parent_authority_id.to_string() != wire.parent_decision_id
            || wire.owner.is_empty()
            || wire.workspace.is_empty()
            || wire.allowed.len() >= parent.allowed.len()
        {
            return Err(invalid());
        }
        let allowed = decode_worker_refs(&wire.allowed, CatalogGeneration(wire.catalog_generation))?;
        let manifest = Self {
            parent_authority_id,
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
            parent_decision_id: self.parent_authority_id.to_string(),
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
}
