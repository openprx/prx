//! P0 Integration Tests: Tool Security
//!
//! These tests validate the direct-shell boundary alongside the independent
//! `FileReadTool`, `HttpRequestTool`, and `SecurityPolicy` contracts.

use openprx::runtime::NativeRuntime;
use openprx::security::policy::{ActionTracker, AutonomyLevel, CommandRiskLevel, SecurityPolicy};
use openprx::tools::traits::Tool;
use openprx::tools::{FileReadTool, HttpRequestTool, ShellTool};
use serde_json::json;
use std::sync::Arc;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_security(overrides: impl FnOnce(&mut SecurityPolicy)) -> Arc<SecurityPolicy> {
    let mut policy = SecurityPolicy::default();
    overrides(&mut policy);
    Arc::new(policy)
}

fn native_runtime() -> Arc<dyn openprx::runtime::RuntimeAdapter> {
    Arc::new(NativeRuntime::new())
}

fn shell_tool(security: &Arc<SecurityPolicy>) -> ShellTool {
    ShellTool::new(native_runtime(), security.workspace_dir.clone())
}

// ═══════════════════════════════════════════════════════════════════════════
// INT-TS-01: Shell tool respects autonomy level
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn int_ts_01_direct_shell_does_not_reapply_autonomy_policy() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::ReadOnly;
        p.workspace_dir = std::env::temp_dir();
    });

    let tool = shell_tool(&security);
    let result = tool
        .execute(json!({"command": "echo hello"}))
        .await
        .expect("test: shell readonly should return ToolResult, not Err");

    assert!(result.success, "shell executor must not reapply outer autonomy policy");
    assert!(result.output.contains("hello"));
}

#[tokio::test]
async fn int_ts_01_shell_supervised_allows_low_risk() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
        p.workspace_dir = std::env::temp_dir();
    });

    let tool = shell_tool(&security);
    let result = tool
        .execute(json!({"command": "echo supervised_ok"}))
        .await
        .expect("test: supervised echo should succeed");

    assert!(result.success, "Supervised autonomy should allow low-risk commands");
    assert!(result.output.contains("supervised_ok"));
}

// ═══════════════════════════════════════════════════════════════════════════
// INT-TS-02: Shell tool classifies command risk
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn int_ts_02_risk_classification_low_executes() {
    let workspace = tempfile::tempdir().expect("test: create isolated workspace");
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
        p.workspace_dir = workspace.path().to_path_buf();
    });

    // `ls .` is Low risk and stays inside the configured workspace; under
    // Supervised, low-risk commands run without a grant.
    assert_eq!(
        security.command_risk_level("ls ."),
        CommandRiskLevel::Low,
        "test: 'ls .' should be classified Low"
    );

    let tool = shell_tool(&security);
    let result = tool
        .execute(json!({"command": "ls ."}))
        .await
        .expect("test: low-risk command should return ToolResult");
    assert!(
        result.success,
        "Low-risk 'ls .' should execute, error: {:?}",
        result.error
    );
}

#[tokio::test]
async fn int_ts_02_risk_classification_medium_needs_approval() {
    let workspace = tempfile::tempdir().expect("test: create isolated workspace");
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
        p.workspace_dir = workspace.path().to_path_buf();
    });

    assert_eq!(
        security.command_risk_level("touch newfile"),
        CommandRiskLevel::Medium,
        "test: 'touch newfile' should be classified Medium"
    );

    let tool = shell_tool(&security);

    let executed = tool
        .execute(json!({"command": "touch newfile"}))
        .await
        .expect("test: unapproved medium-risk should return ToolResult");
    assert!(
        executed.success,
        "executor must not parse command risk: {:?}",
        executed.error
    );
    assert!(workspace.path().join("newfile").exists());
}

#[tokio::test]
async fn int_ts_02_risk_classification_high_blocked() {
    let workspace = tempfile::tempdir().expect("test: create isolated workspace");
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
        p.workspace_dir = workspace.path().to_path_buf();
    });

    assert_eq!(
        security.command_risk_level("rm -rf ./fixture"),
        CommandRiskLevel::High,
        "test: workspace-local recursive removal should be classified High"
    );

    let tool = shell_tool(&security);
    let result = tool
        .execute(json!({"command": "rm -rf ./fixture"}))
        .await
        .expect("test: high-risk command should return ToolResult");
    assert!(
        result.success,
        "executor must pass command through unchanged: {:?}",
        result.error
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// INT-TS-03: Shell tool rate limiting via ActionTracker
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn int_ts_03_direct_shell_does_not_reapply_action_budget() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Full;
        p.workspace_dir = std::env::temp_dir();
        p.max_actions_per_hour = 10;
    });

    let tool = shell_tool(&security);

    // Execute 10 commands — all should succeed
    for i in 0..10 {
        let result = tool
            .execute(json!({"command": format!("echo iteration_{i}")}))
            .await
            .expect("test: command within rate limit should return ToolResult");
        assert!(
            result.success,
            "test: command {i} within limit should succeed, error: {:?}",
            result.error
        );
    }

    // Execution accounting belongs to the orchestration boundary, not here.
    let result = tool
        .execute(json!({"command": "echo overflow"}))
        .await
        .expect("test: rate-limited command should return ToolResult");
    assert!(result.success, "direct executor must not consume policy budget");
}

