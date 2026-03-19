use super::traits::{Tool, ToolResult};
use crate::runtime::RuntimeAdapter;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Maximum shell command execution time before kill.
const SHELL_TIMEOUT_SECS: u64 = 60;
/// Maximum output size in bytes (1MB).
const MAX_OUTPUT_BYTES: usize = 1_048_576;
/// Environment variables safe to pass to shell commands.
/// Only functional variables are included — never API keys or secrets.
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

/// Shell command execution tool with sandboxing
pub struct ShellTool {
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    acl_enabled: bool,
}

impl ShellTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        runtime: Arc<dyn RuntimeAdapter>,
        acl_enabled: bool,
    ) -> Self {
        Self {
            security,
            runtime,
            acl_enabled,
        }
    }

    fn references_protected_memory_path(command: &str) -> bool {
        let lowered = command.to_ascii_lowercase();
        let protected_markers = [
            "memory.md",
            "memory_snapshot.md",
            "memory/brain.db",
            "memory/brain.db-wal",
            "memory/brain.db-shm",
            "memory/brain.db-journal",
            "memory/",
        ];
        protected_markers
            .iter()
            .any(|marker| lowered.contains(marker))
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the workspace directory"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "approved": {
                    "type": "boolean",
                    "description": "Set true to explicitly approve medium/high-risk commands in supervised mode",
                    "default": false
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if self.acl_enabled && Self::references_protected_memory_path(command) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Access denied: shell command references ACL-protected memory path".into(),
                ),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        match self.security.validate_command_execution(command, approved) {
            Ok(_) => {}
            Err(reason) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(reason),
                });
            }
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        // Execute with timeout to prevent hanging commands.
        // Clear the environment to prevent leaking API keys and other secrets
        // (CWE-200), then re-add only safe, functional variables.
        let mut cmd = match self
            .runtime
            .build_shell_command(command, &self.security.workspace_dir)
        {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to build runtime command: {e}")),
                });
            }
        };
        cmd.env_clear();

        for var in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }
        // Override PATH with a safe default to prevent execution of binaries from
        // untrusted directories.  The inherited PATH may include user-writable dirs.
        if cfg!(target_os = "windows") {
            cmd.env(
                "PATH",
                r"C:\Windows\System32;C:\Windows;C:\Windows\System32\Wbem",
            );
        } else {
            cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
        }

        let result =
            tokio::time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

                // Truncate output to prevent OOM
                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(stdout.floor_char_boundary(MAX_OUTPUT_BYTES));
                    stdout.push_str("\n... [output truncated at 1MB]");
                }
                if stderr.len() > MAX_OUTPUT_BYTES {
                    stderr.truncate(stderr.floor_char_boundary(MAX_OUTPUT_BYTES));
                    stderr.push_str("\n... [stderr truncated at 1MB]");
                }

                Ok(ToolResult {
                    success: output.status.success(),
                    output: stdout,
                    error: if stderr.is_empty() {
                        None
                    } else {
                        Some(stderr)
                    },
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to execute command: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Command timed out after {SHELL_TIMEOUT_SECS}s and was killed"
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{NativeRuntime, RuntimeAdapter};
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_security(autonomy: AutonomyLevel) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_runtime() -> Arc<dyn RuntimeAdapter> {
        Arc::new(NativeRuntime::new())
    }

    #[test]
    fn shell_tool_name() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            false,
        );
        assert_eq!(tool.name(), "shell");
    }

    #[test]
    fn shell_tool_description() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            false,
        );
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn shell_tool_schema_has_command() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            false,
        );
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["command"].is_object());
        assert!(schema["required"]
            .as_array()
            .expect("schema required field should be an array")
            .contains(&json!("command")));
        assert!(schema["properties"]["approved"].is_object());
    }

    #[tokio::test]
    async fn shell_executes_allowed_command() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            false,
        );
        let result = tool
            .execute(json!({"command": "echo hello"}))
            .await
            .expect("echo command execution should succeed");
        assert!(result.success);
        assert!(result.output.trim().contains("hello"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn shell_blocks_disallowed_command() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            false,
        );
        let result = tool
            .execute(json!({"command": "rm -rf /"}))
            .await
            .expect("disallowed command execution should return a result");
        assert!(!result.success);
        let error = result.error.as_deref().unwrap_or("");
        assert!(error.contains("not allowed") || error.contains("high-risk"));
    }

    #[tokio::test]
    async fn shell_blocks_readonly() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::ReadOnly),
            test_runtime(),
            false,
        );
        let result = tool
            .execute(json!({"command": "ls"}))
            .await
            .expect("readonly command execution should return a result");
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .expect("error field should be present for blocked command")
            .contains("not allowed"));
    }

    #[tokio::test]
    async fn shell_missing_command_param() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            false,
        );
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("command"));
    }

    #[tokio::test]
    async fn shell_wrong_type_param() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            false,
        );
        let result = tool.execute(json!({"command": 123})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn shell_captures_exit_code() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            false,
        );
        let result = tool
            .execute(json!({"command": "ls /nonexistent_dir_xyz"}))
            .await
            .expect("command with nonexistent path should return a result");
        assert!(!result.success);
    }

    fn test_security_with_env_cmd() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["env".into(), "echo".into()],
            ..SecurityPolicy::default()
        })
    }

    /// RAII guard that restores an environment variable to its original state on drop,
    /// ensuring cleanup even if the test panics.
    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: test-only, single-threaded test runner
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
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

    #[tokio::test(flavor = "current_thread")]
    async fn shell_does_not_leak_api_key() {
        let _g1 = EnvGuard::set("API_KEY", "sk-test-secret-12345");
        let _g2 = EnvGuard::set("ZEROCLAW_API_KEY", "sk-test-secret-67890");

        let tool = ShellTool::new(test_security_with_env_cmd(), test_runtime(), false);
        let result = tool
            .execute(json!({"command": "env"}))
            .await
            .expect("env command execution should succeed");
        assert!(result.success);
        assert!(
            !result.output.contains("sk-test-secret-12345"),
            "API_KEY leaked to shell command output"
        );
        assert!(
            !result.output.contains("sk-test-secret-67890"),
            "ZEROCLAW_API_KEY leaked to shell command output"
        );
    }

    #[tokio::test]
    async fn shell_preserves_path_and_home() {
        let tool = ShellTool::new(test_security_with_env_cmd(), test_runtime(), false);

        let result = tool
            .execute(json!({"command": "echo $HOME"}))
            .await
            .expect("echo HOME command should succeed");
        assert!(result.success);
        assert!(
            !result.output.trim().is_empty(),
            "HOME should be available in shell"
        );

        let result = tool
            .execute(json!({"command": "echo $PATH"}))
            .await
            .expect("echo PATH command should succeed");
        assert!(result.success);
        assert!(
            !result.output.trim().is_empty(),
            "PATH should be available in shell"
        );
    }

    #[tokio::test]
    async fn shell_requires_approval_for_medium_risk_command() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            allowed_commands: vec!["touch".into()],
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });

        let tool = ShellTool::new(security.clone(), test_runtime(), false);
        let denied = tool
            .execute(json!({"command": "touch openprx_shell_approval_test"}))
            .await
            .expect("unapproved command should return a result");
        assert!(!denied.success);
        assert!(denied
            .error
            .as_deref()
            .unwrap_or("")
            .contains("explicit approval"));

        let allowed = tool
            .execute(json!({
                "command": "touch openprx_shell_approval_test",
                "approved": true
            }))
            .await
            .expect("approved command execution should succeed");
        assert!(allowed.success);

        let _ =
            tokio::fs::remove_file(std::env::temp_dir().join("openprx_shell_approval_test")).await;
    }

    // ── §5.2 Shell timeout enforcement tests ─────────────────

    #[test]
    fn shell_timeout_constant_is_reasonable() {
        assert_eq!(SHELL_TIMEOUT_SECS, 60, "shell timeout must be 60 seconds");
    }

    #[test]
    fn shell_output_limit_is_1mb() {
        assert_eq!(
            MAX_OUTPUT_BYTES, 1_048_576,
            "max output must be 1 MB to prevent OOM"
        );
    }

    // ── §5.3 Non-UTF8 binary output tests ────────────────────

    #[test]
    fn shell_safe_env_vars_excludes_secrets() {
        for var in SAFE_ENV_VARS {
            let lower = var.to_lowercase();
            assert!(
                !lower.contains("key") && !lower.contains("secret") && !lower.contains("token"),
                "SAFE_ENV_VARS must not include sensitive variable: {var}"
            );
        }
    }

    #[test]
    fn shell_safe_env_vars_includes_essentials() {
        assert!(
            SAFE_ENV_VARS.contains(&"PATH"),
            "PATH must be in safe env vars"
        );
        assert!(
            SAFE_ENV_VARS.contains(&"HOME"),
            "HOME must be in safe env vars"
        );
        assert!(
            SAFE_ENV_VARS.contains(&"TERM"),
            "TERM must be in safe env vars"
        );
    }

    #[tokio::test]
    async fn shell_blocks_rate_limited() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            max_actions_per_hour: 0,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime(), false);
        let result = tool
            .execute(json!({"command": "echo test"}))
            .await
            .expect("rate-limited command should return a result");
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Rate limit"));
    }

    #[tokio::test]
    async fn shell_blocks_protected_memory_paths_when_acl_enabled() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            true,
        );
        let result = tool
            .execute(json!({"command": "cat memory/brain.db"}))
            .await
            .expect("acl-protected command should return a result");
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("ACL-protected memory path"));
    }

    // ── PATH override verification ──────────────────────────────

    #[tokio::test]
    async fn shell_overrides_path_with_safe_default() {
        // Execute 'echo $PATH' and verify it uses the hardcoded safe PATH
        let tool = ShellTool::new(test_security(AutonomyLevel::Full), test_runtime(), false);
        let result = tool
            .execute(json!({"command": "echo $PATH"}))
            .await
            .expect("test: should execute");
        assert!(result.success, "echo PATH should succeed");
        let path_output = result.output.trim();
        // The safe PATH should contain /usr/bin and /bin
        assert!(
            path_output.contains("/usr/bin") && path_output.contains("/bin"),
            "PATH should use safe defaults, got: {path_output}"
        );
        // Should NOT contain user-specific paths like .cargo/bin or node_modules
        assert!(
            !path_output.contains(".cargo"),
            "safe PATH should not contain .cargo"
        );
    }

    #[tokio::test]
    async fn shell_env_does_not_leak_api_keys() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Full), test_runtime(), false);
        // Try to read an env var that should NOT be passed through
        let result = tool
            .execute(json!({"command": "echo ${OPENPRX_API_KEY:-unset}"}))
            .await
            .expect("test: should execute");
        assert!(result.success);
        assert_eq!(
            result.output.trim(),
            "unset",
            "API keys should not be in child env"
        );
    }

    #[tokio::test]
    async fn shell_fast_command_succeeds() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Full), test_runtime(), false);
        let result = tool
            .execute(json!({"command": "echo done"}))
            .await
            .expect("test: should execute");
        assert!(result.success);
        assert_eq!(result.output.trim(), "done");
    }
}
