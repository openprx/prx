#![cfg(test)]

use super::*;
use crate::security::SecurityPolicy;
use crate::security::policy::AutonomyLevel;
use std::sync::Arc;

fn plan() -> diff_apply::DiffApplyPlan {
    diff_apply::parse_unified_diff("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n").expect("valid plan")
}

fn policy(workspace: &std::path::Path) -> SecurityPolicy {
    let autonomy = crate::config::AutonomyConfig {
        level: AutonomyLevel::Full,
        ..crate::config::AutonomyConfig::default()
    };
    SecurityPolicy::from_config(&autonomy, workspace)
}

#[test]
fn plain_apply_ignores_openprx_approval_override_and_leaves_file_unchanged() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let exe = std::env::current_exe().expect("current test exe");
    let status = std::process::Command::new(exe)
        .arg("--exact")
        .arg("chat::iss_019_diff_apply_tests::plain_apply_no_bypass_child")
        .arg("--nocapture")
        .env("OPENPRX_APPROVAL_OVERRIDE", "allow")
        .env("ISS019_CHILD_WORKSPACE", temp.path())
        .status()
        .expect("run child no-bypass test");
    assert!(status.success(), "child no-bypass test failed: {status}");
}

#[test]
fn plain_apply_no_bypass_child() {
    let Some(workspace) = std::env::var_os("ISS019_CHILD_WORKSPACE") else {
        return;
    };
    assert_eq!(
        std::env::var("OPENPRX_APPROVAL_OVERRIDE").as_deref(),
        Ok("allow"),
        "child test must exercise the env-override condition"
    );
    let workspace = std::path::PathBuf::from(workspace);
    std::fs::write(workspace.join("a.txt"), "old\n").expect("seed");
    let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();
    let router = Arc::new(dispatcher::ApprovalRouter::new());

    let result = request_diff_apply_approval(plan(), false, Some(&router), &dispatcher);

    assert!(
        result.is_err(),
        "plain mode must fail closed before approval registration"
    );
    assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "old\n");
}

#[test]
fn missing_router_fails_closed_before_pending_apply() {
    let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();
    let result = request_diff_apply_approval(plan(), true, None, &dispatcher);
    assert!(result.is_err());
}

#[test]
fn channel_drop_resolves_false_and_returns_no_pending_apply() {
    let (dispatcher, rx) = dispatcher::ChatDispatcher::new();
    drop(rx);
    let router = Arc::new(dispatcher::ApprovalRouter::new());
    let result = request_diff_apply_approval(plan(), true, Some(&router), &dispatcher);
    assert!(result.is_err());
}

#[tokio::test]
async fn tui_approval_true_is_required_before_apply_writes() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(temp.path().join("a.txt"), "old\n").expect("seed");
    let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();
    let router = Arc::new(dispatcher::ApprovalRouter::new());

    let pending = request_diff_apply_approval(plan(), true, Some(&router), &dispatcher).expect("approval requested");
    assert_eq!(std::fs::read_to_string(temp.path().join("a.txt")).unwrap(), "old\n");
    assert!(router.resolve(&pending.tool_id, true));
    assert!(pending.approval_rx.await.expect("approval rx"));

    let message = diff_apply::execute_plan(&pending.plan, &policy(temp.path()))
        .await
        .expect("apply");
    assert_eq!(message, "Applied fenced diff to 1 file.");
    assert_eq!(std::fs::read_to_string(temp.path().join("a.txt")).unwrap(), "new\n");
}

#[tokio::test]
async fn tui_approval_false_leaves_workspace_unchanged() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(temp.path().join("a.txt"), "old\n").expect("seed");
    let (dispatcher, _rx) = dispatcher::ChatDispatcher::new();
    let router = Arc::new(dispatcher::ApprovalRouter::new());

    let pending = request_diff_apply_approval(plan(), true, Some(&router), &dispatcher).expect("approval requested");
    assert!(router.resolve(&pending.tool_id, false));
    assert!(!pending.approval_rx.await.expect("approval rx"));

    assert_eq!(std::fs::read_to_string(temp.path().join("a.txt")).unwrap(), "old\n");
}
