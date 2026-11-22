//! Run provenance of memory and document writes.
//!
//! A turn installs its run identity once ([`scope_turn`]); the tool scheduler
//! narrows it to one tool call ([`scope_operation`]). A write path that wants
//! its write attributed asks for the installed provenance view of its memory
//! handle ([`apply`]); the backend then appends one content-free
//! `memory_write_log` row in the same transaction as the write. A write made
//! with no provenance installed (import, CLI, webhook, snapshot) is not a
//! run's write and records nothing.
use super::Memory;
use std::future::Future;
use std::sync::Arc;

/// The run (and, inside a tool call, the call) a write belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteProvenance {
    root_run_id: String,
    run_id: String,
    parent_run_id: Option<String>,
    operation_id: Option<String>,
}

impl WriteProvenance {
    /// Provenance of a run. The root is the explicit root when known, else the
    /// parent run (a first-level child), else the run itself (a top-level run).
    #[must_use]
    pub fn for_run(run_id: String, parent_run_id: Option<String>, root_run_id: Option<String>) -> Self {
        let root_run_id = root_run_id
            .or_else(|| parent_run_id.clone())
            .unwrap_or_else(|| run_id.clone());
        Self {
            root_run_id,
            run_id,
            parent_run_id,
            operation_id: None,
        }
    }

    /// The same run, narrowed to one tool call.
    #[must_use]
    pub fn for_operation(&self, operation_id: &str) -> Self {
        Self {
            operation_id: Some(operation_id.to_owned()),
            ..self.clone()
        }
    }

    #[must_use]
    pub fn root_run_id(&self) -> &str {
        &self.root_run_id
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[must_use]
    pub fn parent_run_id(&self) -> Option<&str> {
        self.parent_run_id.as_deref()
    }

    #[must_use]
    pub fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }
}

tokio::task_local! { static INSTALLED: WriteProvenance; }

/// A run's id and its root, known to a process that hosts children of that
/// run without running it (a session worker learns them from its sealed
/// manifest). Consulted only to resolve a child's root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InheritedRoot {
    run_id: String,
    root_run_id: String,
}

impl InheritedRoot {
    pub(crate) fn new(run_id: Option<&str>, root_run_id: Option<&str>) -> Option<Self> {
        Some(Self {
            run_id: run_id?.to_owned(),
            root_run_id: root_run_id?.to_owned(),
        })
    }
}

tokio::task_local! { static INHERITED_ROOT: InheritedRoot; }

/// Run `future` with `inherited` available to [`child_root`].
pub(crate) async fn scope_inherited_root<F: Future>(inherited: Option<InheritedRoot>, future: F) -> F::Output {
    match inherited {
        Some(inherited) => INHERITED_ROOT.scope(inherited, future).await,
        None => future.await,
    }
}

/// The provenance installed for the current task, if any.
pub(crate) fn installed() -> Option<WriteProvenance> {
    INSTALLED.try_with(Clone::clone).ok()
}

/// Run one turn under its run provenance (or a spawned task under the
/// provenance captured from the call that started it). `None` (a turn
/// without a run identity) leaves whatever is installed untouched.
pub(crate) async fn scope_turn<F: Future>(provenance: Option<WriteProvenance>, future: F) -> F::Output {
    match provenance {
        Some(provenance) => INSTALLED.scope(provenance, future).await,
        None => future.await,
    }
}

/// Run one tool call under the installed run provenance narrowed to the
/// call's `operation_id`. Without an installed run nothing is attributed.
pub(crate) async fn scope_operation<F: Future>(operation_id: &str, future: F) -> F::Output {
    scope_turn(installed().map(|run| run.for_operation(operation_id)), future).await
}

/// The provenance view of `memory` for the installed provenance, or `memory`
/// itself when nothing is installed. A backend without a write log (anything
/// but SQLite) keeps writing unattributed; the forget step refuses those
/// backends instead.
pub(crate) fn apply(memory: &Arc<dyn Memory>) -> Arc<dyn Memory> {
    let Some(provenance) = installed() else {
        return Arc::clone(memory);
    };
    memory
        .with_write_provenance(provenance)
        .unwrap_or_else(|_unsupported| Arc::clone(memory))
}

/// The root run a child of `parent_run_id` inherits: the installed root when
/// the child is started from inside its parent's run, else the root the
/// hosting process inherited for that parent, else the parent itself.
pub(crate) fn child_root(parent_run_id: Option<&str>) -> Option<String> {
    let parent_run_id = parent_run_id?;
    let installed = installed()
        .filter(|installed| installed.run_id() == parent_run_id)
        .map(|installed| installed.root_run_id().to_owned());
    let inherited = || {
        INHERITED_ROOT
            .try_with(|inherited| (inherited.run_id == parent_run_id).then(|| inherited.root_run_id.clone()))
            .ok()
            .flatten()
    };
    Some(installed.or_else(inherited).unwrap_or_else(|| parent_run_id.to_owned()))
}
