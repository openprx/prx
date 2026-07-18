use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::principal::MemoryWriteContext;
use crate::memory::{Memory, MemoryEntry, MemoryReadMode};
use async_trait::async_trait;
#[cfg(test)]
use rusqlite::Connection;
use serde_json::json;
use std::sync::Arc;

// Behavior-limits Phase 1: DEFAULT 5 -> 10, MAX 100 -> 500 (fuller recall).
const DEFAULT_MAX_RESULTS: usize = 10;
const MAX_RESULTS_LIMIT: usize = 500;

/// Search the configured memory backend with ACL-aware read semantics.
pub struct MemorySearchTool {
    memory: Arc<dyn Memory>,
    acl_enabled: bool,
}

impl MemorySearchTool {
    pub fn new(_workspace_dir: std::path::PathBuf, memory: Arc<dyn Memory>, acl_enabled: bool) -> Self {
        Self { memory, acl_enabled }
    }
}

fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect()
}

fn compute_score(line: &str, terms: &[String]) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }

    let haystack = line.to_lowercase();
    let matched = terms.iter().filter(|term| haystack.contains(term.as_str())).count();

    if matched == 0 {
        0.0
    } else {
        matched as f64 / terms.len() as f64
    }
}

fn parse_max_results(args: &serde_json::Value) -> usize {
    #[allow(clippy::cast_possible_truncation)]
    args.get("maxResults")
        .or_else(|| args.get("max_results"))
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_MAX_RESULTS, |n| n as usize)
        .clamp(1, MAX_RESULTS_LIMIT)
}

fn parse_min_score(args: &serde_json::Value) -> f64 {
    args.get("minScore")
        .and_then(serde_json::Value::as_f64)
        .map_or(0.0, |score| score.clamp(0.0, 1.0))
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
    let workspace_id = scope
        .get("workspace_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let owner_id = scope
        .get("owner_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    Some(MemoryWriteContext {
        workspace_id,
        channel,
        chat_type,
        chat_id,
        sender_id: owner_id,
        raw_sender: sender,
    })
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search memories from the configured backend with ACL observe/enforce mode."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text query to search for in workspace memory files"
                },
                "maxResults": {
                    "type": "integer",
                    "description": "Maximum snippets to return (default: 5, max: 100)"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Alias of maxResults for compatibility"
                },
                "minScore": {
                    "type": "number",
                    "description": "Minimum match score between 0.0 and 1.0"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'query' parameter"))?;

        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matches found for an empty query.".to_string(),
                error: None,
            });
        }

        let max_results = parse_max_results(&args);
        let min_score = parse_min_score(&args);

        let scope_ctx = parse_scope_ctx(&args).unwrap_or_default();
        let mode = if self.acl_enabled {
            MemoryReadMode::Enforce
        } else {
            MemoryReadMode::Observe
        };
        let terms = tokenize_query(trimmed_query);
        let mut entries = self
            .memory
            .recall_with_context_mode(
                trimmed_query,
                max_results.saturating_mul(3),
                None,
                Some(&scope_ctx),
                mode,
            )
            .await?;
        entries.retain(|entry| {
            let score = entry
                .score
                .unwrap_or_else(|| compute_score(&entry.content, &terms))
                .max(compute_score(&entry.content, &terms));
            score > 0.0 && score >= min_score
        });
        entries.sort_by(|left, right| {
            right
                .score
                .unwrap_or_else(|| compute_score(&right.content, &terms))
                .partial_cmp(&left.score.unwrap_or_else(|| compute_score(&left.content, &terms)))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.key.cmp(&right.key))
        });
        entries.truncate(max_results);
        Ok(render_search_result(trimmed_query, entries))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Memory]
    }
}

fn render_search_result(trimmed_query: &str, rows: Vec<MemoryEntry>) -> ToolResult {
    if rows.is_empty() {
        return ToolResult {
            success: true,
            output: format!("No matches found for query: '{trimmed_query}'"),
            error: None,
        };
    }

    let terms = tokenize_query(trimmed_query);
    let mut output = format!("Found {} matches:\n", rows.len());
    for row in rows {
        let snippet = best_snippet(&row.content, &terms);
        let content = condensed_content(&row.content);
        output.push_str(&format!(
            "- key: {}\n  content: {}\n  snippet: {}\n",
            row.key, content, snippet
        ));
    }

    ToolResult {
        success: true,
        output,
        error: None,
    }
}

fn condensed_content(content: &str) -> String {
    const MAX_CHARS: usize = 240;
    let flattened = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= MAX_CHARS {
        return flattened;
    }
    let truncated = flattened.chars().take(MAX_CHARS).collect::<String>();
    format!("{truncated}...")
}

