use super::traits::{TOOL_EXECUTION_CANCELLED, Tool, ToolCategory, ToolResult, ToolTier};
use crate::runtime::RuntimeAdapter;
use crate::runtime::shell_process::{
    SHELL_OUTPUT_TRUNCATED_MARKER, ShellProcessAdapter, ShellProcessError, ShellProcessRequest,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Maximum shell command execution time before kill.
const SHELL_TIMEOUT_SECS: u64 = 60;

/// Direct host shell command execution tool.
///
/// Authorization belongs to the orchestration boundary. This executor passes
/// command text unchanged to the configured runtime and applies no ACL, path
/// parser, environment filtering, or OS sandbox.
pub struct ShellTool {
    process: ShellProcessAdapter,
    workspace_dir: std::path::PathBuf,
}

impl ShellTool {
    pub fn new(runtime: Arc<dyn RuntimeAdapter>, workspace_dir: std::path::PathBuf) -> Self {
        Self {
            process: ShellProcessAdapter::new(runtime),
            workspace_dir,
        }
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
            ShellProcessError::Cancelled => TOOL_EXECUTION_CANCELLED.to_string(),
            ShellProcessError::Timeout(_) => {
                format!("Command timed out after {SHELL_TIMEOUT_SECS}s and was killed")
            }
            ShellProcessError::Runtime(error) => format!("Failed to build runtime command: {error}"),
            ShellProcessError::Spawn(error) | ShellProcessError::Wait(error) | ShellProcessError::Output(error) => {
                format!("Failed to execute command: {error}")
            }
        }
    }

    async fn execute_inner(
        &self,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        match self
            .process
            .execute(ShellProcessRequest {
                command,
                workspace_dir: &self.workspace_dir,
                timeout: Duration::from_secs(SHELL_TIMEOUT_SECS),
                cancellation,
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
                Ok(ToolResult {
                    success: output.status.success(),
                    output: stdout,
                    error: if stderr.is_empty() { None } else { Some(stderr) },
                })
            }
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(Self::legacy_process_error_message(error)),
            }),
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
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
        self.execute_inner(args, None).await
    }

    async fn execute_with_cancellation(
        &self,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        self.execute_inner(args, cancellation).await
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
    async fn direct_shell_inherits_parent_path_and_environment() {
        let tmp = TempDir::new().unwrap();
        let expected_path = std::env::var("PATH").unwrap_or_default();
        let result = tool(tmp.path())
            .execute(json!({"command": "printf '%s' \"$PATH\""}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output, expected_path);
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