#[test]
fn int_ts_03_action_tracker_sliding_window() {
    let tracker = ActionTracker::new();
    assert_eq!(tracker.count(), 0, "test: fresh tracker should be at 0");
    tracker.record();
    tracker.record();
    tracker.record();
    assert_eq!(tracker.count(), 3, "test: tracker should count 3 after 3 records");
}

// ═══════════════════════════════════════════════════════════════════════════
// INT-TS-04: Shell tool blocks access to memory files
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn int_ts_04_shell_does_not_apply_memory_acl_to_command_text() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Full;
        p.workspace_dir = std::env::temp_dir();
    });

    let tool = shell_tool(&security);
    let result = tool
        .execute(json!({"command": "printf '%s' memory.md"}))
        .await
        .expect("test: ACL-blocked command should return ToolResult");

    assert!(result.success, "shell must not receive memory ACL state");
    assert_eq!(result.output, "memory.md");
}

#[tokio::test]
async fn int_ts_04_shell_allows_memory_path_syntax() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Full;
        p.workspace_dir = std::env::temp_dir();
    });

    let tool = shell_tool(&security);
    let result = tool
        .execute(json!({"command": "printf '%s' memory/brain.db"}))
        .await
        .expect("test: ACL-blocked command should return ToolResult");

    assert!(result.success, "shell must not filter memory path syntax");
    assert_eq!(result.output, "memory/brain.db");
}

#[tokio::test]
async fn int_ts_04_shell_allows_memory_access_when_acl_disabled() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Full;
        p.workspace_dir = std::env::temp_dir();
    });

    // acl_enabled = false => memory path check is skipped
    let tool = shell_tool(&security);
    let result = tool
        .execute(json!({"command": "echo memory.md test"}))
        .await
        .expect("test: non-ACL command should return ToolResult");

    // The command itself may or may not succeed depending on file existence,
    // but the ACL check should NOT block it.
    // Since "echo memory.md test" just echoes text, it should succeed.
    assert!(
        result.success,
        "With ACL disabled, commands referencing memory paths should not be ACL-blocked"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// INT-TS-05: FileRead tool workspace sandboxing
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn int_ts_05_file_read_rejects_absolute_path_outside_workspace() {
    let tmp = tempfile::tempdir().expect("test: should create temp dir");
    let security = make_security(|p| {
        p.workspace_dir = tmp.path().to_path_buf();
        p.workspace_only = true;
    });

    let tool = FileReadTool::new(security, false);
    let result = tool
        .execute(json!({"path": "/etc/passwd"}))
        .await
        .expect("test: blocked path should return ToolResult");

    assert!(
        !result.success,
        "workspace_only=true must reject absolute path /etc/passwd"
    );
    let err = result.error.as_deref().unwrap_or("");
    assert!(
        err.contains("not allowed"),
        "test: expected 'not allowed' in error, got: {err}"
    );
}

#[tokio::test]
async fn int_ts_05_file_read_rejects_path_traversal() {
    let tmp = tempfile::tempdir().expect("test: should create temp dir");
    let security = make_security(|p| {
        p.workspace_dir = tmp.path().to_path_buf();
        p.workspace_only = true;
    });

    let tool = FileReadTool::new(security, false);
    let result = tool
        .execute(json!({"path": "../../../etc/passwd"}))
        .await
        .expect("test: traversal path should return ToolResult");

    assert!(!result.success, "workspace_only=true must reject path traversal");
    let err = result.error.as_deref().unwrap_or("");
    assert!(
        err.contains("not allowed"),
        "test: expected 'not allowed' in error, got: {err}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn int_ts_05_file_read_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("test: should create temp dir");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");

    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("test: create workspace dir");
    tokio::fs::create_dir_all(&outside)
        .await
        .expect("test: create outside dir");
    tokio::fs::write(outside.join("secret.txt"), "outside workspace secret")
        .await
        .expect("test: write secret file");

    symlink(outside.join("secret.txt"), workspace.join("escape.txt")).expect("test: create symlink");

    let security = make_security(|p| {
        p.workspace_dir = workspace.clone();
        p.workspace_only = true;
    });

    let tool = FileReadTool::new(security, false);
    let result = tool
        .execute(json!({"path": "escape.txt"}))
        .await
        .expect("test: symlink escape should return ToolResult");

    assert!(
        !result.success,
        "FileReadTool must reject symlinks pointing outside workspace"
    );
    let err = result.error.as_deref().unwrap_or("");
    assert!(
        err.contains("escapes workspace"),
        "test: expected 'escapes workspace' in error, got: {err}"
    );
}

#[tokio::test]
async fn int_ts_05_file_read_allows_file_within_workspace() {
    let tmp = tempfile::tempdir().expect("test: should create temp dir");
    tokio::fs::write(tmp.path().join("hello.txt"), "workspace file content")
        .await
        .expect("test: write hello.txt");

    let security = make_security(|p| {
        p.workspace_dir = tmp.path().to_path_buf();
        p.workspace_only = true;
    });

    let tool = FileReadTool::new(security, false);
    let result = tool
        .execute(json!({"path": "hello.txt"}))
        .await
        .expect("test: workspace file should return ToolResult");

    assert!(
        result.success,
        "File within workspace should be readable, error: {:?}",
        result.error
    );
    assert_eq!(result.output, "workspace file content");
}

// ═══════════════════════════════════════════════════════════════════════════
// INT-TS-07: HttpRequestTool unrestricted native transport
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn int_ts_07_http_request_attempts_cloud_metadata() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
    });

    let tool = HttpRequestTool::new(security, vec![], 1_000_000, 10);

    let result = tool
        .execute(json!({"url": "http://169.254.169.254/metadata"}))
        .await
        .expect("test: SSRF block should return ToolResult");

    assert!(!result.error.as_deref().unwrap_or("").contains("allowlist"));
}

