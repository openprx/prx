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

/// A string that is not a decision id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a decision id")]
pub(crate) struct InvalidDecisionId;

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

    /// The id of an imported v1 decision: a UUIDv8 over the first 16 bytes
    /// of `digest`, a keyed digest of the line's origin.
    pub(crate) fn legacy(digest: &[u8; 32]) -> Self {
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self(uuid::Builder::from_custom_bytes(bytes).into_uuid())
    }

    /// Parse the hyphenated form an operator types.
    ///
    /// # Errors
    ///
    /// [`InvalidDecisionId`] for anything that is not a UUID.
    pub(crate) fn parse(text: &str) -> Result<Self, InvalidDecisionId> {
        Uuid::parse_str(text.trim()).map(Self).map_err(|_| InvalidDecisionId)
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
    fn a_legacy_id_is_stable_and_round_trips() {
        let digest = [7_u8; 32];
        let id = DecisionId::legacy(&digest);
        assert_eq!(id, DecisionId::legacy(&digest), "the same origin gives the same id");
        assert_ne!(id, DecisionId::legacy(&[8_u8; 32]));
        assert_eq!(DecisionId::parse(&id.to_string()), Ok(id));
        assert_eq!(id.0.get_version_num(), 8);
        assert_eq!(DecisionId::parse("not-a-uuid"), Err(InvalidDecisionId));
    }

    #[test]
    fn runtime_id_is_v7_and_round_trips() {
        let first = DecisionId::new_runtime();
        let second = DecisionId::new_runtime();
        assert_ne!(first, second);
        assert_eq!(first.0.get_version_num(), 7);
        assert_eq!(DecisionId::parse(&first.to_string()), Ok(first));
    }
}
