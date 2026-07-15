use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::Memory;
use crate::memory::principal::MemoryWriteContext;
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Let the agent forget/delete a memory entry
pub struct MemoryForgetTool {
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
}

impl MemoryForgetTool {
    pub fn new(memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self { memory, security }
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
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn description(&self) -> &str {
        "Remove a memory by key. Use to delete outdated facts or sensitive data. Returns whether the memory was found and removed."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key of the memory to forget"
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'key' parameter"))?;

        let owner_ref = args
            .get("_zc_principal")
            .and_then(serde_json::Value::as_str)
            .map(op_id::ref_for_owner)
            .unwrap_or_else(|| "default".to_string());
        let key_ref = op_id::fingerprint16(key);
        let operation_name = op_id::op_id(self.name(), "delete", &[&owner_ref, &key_ref]);
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
        if let Err(error) = SideEffectGate::new(&self.security).authorize_resource_operation(
            self.name(),
            &operation_name,
            ResourceRiskLevel::High,
            approval_grant.as_ref(),
        ) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let scope_ctx = parse_scope_ctx(&args);
        match self.memory.forget_with_context(key, scope_ctx.as_ref()).await {
            Ok(true) => Ok(ToolResult {
                success: true,
                output: format!("Forgot memory: {key}"),
                error: None,
            }),
            Ok(false) => Ok(ToolResult {
                success: true,
                output: format!("No memory found with key: {key}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to forget memory: {e}")),
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
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
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use tempfile::TempDir;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    // A matching runtime grant lets a High-risk op traverse an explicit
    // Supervised gate.
    fn test_security_allow_high() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        })
    }

    fn test_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    fn approved_forget_args(key: &str) -> serde_json::Value {
        let key_ref = op_id::fingerprint16(key);
        let operation = op_id::op_id("memory_forget", "delete", &["default", &key_ref]);
        json!({
            "key": key,
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: ApprovalGrant::for_resource_operation(
                "memory_forget",
                &operation,
                "test",
                None,
            )
        })
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security());
        assert_eq!(tool.name(), "memory_forget");
        assert!(tool.parameters_schema()["properties"]["key"].is_object());
    }

    #[tokio::test]
    async fn forget_existing() {
        let (_tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security_allow_high());
        let result = tool.execute(approved_forget_args("temp")).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Forgot"));

        assert!(mem.get("temp").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_nonexistent() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security_allow_high());
        let result = tool.execute(approved_forget_args("nope")).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memory found"));
    }

    #[tokio::test]
    async fn forget_denies_cross_scope_delete() {
        let tmp = TempDir::new().unwrap();
        let mem = Arc::new(SqliteMemory::new_with_path(tmp.path().join("brain.db")).unwrap());
        let alice_ctx = MemoryWriteContext {
            channel: Some("telegram".into()),
            chat_type: Some("private".into()),
            chat_id: Some("dm-alice".into()),
            sender_id: None,
            raw_sender: Some("alice".into()),
        };
        mem.store_with_context(
            "alice-private",
            "Alice private note",
            MemoryCategory::Conversation,
            None,
            Some(&alice_ctx),
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security_allow_high());
        let mut args = approved_forget_args("alice-private");
        args["_zc_scope_trusted"] = json!(true);
        args["_zc_scope"] = json!({
            "sender": "bob",
            "channel": "telegram",
            "chat_type": "private",
            "chat_id": "dm-bob"
        });
        let result = tool.execute(args).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("No memory found"));
        assert!(mem.get("alice-private").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn forget_allows_same_scope_delete() {
        let tmp = TempDir::new().unwrap();
        let mem = Arc::new(SqliteMemory::new_with_path(tmp.path().join("brain.db")).unwrap());
        let alice_ctx = MemoryWriteContext {
            channel: Some("telegram".into()),
            chat_type: Some("private".into()),
            chat_id: Some("dm-alice".into()),
            sender_id: None,
            raw_sender: Some("alice".into()),
        };
        mem.store_with_context(
            "alice-private",
            "Alice private note",
            MemoryCategory::Conversation,
            None,
            Some(&alice_ctx),
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security_allow_high());
        let mut args = approved_forget_args("alice-private");
        args["_zc_scope_trusted"] = json!(true);
        args["_zc_scope"] = json!({
            "sender": "alice",
            "channel": "telegram",
            "chat_type": "private",
            "chat_id": "dm-alice"
        });
        let result = tool.execute(args).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("Forgot"));
        assert!(mem.get("alice-private").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_missing_key() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn forget_blocked_in_readonly_mode() {
        let (_tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = MemoryForgetTool::new(mem.clone(), readonly);
        let result = tool.execute(json!({"key": "temp"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only mode"));
        assert!(mem.get("temp").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn forget_blocked_when_rate_limited() {
        let (_tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        let limited = Arc::new(SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = MemoryForgetTool::new(mem.clone(), limited);
        let result = tool.execute(json!({"key": "temp"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Rate limit exceeded"));
        assert!(mem.get("temp").await.unwrap().is_some());
    }
}
