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
    pub factory: AtomicUsize,
    pub constructor: AtomicUsize,
    pub backend_slots: AtomicUsize,
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

    pub fn assert_constructs_nothing(&self, cell: &str) {
        assert_eq!(
            self.counts.factory.load(Ordering::SeqCst),
            0,
            "{cell}: prediction_port must not run"
        );
        assert_eq!(
            self.counts.backend_slots.load(Ordering::SeqCst),
            0,
            "{cell}: disabled engine must construct no backend slot"
        );
        assert_eq!(
            self.counts.constructor.load(Ordering::SeqCst),
            0,
            "{cell}: decision engine constructor must not run"
        );
        assert_eq!(
            self.counts.stores.load(Ordering::SeqCst),
            0,
            "{cell}: SqliteTelemetryWriter::open must not run"
        );
        assert_eq!(
            self.counts.credentials.load(Ordering::SeqCst),
            0,
            "{cell}: decision-backend credential resolver must not run"
        );
    }
}
impl Drop for AccessSpy {
    fn drop(&mut self) {
        SPIES.lock().remove(&self.workspace);
    }
}
pub(crate) fn record(workspace: &Path, credential: bool) {
    bump(workspace, |counts| {
        if credential {
            &counts.credentials
        } else {
            &counts.stores
        }
    });
}

pub(crate) fn record_factory(workspace: &Path) {
    bump(workspace, |counts| &counts.factory);
}

pub(crate) fn record_constructor(workspace: &Path) {
    bump(workspace, |counts| &counts.constructor);
}

fn bump(workspace: &Path, counter: impl FnOnce(&AccessCounts) -> &AtomicUsize) {
    let counts = SPIES.lock().get(workspace).and_then(Weak::upgrade);
    if let Some(counts) = counts {
        counter(&counts).fetch_add(1, Ordering::SeqCst);
    }
}

pub(crate) fn record_backend_slot(workspace: &Path) {
    bump(workspace, |counts| &counts.backend_slots);
}
