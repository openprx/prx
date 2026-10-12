//! Test-only restoration of historical registry metadata; never grants launch authority.
use crate::tools::sessions_spawn::SubAgentRun;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;

struct PendingRows {
    workspace: PathBuf,
    rows: Option<Vec<SubAgentRun>>,
}

thread_local! {
    static PENDING: RefCell<Option<PendingRows>> = const { RefCell::new(None) };
}

/// Keep the fixture local to this synchronous factory call and clear it on panic.
pub(super) fn with_rows<T>(workspace: &Path, rows: Vec<SubAgentRun>, build: impl FnOnce() -> T) -> T {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            PENDING.with(|pending| *pending.borrow_mut() = None);
        }
    }
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        assert!(pending.is_none(), "legacy registry fixture cannot be nested");
        *pending = Some(PendingRows {
            workspace: workspace.to_path_buf(),
            rows: Some(rows),
        });
    });
    let _clear = Clear;
    let built = build();
    PENDING.with(|pending| {
        assert!(
            pending.borrow().as_ref().is_some_and(|pending| pending.rows.is_none()),
            "production registrar must consume the legacy rows exactly once"
        );
    });
    built
}

pub(super) fn seed_registered_runs(workspace: &Path, runs: &Arc<RwLock<Vec<SubAgentRun>>>) {
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        let Some(pending) = pending.as_mut() else {
            return;
        };
        assert_eq!(
            pending.workspace, workspace,
            "legacy rows belong to this workspace only"
        );
        let rows = pending.rows.take().expect("legacy rows already consumed");
        let mut runs = runs.try_write().expect("new registry must not be borrowed");
        assert!(runs.is_empty(), "legacy fixture must not overwrite actual runs");
        *runs = rows;
    });
}