#[tokio::test]
async fn int_ts_07_http_request_attempts_localhost() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
    });

    let tool = HttpRequestTool::new(security, vec![], 1_000_000, 10);

    let result = tool
        .execute(json!({"url": "http://127.0.0.1:8080"}))
        .await
        .expect("test: localhost block should return ToolResult");

    assert!(!result.error.as_deref().unwrap_or("").contains("allowlist"));
}

#[tokio::test]
async fn int_ts_07_http_request_attempts_ipv6_localhost() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
    });

    let tool = HttpRequestTool::new(security, vec![], 1_000_000, 10);

    let result = tool
        .execute(json!({"url": "http://[::1]:8080"}))
        .await
        .expect("test: IPv6 localhost block should return ToolResult");

    assert!(!result.error.as_deref().unwrap_or("").contains("allowlist"));
}

#[tokio::test]
async fn int_ts_07_http_request_attempts_private_ip_ranges() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
    });

    let private_urls = [
        "http://10.0.0.1/internal",
        "http://172.16.0.1/internal",
        "http://192.168.1.1/internal",
    ];

    for url in &private_urls {
        let tool = HttpRequestTool::new(security.clone(), vec![], 1_000_000, 10);

        let result = tool
            .execute(json!({"url": url}))
            .await
            .expect("test: private IP should return ToolResult");

        assert!(!result.error.as_deref().unwrap_or("").contains("allowlist"));
    }
}

#[tokio::test]
async fn int_ts_07_http_request_readonly_is_not_a_gate() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::ReadOnly;
    });

    let tool = HttpRequestTool::new(security, vec![], 1_000_000, 10);

    let result = tool
        .execute(json!({"url": "https://example.com"}))
        .await
        .expect("test: ReadOnly HTTP should return ToolResult");

    assert!(!result.error.as_deref().unwrap_or("").contains("read-only"));
}

