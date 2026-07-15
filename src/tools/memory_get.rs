use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::principal::MemoryWriteContext;
use crate::memory::{Memory, MemoryReadMode};
use async_trait::async_trait;
#[cfg(test)]
use rusqlite::Connection;
use serde_json::json;
use std::fmt::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

// Behavior-limits Phase 1: MAX 2000 -> 10000 (read more memory lines per key).
const DEFAULT_LINE_COUNT: usize = 50;
const MAX_LINE_COUNT: usize = 10_000;

fn requested_session_id(args: &serde_json::Value) -> Option<&str> {
    args.get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn validate_reserved_router_access(key: &str, session_id: Option<&str>) -> anyhow::Result<()> {
    if key.starts_with("router/") && session_id != Some(crate::self_system::SELF_SYSTEM_SESSION_ID) {
        anyhow::bail!("router/ memory requires session_id=\"self_system\"");
    }
    Ok(())
}

/// Read selected lines from memory by key (ACL-aware), with file fallback.
pub struct MemoryGetTool {
    workspace_dir: PathBuf,
    memory: Arc<dyn Memory>,
    acl_enabled: bool,
}

impl MemoryGetTool {
    pub fn new(workspace_dir: PathBuf, memory: Arc<dyn Memory>, acl_enabled: bool) -> Self {
        Self {
            workspace_dir,
            memory,
            acl_enabled,
        }
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

fn parse_scope_ctx(args: &serde_json::Value) -> Option<MemoryWriteContext> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }

    let scope = args.get("_zc_scope").and_then(serde_json::Value::as_object)?;

    let channel = scope
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let chat_type = scope
        .get("chat_type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let chat_id = scope
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let sender = scope
        .get("sender")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    Some(MemoryWriteContext {
        channel,
        chat_type,
        chat_id,
        sender_id: None,
        raw_sender: sender,
    })
}

#[async_trait]
impl Tool for MemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Read memory by key from the configured backend with ACL observe/enforce mode."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Memory key (preferred) or fallback memory file path"
                },
                "key": {
                    "type": "string",
                    "description": "Alias of path; memory key or fallback file path"
                },
                "from": {
                    "type": "integer",
                    "description": "1-based starting line number (default: 1)"
                },
                "lines": {
                    "type": "integer",
                    "description": "Number of lines to return (default: 50, max: 2000)"
                },
                "session_id": {
                    "type": "string",
                    "description": "Optional session scope; required as self_system for reserved router/ keys"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args
            .get("path")
            .or_else(|| args.get("key"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;
        let session_id = requested_session_id(&args);
        validate_reserved_router_access(path, session_id)?;

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

        let scope_ctx = parse_scope_ctx(&args).unwrap_or_default();
        let mode = if self.acl_enabled {
            MemoryReadMode::Enforce
        } else {
            MemoryReadMode::Observe
        };
        match self.memory.get_with_context_mode(path, Some(&scope_ctx), mode).await? {
            Some(entry) => Ok(ToolResult {
                success: true,
                output: render_range(&entry.key, &entry.content, from, requested_lines),
                error: None,
            }),
            None if !self.acl_enabled => self.read_fallback_file(path, from, requested_lines),
            None => Ok(ToolResult {
                success: true,
                output: format!("No memory entry found for key: '{path}'"),
                error: None,
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Memory]
    }
}

impl MemoryGetTool {
    fn read_fallback_file(&self, path: &str, from: usize, requested_lines: usize) -> anyhow::Result<ToolResult> {
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

        Ok(ToolResult {
            success: true,
            output: render_range(path, &contents, from, requested_lines),
            error: None,
        })
    }
}

fn render_range(label: &str, content: &str, from: usize, requested_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return format!("{label} is empty.");
    }

    let start_idx = from.saturating_sub(1).min(lines.len());
    let end_idx = start_idx.saturating_add(requested_lines).min(lines.len());

    if start_idx >= lines.len() {
        return format!(
            "No content: requested line {from} is beyond end of entry ({})",
            lines.len()
        );
    }

    let mut output = format!("{label} lines {}-{}:\n", start_idx + 1, end_idx);
    for (line_no, line_text) in lines.get(start_idx..end_idx).unwrap_or_default().iter().enumerate() {
        let _ = writeln!(output, "{:>6}: {}", start_idx + line_no + 1, line_text);
    }
    output
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Memory, MemoryCategory, SqliteMemory};
    use chrono::Utc;
    use rusqlite::params;
    use tempfile::TempDir;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn test_tool(tmp: &TempDir, acl_enabled: bool) -> MemoryGetTool {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        MemoryGetTool::new(tmp.path().to_path_buf(), memory, acl_enabled)
    }

    fn open_conn(tmp: &TempDir) -> Connection {
        Connection::open(tmp.path().join("memory").join("brain.db")).unwrap()
    }

    #[tokio::test]
    async fn get_reads_sqlite_key_first() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("memory_key", "line1\nline2\nline3\n", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({"path": "memory_key", "from": 2, "lines": 1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("memory_key lines 2-2"));
        assert!(result.output.contains("2: line2"));
    }

    #[tokio::test]
    async fn get_reads_memory_md_range() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("MEMORY.md"), "a\nb\nc\n");

        let tool = test_tool(&tmp, false);
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
        write_file(&tmp.path().join("memory/2026-02-22.md"), "entry1\nentry2\nentry3\n");

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({"path": "memory/2026-02-22.md", "from": 1, "lines": 1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("memory/2026-02-22.md lines 1-1"));
        assert!(result.output.contains("1: entry1"));
    }

    #[tokio::test]
    async fn acl_mode_disables_file_fallback() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("MEMORY.md"), "line1\nline2\n");

        let tool = test_tool(&tmp, true);
        let result = tool.execute(json!({"path": "MEMORY.md"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("No memory entry found for key"));
    }

    #[tokio::test]
    async fn get_blocks_non_memory_paths() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("notes.md"), "not allowed\n");

        let tool = test_tool(&tmp, false);
        let result = tool.execute(json!({"path": "notes.md"})).await.unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Only MEMORY.md or memory/*.md")
        );
    }

    #[tokio::test]
    async fn get_requires_path() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(&tmp, false);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_accepts_key_alias() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("memory_key", "line1\nline2\n", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool.execute(json!({"key": "memory_key"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("memory_key lines 1-2"));
    }

    #[tokio::test]
    async fn observe_mode_returns_entry_but_audits_would_deny() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("open", "project summary", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by)
             VALUES ('b1', 'member_a', 'telegram', 'sender-a', '2026-02-23T00:00:00Z', 'system')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at)
             VALUES ('member_a', 'member', '[]', 'private', '[\"summary\"]', '2026-02-23T00:00:00Z')",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({
                "path": "open",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "sender-a"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("open lines"));

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM access_audit_log WHERE result = 'would_deny'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn acl_deny_returns_empty_for_unauthorized_key() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("private_key", "acl denied payload", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by)
             VALUES ('b1', 'member_a', 'telegram', 'sender-a', ?1, 'system')",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at)
             VALUES ('member_a', 'member', '[]', 'private', '[]', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'user', sender_id = 'other_user', sensitivity = 'normal' WHERE key = 'private_key'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "path": "private_key",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "sender-a"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("No memory entry found for key"));
    }

    #[tokio::test]
    async fn untrusted_scope_payload_defaults_to_anonymous() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("private_key", "owner note", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by)
             VALUES ('b1', 'owner_a', 'telegram', 'sender-owner', ?1, 'system')",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at)
             VALUES ('owner_a', 'owner', '[]', 'public', '[]', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'private', sensitivity = 'normal' WHERE key = 'private_key'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "path": "private_key",
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "sender-owner"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("No memory entry found for key"));
    }

    #[test]
    fn schema_exposes_openclaw_parameters() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(&tmp, true);
        let schema = tool.parameters_schema();

        assert_eq!(tool.name(), "memory_get");
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["key"].is_object());
        assert!(schema["properties"]["from"].is_object());
        assert!(schema["properties"]["lines"].is_object());
        assert!(schema["properties"]["session_id"].is_object());
    }

    #[test]
    fn production_key_reads_do_not_open_sqlite_directly() {
        let production = include_str!("memory_get.rs")
            .rsplit_once("\n#[cfg(test)]\nmod tests {")
            .unwrap()
            .0;
        assert!(production.contains("get_with_context_mode"));
        assert!(!production.contains("Connection::open"));
        assert!(!production.contains("brain.db"));
    }

    #[tokio::test]
    async fn reserved_router_key_requires_self_system_session() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store(
                "router/elo/test",
                "{\"dynamic_elo\":1000}",
                MemoryCategory::Custom("router".into()),
                Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
            )
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let denied = tool.execute(json!({"path": "router/elo/test"})).await;
        assert!(denied.is_err());
        assert!(denied.unwrap_err().to_string().contains("session_id=\"self_system\""));

        let allowed = tool
            .execute(json!({
                "path": "router/elo/test",
                "session_id": crate::self_system::SELF_SYSTEM_SESSION_ID
            }))
            .await
            .unwrap();
        assert!(allowed.success);
        assert!(allowed.output.contains("router/elo/test"));
    }
}
