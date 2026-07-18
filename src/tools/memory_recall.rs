use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::Memory;
use crate::memory::principal::MemoryWriteContext;
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;

/// Build an owner-scoped [`MemoryWriteContext`] from the trusted runtime scope
/// injected by the tool-call loop (`_zc_scope` guarded by `_zc_scope_trusted`).
///
/// Only a runtime-trusted scope is honoured; user/model-supplied scope fields
/// are ignored. Returns `None` when no trusted scope is present, in which case
/// ACL recall falls back to an empty owner context that returns no
/// cross-principal memories rather than leaking the whole store.
fn parse_scope_ctx(args: &serde_json::Value) -> Option<MemoryWriteContext> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }
    let scope = args.get("_zc_scope").and_then(serde_json::Value::as_object)?;
    let str_field = |key: &str| {
        scope
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Some(MemoryWriteContext {
        workspace_id: str_field("workspace_id"),
        channel: str_field("channel"),
        chat_type: str_field("chat_type"),
        chat_id: str_field("chat_id"),
        sender_id: str_field("owner_id").or_else(|| str_field("sender_id")),
        raw_sender: str_field("sender").or_else(|| str_field("raw_sender")),
    })
}

/// Let the agent search its own memory
pub struct MemoryRecallTool {
    memory: Arc<dyn Memory>,
    acl_enabled: bool,
}

impl MemoryRecallTool {
    pub fn new(memory: Arc<dyn Memory>, acl_enabled: bool) -> Self {
        Self { memory, acl_enabled }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search long-term memory for relevant facts, preferences, or context. Returns scored results ranked by relevance."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or phrase to search for in memory"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default: 5)"
                },
                "session_id": {
                    "type": "string",
                    "description": "Optional session scope; required as self_system for reserved router/ queries"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'query' parameter"))?;
        let session_id = args
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if query.starts_with("router/") && session_id != Some(crate::self_system::SELF_SYSTEM_SESSION_ID) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("router/ memory requires session_id=\"self_system\"".to_string()),
            });
        }

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(5, |v| v as usize);

        // FIX-#50: When memory ACL is enabled we no longer disable recall
        // outright (that was a UX regression). Instead we perform an
        // owner-scoped recall using the caller's trusted runtime scope so each
        // principal only sees memories it owns (plus public ones). When no
        // trusted scope is present under ACL we pass an empty owner context,
        // which the ACL-aware backend resolves to an anonymous principal that
        // returns no cross-principal memories rather than leaking the whole
        // store. With ACL disabled the legacy unscoped recall path is preserved
        // unchanged.
        let recall_result = if self.acl_enabled {
            let scope_ctx = parse_scope_ctx(&args).unwrap_or(MemoryWriteContext {
                workspace_id: None,
                channel: None,
                chat_type: None,
                chat_id: None,
                sender_id: None,
                raw_sender: None,
            });
            self.memory
                .recall_with_context(query, limit, session_id, Some(&scope_ctx))
                .await
        } else {
            self.memory.recall(query, limit, session_id).await
        };

        match recall_result {
            Ok(entries) if entries.is_empty() => Ok(ToolResult {
                success: true,
                output: "No memories found matching that query.".into(),
                error: None,
            }),
            Ok(entries) => {
                let mut output = format!("Found {} memories:\n", entries.len());
                for entry in &entries {
                    let score = entry.score.map_or_else(String::new, |s| format!(" [{s:.0}%]"));
                    let _ = writeln!(output, "- [{}] {}: {}{score}", entry.category, entry.key, entry.content);
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Memory recall failed: {e}")),
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

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryCategory, SqliteMemory};
    use tempfile::TempDir;

    fn seeded_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    #[tokio::test]
    async fn recall_empty() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem, false);
        let result = tool.execute(json!({"query": "anything"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memories found"));
    }

    #[tokio::test]
    async fn recall_finds_match() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("tz", "Timezone is EST", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem, false);
        let result = tool.execute(json!({"query": "Rust"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(result.output.contains("Found 1"));
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let (_tmp, mem) = seeded_mem();
        for i in 0..10 {
            mem.store(&format!("k{i}"), &format!("Rust fact {i}"), MemoryCategory::Core, None)
                .await
                .unwrap();
        }

        let tool = MemoryRecallTool::new(mem, false);
        let result = tool.execute(json!({"query": "Rust", "limit": 3})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 3"));
    }

    #[tokio::test]
    async fn recall_missing_query() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem, false);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recall_acl_mode_does_owner_scoped_recall_not_disabled() {
        // FIX-#50: with ACL enabled, recall must NOT return the old "disabled"
        // error. Instead it performs an owner-scoped recall via
        // recall_with_context. With no trusted scope and no stored memories the
        // call succeeds and reports no matches (never the disabled error).
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem, true);
        let result = tool.execute(json!({"query": "Rust"})).await.unwrap();
        assert!(result.success, "ACL recall should succeed, not be disabled");
        assert!(
            result.error.as_deref().map_or(true, |msg| !msg.contains("disabled")),
            "ACL recall must not return the legacy disabled error"
        );
        assert!(result.output.contains("No memories found"));
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem, false);
        assert_eq!(tool.name(), "memory_recall");
        assert!(tool.parameters_schema()["properties"]["query"].is_object());
        assert!(tool.parameters_schema()["properties"]["session_id"].is_object());
    }

    #[tokio::test]
    async fn recall_rejects_reserved_router_query_without_self_system_session() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem, false);
        let result = tool.execute(json!({"query": "router/elo/test"})).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|msg| msg.contains("session_id=\"self_system\""))
        );
    }
}
