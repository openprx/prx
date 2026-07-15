//! Shutdown token helpers shared across runtime modes (D5/D9).
//!
//! This module lives in the **lib** crate (declared from `runtime/mod.rs`) so
//! that background callers such as the daemon heartbeat, the cron scheduler and
//! the xin runner — all compiled into the lib — can reach it. It must **not**
//! live in `runtime/mode.rs`, which is binary-only.

use tokio_util::sync::CancellationToken;

/// Returns a [`CancellationToken`] that is never cancelled by this helper.
///
/// Used by background call sites (daemon heartbeat, cron scheduler, xin runner,
/// daemon channel supervisor) that drive a mode's `run` but have no cooperative
/// shutdown signal of their own. Semantically this is **not** a child token that
/// would propagate cancellation — naming it explicitly avoids the misreading
/// that an `CancellationToken::new()` passed here might ever fire. The owning
/// supervisor terminates these tasks via abort/drop, not via this token.
pub fn never_cancelled_shutdown() -> CancellationToken {
    CancellationToken::new()
}