fn best_snippet(content: &str, terms: &[String]) -> String {
    const MAX_SNIPPET_CHARS: usize = 160;
    let first_match = content.lines().map(str::trim).find(|line| {
        let lower = line.to_lowercase();
        terms.iter().any(|term| lower.contains(term))
    });
    let line = first_match
        .or_else(|| content.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or_else(|| content.trim());
    if line.chars().count() <= MAX_SNIPPET_CHARS {
        return line.to_string();
    }
    let truncated = line.chars().take(MAX_SNIPPET_CHARS).collect::<String>();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;
    use crate::memory::{Memory, MemoryCategory, SqliteMemory};
    use chrono::Utc;
    use rusqlite::params;
    use tempfile::TempDir;

    fn test_tool(tmp: &TempDir, acl_enabled: bool) -> MemorySearchTool {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        MemorySearchTool::new(tmp.path().to_path_buf(), memory, acl_enabled)
    }

    fn open_conn(tmp: &TempDir) -> Connection {
        Connection::open(tmp.path().join("memory").join("brain.db")).unwrap()
    }

    #[tokio::test]
    async fn search_uses_sqlite_memory_recall() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store(
                "user_pref",
                "Core preference: Rust for reliability",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        memory
            .store("daily_note", "Daily note mentions tests", MemoryCategory::Daily, None)
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({"query": "rust", "maxResults": 10, "minScore": 0.1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("key: user_pref"));
        assert!(result.output.contains("snippet:"));
    }

    #[tokio::test]
    async fn search_respects_min_score_and_limit() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("k1", "alpha beta gamma", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("k2", "alpha only", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({"query": "alpha beta", "maxResults": 1, "minScore": 1.0}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Found 1 matches"));
        assert!(result.output.contains("key: k1"));
        assert!(!result.output.contains("key: k2"));
    }

    #[tokio::test]
    async fn search_accepts_snake_case_max_results_alias() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("k1", "alpha beta gamma", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("k2", "alpha beta delta", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({"query": "alpha beta", "max_results": 1, "minScore": 0.1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Found 1 matches"));
    }

    #[tokio::test]
    async fn search_requires_query() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(&tmp, true);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn acl_mode_disables_file_fallback() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "alpha fallback line\n").unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool.execute(json!({"query": "alpha"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("No matches found for query"));
    }

    #[tokio::test]
    async fn observe_mode_returns_results_while_recording_would_deny() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("open", "topic summary", MemoryCategory::Core, None)
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
                "query": "summary",
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
        assert!(result.output.contains("key: open"));

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
    async fn acl_deny_anonymous_only_sees_public() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("public_k", "acl probe", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("private_k", "acl probe", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        conn.execute(
            "UPDATE memories SET visibility = 'public', sensitivity = 'normal' WHERE key = 'public_k'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'private', sensitivity = 'normal' WHERE key = 'private_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "acl probe",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "unknown-sender"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("key: public_k"));
        assert!(!result.output.contains("key: private_k"));
    }

    #[tokio::test]
    async fn acl_deny_member_respects_visibility_ceiling() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("pub_k", "ceiling probe", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("user_k", "ceiling probe", MemoryCategory::Core, None)
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
            "UPDATE memories SET visibility = 'public', sensitivity = 'normal' WHERE key = 'pub_k'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'user', sender_id = 'member_a', sensitivity = 'normal' WHERE key = 'user_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "ceiling probe",
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
        assert!(result.output.contains("key: pub_k"));
        assert!(!result.output.contains("key: user_k"));
    }

    #[tokio::test]
    async fn acl_owner_sees_all() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("owner_k", "owner probe", MemoryCategory::Core, None)
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
            "UPDATE memories SET visibility = 'private', sensitivity = 'normal' WHERE key = 'owner_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "owner probe",
                "_zc_scope_trusted": true,
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
        assert!(result.output.contains("key: owner_k"));
    }

    #[tokio::test]
    async fn untrusted_scope_payload_defaults_to_anonymous() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("private_k", "owner probe", MemoryCategory::Core, None)
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
            "UPDATE memories SET visibility = 'private', sensitivity = 'normal' WHERE key = 'private_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "owner probe",
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
        assert!(result.output.contains("No matches found for query"));
    }

    #[tokio::test]
    async fn acl_deny_blocked_patterns() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("safe_k", "entry safe", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("blocked_k", "entry secret token", MemoryCategory::Core, None)
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
             VALUES ('member_a', 'member', '[]', 'public', '[\"secret\"]', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'public', sensitivity = 'normal' WHERE key IN ('safe_k','blocked_k')",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "entry",
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
        assert!(result.output.contains("key: safe_k"));
        assert!(!result.output.contains("key: blocked_k"));
    }

    #[tokio::test]
    async fn topic_hit_loads_related_memories() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store(
                "topic_related_k",
                "cross channel checkpoint",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO topics (id, title, project, status, created_at, updated_at)
             VALUES ('topic-1', 'openpr migration phase', 'openpr', 'open', ?1, ?1)",
            params![&now],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET topic_id = 'topic-1', visibility = 'public', sensitivity = 'normal'
             WHERE key = 'topic_related_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({"query": "openpr migration phase", "maxResults": 5}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("key: topic_related_k"));
    }

    #[test]
    fn schema_exposes_openclaw_parameters() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(&tmp, true);
        let schema = tool.parameters_schema();

        assert_eq!(tool.name(), "memory_search");
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["maxResults"].is_object());
        assert!(schema["properties"]["max_results"].is_object());
        assert!(schema["properties"]["minScore"].is_object());
    }

    #[test]
    fn production_search_does_not_open_sqlite_or_scan_files() {
        let production = include_str!("memory_search.rs")
            .rsplit_once("\n#[cfg(test)]\nmod tests {")
            .unwrap()
            .0;
        assert!(production.contains("recall_with_context_mode"));
        assert!(!production.contains("Connection::open"));
        assert!(!production.contains("brain.db"));
        assert!(!production.contains("read_to_string"));
    }
}
