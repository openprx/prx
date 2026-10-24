use super::traits::{ProcessExecutionDetails, ProcessTermination, ToolInvocationResult};
use super::traits::{TOOL_EXECUTION_CANCELLED, Tool, ToolCategory, ToolResult, ToolTier};
use crate::runtime::RuntimeAdapter;
use crate::runtime::shell_process::{
    SHELL_OUTPUT_TRUNCATED_MARKER, ShellProcessAdapter, ShellProcessError, ShellProcessRequest,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Direct host shell command execution tool.
///
/// Authorization belongs to the orchestration boundary. This executor passes
/// command text unchanged to the configured runtime and applies no ACL, path
/// parser or OS sandbox. The child environment is cleared and allowlisted.
pub struct ShellTool {
    process: ShellProcessAdapter,
    workspace_dir: std::path::PathBuf,
}

impl ShellTool {
    pub fn new(runtime: Arc<dyn RuntimeAdapter>, workspace_dir: std::path::PathBuf) -> Self {
        Self {
            process: ShellProcessAdapter::new(runtime).with_environment_allowlist(Vec::new()),
            workspace_dir,
        }
    }

    /// Inherit additional exact environment names configured by the operator.
    pub fn with_env_allowlist(mut self, names: Vec<String>) -> Self {
        self.process = self.process.with_environment_allowlist(names);
        self
    }

    fn legacy_truncation_marker(mut output: String, truncated: bool, marker: &str) -> String {
        if truncated && output.ends_with(SHELL_OUTPUT_TRUNCATED_MARKER) {
            output.truncate(output.len() - SHELL_OUTPUT_TRUNCATED_MARKER.len());
            output.push_str(marker);
        }
        output
    }

    fn legacy_process_error_message(error: ShellProcessError) -> String {
        match error {
            ShellProcessError::Interrupted { reason, .. } => Self::legacy_process_error_message(*reason),
            ShellProcessError::Cancelled => TOOL_EXECUTION_CANCELLED.to_string(),
            ShellProcessError::Timeout(timeout) => format!("Command timed out after {timeout:?} and was killed"),
            ShellProcessError::Runtime(error) => format!("Failed to build runtime command: {error}"),
            ShellProcessError::GitRefused => ShellProcessError::GitRefused.to_string(),
            ShellProcessError::Spawn(error) | ShellProcessError::Wait(error) | ShellProcessError::Output(error) => {
                format!("Failed to execute command: {error}")
            }
        }
    }

    fn legacy_result(mut observed: ToolInvocationResult) -> ToolResult {
        if let Some(diagnostic) = observed.process.and_then(|process| process.diagnostic) {
            observed.result.error = Some(diagnostic);
        }
        observed.result
    }

    async fn execute_inner(
        &self,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolInvocationResult> {
        let command = args
            .get("command")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        match self
            .process
            .execute_observed(ShellProcessRequest {
                command,
                workspace_dir: &self.workspace_dir,
                timeout: None,
                cancellation,
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await
        {
            Ok(output) => {
                let stdout = Self::legacy_truncation_marker(
                    output.stdout,
                    output.stdout_truncated,
                    "\n... [output truncated at 1MB]",
                );
                let stderr = Self::legacy_truncation_marker(
                    output.stderr,
                    output.stderr_truncated,
                    "\n... [stderr truncated at 1MB]",
                );
                #[cfg(unix)]
                let signal = {
                    use std::os::unix::process::ExitStatusExt;
                    output.status.signal()
                };
                #[cfg(not(unix))]
                let signal = None;
                Ok(ToolInvocationResult {
                    process: Some(Box::new(ProcessExecutionDetails {
                        termination: ProcessTermination::Exited,
                        started: true,
                        output_complete: true,
                        diagnostic: None,
                        exit_code: output.status.code(),
                        signal,
                        stdout_truncated: output.stdout_truncated,
                        stderr_truncated: output.stderr_truncated,
                    })),
                    result: ToolResult {
                        success: output.status.success(),
                        output: stdout,
                        error: if stderr.is_empty() { None } else { Some(stderr) },
                    },
                })
            }
            Err(error) => {
                let (error, partial) = match error {
                    ShellProcessError::Interrupted { reason, output } => (*reason, Some(output)),
                    error => (error, None),
                };
                let termination = match &error {
                    ShellProcessError::Cancelled => ProcessTermination::Cancelled,
                    ShellProcessError::Timeout(_) => ProcessTermination::TimedOut,
                    ShellProcessError::Wait(_) | ShellProcessError::Output(_) => ProcessTermination::Failed,
                    _ => ProcessTermination::NotStarted,
                };
                let started =
                    partial.is_some() || matches!(error, ShellProcessError::Wait(_) | ShellProcessError::Output(_));
                let diagnostic = Self::legacy_process_error_message(error);
                Ok(ToolInvocationResult {
                    process: Some(Box::new(ProcessExecutionDetails {
                        termination,
                        started,
                        output_complete: partial.as_ref().is_some_and(|output| output.complete),
                        diagnostic: Some(diagnostic),
                        exit_code: None,
                        signal: None,
                        stdout_truncated: partial.as_ref().is_some_and(|output| output.stdout_truncated),
                        stderr_truncated: partial.as_ref().is_some_and(|output| output.stderr_truncated),
                    })),
                    result: ToolResult {
                        success: false,
                        output: partial.as_ref().map(|output| output.stdout.clone()).unwrap_or_default(),
                        error: partial.map(|output| output.stderr).filter(|stderr| !stderr.is_empty()),
                    },
                })
            }
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    async fn execute_named_observed(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolInvocationResult> {
        anyhow::ensure!(name == self.name(), "unsupported shell tool name");
        self.execute_inner(args, cancellation).await
    }

    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command directly in the workspace directory"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_inner(args, None).await.map(Self::legacy_result)
    }

    async fn execute_with_cancellation(
        &self,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        self.execute_inner(args, cancellation).await.map(Self::legacy_result)
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::FileSystem, ToolCategory::System]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::NativeRuntime;
    use tempfile::TempDir;

    fn tool(workspace: &std::path::Path) -> ShellTool {
        ShellTool::new(Arc::new(NativeRuntime::new()), workspace.to_path_buf())
    }

    #[test]
    fn shell_tool_contract() {
        let tmp = TempDir::new().unwrap();
        let tool = tool(tmp.path());
        assert_eq!(tool.name(), "shell");
        assert_eq!(tool.tier(), ToolTier::Core);
        assert!(tool.categories().contains(&ToolCategory::System));
        assert_eq!(
            tool.parameters_schema()
                .pointer("/required/0")
                .and_then(serde_json::Value::as_str),
            Some("command")
        );
    }

    #[tokio::test]
    async fn direct_shell_supports_dev_null_variables_substitution_and_background_wait() {
        let tmp = TempDir::new().unwrap();
        let result = tool(tmp.path())
            .execute(json!({
                "command": "printf ignored >/dev/null; value=$(pwd); sleep 0.01 & pid=$!; wait \"$pid\"; printf '%s' \"$value\""
            }))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            result.output.trim(),
            tmp.path().canonicalize().unwrap().to_string_lossy()
        );
    }

    #[tokio::test]
    async fn direct_shell_inherits_parent_path() {
        let tmp = TempDir::new().unwrap();
        let expected_path = std::env::var("PATH").unwrap_or_default();
        let result = tool(tmp.path())
            .execute(json!({"command": "printf '%s' \"$PATH\""}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output, expected_path);
    }

    #[test]
    fn direct_shell_filters_parent_environment() {
        const PROBE: &str = "PRX_SHELL_ENV_PROBE";
        if std::env::var_os(PROBE).is_none() {
            // Give the probe its own environment without mutating the parallel
            // test process or reading any real credential value.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tools::shell::tests::direct_shell_filters_parent_environment",
                    "--nocapture",
                ])
                .env(PROBE, "1")
                .env("PRX_SHELL_TEST_TOKEN", "synthetic-secret")
                .env("PRX_SHELL_TEST_ALLOWED", "synthetic-allowed")
                .env("LC_PRX_TEST_TOKEN", "synthetic-locale-secret")
                .env("BASH_ENV", "/nonexistent/prx-shell-startup")
                .env("ENV", "/nonexistent/prx-shell-startup")
                .status()
                .unwrap();
            assert!(status.success(), "isolated environment probe failed");
            return;
        }
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let tmp = TempDir::new().unwrap();
            let result = tool(tmp.path()).execute(json!({"command":
                "test -n \"$PATH\" && test -n \"$HOME\" && test -z \"${PRX_SHELL_TEST_TOKEN+x}\" && test -z \"${PRX_SHELL_TEST_ALLOWED+x}\" && test -z \"${PRX_SHELL_ENV_PROBE+x}\" && printf ENV_FILTERED"
            })).await.unwrap();
            assert!(result.success, "unlisted host variables reached the shell");
            assert_eq!(result.output, "ENV_FILTERED");
            // Exercise the production registry as well as the default
            // constructor: the operator override must actually be wired.
            let mut config = crate::config::Config::default();
            config.workspace_dir = tmp.path().to_path_buf();
            config.config_path = tmp.path().join("config.toml");
            config.runtime.shell_env_allowlist = vec!["PRX_SHELL_TEST_ALLOWED".into()];
            let security = Arc::new(crate::security::SecurityPolicy::from_config(&config.autonomy, tmp.path()));
            let memory = Arc::new(crate::memory::SqliteMemory::new(tmp.path()).unwrap());
            let tools = crate::tools::all_tools(
                Arc::new(config.clone()), &security, memory, None, None,
                &config.browser, &config.http_request, tmp.path(), &config.agents, None, &config,
            );
            let shell = tools.iter().find(|tool| tool.name() == "shell").unwrap();
            let result = shell.execute(json!({"command":
                "test \"$PRX_SHELL_TEST_ALLOWED\" = synthetic-allowed && test -z \"${PRX_SHELL_TEST_TOKEN+x}\" && test -z \"${LC_PRX_TEST_TOKEN+x}\" && test -z \"${BASH_ENV+x}\" && test -z \"${ENV+x}\" && printf OVERRIDE_FILTERED"
            })).await.unwrap();
            assert!(result.success, "override must only admit the named variable");
            assert_eq!(result.output, "OVERRIDE_FILTERED");
            let invalid = tool(tmp.path()).with_env_allowlist(vec!["*".into()])
                .execute(json!({"command": "printf MUST_NOT_RUN"})).await.unwrap();
            assert!(!invalid.success);
            assert_eq!(invalid.output, "");
            assert!(invalid.error.unwrap().contains("shell_env_allowlist"));
        });
    }

    #[tokio::test]
    async fn direct_shell_can_read_paths_outside_workspace() {
        let tmp = TempDir::new().unwrap();
        let result = tool(tmp.path())
            .execute(json!({"command": "test -r /dev/null && test -r /etc/passwd && printf readable"}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output, "readable");
    }

    #[tokio::test]
    async fn direct_shell_preserves_nonzero_status_and_stderr() {
        let tmp = TempDir::new().unwrap();
        let result = tool(tmp.path())
            .execute(json!({"command": "printf failure >&2; exit 7"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("failure"));
    }

    #[tokio::test]
    async fn direct_shell_honors_pre_cancelled_token() {
        let tmp = TempDir::new().unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = tool(tmp.path())
            .execute_with_cancellation(json!({"command": "sleep 10"}), Some(cancellation))
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some(TOOL_EXECUTION_CANCELLED));
    }
}
