//! Source-owned telemetry routes for a shared decision-store writer.
use crate::memory::Memory;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, Weak},
};

pub(crate) struct EventMemory {
    pub(super) memory: Arc<dyn Memory>,
    pub(super) workspace: Option<Arc<str>>,
    pub(super) generation: u64,
    pub(super) revision: Arc<str>,
}

impl EventMemory {
    pub(crate) fn fallback(memory: Arc<dyn Memory>, generation: u64, revision: &str) -> Arc<Self> {
        Arc::new(Self {
            memory,
            workspace: None,
            generation,
            revision: Arc::from(revision),
        })
    }
    pub(super) fn same_context(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.memory, &other.memory)
            && self.workspace == other.workspace
            && self.generation == other.generation
            && self.revision == other.revision
    }
}

type Routes = HashMap<(String, String), Weak<EventMemory>>;
fn routes() -> &'static Mutex<Routes> {
    static ROUTES: OnceLock<Mutex<Routes>> = OnceLock::new();
    ROUTES.get_or_init(Mutex::default)
}

/// Only a source-bound decision service can issue this capability. Contexts
/// and queued events retain it; the registry retains no completed turns.
pub(crate) fn register(
    namespace: &str,
    workspace: &str,
    run: &str,
    memory: Arc<dyn Memory>,
    generation: u64,
    revision: &str,
) -> anyhow::Result<Arc<EventMemory>> {
    anyhow::ensure!(
        !namespace.is_empty() && !workspace.is_empty() && !run.is_empty(),
        "event memory requires source identity"
    );
    let mut routes = routes().lock();
    routes.retain(|_, route| route.strong_count() > 0);
    let key = (namespace.to_owned(), run.to_owned());
    let route = Arc::new(EventMemory {
        memory,
        workspace: Some(Arc::from(workspace)),
        generation,
        revision: Arc::from(revision),
    });
    if let Some(existing) = routes.get(&key).and_then(Weak::upgrade) {
        anyhow::ensure!(existing.same_context(&route), "event memory source binding changed");
        return Ok(existing);
    }
    routes.insert(key, Arc::downgrade(&route));
    Ok(route)
}

pub(super) fn lookup(namespace: &str, run: Option<&str>) -> Option<Arc<EventMemory>> {
    let run = run?;
    routes()
        .lock()
        .get(&(namespace.to_owned(), run.to_owned()))
        .and_then(Weak::upgrade)
}
