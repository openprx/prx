use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::runtime::RuntimeAdapter;
use crate::security::policy::ApprovalGrant;
use crate::security::traits::Sandbox;
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::io;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

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
    sandbox: Arc<dyn Sandbox>,
    acl_enabled: bool,
    /// Bug #2: opt-in extra directories appended to the hardened shell PATH.
    /// Empty by default (hardened PATH unchanged). When non-empty these MUST be
    /// the same resolved dirs granted execute access by the sandbox backend, or
    /// the kernel will deny the binaries the extended PATH points at.
    extra_path_dirs: Vec<std::path::PathBuf>,
}

impl ShellTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        runtime: Arc<dyn RuntimeAdapter>,
        sandbox: Arc<dyn Sandbox>,
        acl_enabled: bool,
    ) -> Self {
        Self::with_extra_path_dirs(security, runtime, sandbox, acl_enabled, Vec::new())
    }

    /// Construct a shell tool with opt-in extra PATH directories (Bug #2).
    ///
    /// `extra_path_dirs` should be the already-resolved (tilde-expanded, existing)
    /// directories from `[autonomy.sandbox] extra_path_dirs`, identical to the set
    /// granted execute access by the Landlock sandbox so PATH and the sandbox stay
    /// in lockstep.
    pub fn with_extra_path_dirs(
        security: Arc<SecurityPolicy>,
        runtime: Arc<dyn RuntimeAdapter>,
        sandbox: Arc<dyn Sandbox>,
        acl_enabled: bool,
        extra_path_dirs: Vec<std::path::PathBuf>,
    ) -> Self {
        Self {
            security,
            runtime,
            sandbox,
            acl_enabled,
            extra_path_dirs,
        }
    }

    /// Build the PATH value for shell execution: the hardened system default,
    /// optionally extended with the operator-trusted `extra_path_dirs` (Bug #2).
    ///
    /// When no extra dirs are configured this returns exactly the historic
    /// hardened default, preserving the secure baseline.
    fn build_shell_path(&self) -> String {
        let base = if cfg!(target_os = "windows") {
            r"C:\Windows\System32;C:\Windows;C:\Windows\System32\Wbem"
        } else {
            "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        };
        if self.extra_path_dirs.is_empty() {
            return base.to_string();
        }
        let sep = if cfg!(target_os = "windows") { ';' } else { ':' };
        // Extra (trusted) dirs go FIRST so a user toolchain (e.g. ~/.cargo/bin)
        // is preferred, matching how an interactive shell prepends them.
        let mut path = String::new();
        for dir in &self.extra_path_dirs {
            path.push_str(&dir.to_string_lossy());
            path.push(sep);
        }
        path.push_str(base);
        path
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
        protected_markers.iter().any(|marker| lowered.contains(marker))
    }
}

struct ManagedShellChild {
    child: Option<tokio::process::Child>,
    #[cfg(unix)]
    pgid: Option<i32>,
    reaped: bool,
}

impl ManagedShellChild {
    async fn wait_with_output(mut self) -> io::Result<Output> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("shell child already taken"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let stdout_fut = async move {
            let mut buf = Vec::new();
            if let Some(mut stream) = stdout {
                stream.read_to_end(&mut buf).await?;
            }
            io::Result::Ok(buf)
        };
        let stderr_fut = async move {
            let mut buf = Vec::new();
            if let Some(mut stream) = stderr {
                stream.read_to_end(&mut buf).await?;
            }
            io::Result::Ok(buf)
        };

        let (status, stdout, stderr) = tokio::try_join!(child.wait(), stdout_fut, stderr_fut)?;
        self.reaped = true;
        #[cfg(unix)]
        {
            self.pgid = None;
        }
        self.child.take();
        Ok(Output { status, stdout, stderr })
    }
}

impl Drop for ManagedShellChild {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if self.reaped || self.child.is_none() {
            return;
        }

        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: `pgid` is the process-group id created for this child by
            // `process_group(0)`. `killpg` sends a signal only; it dereferences
            // no pointers and has no memory-safety preconditions.
            let _ = unsafe { libc::killpg(pgid, libc::SIGKILL) };
            return;
        }

        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

