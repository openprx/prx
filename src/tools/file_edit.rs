use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use crate::tools::tool_diff::build_unified_diff;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Perform an exact string replacement inside an existing workspace file.
///
/// This is the surgical counterpart to `file_write` (which overwrites whole
/// files). It reads the target, replaces `old_string` with `new_string`, and
/// writes the result back. When `replace_all` is false (the default) the
/// `old_string` must occur **exactly once** — zero occurrences are reported as
/// "not found" and multiple occurrences as "not unique"; in both cases the file
/// on disk is left untouched (the operation is atomic with respect to errors).
pub struct FileEditTool {
    security: Arc<SecurityPolicy>,
}

impl FileEditTool {
    pub const fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Perform an exact string replacement in an existing workspace file (like Claude Code's Edit). \
         Reads the file, replaces 'old_string' with 'new_string', and writes it back. \
         Prefer this over file_write for targeted edits to large files — it never rewrites \
         unrelated content. By default 'old_string' must match exactly once: include enough \
         surrounding context to make it unique. Set 'replace_all' to true to replace every \
         occurrence (e.g. renaming a symbol). If 'old_string' is missing or not unique the file \
         is left unchanged and an error is returned."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file within the workspace"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to replace. Must match the file content verbatim, including whitespace and indentation."
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with. Must differ from old_string."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences instead of requiring a unique match. Default: false."
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        let old_string = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'old_string' parameter"))?;

        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'new_string' parameter"))?;

        let replace_all = args
            .get("replace_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if old_string.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'old_string' must not be empty".into()),
            });
        }

        if old_string == new_string {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'old_string' and 'new_string' are identical; nothing to change".into()),
            });
        }

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

        // Security check: validate path is within workspace (same policy as file_write).
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

        // file_edit only edits existing files; the parent directory must already
        // exist. Resolve it to block symlink escapes (mirrors file_write).
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
        let operation_name = op_id::op_id(self.name(), "edit", &[&op_id::ref_for_file(&resolved_target)]);
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

        // Read the current contents. Use O_NOFOLLOW on Unix so a symlinked target
        // is rejected atomically (no TOCTOU between resolve and open).
        let read_target = resolved_target.clone();
        let read_result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true);
            #[cfg(unix)]
            opts.custom_flags(libc::O_NOFOLLOW);

            let mut file = opts.open(&read_target).map_err(|e| {
                #[cfg(unix)]
                if e.raw_os_error() == Some(libc::ELOOP) {
                    return format!("Refusing to edit through symlink: {}", read_target.display());
                }
                if e.kind() == std::io::ErrorKind::NotFound {
                    return format!("File not found: {}", read_target.display());
                }
                format!("Failed to read file: {e}")
            })?;
            let mut contents = String::new();
            std::io::Read::read_to_string(&mut file, &mut contents).map_err(|e| format!("Failed to read file: {e}"))?;
            Ok(contents)
        })
        .await;

        let contents = match read_result {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read file: {e}")),
                });
            }
        };

        // Count matches BEFORE touching disk so error cases are fully atomic.
        let occurrences = contents.matches(old_string).count();
        if occurrences == 0 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "old_string not found in {path}; the file was not modified. \
                     Ensure it matches the file content exactly (including whitespace)."
                )),
            });
        }

        if !replace_all && occurrences > 1 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "old_string is not unique in {path}: found {occurrences} occurrences. \
                     The file was not modified. Add more surrounding context to make the match \
                     unique, or set replace_all=true to replace every occurrence."
                )),
            });
        }

        let (new_contents, replacements) = if replace_all {
            (contents.replace(old_string, new_string), occurrences)
        } else {
            // Exactly one occurrence: replacen with count 1 is sufficient.
            (contents.replacen(old_string, new_string, 1), 1)
        };
        let diff = build_unified_diff(path, &contents, &new_contents);

        // Write the result back. O_NOFOLLOW again guards against a symlink being
        // swapped in between read and write.
        let write_target = resolved_target.clone();
        let data = new_contents;
        let write_result = tokio::task::spawn_blocking(move || -> Result<(), String> {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(false).truncate(true);
            #[cfg(unix)]
            opts.custom_flags(libc::O_NOFOLLOW);

            let mut file = opts.open(&write_target).map_err(|e| {
                #[cfg(unix)]
                if e.raw_os_error() == Some(libc::ELOOP) {
                    return format!("Refusing to write through symlink: {}", write_target.display());
                }
                format!("Failed to write file: {e}")
            })?;
            file.write_all(data.as_bytes())
                .map_err(|e| format!("Failed to write file: {e}"))?;
            Ok(())
        })
        .await;

        // Only count the action against the rate budget once the write succeeds —
        // error paths above are pure reads / no-ops on disk.
        match write_result {
            Ok(Ok(())) => {
                if !self.security.record_action() {
                    tracing::warn!("file_edit succeeded for {path} but action budget was already exhausted");
                }
                let suffix = if replacements == 1 { "" } else { "s" };
                Ok(ToolResult {
                    success: true,
                    output: format!("Applied {replacements} replacement{suffix} in {path}\n\n{diff}"),
                    error: None,
                })
            }
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

    fn approved_args(
        workspace: &std::path::Path,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> serde_json::Value {
        let target = workspace.join(path);
        let parent = target.parent().unwrap_or(workspace);
        // Tests create real files, so the parent canonicalizes to itself; mirror
        // the production op_id derivation using the canonical parent when possible.
        let canonical_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        let file_name = target.file_name().unwrap_or_default();
        let resolved_target = canonical_parent.join(file_name);
        let operation = op_id::op_id("file_edit", "edit", &[&op_id::ref_for_file(&resolved_target)]);
        json!({
            "path": path,
            "old_string": old_string,
            "new_string": new_string,
            "replace_all": replace_all,
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: ApprovalGrant::for_resource_operation(
                "file_edit",
                &operation,
                "test",
                None,
            )
        })
    }

    #[test]
    fn file_edit_name() {
        let tool = FileEditTool::new(test_security(std::env::temp_dir()));
        assert_eq!(tool.name(), "file_edit");
    }

    #[test]
    fn file_edit_schema_has_required_fields() {
        let tool = FileEditTool::new(test_security(std::env::temp_dir()));
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["old_string"].is_object());
        assert!(schema["properties"]["new_string"].is_object());
        assert!(schema["properties"]["replace_all"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("path")));
        assert!(required.contains(&json!("old_string")));
        assert!(required.contains(&json!("new_string")));
        assert!(!required.contains(&json!("replace_all")));
    }

    // ① exactly-one-occurrence replacement succeeds
    #[tokio::test]
    async fn file_edit_replaces_single_occurrence() {
        let dir = std::env::temp_dir().join("openprx_test_file_edit_single");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "hello world\n").await.unwrap();

        let tool = FileEditTool::new(test_security(dir.clone()));
        let result = tool
            .execute(approved_args(&dir, "f.txt", "world", "rust", false))
            .await
            .unwrap();
        assert!(result.success, "error: {:?}", result.error);
        assert!(result.output.contains("Applied 1 replacement"));
        assert!(result.output.contains("--- a/f.txt"));
        assert!(result.output.contains("+++ b/f.txt"));
        assert!(result.output.contains("@@ -"));
        assert!(result.output.contains("-hello world"));
        assert!(result.output.contains("+hello rust"));

        let content = tokio::fs::read_to_string(dir.join("f.txt")).await.unwrap();
        assert_eq!(content, "hello rust\n");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ② zero occurrences -> not found, file untouched
    #[tokio::test]
    async fn file_edit_not_found_leaves_file_untouched() {
        let dir = std::env::temp_dir().join("openprx_test_file_edit_notfound");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "original content").await.unwrap();

        let tool = FileEditTool::new(test_security(dir.clone()));
        let result = tool
            .execute(approved_args(&dir, "f.txt", "absent", "x", false))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("not found"));

        let content = tokio::fs::read_to_string(dir.join("f.txt")).await.unwrap();
        assert_eq!(content, "original content", "file must be unchanged on not-found");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ③ multiple occurrences without replace_all -> not unique, file untouched
    #[tokio::test]
    async fn file_edit_non_unique_leaves_file_untouched() {
        let dir = std::env::temp_dir().join("openprx_test_file_edit_nonunique");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "a a a").await.unwrap();

        let tool = FileEditTool::new(test_security(dir.clone()));
        let result = tool
            .execute(approved_args(&dir, "f.txt", "a", "b", false))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(err.contains("not unique"), "got: {err}");
        assert!(err.contains("3 occurrences"), "got: {err}");

        let content = tokio::fs::read_to_string(dir.join("f.txt")).await.unwrap();
        assert_eq!(content, "a a a", "file must be unchanged on non-unique match");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ④ replace_all replaces every occurrence
    #[tokio::test]
    async fn file_edit_replace_all() {
        let dir = std::env::temp_dir().join("openprx_test_file_edit_all");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "x x x x").await.unwrap();

        let tool = FileEditTool::new(test_security(dir.clone()));
        let result = tool
            .execute(approved_args(&dir, "f.txt", "x", "y", true))
            .await
            .unwrap();
        assert!(result.success, "error: {:?}", result.error);
        assert!(result.output.contains("Applied 4 replacements"));
        assert!(result.output.contains("--- a/f.txt"));
        assert!(result.output.contains("+++ b/f.txt"));
        assert!(result.output.contains("-x x x x"));
        assert!(result.output.contains("+y y y y"));

        let content = tokio::fs::read_to_string(dir.join("f.txt")).await.unwrap();
        assert_eq!(content, "y y y y");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ⑤ path traversal / outside-workspace is rejected
    #[tokio::test]
    async fn file_edit_blocks_path_traversal() {
        let dir = std::env::temp_dir().join("openprx_test_file_edit_traversal");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileEditTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "../../etc/passwd", "old_string": "root", "new_string": "evil"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("not allowed"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_blocks_absolute_path() {
        let tool = FileEditTool::new(test_security(std::env::temp_dir()));
        let result = tool
            .execute(json!({"path": "/etc/passwd", "old_string": "root", "new_string": "evil"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("not allowed"));
    }

    // ⑥ atomicity: a failed edit (non-unique) does not mutate the file — covered
    // above; here we additionally assert that an identical old/new is rejected
    // before any disk write.
    #[tokio::test]
    async fn file_edit_rejects_identical_strings() {
        let dir = std::env::temp_dir().join("openprx_test_file_edit_identical");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "same").await.unwrap();

        let tool = FileEditTool::new(test_security(dir.clone()));
        let result = tool
            .execute(approved_args(&dir, "f.txt", "same", "same", false))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("identical"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_missing_old_string_param() {
        let tool = FileEditTool::new(test_security(std::env::temp_dir()));
        let result = tool.execute(json!({"path": "f.txt", "new_string": "x"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn file_edit_missing_file_returns_error() {
        let dir = std::env::temp_dir().join("openprx_test_file_edit_missing_file");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileEditTool::new(test_security(dir.clone()));
        let result = tool
            .execute(approved_args(&dir, "nope.txt", "a", "b", false))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("not found"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_blocks_readonly_mode() {
        let dir = std::env::temp_dir().join("openprx_test_file_edit_readonly");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "hello").await.unwrap();

        let tool = FileEditTool::new(test_security_with(dir.clone(), AutonomyLevel::ReadOnly, 20));
        let result = tool
            .execute(json!({"path": "f.txt", "old_string": "hello", "new_string": "bye"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only"));

        let content = tokio::fs::read_to_string(dir.join("f.txt")).await.unwrap();
        assert_eq!(content, "hello", "read-only mode must not modify the file");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_edit_blocks_symlink_target() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("openprx_test_file_edit_symlink");
        let workspace = root.join("workspace");
        let outside = root.join("outside");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        tokio::fs::write(outside.join("target.txt"), "secret").await.unwrap();
        symlink(outside.join("target.txt"), workspace.join("linked.txt")).unwrap();

        let tool = FileEditTool::new(test_security(workspace.clone()));
        let result = tool
            .execute(approved_args(&workspace, "linked.txt", "secret", "leaked", false))
            .await
            .unwrap();

        assert!(!result.success, "editing through a symlink must be blocked");
        assert!(result.error.as_deref().unwrap_or("").contains("symlink"));

        let content = tokio::fs::read_to_string(outside.join("target.txt")).await.unwrap();
        assert_eq!(content, "secret", "symlink target must not be modified");

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[test]
    fn build_unified_diff_shows_minus_and_plus() {
        let diff = build_unified_diff("f.txt", "one\nold\nthree\n", "one\nnew\nthree\n");
        assert!(diff.contains("--- a/f.txt"));
        assert!(diff.contains("+++ b/f.txt"));
        assert!(diff.contains("@@ -"));
        assert!(diff.contains(" one"));
        assert!(diff.contains("-old"));
        assert!(diff.contains("+new"));
        assert!(diff.contains(" three"));
    }
}
