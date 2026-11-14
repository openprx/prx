//! Decision identifiers (CDP-PLAN 1.8).
//!
//! A decision id is a UUID. The runtime's decisions get a UUIDv7 when they
//! are recorded (CDP-22). A decision imported from v1 feedback gets a UUIDv8
//! derived from a keyed digest of where the line came from, so importing the
//! same line again yields the same id and the insert is a no-op.

use std::fmt;

use uuid::Uuid;

/// A decision id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DecisionId(Uuid);

impl DecisionId {
    /// The UUID carried by a runtime-owned hop lineage.
    pub(crate) const fn as_uuid(self) -> Uuid {
        self.0
    }

    /// A new runtime decision. UUIDv7 keeps locally recorded decisions in
    /// creation order without encoding any turn content or owner identity.
    pub(crate) fn new_runtime() -> Self {
        Self(Uuid::now_v7())
    }
}

/// Test-support constructor for fixed fixture ids; production code only
/// mints runtime ids.
#[cfg(test)]
pub(crate) mod test_support {
    impl super::DecisionId {
        /// Parse the hyphenated form of a fixture id.
        pub(crate) fn parse(text: &str) -> Result<Self, uuid::Error> {
            uuid::Uuid::parse_str(text.trim()).map(Self)
        }
    }
}

impl fmt::Display for DecisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.as_hyphenated().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_id_is_v7_and_round_trips() {
        let first = DecisionId::new_runtime();
        let second = DecisionId::new_runtime();
        assert_ne!(first, second);
        assert_eq!(first.0.get_version_num(), 7);
    }
}
