//! Defer ordinary telemetry I/O while an authority-bound history hop runs.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct PersistenceDeferral {
    active: AtomicUsize,
    resumed: Notify,
}

pub(crate) struct PersistenceGuard(Arc<PersistenceDeferral>);

impl PersistenceDeferral {
    pub(super) fn begin(self: &Arc<Self>) -> PersistenceGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        PersistenceGuard(Arc::clone(self))
    }

    pub(super) async fn wait(&self) {
        loop {
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);
            resumed.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            resumed.await;
        }
    }
}

impl Drop for PersistenceGuard {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.resumed.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests;