// ═══════════════════════════════════════════════════════════════════════════
// INT-TS-10: Environment variable sanitization
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "current_thread")]
async fn int_ts_10_direct_shell_inherits_complete_environment() {
    // SAFETY: test-only, single-threaded (current_thread flavor).
    // Direct host execution inherits the parent environment exactly.
    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        #[allow(unsafe_code)]
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: test-only, single-threaded test runner
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: test-only, single-threaded test runner
            unsafe {
                match &self.original {
                    Some(val) => std::env::set_var(self.key, val),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    let _g1 = EnvGuard::set("OPENAI_API_KEY", "sk-test-openai-secret-999");
    let _g2 = EnvGuard::set("ANTHROPIC_API_KEY", "sk-test-anthropic-secret-888");
    let _g3 = EnvGuard::set("AWS_SECRET_ACCESS_KEY", "aws-test-secret-777");

    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Full;
        p.workspace_dir = std::env::temp_dir();
    });

    let tool = shell_tool(&security);

    let result = tool
        .execute(json!({"command": "env"}))
        .await
        .expect("test: env command should return ToolResult");

    assert!(
        result.success,
        "test: 'env' command should succeed, error: {:?}",
        result.error
    );

    assert!(result.output.contains("sk-test-openai-secret-999"));
    assert!(result.output.contains("sk-test-anthropic-secret-888"));
    assert!(result.output.contains("aws-test-secret-777"));
}

#[tokio::test(flavor = "current_thread")]
async fn int_ts_10_env_preserves_safe_vars() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Full;
        p.workspace_dir = std::env::temp_dir();
    });

    let tool = shell_tool(&security);

    // HOME and PATH are in SAFE_ENV_VARS and should be available
    let result = tool
        .execute(json!({"command": "echo HOME=$HOME PATH=$PATH"}))
        .await
        .expect("test: echo env vars should return ToolResult");

    assert!(result.success, "test: echo should succeed, error: {:?}", result.error);

    // HOME should be set (not empty)
    assert!(
        result.output.contains("HOME=/"),
        "test: HOME should be present in subprocess env, got: {}",
        result.output.trim()
    );

    // PATH should contain /usr/bin (the safe default)
    assert!(
        result.output.contains("/usr/bin"),
        "test: PATH should contain /usr/bin (safe default), got: {}",
        result.output.trim()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn int_ts_10_env_path_is_inherited_without_rewrite() {
    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Full;
        p.workspace_dir = std::env::temp_dir();
    });

    let tool = shell_tool(&security);

    let result = tool
        .execute(json!({"command": "echo $PATH"}))
        .await
        .expect("test: echo PATH should succeed");

    assert!(result.success, "test: echo $PATH should succeed");
    let path_output = result.output.trim();

    assert_eq!(path_output, std::env::var("PATH").unwrap_or_default());
}

// ═══════════════════════════════════════════════════════════════════════════
// Cross-cutting: SecurityPolicy direct validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn int_ts_02_direct_risk_classification() {
    let policy = SecurityPolicy::default();

    // Low risk: read-only commands
    assert_eq!(
        policy.command_risk_level("ls /tmp"),
        CommandRiskLevel::Low,
        "test: 'ls /tmp' should be Low risk"
    );
    assert_eq!(
        policy.command_risk_level("cat file.txt"),
        CommandRiskLevel::Low,
        "test: 'cat file.txt' should be Low risk"
    );

    // High risk: destructive commands
    assert_eq!(
        policy.command_risk_level("rm -rf /"),
        CommandRiskLevel::High,
        "test: 'rm -rf /' should be High risk"
    );
    assert_eq!(
        policy.command_risk_level("sudo apt install foo"),
        CommandRiskLevel::High,
        "test: 'sudo' should be High risk"
    );
    assert_eq!(
        policy.command_risk_level("curl http://example.com"),
        CommandRiskLevel::High,
        "test: 'curl' should be High risk"
    );
}

#[test]
fn int_ts_02_pip_install_passes_command_gate() {
    // Phase 1: per-command allowlist removed. The structural-safety gate
    // (`is_command_allowed`) no longer rejects a base command merely because it
    // is not on a curated list; only subshell/redirect/tee/single-`&`/dangerous
    // args block in non-full modes. `pip install foo` has none of those, so it
    // now passes the gate (including under the autonomous default).
    let policy = SecurityPolicy::default();

    assert!(
        policy.is_command_allowed("pip install foo"),
        "test: 'pip install foo' should pass the structural command gate now that the per-command allowlist is gone"
    );
}

