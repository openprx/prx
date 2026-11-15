//! Drive the real `prx` binary on a pseudo-terminal, bounded.
//!
//! The child is the session leader of the PTY (so `/dev/tty` is the PTY and
//! Ctrl-C reaches it as SIGINT). Reads are bounded by a byte cap and a
//! deadline; dropping the handle kills and reaps the child (bounded).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::Command;
use std::time::{Duration, Instant};

use expectrl::session::Session;

/// Most bytes kept from the PTY; more fails the test.
pub const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
/// Longest wait for one expected output.
pub const WAIT: Duration = Duration::from_secs(120);
/// Poll interval while the PTY has no data.
const POLL: Duration = Duration::from_millis(20);
/// Bound on kill-and-reap during cleanup.
const REAP_BOUND: Duration = Duration::from_secs(10);

pub struct Pty {
    session: Session,
    capture: Vec<u8>,
    exit: Option<expectrl::WaitStatus>,
}

impl Pty {
    pub fn spawn(command: Command) -> Self {
        let mut session = Session::spawn(command).expect("test: spawn prx on a PTY");
        let _ = session.get_process_mut().set_window_size(120, 40);
        Self {
            session,
            capture: Vec::new(),
            exit: None,
        }
    }

    /// Everything read so far, lossily decoded.
    pub fn captured(&self) -> String {
        String::from_utf8_lossy(&self.capture).into_owned()
    }

    /// Bytes typed at the terminal.
    pub fn send(&mut self, bytes: &str) {
        self.session.send(bytes).expect("test: write to the PTY");
    }

    /// Read until the capture after `from` contains `needle`; returns that
    /// part of the capture.
    pub fn read_until(&mut self, from: usize, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let tail = String::from_utf8_lossy(self.capture.get(from..).unwrap_or_default()).into_owned();
            if tail.contains(needle) {
                return tail;
            }
            assert!(
                Instant::now() < deadline,
                "timeout waiting for `{needle}`. Captured:\n----\n{}\n----",
                self.captured()
            );
            if !self.pump() {
                let tail = String::from_utf8_lossy(self.capture.get(from..).unwrap_or_default()).into_owned();
                assert!(
                    tail.contains(needle),
                    "prx exited before `{needle}`. Captured:\n----\n{}\n----",
                    self.captured()
                );
                return tail;
            }
        }
    }

    /// Current capture length, a mark for [`Self::read_until`].
    pub const fn mark(&self) -> usize {
        self.capture.len()
    }

    /// Wait for the child to exit; returns its exit code (`None` if signalled).
    pub fn wait_exit(&mut self) -> Option<i32> {
        let deadline = Instant::now() + WAIT;
        while self.pump() {
            assert!(
                Instant::now() < deadline,
                "prx did not exit. Captured:\n----\n{}\n----",
                self.captured()
            );
        }
        match self.exit {
            Some(expectrl::WaitStatus::Exited(_, code)) => Some(code),
            _ => None,
        }
    }

    /// Read what is available; `false` once the child has exited and its
    /// output is drained.
    fn pump(&mut self) -> bool {
        let mut buffer = [0_u8; 4096];
        match self.session.try_read(&mut buffer) {
            Ok(0) => {}
            Ok(read) => {
                assert!(
                    self.capture.len() + read <= MAX_CAPTURE_BYTES,
                    "PTY output exceeded MAX_CAPTURE_BYTES"
                );
                self.capture.extend_from_slice(&buffer[..read]);
                return true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
        if self.exited() {
            return false;
        }
        std::thread::sleep(POLL);
        true
    }

    fn exited(&mut self) -> bool {
        if self.exit.is_some() {
            return true;
        }
        match self.session.get_process().status() {
            Ok(status @ (expectrl::WaitStatus::Exited(_, _) | expectrl::WaitStatus::Signaled(_, _, _))) => {
                self.exit = Some(status);
                true
            }
            _ => false,
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.exited() {
            return;
        }
        let _ = self.session.get_process_mut().signal(expectrl::Signal::SIGKILL);
        let deadline = Instant::now() + REAP_BOUND;
        while Instant::now() < deadline && !self.exited() {
            std::thread::sleep(POLL);
        }
    }
}
