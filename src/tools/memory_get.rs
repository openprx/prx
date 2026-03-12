use super::traits::{Tool, ToolResult};
use crate::memory::principal::{
    log_access, post_filter, resolve_principal, ChatType, MemoryWriteContext, Principal, Role,
    Visibility,
};
use crate::memory::Memory;
use async_trait::async_trait;
use rusqlite::{params_from_iter, types::Value, Connection};
use serde_json::json;
use std::fmt::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const DEFAULT_LINE_COUNT: usize = 50;
const MAX_LINE_COUNT: usize = 2000;

static OBSERVE_TOTAL_QUERIES: AtomicU64 = AtomicU64::new(0);
static OBSERVE_WOULD_DENY_QUERIES: AtomicU64 = AtomicU64::new(0);

fn requested_session_id(args: &serde_json::Value) -> Option<&str> {
    args.get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn validate_reserved_router_access(key: &str, session_id: Option<&str>) -> anyhow::Result<()> {
    if key.starts_with("router/") && session_id != Some(crate::self_system::SELF_SYSTEM_SESSION_ID)
    {
        anyhow::bail!("router/ memory requires session_id=\"self_system\"");
    }
    Ok(())
}

/// Read selected lines from memory by key (ACL-aware), with file fallback.
pub struct MemoryGetTool {
    workspace_dir: PathBuf,
    _memory: Arc<dyn Memory>,
    acl_enabled: bool,
}

impl MemoryGetTool {
    pub fn new(workspace_dir: PathBuf, memory: Arc<dyn Memory>, acl_enabled: bool) -> Self {
        Self {
            workspace_dir,
            _memory: memory,
            acl_enabled,
        }
    }

    fn db_path(&self) -> PathBuf {
        self.workspace_dir.join("memory").join("brain.db")
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

#[derive(Debug, Clone)]
struct MemoryRow {
    id: String,
    key: String,
    content: String,
}

fn parse_scope_ctx(args: &serde_json::Value) -> Option<MemoryWriteContext> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }

    let scope = args
        .get("_zc_scope")
        .and_then(serde_json::Value::as_object)?;

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

fn fallback_principal(ctx: &MemoryWriteContext) -> Principal {
    Principal {
        user_id: "anonymous:unknown:unknown".to_string(),
        role: Role::Anonymous,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Private,
        blocked_patterns: Vec::new(),
        current_channel: ctx.channel.clone().unwrap_or_default(),
        current_chat_id: ctx.chat_id.clone().unwrap_or_default(),
        current_chat_type: ctx
            .chat_type
            .as_deref()
            .map(ChatType::from_str)
            .unwrap_or(ChatType::Dm),
        acl_enforced: true,
    }
}

fn anonymous_principal() -> Principal {
    Principal {
        user_id: "anonymous:unknown:unknown".to_string(),
        role: Role::Anonymous,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Private,
        blocked_patterns: Vec::new(),
        current_channel: String::new(),
        current_chat_id: String::new(),
        current_chat_type: ChatType::Dm,
        acl_enforced: true,
    }
}

fn fetch_memory_by_key_with_scope(
    conn: &Connection,
    key: &str,
    scope_sql: &str,
    scope_params: &[Value],
) -> anyhow::Result<Option<MemoryRow>> {
    let sql = format!(
        "SELECT id, key, content
         FROM memories
         WHERE key = ? AND ({scope_sql})
         LIMIT 1"
    );

    let mut params = Vec::with_capacity(scope_params.len() + 1);
    params.push(Value::from(key.to_string()));
    params.extend(scope_params.iter().cloned());

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(params))?;
    if let Some(row) = rows.next()? {
        return Ok(Some(MemoryRow {
            id: row.get(0)?,
            key: row.get(1)?,
            content: row.get(2)?,
        }));
    }
    Ok(None)
}

fn observe_log_query(would_deny: bool) {
    let total = OBSERVE_TOTAL_QUERIES.fetch_add(1, Ordering::Relaxed) + 1;
    if would_deny {
        OBSERVE_WOULD_DENY_QUERIES.fetch_add(1, Ordering::Relaxed);
    }
    let would_deny_count = OBSERVE_WOULD_DENY_QUERIES.load(Ordering::Relaxed);
    tracing::info!(
        total_queries = total,
        would_deny_count,
        "memory_get acl observe metrics"
    );
}

#[async_trait]
impl Tool for MemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Read memory by key from SQLite with ACL observe/enforce mode; file fallback is only used when ACL is disabled."
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

        let db_path = self.db_path();
        if db_path.exists() {
            let conn = match Connection::open(&db_path) {
                Ok(conn) => conn,
                Err(error) => {
                    if self.acl_enabled {
                        tracing::warn!(
                            "memory_get sqlite open failed while acl is enabled: {error}"
                        );
                        return Ok(ToolResult {
                            success: true,
                            output: format!("No memory entry found for key: '{path}'"),
                            error: None,
                        });
                    }
                    tracing::warn!("memory_get sqlite open failed, using file fallback: {error}");
                    return self.read_fallback_file(path, from, requested_lines);
                }
            };
            let scope_ctx = parse_scope_ctx(&args);
            let principal = if let Some(ref ctx) = scope_ctx {
                resolve_principal(&conn, ctx).unwrap_or_else(|_| fallback_principal(ctx))
            } else {
                anonymous_principal()
            };
            let (scope_sql, scope_params) = principal.build_sql_scope();

            if self.acl_enabled && principal.acl_enforced {
                let scoped =
                    fetch_memory_by_key_with_scope(&conn, path, &scope_sql, &scope_params)?;
                if let Some(row) = scoped {
                    let filtered =
                        post_filter(vec![row], &principal, |entry| entry.content.as_str());
                    if let Some(entry) = filtered.into_iter().next() {
                        log_access(
                            &conn,
                            &principal,
                            "get",
                            None,
                            Some(&entry.id),
                            Some("acl_enforced"),
                            "allowed",
                        );
                        return Ok(ToolResult {
                            success: true,
                            output: render_range(&entry.key, &entry.content, from, requested_lines),
                            error: None,
                        });
                    }
                }

                log_access(
                    &conn,
                    &principal,
                    "get_denied",
                    None,
                    Some(path),
                    Some("scope_or_post_filter"),
                    "denied",
                );
                return Ok(ToolResult {
                    success: true,
                    output: format!("No memory entry found for key: '{path}'"),
                    error: None,
                });
            }

            // Observe mode: evaluate ACL but still return unrestricted result.
            let scoped = fetch_memory_by_key_with_scope(&conn, path, &scope_sql, &scope_params)?;
            let filtered = post_filter(
                scoped.clone().into_iter().collect::<Vec<_>>(),
                &principal,
                |entry| entry.content.as_str(),
            );
            let unrestricted = fetch_memory_by_key_with_scope(&conn, path, "1=1", &[])?;

            let would_deny = unrestricted.is_some() && (scoped.is_none() || filtered.is_empty());
            observe_log_query(would_deny);
            log_access(
                &conn,
                &principal,
                "get",
                None,
                Some(path),
                Some("observe_mode"),
                if would_deny {
                    "would_deny"
                } else if unrestricted.is_some() {
                    "allowed"
                } else {
                    "no_results"
                },
            );

            if let Some(entry) = unrestricted {
                return Ok(ToolResult {
                    success: true,
                    output: render_range(&entry.key, &entry.content, from, requested_lines),
                    error: None,
                });
            }
        } else if self.acl_enabled {
            return Ok(ToolResult {
                success: true,
                output: format!("No memory entry found for key: '{path}'"),
                error: None,
            });
        }

        self.read_fallback_file(path, from, requested_lines)
    }
}

impl MemoryGetTool {
    fn read_fallback_file(
        &self,
        path: &str,
        from: usize,
        requested_lines: usize,
    ) -> anyhow::Result<ToolResult> {
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
    for (line_no, line_text) in lines[start_idx..end_idx].iter().enumerate() {
        let _ = writeln!(output, "{:>6}: {}", start_idx + line_no + 1, line_text);
    }
    output
}

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
            .store(
                "memory_key",
                "line1\nline2\nline3\n",
                MemoryCategory::Core,
                None,
            )
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
        write_file(
            &tmp.path().join("memory/2026-02-22.md"),
            "entry1\nentry2\nentry3\n",
        );

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
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Only MEMORY.md or memory/*.md"));
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
            .store(
                "private_key",
                "acl denied payload",
                MemoryCategory::Core,
                None,
            )
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
        assert!(denied
            .unwrap_err()
            .to_string()
            .contains("session_id=\"self_system\""));

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
