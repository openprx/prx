//! Narrow a verified child's monetary ceiling before publishing its seal.
use super::{CapabilityConstraints, ChildCapabilityManifest, ChildManifestError};

impl ChildCapabilityManifest {
    pub(crate) fn with_cost_ceiling(
        mut self,
        parent: &CapabilityConstraints,
        cents: u32,
    ) -> Result<Self, ChildManifestError> {
        self.verify(parent)?;
        if cents > self.authority.max_cost_cents {
            return Err(ChildManifestError::AuthorityWidened);
        }
        self.authority.max_cost_cents = cents;
        self.seal = self.compute_seal(&parent.key);
        self.verify(parent)?;
        Ok(self)
    }
}
