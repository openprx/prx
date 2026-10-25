//! Explicit recursive spawn requests narrow authenticated parent constraints.

use super::{Arc, ChildGrantData, IssuedChildGrant};
use anyhow::Context as _;

impl IssuedChildGrant {
    pub(in crate::runtime) fn recursive_child_allowance(
        &self,
    ) -> Option<Arc<crate::runtime::turn_decision::recursive_allowance::RecursiveAllowance>> {
        self.0.recursive_child_claimed.clone()
    }

    pub(crate) fn has_recursive_ancestry(&self) -> bool {
        self.scope().gate_inputs().0.authority.delegation.is_some()
    }

    /// Publication claims atomically after this pre-journaling availability check.
    pub(crate) fn ensure_recursive_child_available(&self) -> anyhow::Result<()> {
        self.0
            .recursive_child_claimed
            .as_ref()
            .context("recursive grant has no child allowance owner")?
            .ensure_available()
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
            return self.with_budgeted_scope(issued.clone());
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
        self.with_budgeted_scope(manifest)
    }

    fn with_budgeted_scope(
        self: &Arc<Self>,
        manifest: crate::tools::capability::child::ChildCapabilityManifest,
    ) -> anyhow::Result<Arc<Self>> {
        let (_, parent) = self.scope().gate_inputs();
        let owner = self
            .0
            .recursive_child_claimed
            .as_ref()
            .context("child grant has no budget owner")?;
        let (cents, recursive_child_claimed) = if manifest.authority.delegation.is_some() {
            owner.carve_child(manifest.authority.max_cost_cents)?
        } else {
            owner.carve_ordinary_child(manifest.authority.max_cost_cents)?
        };
        let manifest = manifest.with_cost_ceiling(parent, cents)?;
        let scope = Arc::new(crate::tools::capability::child::VerifiedChildScope::new(
            manifest, parent,
        )?);
        Ok(Arc::new(Self(Arc::new(ChildGrantData {
            recursive_child_claimed: Some(recursive_child_claimed),
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
