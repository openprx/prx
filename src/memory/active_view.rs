//! Runtime-owned memory view. Models cannot select or replace this capability.
use super::Memory;
use std::{future::Future, sync::Arc};

tokio::task_local! { static ACTIVE: Arc<dyn Memory>; }

pub(crate) fn current() -> Option<Arc<dyn Memory>> {
    ACTIVE.try_with(Arc::clone).ok()
}

pub(crate) async fn scope<F: Future>(memory: Arc<dyn Memory>, future: F) -> F::Output {
    ACTIVE.scope(memory, future).await
}

/// The memory a tool uses: the installed view, else the tool's own handle,
/// carrying the installed write provenance (`write_provenance::apply`).
pub(crate) fn select(default: &Arc<dyn Memory>) -> Arc<dyn Memory> {
    super::write_provenance::apply(&current().unwrap_or_else(|| Arc::clone(default)))
}

pub(crate) fn select_optional(default: &Option<Arc<dyn Memory>>) -> Option<Arc<dyn Memory>> {
    current()
        .or_else(|| default.clone())
        .map(|memory| super::write_provenance::apply(&memory))
}
