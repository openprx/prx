//! Explicit recursive spawn requests narrow authenticated parent constraints.

use super::{Arc, ChildGrantData, IssuedChildGrant};
use anyhow::Context as _;

impl IssuedChildGrant {
    pub(in crate::runtime) fn recursive_child_allowance(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        self.0.recursive_child_claimed.clone()
    }

    pub(crate) fn has_recursive_ancestry(&self) -> bool {
        self.scope().gate_inputs().0.authority.delegation.is_some()
    }

    /// Refuse a spent grant before batch journaling. Publication still claims
    /// atomically, since concurrent requests can pass this read-only check.
    pub(crate) fn ensure_recursive_child_available(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self
                .0
                .recursive_child_claimed
                .as_ref()
                .context("recursive grant has no child allowance owner")?
                .load(std::sync::atomic::Ordering::Acquire),
            "recursive grant already allocated its one child allowance"
        );
        Ok(())
    }

    fn claim_recursive_child(&self) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        anyhow::ensure!(
            self.0
                .recursive_child_claimed
                .as_ref()
                .context("recursive grant has no child allowance owner")?
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "recursive grant already allocated its one child allowance"
        );
        Ok(())
    }

    pub(super) fn verify_child_model(&self, provider: &str, model: &str) -> anyhow::Result<()> {
        let (issued, _) = self.scope().gate_inputs();
        if let Some(pins) = &issued.authority.delegation {
            anyhow::ensure!(
                crate::providers::ProviderId::of(provider) == crate::providers::ProviderId::of(&pins.provider)
                    && model == pins.model,
                "child model differs from the sealed delegation pins"
            );
        }
        Ok(())
    }

    pub(crate) fn for_spawn_request(
        self: &Arc<Self>,
        requested: bool,
        provider: &str,
        model: &str,
    ) -> anyhow::Result<Arc<Self>> {
        let (issued, parent) = self.scope().gate_inputs();
        self.verify_child_model(provider, model)?;
        if !requested {
            if issued.authority.delegation.is_some() {
                self.claim_recursive_child()?;
            }
            return Ok(Arc::clone(self));
        }
        let config = &self.generation().effective.sessions_spawn;
        anyhow::ensure!(
            config.allow_recursive_delegation,
            "recursive child delegation is disabled by operator configuration"
        );
        config.validate()?;
        let catalog = self.catalog().view(&Default::default());
        let spawn = parent
            .allowed
            .iter()
            .find(|reference| {
                catalog.entry_any(&reference.id).is_some_and(|entry| {
                    entry.public_name == "sessions_spawn"
                        && entry.fingerprint == reference.fingerprint
                        && entry.generation == reference.generation
                })
            })
            .context("parent has no sessions_spawn authority to delegate")?;
        let mut allowed = issued.allowed.clone();
        allowed.insert(spawn.clone());
        let provider = issued
            .authority
            .delegation
            .as_ref()
            .map_or_else(|| provider.trim(), |pins| pins.provider.as_str());
        let manifest = crate::tools::capability::child::ChildCapabilityManifest::for_recursive_spawn(
            parent,
            &allowed.into_iter().collect::<Vec<_>>(),
            provider,
            model.trim(),
            config.max_recursive_depth,
        )?;
        let scope = Arc::new(crate::tools::capability::child::VerifiedChildScope::new(
            manifest, parent,
        )?);
        self.claim_recursive_child()?;
        Ok(Arc::new(Self(Arc::new(ChildGrantData {
            // The newly published child owns one further child slot; the
            // parent slot was consumed above and stays consumed across reissue.
            recursive_child_claimed: Some(Arc::new(std::sync::atomic::AtomicBool::new(false))),
            parent_model: self.0.parent_model.clone(),
            source_fabric: self.0.source_fabric.clone(),
            security: Arc::clone(&self.0.security),
            parent_record_binding: self.0.parent_record_binding.clone(),
            provider_catalog: Arc::clone(&self.0.provider_catalog),
            catalog: Arc::clone(&self.0.catalog),
            generation: Arc::clone(&self.0.generation),
            scope,
            parent: self.0.parent.clone(),
        }))))
    }
}
