//! Exact parent constraints for cross-process subset verification.
use super::{
    Arc, AuthorityEnvelope, CapabilityConstraints, CatalogGeneration, ChildManifestError, DecisionId, DecisionKey,
    KeyDomain, decode_worker_refs, wire,
};
use wire::{WorkerChildCapabilityRef, WorkerParentConstraints};

fn signing_payload(wire: &WorkerParentConstraints) -> Result<Vec<u8>, ChildManifestError> {
    serde_json::to_vec(&(
        &wire.decision_id,
        &wire.allowed,
        &wire.owner,
        &wire.workspace,
        wire.level,
        wire.max_cost_cents,
        wire.remaining_depth,
        wire.catalog_generation,
    ))
    .map_err(|_| ChildManifestError::InvalidTransport)
}

impl CapabilityConstraints {
    pub(crate) fn to_worker_constraints(&self) -> Result<WorkerParentConstraints, ChildManifestError> {
        let mut wire = WorkerParentConstraints {
            decision_id: self.decision_id.to_string(),
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
        let decision_id = DecisionId::parse(&wire.decision_id).map_err(|_| ChildManifestError::InvalidTransport)?;
        if decision_id.to_string() != wire.decision_id || wire.owner.is_empty() || wire.workspace.is_empty() {
            return Err(ChildManifestError::InvalidTransport);
        }
        let catalog_generation = CatalogGeneration(wire.catalog_generation);
        let allowed = decode_worker_refs(&wire.allowed, catalog_generation)?;
        Ok(Self {
            decision_id,
            allowed,
            catalog_generation,
            key,
            authority: AuthorityEnvelope {
                owner: wire.owner.clone(),
                workspace: wire.workspace.clone(),
                level: wire.level,
                max_cost_cents: wire.max_cost_cents,
                remaining_depth: wire.remaining_depth,
            },
        })
    }
}
