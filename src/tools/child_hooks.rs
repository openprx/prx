//! The hook manager a sub-agent shares with its parent (finding 96b).
//!
//! The parent's registry carries the live manager through its hook control
//! tools ([`Tool::live_hook_manager`]). A child that runs without an Ordinary
//! parent runtime uses that same manager, so its events join the parent's
//! queue (per-manager order holds and the parent owner's shutdown drain covers
//! them) and reach the parent's plugin runtime. A parent that has none leaves
//! the child with an inert manager, as before.

use crate::hooks::HookManager;
use crate::tools::Tool;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

/// The live manager of the parent registry, when it carries one.
pub(crate) fn parent_hook_manager<T>(parent_tools: &[T]) -> Option<Arc<HookManager>>
where
    T: Deref<Target = dyn Tool>,
{
    parent_tools.iter().find_map(|tool| tool.live_hook_manager())
}

/// The manager a child without an Ordinary parent runtime uses: the parent's,
/// else an inert one rooted at `workspace_dir`.
pub(crate) fn child_hooks(parent: Option<Arc<HookManager>>, workspace_dir: &Path) -> Arc<HookManager> {
    parent.unwrap_or_else(|| Arc::new(HookManager::new(workspace_dir.to_path_buf())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityPolicy;

    fn security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    #[tokio::test]
    async fn the_parent_manager_is_found_through_both_registry_shapes() {
        let workspace = tempfile::tempdir().unwrap();
        let manager = Arc::new(HookManager::new(workspace.path().to_path_buf()));
        let controls = manager.control_tool_arcs(security());

        let found = parent_hook_manager(&controls).expect("test: arc registry carries the manager");
        assert!(Arc::ptr_eq(&found, &manager));

        let boxed: Vec<Box<dyn Tool>> = controls
            .into_iter()
            .map(super::super::ArcDelegatingTool::boxed)
            .collect();
        let found = parent_hook_manager(&boxed).expect("test: boxed registry carries the manager");
        assert!(Arc::ptr_eq(&found, &manager));
    }

    #[tokio::test]
    async fn without_a_parent_manager_the_child_stays_inert() {
        let workspace = tempfile::tempdir().unwrap();
        let no_tools: Vec<Arc<dyn Tool>> = Vec::new();
        assert!(parent_hook_manager(&no_tools).is_none());

        let inert = child_hooks(None, workspace.path());
        inert
            .emit(crate::hooks::HookEvent::TurnComplete, serde_json::json!({}))
            .await;
        assert!(inert.drain(std::time::Duration::from_secs(5)).await);
        // No hooks.json and no opt-in: nothing runs, and it is a manager of its own.
        let parent = Arc::new(HookManager::new(workspace.path().to_path_buf()));
        assert!(!Arc::ptr_eq(&inert, &parent));
        assert_eq!(parent.dispatch_stats().queued, 0);
    }

    /// A sessions_spawn task child and a delegate child both take their
    /// manager from `child_hooks` when no Ordinary parent runtime exists: an
    /// event emitted through it runs the parent's hook and is counted in the
    /// parent's queue (so the parent owner's shutdown drain covers it).
    #[tokio::test]
    async fn a_child_event_is_delivered_through_the_parents_manager() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let log = temp.path().join("seen.log");
        let hooks = serde_json::json!({
            "timeout_ms": 60_000,
            "hooks": {"turn_complete": [{
                "command": "sh",
                "args": ["-c", format!("echo hit >> '{}'", log.display())],
                "stdin_json": false
            }]}
        });
        std::fs::write(workspace.join("hooks.json"), hooks.to_string()).unwrap();
        let parent = Arc::new(HookManager::new(workspace.clone()).allowing_workspace_hooks(temp.path().join("hooks")));
        let registry: Vec<Box<dyn Tool>> = parent
            .control_tool_arcs(security())
            .into_iter()
            .map(super::super::ArcDelegatingTool::boxed)
            .collect();

        let child = child_hooks(parent_hook_manager(&registry), &workspace);
        child
            .emit(crate::hooks::HookEvent::TurnComplete, serde_json::json!({}))
            .await;
        // The parent's own drain, as its owner runs at shutdown, covers the child's event.
        parent.drain_for_shutdown().await;
        assert_eq!(parent.dispatch_stats().delivered, 1);
        assert!(std::fs::read_to_string(&log).unwrap_or_default().contains("hit"));
    }

    #[tokio::test]
    async fn a_shared_manager_is_the_one_the_child_uses() {
        let workspace = tempfile::tempdir().unwrap();
        let parent = Arc::new(HookManager::new(workspace.path().to_path_buf()));
        let child = child_hooks(Some(Arc::clone(&parent)), workspace.path());
        assert!(Arc::ptr_eq(&child, &parent));
    }
}
