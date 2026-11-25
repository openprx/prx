//! Whether the prompt sources may name this machine (`[agent.prompt]
//! share_host_identity`).
//!
//! The setting is process-wide: it is applied when the configuration loads
//! and when it reloads, so every prompt built afterwards follows it. Tools are
//! not affected and keep real paths.
use std::sync::atomic::{AtomicBool, Ordering};

static SHARE_HOST_IDENTITY: AtomicBool = AtomicBool::new(true);

/// Apply `[agent.prompt] share_host_identity`.
pub fn set_share_host_identity(share: bool) {
    SHARE_HOST_IDENTITY.store(share, Ordering::Relaxed);
}

/// Whether the prompt may carry the host name and the absolute workspace path.
pub(super) fn share_host_identity() -> bool {
    SHARE_HOST_IDENTITY.load(Ordering::Relaxed)
}
