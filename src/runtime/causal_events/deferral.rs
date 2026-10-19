//! Each admission may defer only its own ordinary events, until its deadline.
use crate::decision::telemetry::CausalLineage;
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::sync::Notify;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AdmissionKey {
    workspace: Arc<str>,
    owner: Option<Arc<str>>,
    session: Option<Arc<str>>,
    run: Option<Arc<str>>,
    source: Option<Arc<str>>,
}

impl From<&CausalLineage> for AdmissionKey {
    fn from(lineage: &CausalLineage) -> Self {
        Self {
            workspace: Arc::clone(&lineage.workspace_id),
            owner: lineage.owner_id.clone(),
            session: lineage.session_key.clone(),
            run: lineage.run_id.clone(),
            source: lineage.source_event_id.clone(),
        }
    }
}

pub(super) struct AdmissionDeferral {
    pub(super) deadline: tokio::time::Instant,
    released: AtomicBool,
}

impl AdmissionDeferral {
    pub(super) fn ready(&self) -> bool {
        self.released.load(Ordering::Acquire) || tokio::time::Instant::now() >= self.deadline
    }
}

#[derive(Default)]
pub(super) struct PersistenceDeferral {
    admissions: Mutex<BTreeMap<AdmissionKey, Weak<AdmissionDeferral>>>,
    pub(super) changed: Notify,
}

pub(crate) struct PersistenceGuard {
    state: Arc<AdmissionDeferral>,
    registry: Weak<PersistenceDeferral>,
    key: AdmissionKey,
}

impl PersistenceDeferral {
    pub(super) fn begin(self: &Arc<Self>, lineage: &CausalLineage, deadline: Instant) -> PersistenceGuard {
        let key = AdmissionKey::from(lineage);
        let state = Arc::new(AdmissionDeferral {
            deadline: deadline.into(),
            released: AtomicBool::new(false),
        });
        let previous = self.admissions.lock().insert(key.clone(), Arc::downgrade(&state));
        if let Some(previous) = previous.and_then(|old| old.upgrade()) {
            // Re-admitting the same source must never extend an earlier event's
            // deferral. Already-queued events keep their original token.
            previous.released.store(true, Ordering::Release);
            self.changed.notify_one();
        }
        PersistenceGuard {
            state,
            registry: Arc::downgrade(self),
            key,
        }
    }

    pub(super) fn for_event(&self, lineage: &CausalLineage) -> Option<Arc<AdmissionDeferral>> {
        let mut admissions = self.admissions.lock();
        if admissions.is_empty() {
            return None;
        }
        let key = AdmissionKey::from(lineage);
        let state = admissions.get(&key).and_then(Weak::upgrade);
        if state.as_ref().is_none_or(|state| state.ready()) {
            admissions.remove(&key);
            return None;
        }
        state
    }
}

impl Drop for PersistenceGuard {
    fn drop(&mut self) {
        self.state.released.store(true, Ordering::Release);
        if let Some(registry) = self.registry.upgrade() {
            let mut admissions = registry.admissions.lock();
            if admissions
                .get(&self.key)
                .is_some_and(|state| state.ptr_eq(&Arc::downgrade(&self.state)))
            {
                admissions.remove(&self.key);
            }
            drop(admissions);
            registry.changed.notify_one();
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "deferral/independence_tests.rs"]
mod independence_tests;
