use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Write file contents with path sandboxing
pub struct FileWriteTool {
    security: Arc<SecurityPolicy>,
}

impl FileWriteTool {
    pub const fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write contents to a file in the workspace"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file within the workspace"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content' parameter"))?;

        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        // Security check: validate path is within workspace
        if !self.security.is_path_allowed(path) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Path not allowed by security policy: {path}")),
            });
        }

        let full_path = self.security.workspace_dir.join(path);

        let Some(parent) = full_path.parent() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid path: missing parent directory".into()),
            });
        };

        // Ensure parent directory exists
        tokio::fs::create_dir_all(parent).await?;

        // Resolve parent AFTER creation to block symlink escapes.
        let resolved_parent = match tokio::fs::canonicalize(parent).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to resolve file path: {e}")),
                });
            }
        };

        if !self.security.is_resolved_path_allowed(&resolved_parent) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Resolved path escapes workspace: {}",
                    resolved_parent.display()
                )),
            });
        }

        let Some(file_name) = full_path.file_name() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid path: missing file name".into()),
            });
        };

        let resolved_target = resolved_parent.join(file_name);
        let operation_name = op_id::op_id(self.name(), "write", &[&op_id::ref_for_file(&resolved_target)]);
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
        if let Err(error) = SideEffectGate::new(&self.security).authorize_resource_operation(
            self.name(),
            &operation_name,
            ResourceRiskLevel::Medium,
            approval_grant.as_ref(),
        ) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        // Use O_NOFOLLOW on Unix to atomically reject symlinks during open,
        // eliminating the TOCTOU race between symlink check and write.
        let target = resolved_target.clone();
        let data = content.to_string();
        let write_result = tokio::task::spawn_blocking(move || -> Result<(), String> {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            opts.custom_flags(libc::O_NOFOLLOW);

            let mut file = opts.open(&target).map_err(|e| {
                #[cfg(unix)]
                if e.raw_os_error() == Some(libc::ELOOP) {
                    return format!("Refusing to write through symlink: {}", target.display());
                }
                format!("Failed to write file: {e}")
            })?;
            file.write_all(data.as_bytes())
                .map_err(|e| format!("Failed to write file: {e}"))?;
            Ok(())
        })
        .await;

        match write_result {
            Ok(Ok(())) => Ok(ToolResult {
                success: true,
                output: format!("Written {} bytes to {path}", content.len()),
                error: None,
            }),
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e),
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to write file: {e}")),
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::FileSystem]
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_security(workspace: std::path::PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        })
    }

    fn test_security_with(
        workspace: std::path::PathBuf,
        autonomy: AutonomyLevel,
        max_actions_per_hour: u32,
    ) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: workspace,
            max_actions_per_hour,
            ..SecurityPolicy::default()
        })
    }

    fn approved_args(workspace: &std::path::Path, path: &str, content: &str) -> serde_json::Value {
        let target = workspace.join(path);
        let parent = target.parent().unwrap_or(workspace);
        let file_name = target.file_name().unwrap_or_default();
        let resolved_target = parent.join(file_name);
        let operation = op_id::op_id("file_write", "write", &[&op_id::ref_for_file(&resolved_target)]);
        json!({
            "path": path,
            "content": content,
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: ApprovalGrant::for_resource_operation(
                "file_write",
                &operation,
                "test",
                None,
            )
        })
    }

    #[test]
    fn file_write_name() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        assert_eq!(tool.name(), "file_write");
    }

    #[test]
    fn file_write_schema_has_path_and_content() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["content"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("path")));
        assert!(required.contains(&json!("content")));
    }

    #[tokio::test]
    async fn file_write_creates_file() {
        let dir = std::env::temp_dir().join("openprx_test_file_write");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool.execute(approved_args(&dir, "out.txt", "written!")).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("8 bytes"));

        let content = tokio::fs::read_to_string(dir.join("out.txt")).await.unwrap();
        assert_eq!(content, "written!");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_creates_parent_dirs() {
        let dir = std::env::temp_dir().join("openprx_test_file_write_nested");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(approved_args(&dir, "a/b/c/deep.txt", "deep"))
            .await
            .unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(dir.join("a/b/c/deep.txt")).await.unwrap();
        assert_eq!(content, "deep");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_overwrites_existing() {
        let dir = std::env::temp_dir().join("openprx_test_file_write_overwrite");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("exist.txt"), "old").await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool.execute(approved_args(&dir, "exist.txt", "new")).await.unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(dir.join("exist.txt")).await.unwrap();
        assert_eq!(content, "new");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_blocks_path_traversal() {
        let dir = std::env::temp_dir().join("openprx_test_file_write_traversal");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "../../etc/evil", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not allowed"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_blocks_absolute_path() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let result = tool
            .execute(json!({"path": "/etc/evil", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn file_write_rejected_path_carries_descriptive_reason() {
        // BUG-05: a rejected write must return a non-empty, descriptive error
        // (the offending path + policy reason). The dispatcher surfaces this
        // reason to the LLM; if it were empty the model would think the call
        // "returned nothing / looked fine".
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let result = tool
            .execute(json!({"path": "/etc/passwd", "content": "x"}))
            .await
            .unwrap();
        assert!(!result.success, "writing /etc/passwd must be rejected");
        let err = result.error.as_deref().unwrap_or("");
        assert!(!err.is_empty(), "rejection reason must not be empty");
        assert!(
            err.contains("not allowed") && err.contains("/etc/passwd"),
            "error must name the policy + the offending path, got: {err}"
        );
    }

    #[tokio::test]
    async fn file_write_missing_path_param() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let result = tool.execute(json!({"content": "data"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn file_write_missing_content_param() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let result = tool.execute(json!({"path": "file.txt"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn file_write_empty_content() {
        let dir = std::env::temp_dir().join("openprx_test_file_write_empty");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool.execute(approved_args(&dir, "empty.txt", "")).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("0 bytes"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_write_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("openprx_test_file_write_symlink_escape");
        let workspace = root.join("workspace");
        let outside = root.join("outside");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        symlink(&outside, workspace.join("escape_dir")).unwrap();

        let tool = FileWriteTool::new(test_security(workspace.clone()));
        let result = tool
            .execute(json!({"path": "escape_dir/hijack.txt", "content": "bad"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("escapes workspace"));
        assert!(!outside.join("hijack.txt").exists());

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn file_write_blocks_readonly_mode() {
        let dir = std::env::temp_dir().join("openprx_test_file_write_readonly");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security_with(dir.clone(), AutonomyLevel::ReadOnly, 20));
        let result = tool
            .execute(json!({"path": "out.txt", "content": "should-block"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only"));
        assert!(!dir.join("out.txt").exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_blocks_when_rate_limited() {
        let dir = std::env::temp_dir().join("openprx_test_file_write_rate_limited");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security_with(dir.clone(), AutonomyLevel::Supervised, 0));
        let result = tool
            .execute(json!({"path": "out.txt", "content": "should-block"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Rate limit exceeded"));
        assert!(!dir.join("out.txt").exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── §5.1 TOCTOU / symlink file write protection tests ────

    #[cfg(unix)]
    #[tokio::test]
    async fn file_write_blocks_symlink_target_file() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("openprx_test_file_write_symlink_target");
        let workspace = root.join("workspace");
        let outside = root.join("outside");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        // Create a file outside and symlink to it inside workspace
        tokio::fs::write(outside.join("target.txt"), "original").await.unwrap();
        symlink(outside.join("target.txt"), workspace.join("linked.txt")).unwrap();

        let tool = FileWriteTool::new(test_security(workspace.clone()));
        let result = tool
            .execute(approved_args(&workspace, "linked.txt", "overwritten"))
            .await
            .unwrap();

        assert!(!result.success, "writing through symlink must be blocked");
        assert!(
            result.error.as_deref().unwrap_or("").contains("symlink"),
            "error should mention symlink"
        );

        // Verify original file was not modified
        let content = tokio::fs::read_to_string(outside.join("target.txt")).await.unwrap();
        assert_eq!(content, "original", "original file must not be modified");

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn file_write_blocks_null_byte_in_path() {
        let dir = std::env::temp_dir().join("openprx_test_file_write_null");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "file\u{0000}.txt", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success, "paths with null bytes must be blocked");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
