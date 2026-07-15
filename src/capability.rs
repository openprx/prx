//! Shared capability availability vocabulary.
//!
//! Configuration, construction, readiness, and observed health are different
//! facts.  User-facing catalogs must preserve that distinction instead of
//! promoting a non-empty configuration field to an `Active` claim.

use serde::{Deserialize, Serialize};

/// Strongest availability fact currently established for a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityAvailabilityLevel {
    /// The capability is known to the product, but is not configured.
    Declared,
    /// Required configuration was detected; no executable readiness is claimed.
    Configured,
    /// An executable backend is registered and can be selected.
    Ready,
    /// A runtime probe has positively observed the backend.
    Healthy,
}

impl CapabilityAvailabilityLevel {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Declared => "Declared",
            Self::Configured => "Configured",
            Self::Ready => "Ready",
            Self::Healthy => "Healthy",
        }
    }
}

/// Availability evidence carried by every canonical capability descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAvailability {
    pub level: CapabilityAvailabilityLevel,
    pub reason: String,
}

impl CapabilityAvailability {
    #[must_use]
    pub fn new(level: CapabilityAvailabilityLevel, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        debug_assert!(!reason.trim().is_empty(), "capability availability requires a reason");
        Self { level, reason }
    }

    #[must_use]
    pub fn declared(reason: impl Into<String>) -> Self {
        Self::new(CapabilityAvailabilityLevel::Declared, reason)
    }

    #[must_use]
    pub fn configured(reason: impl Into<String>) -> Self {
        Self::new(CapabilityAvailabilityLevel::Configured, reason)
    }

    #[must_use]
    pub fn ready(reason: impl Into<String>) -> Self {
        Self::new(CapabilityAvailabilityLevel::Ready, reason)
    }

    #[must_use]
    pub fn healthy(reason: impl Into<String>) -> Self {
        Self::new(CapabilityAvailabilityLevel::Healthy, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_levels_are_ordered_by_evidence_strength() {
        assert!(CapabilityAvailabilityLevel::Declared < CapabilityAvailabilityLevel::Configured);
        assert!(CapabilityAvailabilityLevel::Configured < CapabilityAvailabilityLevel::Ready);
        assert!(CapabilityAvailabilityLevel::Ready < CapabilityAvailabilityLevel::Healthy);
    }
}
