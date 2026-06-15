//! Stable session identifier for the chat multi-session runtime.
//!
//! The underlying `sessions_spawn` registry keys runs by `String` UUID
//! (`SubAgentRun.id`). To avoid a migration while still giving the chat side a
//! cheap-to-clone, hashable handle, [`SessionId`] is a newtype over `Arc<str>`
//! that converts losslessly to/from the existing run id.
//!
//! The user-facing short alias (`#N`) is *not* part of this type — it is a
//! display-only sequence number owned by the single-threaded chat main loop
//! (see [`super::runtime::ChatSessionsHandle`]). The real key is always the
//! UUID.

use std::sync::Arc;

/// A chat session identifier, backed by the `sessions_spawn` run UUID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(Arc<str>);

impl SessionId {
    /// Build a [`SessionId`] from a `sessions_spawn` run id (`SubAgentRun.id`).
    #[must_use]
    pub fn from_run_id(run_id: &str) -> Self {
        Self(Arc::from(run_id))
    }

    /// Borrow the underlying id string (e.g. to look up a run in the registry).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_run_id() {
        let id = SessionId::from_run_id("abc-123");
        assert_eq!(id.as_str(), "abc-123");
        assert_eq!(id.to_string(), "abc-123");
    }

    #[test]
    fn equality_and_clone_share_the_same_key() {
        let a = SessionId::from_run_id("dup");
        let b = a.clone();
        assert_eq!(a, b);
    }
}
