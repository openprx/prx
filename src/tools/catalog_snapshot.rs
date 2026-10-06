//! Owned immutable metadata for one publication of a dynamic tool catalog.
//! A snapshot is not execution authority: readers must acquire the live view.
use super::traits::ToolSpec;
use std::ops::Range;
use std::sync::Arc;

#[derive(Debug)]
pub struct SharedToolCatalogEntry {
    spec: ToolSpec,
    fingerprint: [u8; 32],
    schema_valid: bool,
    binding_digest: Option<[u8; 32]>,
}

impl SharedToolCatalogEntry {
    #[must_use]
    pub const fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    #[must_use]
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
    #[must_use]
    pub(crate) const fn schema_valid(&self) -> bool {
        self.schema_valid
    }
    #[must_use]
    pub const fn binding_digest(&self) -> Option<[u8; 32]> {
        self.binding_digest
    }
}

/// Cheap cloned view; restricted wrappers share storage without exposing siblings.
/// Duplicate names are retained so the executor's parity checks still reject them.
#[derive(Debug, Clone, Default)]
pub struct SharedToolCatalogSnapshot {
    entries: Arc<[Arc<SharedToolCatalogEntry>]>,
    visible: Range<usize>,
    bindings: Range<usize>,
}

impl SharedToolCatalogSnapshot {
    #[must_use]
    pub fn new(specs: Vec<(ToolSpec, Option<[u8; 32]>)>) -> Self {
        let mut entries: Vec<_> = specs
            .into_iter()
            .map(|(spec, binding_digest)| {
                let fingerprint = super::capability::fingerprint::tool_spec_fingerprint(
                    &spec.name,
                    &spec.description,
                    &spec.parameters,
                )
                .0;
                let schema_valid = super::schema::SchemaCleanr::validate(&spec.parameters).is_ok();
                Arc::new(SharedToolCatalogEntry {
                    spec,
                    fingerprint,
                    schema_valid,
                    binding_digest,
                })
            })
            .collect();
        entries.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        let range = 0..entries.len();
        Self {
            entries: entries.into(),
            visible: range.clone(),
            bindings: range,
        }
    }

    #[must_use]
    pub fn entries(&self) -> &[Arc<SharedToolCatalogEntry>] {
        self.entries.get(self.visible.clone()).unwrap_or_default()
    }

    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&SharedToolCatalogEntry> {
        let entries = self.entries();
        let index = entries.partition_point(|entry| entry.spec.name.as_str() < name);
        entries
            .get(index)
            .filter(|entry| entry.spec.name == name)
            .map(AsRef::as_ref)
    }

    #[must_use]
    pub fn binding_digest(&self, name: &str) -> Option<[u8; 32]> {
        let entries = self.entries.get(self.bindings.clone())?;
        let index = entries.partition_point(|entry| entry.spec.name.as_str() < name);
        entries
            .get(index)
            .filter(|entry| entry.spec.name == name)?
            .binding_digest
    }

    /// A root projection may retain its dispatch-binding inventory, as the
    /// existing wrappers do, but only the chosen spec remains visible.
    #[must_use]
    pub fn restrict_to(mut self, name: &str, retain_root_bindings: bool) -> Self {
        let entries = self.entries();
        let start = entries.partition_point(|entry| entry.spec.name.as_str() < name);
        let end = entries.partition_point(|entry| entry.spec.name.as_str() <= name);
        self.visible = self.visible.start + start..self.visible.start + end;
        if !retain_root_bindings {
            self.bindings = self.visible.clone();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn alias_restriction_retains_duplicates_for_parity_and_hides_siblings() {
        let make = |name: &str| ToolSpec {
            name: name.into(),
            description: String::new(),
            parameters: json!({"type":"object","properties":{}}),
        };
        let snapshot = SharedToolCatalogSnapshot::new(vec![
            (make("root"), None),
            (make("alias"), Some([1; 32])),
            (make("alias"), Some([2; 32])),
            (make("other"), Some([3; 32])),
        ]);
        let restricted = snapshot.clone().restrict_to("alias", false);
        assert_eq!(restricted.entries().len(), 2);
        assert!(restricted.entry("other").is_none());
        assert!(restricted.binding_digest("other").is_none());
        let root = snapshot.restrict_to("root", true);
        assert_eq!(root.entries().len(), 1);
        assert_eq!(root.binding_digest("other"), Some([3; 32]));
        assert!(root.entry("other").is_none());
    }
}
