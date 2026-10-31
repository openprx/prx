//! One parent scratch owner attached to the original stage cleanup tracker.
//! Unknown owned case work is quarantined; Drop never recursively removes it.
use crate::runtime::turn_cleanup::{CleanupTicket, current_tracker};
use crate::skills::lifecycle::CleanupState;
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub(super) struct OwnedStudyScratch {
    path: PathBuf,
    unstarted: Option<tempfile::TempDir>,
    ticket: Option<CleanupTicket>,
    started: bool,
}
impl OwnedStudyScratch {
    pub(super) fn new(workspace: &Path, cancel: &CancellationToken) -> Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix("prx-v01-owned-")
            .tempdir_in(workspace)?;
        let tracker = current_tracker().context("study parent scratch has no stage cleanup owner")?;
        let ticket = match tracker.register(cancel.child_token()) {
            Ok(ticket) => ticket,
            Err(error) => {
                scratch.close().context("unstarted study parent cleanup failed")?;
                return Err(error);
            }
        };
        Ok(Self {
            path: scratch.path().to_path_buf(),
            unstarted: Some(scratch),
            ticket: Some(ticket),
            started: false,
        })
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    /// The original stage ticket already owns this responsibility before RAII
    /// is released, and before the first poll that can spawn case work.
    pub(super) fn start(&mut self) -> Result<()> {
        let scratch = self.unstarted.take().context("study scratch was already started")?;
        self.started = true;
        self.path = scratch.keep();
        Ok(())
    }
    /// AgentCaseExecutor has returned only after its own children and case
    /// cleanup. Empty parent alone is insufficient without that owner result.
    pub(super) fn confirm_case_removed(&mut self, cleanup: CleanupState) -> Result<()> {
        ensure!(
            self.started && cleanup == CleanupState::Removed,
            "study case cleanup is not confirmed; parent retained"
        );
        ensure!(
            std::fs::read_dir(&self.path)?.next().is_none(),
            "study parent contains owned work; parent retained"
        );
        std::fs::remove_dir(&self.path).context("study empty parent cleanup failed")?;
        self.started = false;
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(true);
        }
        Ok(())
    }
}
impl Drop for OwnedStudyScratch {
    fn drop(&mut self) {
        let confirmed = if let Some(unstarted) = self.unstarted.take() {
            // The executor constructor cannot spawn work. Recursive RAII close
            // is safe only before the explicit start handoff.
            unstarted.close().is_ok()
        } else {
            !self.started
        };
        if let Some(ticket) = self.ticket.take() {
            ticket.finish(confirmed);
        }
    }
}
