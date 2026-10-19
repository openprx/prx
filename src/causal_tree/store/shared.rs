//! Per-path initialization: filesystem work never holds the process registry.
use super::{CausalDecisionStore, CausalEventSink, Retention, StoreError, StoreMode, WriterSetup, db_path};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Weak},
};

#[derive(Default)]
pub(super) struct StoreSlot {
    opening: Mutex<()>,
    current: Mutex<Weak<CausalDecisionStore>>,
}

static STORES: LazyLock<Mutex<HashMap<PathBuf, Weak<StoreSlot>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn slot(path: PathBuf) -> Arc<StoreSlot> {
    let mut stores = STORES.lock();
    if let Some(slot) = stores.get(&path).and_then(Weak::upgrade) {
        return slot;
    }
    stores.retain(|_, slot| slot.strong_count() > 0);
    let slot = Arc::new(StoreSlot::default());
    stores.insert(path, Arc::downgrade(&slot));
    slot
}

pub(super) fn existing_writer(workspace: &Path) -> Option<Arc<CausalDecisionStore>> {
    let slot = STORES.lock().get(&db_path(workspace)).and_then(Weak::upgrade)?;
    let existing = slot.current.lock().upgrade();
    existing.filter(|store| store.mode == StoreMode::ReadWrite && store.writer.is_some())
}

pub(super) fn open(
    workspace: &Path,
    retention: Retention,
    events: Arc<dyn CausalEventSink>,
    mode: StoreMode,
) -> Result<Arc<CausalDecisionStore>, StoreError> {
    let slot = slot(db_path(workspace));
    let _opening = slot.opening.lock();
    let existing = slot.current.lock().upgrade();
    if let Some(existing) = existing {
        if existing.mode == StoreMode::ReadWrite || mode == StoreMode::ReadOnly {
            return Ok(existing);
        }
    }
    let mut store = CausalDecisionStore::open(workspace, retention, events, mode, WriterSetup::default())?;
    store.shared_slot = Some(Arc::clone(&slot));
    let store = Arc::new(store);
    *slot.current.lock() = Arc::downgrade(&store);
    Ok(store)
}

/// Check holder count and detach under the same short state lock used by
/// borrowers. Neither this lock nor the registry participates in SQLite I/O.
pub(super) fn detach_last(store: &Arc<CausalDecisionStore>) -> bool {
    let Some(slot) = &store.shared_slot else {
        return Arc::strong_count(store) == 1;
    };
    let mut current = slot.current.lock();
    if Arc::strong_count(store) > 1 {
        return false;
    }
    if current.ptr_eq(&Arc::downgrade(store)) {
        *current = Weak::new();
    }
    true
}

#[cfg(test)]
mod tests;
