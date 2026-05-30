use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::Memory;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct MemoryReindexTool {
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
}

impl MemoryReindexTool {
    pub fn new(memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self { memory, security }
    }
}

#[async_trait]
impl Tool for MemoryReindexTool {
    fn name(&self) -> &str {
        "memory_reindex"
    }

    fn description(&self) -> &str {
        "Rebuild memory/document search indexes and backfill stale embeddings for the active memory backend."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let operation_name = "memory_reindex:rebuild";
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
        if let Err(error) = SideEffectGate::new(&self.security).authorize_resource_operation(
            self.name(),
            operation_name,
            ResourceRiskLevel::Medium,
            approval_grant.as_ref(),
        ) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let backend = self.memory.name().to_string();
        match self.memory.reindex().await {
            Ok(count) => Ok(ToolResult {
                success: true,
                output: format!("Memory reindex complete for {backend}: {count} stale vectors rebuilt"),
                error: None,
            }),
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Memory reindex failed: {error}")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryCategory, SqliteMemory};
    use crate::security::AutonomyLevel;
    use crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG;
    use tempfile::TempDir;

    fn approved_reindex_args() -> serde_json::Value {
        serde_json::json!({
            RUNTIME_APPROVAL_GRANT_ARG: ApprovalGrant::for_resource_operation(
                "memory_reindex",
                "memory_reindex:rebuild",
                "test",
                None,
            )
        })
    }

    #[tokio::test]
    async fn memory_reindex_tool_runs_backend_reindex() {
        let tmp = TempDir::new().unwrap();
        let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        mem.store("core-key", "core content", MemoryCategory::Core, None)
            .await
            .unwrap();
        let tool = MemoryReindexTool::new(mem, Arc::new(SecurityPolicy::default()));

        let result = tool.execute(approved_reindex_args()).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("Memory reindex complete"));
    }

    #[tokio::test]
    async fn memory_reindex_tool_blocks_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = MemoryReindexTool::new(mem, readonly);

        let result = tool.execute(serde_json::json!({})).await.unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only mode"));
    }
}
