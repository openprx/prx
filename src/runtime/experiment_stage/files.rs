//! Conservative, nonrefundable native-write admission for isolated case roots.
//! Runtime metadata is checked separately; this is not an OS filesystem quota.
use super::{Arc, AtomicU64, ExperimentStageRuntime, Ordering, current_tracker};
use crate::runtime::turn_cleanup::CleanupTicket;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(super) struct ScratchWrites {
    roots: parking_lot::Mutex<BTreeSet<PathBuf>>,
    baseline: AtomicU64,
    written: AtomicU64,
}
pub(crate) struct ScratchRoot {
    stage: Arc<ExperimentStageRuntime>,
    root: PathBuf,
}
impl Drop for ScratchRoot {
    fn drop(&mut self) {
        self.stage.files.roots.lock().remove(&self.root);
    }
}
pub(crate) struct NativeWrite {
    stage: Arc<ExperimentStageRuntime>,
    ticket: CleanupTicket,
}
impl NativeWrite {
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        self.stage.ensure_active()
    }
    pub(crate) fn finish(self) {
        self.ticket.finish(true);
    }
}
impl ExperimentStageRuntime {
    pub(crate) fn register_scratch(self: &Arc<Self>, root: &Path, bytes: u64) -> anyhow::Result<ScratchRoot> {
        self.ensure_active()?;
        self.account_scratch(bytes)?;
        let root = root.canonicalize()?;
        anyhow::ensure!(
            self.files.roots.lock().insert(root.clone()),
            "scratch root already registered"
        );
        self.files.baseline.fetch_max(bytes, Ordering::AcqRel);
        Ok(ScratchRoot {
            stage: Arc::clone(self),
            root,
        })
    }
    pub(crate) const fn scratch_limit(&self) -> u64 {
        self.resources.scratch_bytes
    }
    /// Charge the entire resulting file, including edits, before allocating
    /// replacement bytes. Failed or dropped writes never refresh this allowance.
    pub(crate) fn prepare_native_write(
        self: &Arc<Self>,
        workspace: &Path,
        target: &Path,
        bytes: usize,
    ) -> anyhow::Result<NativeWrite> {
        self.ensure_active()?;
        let root = workspace.canonicalize()?;
        anyhow::ensure!(
            self.files.roots.lock().contains(&root) && target.starts_with(&root),
            "file effect is outside an admitted isolated scratch root"
        );
        let bytes = u64::try_from(bytes)?;
        let baseline = self.files.baseline.load(Ordering::Acquire);
        self.files
            .written
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(bytes).filter(|next| {
                    baseline
                        .checked_add(*next)
                        .is_some_and(|total| total <= self.resources.scratch_bytes)
                })
            })
            .map_err(|_| anyhow::anyhow!("experiment native-write allowance exhausted"))?;
        let ticket = current_tracker()
            .ok_or_else(|| anyhow::anyhow!("file write has no cleanup owner"))?
            .register(self.cancel.child_token())?;
        Ok(NativeWrite {
            stage: Arc::clone(self),
            ticket,
        })
    }
    pub(super) fn written_scratch_bytes(&self) -> u64 {
        self.files
            .baseline
            .load(Ordering::Acquire)
            .saturating_add(self.files.written.load(Ordering::Acquire))
    }
}
