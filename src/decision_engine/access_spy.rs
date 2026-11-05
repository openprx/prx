//! Workspace-scoped counters observe real access sites without cross-test races.
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};
#[derive(Default)]
pub(crate) struct AccessCounts {
    pub stores: AtomicUsize,
    pub credentials: AtomicUsize,
}
static SPIES: LazyLock<Mutex<HashMap<PathBuf, Weak<AccessCounts>>>> = LazyLock::new(Default::default);
pub(crate) struct AccessSpy {
    workspace: PathBuf,
    pub counts: Arc<AccessCounts>,
}
impl AccessSpy {
    pub fn new(workspace: &Path) -> Self {
        let counts = Arc::new(AccessCounts::default());
        SPIES.lock().insert(workspace.to_owned(), Arc::downgrade(&counts));
        Self {
            workspace: workspace.to_owned(),
            counts,
        }
    }
}
impl Drop for AccessSpy {
    fn drop(&mut self) {
        SPIES.lock().remove(&self.workspace);
    }
}
pub(crate) fn record(workspace: &Path, credential: bool) {
    let counts = SPIES.lock().get(workspace).and_then(Weak::upgrade);
    if let Some(counts) = counts {
        let counter = if credential {
            &counts.credentials
        } else {
            &counts.stores
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }
}