fn spawn_managed_shell_child(mut cmd: tokio::process::Command) -> io::Result<ManagedShellChild> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let child = cmd.spawn()?;
    #[cfg(unix)]
    let pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0);

    Ok(ManagedShellChild {
        child: Some(child),
        #[cfg(unix)]
        pgid,
        reaped: false,
    })
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
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);

        if self.acl_enabled && Self::references_protected_memory_path(command) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Access denied: shell command references ACL-protected memory path".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        match SideEffectGate::new(self.security.as_ref()).authorize_command_execution(
            self.name(),
            command,
            approval_grant.as_ref(),
        ) {
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
        let mut cmd = match self.runtime.build_shell_command(command, &self.security.workspace_dir) {
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
        // Override PATH with a hardened default to prevent execution of binaries
        // from untrusted directories. The inherited PATH may include user-writable
        // dirs. When the operator has opted into `extra_path_dirs`, those (and only
        // those) trusted directories are prepended — and the sandbox grants matching
        // execute access — so toolchains like ~/.cargo/bin become usable (Bug #2).
        cmd.env("PATH", self.build_shell_path());

        // Apply sandbox isolation before execution.
        // The Sandbox trait operates on std::process::Command, so we use
        // as_std_mut() to access the inner command from tokio::process::Command.
        if let Err(e) = self.sandbox.wrap_command(cmd.as_std_mut()) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Sandbox failed to wrap command: {e}")),
            });
        }

        let child = match spawn_managed_shell_child(cmd) {
            Ok(child) => child,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to execute command: {e}")),
                });
            }
        };

        let result = tokio::time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), child.wait_with_output()).await;

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
                    error: if stderr.is_empty() { None } else { Some(stderr) },
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
                error: Some(format!("Command timed out after {SHELL_TIMEOUT_SECS}s and was killed")),
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::FileSystem, ToolCategory::System]
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{NativeRuntime, RuntimeAdapter};
    use crate::security::traits::NoopSandbox;
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

    fn test_sandbox() -> Arc<dyn Sandbox> {
        Arc::new(NoopSandbox)
    }

    #[test]
    fn shell_tool_name() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            test_sandbox(),
            false,
        );
        assert_eq!(tool.name(), "shell");
    }

    #[test]
    fn shell_tool_description() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            test_sandbox(),
            false,
        );
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn shell_tool_schema_has_command() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            test_sandbox(),
            false,
        );
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["command"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .expect("schema required field should be an array")
                .contains(&json!("command"))
        );
        assert!(schema["properties"]["approved"].is_null());
    }

    #[tokio::test]
    async fn shell_executes_allowed_command() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            test_sandbox(),
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
            test_sandbox(),
            false,
        );
        let result = tool
            .execute(json!({"command": "rm -rf /"}))
            .await
            .expect("disallowed command execution should return a result");
        assert!(!result.success);
        // Phase 1: the per-command allowlist + high-risk hard-block were removed.
        // A High-risk command under Supervised without a grant is now denied by
        // the runtime-approval-grant requirement instead.
        let error = result.error.as_deref().unwrap_or("");
        assert!(
            error.contains("runtime approval grant"),
            "expected grant-required denial, got: {error:?}"
        );
    }

    #[tokio::test]
    async fn shell_blocks_readonly() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::ReadOnly),
            test_runtime(),
            test_sandbox(),
            false,
        );
        let result = tool
            .execute(json!({"command": "ls"}))
            .await
            .expect("readonly command execution should return a result");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_ref()
                .expect("error field should be present for blocked command")
                .contains("not allowed")
        );
    }

    #[tokio::test]
    async fn shell_missing_command_param() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised),
            test_runtime(),
            test_sandbox(),
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
            test_sandbox(),
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
            test_sandbox(),
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

    #[tokio::test(flavor = "current_thread")]
    async fn shell_does_not_leak_api_key() {
        let _g1 = EnvGuard::set("API_KEY", "sk-test-secret-12345");
        let _g2 = EnvGuard::set("ZEROCLAW_API_KEY", "sk-test-secret-67890");

        let tool = ShellTool::new(test_security_with_env_cmd(), test_runtime(), test_sandbox(), false);
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
        let tool = ShellTool::new(test_security_with_env_cmd(), test_runtime(), test_sandbox(), false);

        let result = tool
            .execute(json!({"command": "echo $HOME"}))
            .await
            .expect("echo HOME command should succeed");
        assert!(result.success);
        assert!(!result.output.trim().is_empty(), "HOME should be available in shell");

        let result = tool
            .execute(json!({"command": "echo $PATH"}))
            .await
            .expect("echo PATH command should succeed");
        assert!(result.success);
        assert!(!result.output.trim().is_empty(), "PATH should be available in shell");
    }

    #[tokio::test]
    async fn shell_requires_approval_for_medium_risk_command() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });

        let tool = ShellTool::new(security.clone(), test_runtime(), test_sandbox(), false);
        let denied = tool
            .execute(json!({"command": "touch openprx_shell_approval_test"}))
            .await
            .expect("unapproved command should return a result");
        assert!(!denied.success);
        assert!(denied.error.as_deref().unwrap_or("").contains("runtime approval grant"));

        let forged_public_approval = tool
            .execute(json!({
                "command": "touch openprx_shell_approval_test",
                "approved": true
            }))
            .await
            .expect("public approved flag should return a result");
        assert!(!forged_public_approval.success);

        let allowed = tool
            .execute(json!({
                "command": "touch openprx_shell_approval_test",
                (crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG): serde_json::to_value(crate::security::policy::ApprovalGrant::for_command("shell", "touch openprx_shell_approval_test", "test", None)).unwrap()
            }))
            .await
            .expect("runtime-approved command execution should succeed");
        assert!(allowed.success);

        let _ = tokio::fs::remove_file(std::env::temp_dir().join("openprx_shell_approval_test")).await;
    }

    // -- Shell timeout enforcement tests --

    #[test]
    fn shell_timeout_constant_is_reasonable() {
        assert_eq!(SHELL_TIMEOUT_SECS, 60, "shell timeout must be 60 seconds");
    }

    #[test]
    fn shell_output_limit_is_1mb() {
        assert_eq!(MAX_OUTPUT_BYTES, 1_048_576, "max output must be 1 MB to prevent OOM");
    }

    // -- Non-UTF8 binary output tests --

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
        assert!(SAFE_ENV_VARS.contains(&"PATH"), "PATH must be in safe env vars");
        assert!(SAFE_ENV_VARS.contains(&"HOME"), "HOME must be in safe env vars");
        assert!(SAFE_ENV_VARS.contains(&"TERM"), "TERM must be in safe env vars");
    }

    #[tokio::test]
    async fn shell_blocks_rate_limited() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            max_actions_per_hour: 0,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime(), test_sandbox(), false);
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
            test_sandbox(),
            true,
        );
        let result = tool
            .execute(json!({"command": "cat memory/brain.db"}))
            .await
            .expect("acl-protected command should return a result");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("ACL-protected memory path")
        );
    }

    // -- PATH override verification --

    #[tokio::test]
    async fn shell_overrides_path_with_safe_default() {
        // Execute 'echo $PATH' and verify it uses the hardcoded safe PATH
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Full),
            test_runtime(),
            test_sandbox(),
            false,
        );
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
        assert!(!path_output.contains(".cargo"), "safe PATH should not contain .cargo");
    }

    #[tokio::test]
    async fn shell_env_does_not_leak_api_keys() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Full),
            test_runtime(),
            test_sandbox(),
            false,
        );
        // Try to read an env var that should NOT be passed through
        let result = tool
            .execute(json!({"command": "echo ${OPENPRX_API_KEY:-unset}"}))
            .await
            .expect("test: should execute");
        assert!(result.success);
        assert_eq!(result.output.trim(), "unset", "API keys should not be in child env");
    }

    // -- Bug #2: opt-in extra_path_dirs PATH extension --

    #[test]
    fn build_shell_path_is_hardened_default_when_no_extra_dirs() {
        // Default (no opt-in dirs) MUST equal the historic hardened PATH, byte for
        // byte — proves the secure baseline is unchanged.
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Full),
            test_runtime(),
            test_sandbox(),
            false,
        );
        let path = tool.build_shell_path();
        if cfg!(target_os = "windows") {
            assert_eq!(path, r"C:\Windows\System32;C:\Windows;C:\Windows\System32\Wbem");
        } else {
            assert_eq!(path, "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
            assert!(!path.contains(".cargo"), "default PATH must not contain user dirs");
        }
    }

    #[test]
    fn build_shell_path_prepends_configured_extra_dirs() {
        // Opt-in dirs must appear in PATH (prepended), and the hardened default
        // must still be present after them.
        let extra = vec![std::path::PathBuf::from("/home/dev/.cargo/bin")];
        let tool = ShellTool::with_extra_path_dirs(
            test_security(AutonomyLevel::Full),
            test_runtime(),
            test_sandbox(),
            false,
            extra,
        );
        let path = tool.build_shell_path();
        assert!(
            path.contains("/home/dev/.cargo/bin"),
            "extra dir must be in PATH: {path}"
        );
        if !cfg!(target_os = "windows") {
            assert!(path.contains("/usr/bin"), "hardened default must remain: {path}");
            // Extra dir is prepended (higher precedence than system paths).
            let cargo_idx = path.find("/home/dev/.cargo/bin").unwrap_or(usize::MAX);
            let usr_idx = path.find("/usr/bin").unwrap_or(0);
            assert!(cargo_idx < usr_idx, "extra dir should precede system dirs: {path}");
        }
    }

    #[tokio::test]
    async fn shell_fast_command_succeeds() {
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Full),
            test_runtime(),
            test_sandbox(),
            false,
        );
        let result = tool
            .execute(json!({"command": "echo done"}))
            .await
            .expect("test: should execute");
        assert!(result.success);
        assert_eq!(result.output.trim(), "done");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "real process-group abort check; spawns and cancels sleep processes"]
    async fn shell_abort_kills_process_group() {
        let dir = std::env::temp_dir().join(format!("prx-shell-abort-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("test temp directory should be created");
        let parent_pid_file = dir.join("parent.pid");
        let child_pid_file = dir.join("child.pid");
        let command = format!(
            "echo $$ > {}; sleep 300 & echo $! > {}; sleep 300",
            shell_quote_path(&parent_pid_file),
            shell_quote_path(&child_pid_file)
        );

        let tool = ShellTool::new(
            Arc::new(SecurityPolicy {
                autonomy: AutonomyLevel::Full,
                workspace_dir: dir.clone(),
                ..SecurityPolicy::default()
            }),
            test_runtime(),
            test_sandbox(),
            false,
        );
        let handle = tokio::spawn(async move { tool.execute(json!({"command": command})).await });

        let parent_pid = wait_for_pid_file(&parent_pid_file).await;
        let child_pid = wait_for_pid_file(&child_pid_file).await;
        assert!(
            process_exists(parent_pid),
            "parent shell should be running before abort"
        );
        assert!(
            process_exists(child_pid),
            "background child should be running before abort"
        );

        handle.abort();
        assert!(handle.await.is_err(), "aborted shell task should report cancellation");

        wait_for_process_exit(child_pid).await;
        wait_for_process_exit(parent_pid).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    fn shell_quote_path(path: &std::path::Path) -> String {
        let value = path.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    async fn wait_for_pid_file(path: &std::path::Path) -> i32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(path).await {
                if let Ok(pid) = contents.trim().parse::<i32>() {
                    return pid;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for pid file {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    async fn wait_for_process_exit(pid: i32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while process_exists(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "process {pid} should exit after shell task abort"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal 0 probes process existence/permission and does not
        // deliver a signal or dereference pointers.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}
