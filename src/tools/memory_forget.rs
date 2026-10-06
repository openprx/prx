use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::Memory;
use crate::memory::principal::MemoryWriteContext;
#[cfg(test)]
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

/// Build a delete context from the runtime-trusted scope block
/// (`_zc_scope`, guarded by `_zc_scope_trusted`), exactly as the read tools do.
///
/// Model- and caller-supplied scope fields never reach here: the execution
/// service strips every `_zc_*` / `_prx_*` key from the request and re-authors
/// this block from the authenticated envelope.
///
/// `require_owner_match` is deliberately **not** read here: [`Tool::execute`]
/// sets it from [`crate::tools::execution::scope_requires_owner_match`]. That
/// reader needs the same trusted scope block this one does, so whenever it
/// reports an isolated caller this context exists too; applying the bit in
/// `execute` is a defence, not a second source. If the two readers ever
/// diverge, the isolated path starts from a default context, which is
/// fail-closed: an isolated caller with no owner deletes nothing.
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
    // The owner-isolation anchor is the same authenticated value, with blanks
    // treated as absent (the rule
    // `crate::memory::owner_predicate::owner_match_suffix` applies to the SQL
    // arm). `sender_id` below keeps the historical, untrimmed mapping so a
    // non-isolated delete resolves the identical principal it always did.
    let owner_anchor = owner_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Some(MemoryWriteContext {
        workspace_id,
        channel,
        chat_type,
        chat_id,
        sender_id: owner_id,
        raw_sender: sender,
        owner_id: owner_anchor,
        require_owner_match: false,
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

        let operation_name = crate::security::approval_operation::memory_delete_operation(
            args.get("_zc_principal").and_then(serde_json::Value::as_str),
            key,
        );
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

        // R-B1d D-1: the delete half of the owner boundary. The isolation bit
        // comes only from the service-authored scope, never from a model
        // argument and never from a config flag. It is read by its own reader,
        // which needs the same trusted scope block `parse_scope_ctx` does, so
        // an isolated caller always has a parsed context here; the default
        // below is a defence in case the two readers ever diverge, and it is
        // fail-closed (no owner, so nothing is deleted). A non-isolated call
        // keeps the historical `Option`: a caller without a trusted scope still
        // reaches the unscoped delete it always did.
        let isolated = crate::tools::execution::scope_requires_owner_match(&args);
        let mut scope_ctx = parse_scope_ctx(&args);
        if isolated {
            let mut context = scope_ctx.unwrap_or_default();
            context.require_owner_match = isolated;
            scope_ctx = Some(context);
        }
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
            workspace_id: Some("workspace-a".into()),
            channel: Some("telegram".into()),
            chat_type: Some("private".into()),
            chat_id: Some("dm-alice".into()),
            sender_id: None,
            raw_sender: Some("alice".into()),
            owner_id: None,
            require_owner_match: false,
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
            workspace_id: Some("workspace-a".into()),
            channel: Some("telegram".into()),
            chat_type: Some("private".into()),
            chat_id: Some("dm-alice".into()),
            sender_id: None,
            raw_sender: Some("alice".into()),
            owner_id: None,
            require_owner_match: false,
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

    // ── R-B1d D-1: the isolation bit reaches the delete ──────────

    const OWNER_A: &str = "owner:ws-isolation:agent:issuer-a.example:shared-subject";
    const OWNER_B: &str = "owner:ws-isolation:agent:issuer-b.example:shared-subject";

    /// The scope block the execution service authors for an externally
    /// authenticated MCP caller: channel, chat id and sender are the external
    /// subject alone, so two issuers differ in `owner_id` and nothing else.
    fn isolated_args(key: &str, owner_id: &str, isolated: bool) -> serde_json::Value {
        let mut args = approved_forget_args(key);
        args["_zc_scope_trusted"] = json!(true);
        args["_zc_scope"] = json!({
            "owner_id": owner_id,
            "sender": "shared-subject",
            "channel": "mcp",
            "chat_type": "gateway",
            "chat_id": "mcp:shared-subject",
            "workspace_id": "ws-isolation",
        });
        if isolated {
            args["_zc_scope"]["owner_isolated"] = json!(true);
        }
        args
    }

    async fn seed_owned(mem: &Arc<SqliteMemory>, owner_id: &str, key: &str) {
        let context = MemoryWriteContext {
            workspace_id: Some("ws-isolation".into()),
            channel: Some("mcp".into()),
            chat_type: Some("gateway".into()),
            chat_id: Some("mcp:shared-subject".into()),
            sender_id: Some(owner_id.to_string()),
            raw_sender: Some("shared-subject".into()),
            owner_id: Some(owner_id.to_string()),
            require_owner_match: false,
        };
        mem.store_with_context(
            key,
            "first issuer note",
            MemoryCategory::Conversation,
            None,
            Some(&context),
        )
        .await
        .expect("test: store an owner-attributed row");
    }

    /// MUTATION GUARD: drop the `scope_requires_owner_match` branch from
    /// `execute` and the first assertion goes red.
    #[tokio::test]
    async fn an_owner_isolated_delete_cannot_reach_another_owners_row() {
        let tmp = TempDir::new().unwrap();
        let mem = Arc::new(SqliteMemory::new_with_path(tmp.path().join("brain.db")).unwrap());
        seed_owned(&mem, OWNER_A, "issuer-a-note").await;

        let tool = MemoryForgetTool::new(mem.clone(), test_security_allow_high());
        let refused = tool
            .execute(isolated_args("issuer-a-note", OWNER_B, true))
            .await
            .expect("test: isolated delete");
        assert!(refused.success, "the refusal is the ordinary no-entry answer");
        assert!(refused.output.contains("No memory found"));
        assert!(
            mem.get("issuer-a-note").await.unwrap().is_some(),
            "an owner-isolated caller must not delete another owner's row"
        );

        let removed = tool
            .execute(isolated_args("issuer-a-note", OWNER_A, true))
            .await
            .expect("test: isolated delete by the owner");
        assert!(removed.output.contains("Forgot"));
        assert!(mem.get("issuer-a-note").await.unwrap().is_none());
    }

    /// The isolation bit is never read from a model argument, and clearing it
    /// in a forged scope changes nothing: the execution service strips the
    /// whole runtime-only block and re-authors it. This asserts the tool's own
    /// half — the bit it acts on is the one in the authored scope.
    #[tokio::test]
    async fn a_non_isolated_delete_keeps_the_historical_scope() {
        let tmp = TempDir::new().unwrap();
        let mem = Arc::new(SqliteMemory::new_with_path(tmp.path().join("brain.db")).unwrap());
        seed_owned(&mem, OWNER_A, "issuer-a-note").await;

        let tool = MemoryForgetTool::new(mem.clone(), test_security_allow_high());
        let removed = tool
            .execute(isolated_args("issuer-a-note", OWNER_B, false))
            .await
            .expect("test: non-isolated delete");
        assert!(
            removed.output.contains("Forgot"),
            "without the isolation marker the historical channel/sender scope still applies: {}",
            removed.output
        );
        assert!(mem.get("issuer-a-note").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_owner_isolated_delete_without_an_owner_id_removes_nothing() {
        let tmp = TempDir::new().unwrap();
        let mem = Arc::new(SqliteMemory::new_with_path(tmp.path().join("brain.db")).unwrap());
        seed_owned(&mem, OWNER_A, "issuer-a-note").await;

        let tool = MemoryForgetTool::new(mem.clone(), test_security_allow_high());
        for blank in ["", "   "] {
            let refused = tool
                .execute(isolated_args("issuer-a-note", blank, true))
                .await
                .expect("test: isolated delete with no owner id");
            assert!(refused.output.contains("No memory found"), "owner {blank:?}");
        }
        assert!(mem.get("issuer-a-note").await.unwrap().is_some());
    }

    /// A caller without a trusted scope keeps the unscoped delete it always
    /// had: the tool passes `None`, not a default context.
    #[tokio::test]
    async fn a_call_without_a_trusted_scope_still_reaches_the_unscoped_delete() {
        let tmp = TempDir::new().unwrap();
        let mem = Arc::new(SqliteMemory::new_with_path(tmp.path().join("brain.db")).unwrap());
        seed_owned(&mem, OWNER_A, "issuer-a-note").await;

        let tool = MemoryForgetTool::new(mem.clone(), test_security_allow_high());
        let removed = tool
            .execute(approved_forget_args("issuer-a-note"))
            .await
            .expect("test: untrusted-scope delete");
        assert!(removed.output.contains("Forgot"), "{}", removed.output);
        assert!(mem.get("issuer-a-note").await.unwrap().is_none());
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
}
