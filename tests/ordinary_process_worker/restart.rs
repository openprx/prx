//! Parent-restart controls used only by `tests/ordinary_process_worker.rs`:
//! kill a running parent while its worker is held, then release the hold.
//! The wire test in `tests/session_worker_stdin_protocol.rs` shares the parent
//! and provider harness but never restarts a parent, so these live apart.

use std::time::{Duration, Instant};

use super::parent::OwnedParent;
use super::provider::OwnedProviderServer;

impl OwnedParent {
    /// SIGKILL the parent and reap it (bounded). Its workers keep running.
    pub fn kill(&mut self) {
        self.kill_and_reap();
        assert!(self.reaped, "killed parent was not reaped within bound");
    }
}

impl OwnedProviderServer {
    /// Wait until the held marker's request arrived, at most `timeout`.
    pub fn wait_for_hold(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.hold.state.lock();
        while !state.reached {
            if self.shared.hold.signal.wait_until(&mut state, deadline).timed_out() {
                return state.reached;
            }
        }
        true
    }

    pub fn release_hold(&self) {
        self.shared.hold.release();
    }
}
