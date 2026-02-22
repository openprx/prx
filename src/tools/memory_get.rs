use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Component, Path, PathBuf};

const DEFAULT_LINE_COUNT: usize = 50;
const MAX_LINE_COUNT: usize = 2000;

/// Read selected lines from MEMORY.md or memory/*.md in the workspace.
pub struct MemoryGetTool {
    workspace_dir: PathBuf,
}

impl MemoryGetTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }

    fn validate_memory_path(path: &str) -> anyhow::Result<()> {
        if path.is_empty() {
            anyhow::bail!("Path cannot be empty");
        }

        let parsed = Path::new(path);
        if parsed.is_absolute() {
            anyhow::bail!("Path must be relative to workspace");
        }

        for component in parsed.components() {
            match component {
                Component::Normal(_) => {}
                _ => anyhow::bail!("Path contains invalid component: {path}"),
            }
        }

        if path == "MEMORY.md" {
            return Ok(());
        }

        let mut components = parsed.components();
        let first = components.next();
        let second = components.next();
        let third = components.next();

        let is_memory_md = match (first, second, third) {
            (Some(Component::Normal(root)), Some(Component::Normal(file)), None) => {
                root == "memory"
                    && Path::new(file)
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.eq_ignore_ascii_case("md"))
                        .unwrap_or(false)
            }
            _ => false,
        };

        if !is_memory_md {
            anyhow::bail!("Only MEMORY.md or memory/*.md paths are allowed");
        }

        Ok(())
    }

    fn resolve_allowed_path(&self, relative_path: &str) -> anyhow::Result<PathBuf> {
        Self::validate_memory_path(relative_path)?;

        let workspace = std::fs::canonicalize(&self.workspace_dir)
            .map_err(|e| anyhow::anyhow!("Failed to resolve workspace path: {e}"))?;

        let full_path = workspace.join(relative_path);
        let resolved = std::fs::canonicalize(&full_path)
            .map_err(|e| anyhow::anyhow!("Failed to resolve memory path '{relative_path}': {e}"))?;

        if !resolved.starts_with(&workspace) {
            anyhow::bail!("Resolved path escapes workspace");
        }

        Ok(resolved)
    }
}

#[async_trait]
impl Tool for MemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Read selected lines from MEMORY.md or memory/*.md in the workspace."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Target memory file path: MEMORY.md or memory/<file>.md"
                },
                "from": {
                    "type": "integer",
                    "description": "1-based starting line number (default: 1)"
                },
                "lines": {
                    "type": "integer",
                    "description": "Number of lines to return (default: 50, max: 2000)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        #[allow(clippy::cast_possible_truncation)]
        let from = args
            .get("from")
            .and_then(serde_json::Value::as_u64)
            .map_or(1, |n| n as usize);

        #[allow(clippy::cast_possible_truncation)]
        let requested_lines = args
            .get("lines")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_LINE_COUNT, |n| n as usize)
            .clamp(1, MAX_LINE_COUNT);

        if from == 0 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'from' must be >= 1".to_string()),
            });
        }

        let resolved = match self.resolve_allowed_path(path) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let contents = match std::fs::read_to_string(&resolved) {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read memory file: {e}")),
                });
            }
        };

        let lines: Vec<&str> = contents.lines().collect();
        if lines.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("{path} is empty."),
                error: None,
            });
        }

        let start_idx = from.saturating_sub(1).min(lines.len());
        let end_idx = start_idx.saturating_add(requested_lines).min(lines.len());

        if start_idx >= lines.len() {
            return Ok(ToolResult {
                success: true,
                output: format!(
                    "No content: requested line {from} is beyond end of file ({})",
                    lines.len()
                ),
                error: None,
            });
        }

        let mut output = format!(
            "{} lines {}-{}:\n",
            path,
            start_idx + 1,
            end_idx
        );

        for (line_no, line_text) in lines[start_idx..end_idx].iter().enumerate() {
            output.push_str(&format!("{:>6}: {}\n", start_idx + line_no + 1, line_text));
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn get_reads_memory_md_range() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("MEMORY.md"), "a\nb\nc\n");

        let tool = MemoryGetTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(json!({"path": "MEMORY.md", "from": 2, "lines": 2}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("MEMORY.md lines 2-3"));
        assert!(result.output.contains("2: b"));
        assert!(result.output.contains("3: c"));
    }

    #[tokio::test]
    async fn get_reads_daily_memory_file() {
        let tmp = TempDir::new().unwrap();
        write_file(
            &tmp.path().join("memory/2026-02-22.md"),
            "entry1\nentry2\nentry3\n",
        );

        let tool = MemoryGetTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(json!({"path": "memory/2026-02-22.md", "from": 1, "lines": 1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("memory/2026-02-22.md lines 1-1"));
        assert!(result.output.contains("1: entry1"));
    }

    #[tokio::test]
    async fn get_blocks_non_memory_paths() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("notes.md"), "not allowed\n");

        let tool = MemoryGetTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(json!({"path": "notes.md"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Only MEMORY.md or memory/*.md"));
    }

    #[tokio::test]
    async fn get_requires_path() {
        let tmp = TempDir::new().unwrap();
        let tool = MemoryGetTool::new(tmp.path().to_path_buf());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn schema_exposes_openclaw_parameters() {
        let tmp = TempDir::new().unwrap();
        let tool = MemoryGetTool::new(tmp.path().to_path_buf());
        let schema = tool.parameters_schema();

        assert_eq!(tool.name(), "memory_get");
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["from"].is_object());
        assert!(schema["properties"]["lines"].is_object());
    }
}