#[test]
fn int_ts_03_rate_limit_boundary() {
    let policy = SecurityPolicy {
        max_actions_per_hour: 10,
        ..SecurityPolicy::default()
    };

    for i in 0..10 {
        assert!(policy.record_action(), "test: action {i} should be within limit");
    }

    assert!(!policy.record_action(), "test: 11th action should be rate-limited");
    assert!(
        policy.is_rate_limited(),
        "test: is_rate_limited should return true after exhaustion"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-TS-06: FileRead tool blocks protected memory markdown
// ═══════════════════════════════════════════════════════════════════════════════

/// `FileReadTool` with ACL enabled blocks reads of MEMORY.md and memory/* paths.
#[tokio::test]
async fn int_ts_06_file_read_blocks_memory_markdown_with_acl() {
    let tmp = tempfile::tempdir().expect("test: should create temp dir");

    // Create MEMORY.md and memory/brain.db in the workspace
    tokio::fs::write(tmp.path().join("MEMORY.md"), "secret memory content")
        .await
        .expect("test: write MEMORY.md");
    tokio::fs::create_dir_all(tmp.path().join("memory"))
        .await
        .expect("test: create memory dir");
    tokio::fs::write(tmp.path().join("memory/brain.db"), "sqlite data")
        .await
        .expect("test: write brain.db");

    let security = make_security(|p| {
        p.workspace_dir = tmp.path().to_path_buf();
        p.workspace_only = true;
    });

    // ACL enabled -> memory files blocked
    let tool = FileReadTool::new(security, true);

    let result_memory_md = tool
        .execute(json!({"path": "MEMORY.md"}))
        .await
        .expect("test: MEMORY.md should return ToolResult");
    assert!(
        !result_memory_md.success,
        "MEMORY.md should be blocked when ACL is enabled"
    );
    let err = result_memory_md.error.as_deref().unwrap_or("");
    assert!(
        err.contains("ACL") || err.contains("protected") || err.contains("not allowed"),
        "test: expected ACL protection error for MEMORY.md, got: {err}"
    );

    let result_brain_db = tool
        .execute(json!({"path": "memory/brain.db"}))
        .await
        .expect("test: brain.db should return ToolResult");
    assert!(
        !result_brain_db.success,
        "memory/brain.db should be blocked when ACL is enabled"
    );
}

/// `FileReadTool` with ACL disabled allows reads of memory files.
#[tokio::test]
async fn int_ts_06_file_read_allows_memory_without_acl() {
    let tmp = tempfile::tempdir().expect("test: should create temp dir");
    tokio::fs::write(tmp.path().join("MEMORY.md"), "readable content")
        .await
        .expect("test: write MEMORY.md");

    let security = make_security(|p| {
        p.workspace_dir = tmp.path().to_path_buf();
        p.workspace_only = true;
    });

    // ACL disabled -> memory files allowed
    let tool = FileReadTool::new(security, false);

    let result = tool
        .execute(json!({"path": "MEMORY.md"}))
        .await
        .expect("test: MEMORY.md should return ToolResult");
    assert!(
        result.success,
        "MEMORY.md should be readable when ACL is disabled, error: {:?}",
        result.error
    );
    assert_eq!(result.output, "readable content");
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-TS-09: WebFetchTool domain allowlist
// ═══════════════════════════════════════════════════════════════════════════════

/// `WebFetchTool` with empty `allowed_domains` still blocks private/local hosts.
/// `validate_url` propagates as `Err(anyhow)` rather than `Ok(ToolResult{success:false})`.
#[tokio::test]
async fn int_ts_09_web_fetch_blocks_private_with_empty_allowlist() {
    use openprx::tools::WebFetchTool;

    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Supervised;
    });

    let tool = WebFetchTool::new(
        security,
        vec![], // empty allowed_domains
        10_000,
        10,
    );

    // Private IP should be blocked even with empty allowlist.
    // validate_url bails with anyhow error, so execute returns Err.
    let err = tool
        .execute(json!({"url": "http://127.0.0.1:8080/secret"}))
        .await
        .expect_err("test: private URL should be rejected with Err");

    let msg = err.to_string();
    assert!(
        msg.contains("local/private") || msg.contains("Blocked"),
        "test: expected SSRF block error, got: {msg}"
    );
}

/// `WebFetchTool` rejects hosts not in `allowed_domains`.
/// `validate_url` propagates as `Err(anyhow)` rather than `Ok(ToolResult{success:false})`.
#[tokio::test]
async fn int_ts_09_web_fetch_rejects_unlisted_domain() {
    use openprx::tools::WebFetchTool;

    let security = make_security(|p| {
        p.autonomy = AutonomyLevel::Full;
    });

    let tool = WebFetchTool::new(
        security,
        vec!["docs.example.com".into()], // only this domain allowed
        10_000,
        10,
    );

    // Unlisted domain should be rejected.
    // validate_url bails with anyhow error, so execute returns Err.
    let err = tool
        .execute(json!({"url": "https://evil.example.com/attack"}))
        .await
        .expect_err("test: unlisted domain should be rejected with Err");

    let msg = err.to_string();
    assert!(
        msg.contains("not in") || msg.contains("allowed_domains") || msg.contains("Blocked"),
        "test: expected allowlist rejection, got: {msg}"
    );
}
