//! Fresh model policy snapshot; no backend output can construct this authority.
use super::catalog::{CapabilityEntry, CatalogView};
use crate::causal_tree::branch::CatalogGeneration;
use crate::tools::Tool;
use std::sync::Arc;

#[derive(PartialEq, Eq)]
pub(crate) struct ModelAdmission {
    denied: Vec<u64>,
    generation: CatalogGeneration,
    layout: [u8; 32],
    digest: [u8; 32],
}

impl ModelAdmission {
    /// Re-evaluate arbitrary supports_name behavior at every original runtime
    /// verification point. Only the slot/name index is pinned at publication.
    pub(crate) fn fresh(
        registry: &[Box<dyn Tool>],
        catalog: &CatalogView<'_>,
        model: &str,
        allowlist: &[String],
        policy_version: &str,
        key: &crate::decision::DecisionKey,
    ) -> Arc<Self> {
        let mut denied = vec![0_u64; catalog.all_entries.len().div_ceil(64)];
        for (word_index, word) in denied.iter_mut().enumerate() {
            for bit in 0..64 {
                if catalog.is_visible_index(word_index * 64 + bit) {
                    *word |= 1_u64 << bit;
                }
            }
        }
        for tool in registry {
            if allowlist.iter().any(|name| tool.supports_name(name))
                && let Some(slots) = catalog.model_root_slots.get(tool.name())
            {
                for &slot in slots {
                    // The private publication index is derived from this
                    // exact slot layout. An absent word cannot grant a bit.
                    if let Some(word) = denied.get_mut(slot / 64) {
                        *word &= !(1_u64 << (slot % 64));
                    }
                }
            }
        }
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        hash.update(b"model-admission-fresh-v1");
        for text in [model, policy_version] {
            hash.update(&(text.len() as u64).to_be_bytes());
            hash.update(text.as_bytes());
        }
        hash.update(&catalog.generation().0);
        hash.update(&catalog.model_layout);
        hash.update(&(catalog.all_entries.len() as u64).to_be_bytes());
        // Bind the visible view even when an allowlist permits every root.
        for (index, _) in catalog.all_entries.iter().enumerate() {
            hash.update(&[u8::from(catalog.is_visible_index(index))]);
        }
        for word in &denied {
            hash.update(&word.to_be_bytes());
        }
        let encoded = hash.finish();
        let digest = key.hmac(
            crate::decision::KeyDomain::StateFingerprint,
            &[b"model-admission-policy-v1", encoded.as_ref()],
        );
        Arc::new(Self {
            denied,
            generation: catalog.generation(),
            layout: catalog.model_layout,
            digest,
        })
    }

    pub(crate) const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) fn matches_catalog(&self, catalog: &CatalogView<'_>) -> bool {
        self.generation == catalog.generation() && self.layout == catalog.model_layout
    }

    pub(crate) fn excludes(&self, catalog: &CatalogView<'_>, entry: &CapabilityEntry) -> bool {
        // Fail closed outside the pinned slot layout. G3 explicitly checks this
        // binding; G2 only queries it when the layout matches.
        if !self.matches_catalog(catalog) {
            return true;
        }
        [&entry.public_name, &entry.root].iter().any(|name| {
            catalog.model_names.get(name.as_str()).is_some_and(|&slot| {
                self.denied
                    .get(slot / 64)
                    .is_none_or(|word| word & (1_u64 << (slot % 64)) != 0)
            })
        })
    }
}

#[cfg(test)]
mod tests;
