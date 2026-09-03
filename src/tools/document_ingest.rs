use super::document_search::{owner_id_for_document_principal, parse_scope_principal};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::{DocumentIngestInput, Memory, MemoryVisibility};
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
struct DocumentIngestRuntime {
    workspace_dir: PathBuf,
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
}

impl DocumentIngestRuntime {
    fn workspace_id(&self) -> String {
        self.workspace_dir.to_string_lossy().to_string()
    }

    async fn content_from_args(
        &self,
        args: &serde_json::Value,
        require_path: bool,
    ) -> anyhow::Result<(String, Option<String>, Option<String>, Option<String>)> {
        let path = args.get("path").and_then(serde_json::Value::as_str).map(str::trim);
        if let Some(path) = path.filter(|path| !path.is_empty()) {
            if !self.security.is_path_allowed(path) {
                anyhow::bail!("Document path is blocked by the workspace security policy: {path}");
            }
            let candidate = self.workspace_dir.join(path);
            let resolved = tokio::fs::canonicalize(&candidate)
                .await
                .map_err(|error| anyhow::anyhow!("Failed to resolve document path {path}: {error}"))?;
            if !self.security.is_resolved_path_allowed(&resolved) || !resolved.is_file() {
                anyhow::bail!("Document path escapes the workspace or is not a regular file: {path}");
            }
            let metadata = tokio::fs::metadata(&resolved).await?;
            if metadata.len() > MAX_DOCUMENT_BYTES {
                anyhow::bail!("Document exceeds the {MAX_DOCUMENT_BYTES}-byte ingestion limit");
            }
            let bytes = tokio::fs::read(&resolved).await?;
            let content = String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("Document is not valid UTF-8: {}", resolved.display()))?;
            let source_uri = resolved.to_string_lossy().to_string();
            let title = args
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .or_else(|| resolved.file_name().and_then(|name| name.to_str()).map(str::to_string));
            let mime_type = mime_type_for_path(&resolved).map(str::to_string);
            return Ok((content, Some(source_uri), title, mime_type));
        }

        if require_path {
            anyhow::bail!("document_sync requires a non-empty workspace-relative 'path'");
        }
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Provide either 'path' or 'content'"))?;
        if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_DOCUMENT_BYTES {
            anyhow::bail!("Document exceeds the {MAX_DOCUMENT_BYTES}-byte ingestion limit");
        }
        let source_uri = args
            .get("source_uri")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|uri| !uri.is_empty())
            .map(str::to_string);
        let title = args
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string);
        let mime_type = args
            .get("mime_type")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|mime| !mime.is_empty())
            .map(str::to_string);
        Ok((content.to_string(), source_uri, title, mime_type))
    }

    async fn ingest(&self, tool_name: &str, args: serde_json::Value, require_path: bool) -> anyhow::Result<ToolResult> {
        if !self.memory.supports_document_ingest() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Memory backend '{}' does not support document ingestion",
                    self.memory.name()
                )),
            });
        }

        let (content, source_uri, title, mime_type) = self.content_from_args(&args, require_path).await?;

        let owner_ref = args
            .get("_zc_principal")
            .and_then(serde_json::Value::as_str)
            .map(op_id::ref_for_owner)
            .unwrap_or_else(|| "default".to_string());
        let operation_name = op_id::op_id(tool_name, "ingest", &[&owner_ref]);
        let approval_grant = ApprovalGrant::from_runtime_args(tool_name, &args);
        if let Err(error) = SideEffectGate::new(&self.security).authorize_resource_operation(
            tool_name,
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

        let principal = parse_scope_principal(&args, self.workspace_id());
        let owner_id = owner_id_for_document_principal(&principal);
        let document_id = args
            .get("document_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| source_uri.as_deref().map(stable_source_document_id));
        let visibility = args
            .get("visibility")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| if owner_id.is_some() { "private" } else { "workspace" })
            .parse::<MemoryVisibility>()?;
        let metadata_json = serde_json::to_string(&json!({
            "managed_by": tool_name,
            "parser_version": "commonmark-blocks-v1",
            "source_uri": source_uri,
            "synced_at": chrono::Utc::now().to_rfc3339(),
        }))?;

        let record = self
            .memory
            .ingest_document(DocumentIngestInput {
                document_id,
                workspace_id: principal.workspace_id,
                owner_id,
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "user_document".to_string(),
                source_uri,
                title,
                content,
                mime_type,
                visibility,
                metadata_json: Some(metadata_json),
            })
            .await?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&record)?,
            error: None,
        })
    }
}

pub struct DocumentIngestTool(DocumentIngestRuntime);

impl DocumentIngestTool {
    pub fn new(workspace_dir: PathBuf, memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self(DocumentIngestRuntime {
            workspace_dir,
            memory,
            security,
        })
    }
}

pub struct DocumentSyncTool(DocumentIngestRuntime);

impl DocumentSyncTool {
    pub fn new(workspace_dir: PathBuf, memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self(DocumentIngestRuntime {
            workspace_dir,
            memory,
            security,
        })
    }
}

fn common_schema(require_path: bool) -> serde_json::Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Workspace-relative UTF-8 document path"},
            "content": {"type": "string", "description": "Inline document content when path is omitted"},
            "document_id": {"type": "string", "description": "Stable id used to update an existing document"},
            "source_uri": {"type": "string", "description": "Optional source identifier for inline content"},
            "title": {"type": "string"},
            "mime_type": {"type": "string"},
            "visibility": {
                "type": "string",
                "enum": ["global", "workspace", "agent", "session", "private", "system"]
            }
        },
        "additionalProperties": false
    });
    if require_path {
        if let Some(object) = schema.as_object_mut() {
            object.insert("required".to_string(), json!(["path"]));
        }
    }
    schema
}

#[async_trait]
impl Tool for DocumentIngestTool {
    fn name(&self) -> &str {
        "document_ingest"
    }

    fn description(&self) -> &str {
        "Ingest inline content or a workspace file into the durable document index; repeated source paths update the same document."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        common_schema(false)
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.0.ingest(self.name(), args, false).await
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Memory, ToolCategory::FileSystem]
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        if self.0.memory.supports_document_ingest() {
            crate::capability::CapabilityAvailability::ready(format!(
                "memory backend '{}' supports document ingestion",
                self.0.memory.name()
            ))
        } else {
            crate::capability::CapabilityAvailability::declared(format!(
                "memory backend '{}' does not support document ingestion",
                self.0.memory.name()
            ))
        }
    }
}

#[async_trait]
impl Tool for DocumentSyncTool {
    fn name(&self) -> &str {
        "document_sync"
    }

    fn description(&self) -> &str {
        "Synchronize a workspace file into the durable document index using a stable source-derived document id."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        common_schema(true)
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.0.ingest(self.name(), args, true).await
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Memory, ToolCategory::FileSystem]
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        if self.0.memory.supports_document_ingest() {
            crate::capability::CapabilityAvailability::ready(format!(
                "memory backend '{}' supports document synchronization",
                self.0.memory.name()
            ))
        } else {
            crate::capability::CapabilityAvailability::declared(format!(
                "memory backend '{}' does not support document synchronization",
                self.0.memory.name()
            ))
        }
    }
}

fn stable_source_document_id(source_uri: &str) -> String {
    format!("source:{:x}", Sha256::digest(source_uri.as_bytes()))
}

fn mime_type_for_path(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some("text/markdown"),
        "txt" => Some("text/plain"),
        "json" => Some("application/json"),
        "csv" => Some("text/csv"),
        "html" | "htm" => Some("text/html"),
        "xml" => Some("application/xml"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::SqliteMemory;
    use crate::security::AutonomyLevel;
    use crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG;
    use tempfile::TempDir;

    fn approved_args(tool_name: &str, mut args: serde_json::Value) -> serde_json::Value {
        let grant = ApprovalGrant::for_resource_operation(
            tool_name,
            &op_id::op_id(tool_name, "ingest", &["default"]),
            "test",
            None,
        );
        args.as_object_mut().unwrap().insert(
            RUNTIME_APPROVAL_GRANT_ARG.to_string(),
            serde_json::to_value(grant).unwrap(),
        );
        args
    }

    #[tokio::test]
    async fn sync_reuses_source_document_id_and_updates_chunks() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("doc.md"), "# One\n\nold content").unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: temp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = DocumentSyncTool::new(temp.path().to_path_buf(), memory, security);

        let first = tool
            .execute(approved_args("document_sync", json!({"path": "doc.md"})))
            .await
            .unwrap();
        assert!(first.success, "{:?}", first.error);
        let first_json: serde_json::Value = serde_json::from_str(&first.output).unwrap();

        std::fs::write(temp.path().join("doc.md"), "# One\n\nnew content").unwrap();
        let second = tool
            .execute(approved_args("document_sync", json!({"path": "doc.md"})))
            .await
            .unwrap();
        assert!(second.success, "{:?}", second.error);
        let second_json: serde_json::Value = serde_json::from_str(&second.output).unwrap();
        assert_eq!(first_json.get("document_id"), second_json.get("document_id"));
        assert_ne!(first_json.get("content_sha256"), second_json.get("content_sha256"));
    }

    #[tokio::test]
    async fn ingest_rejects_workspace_escape() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.md"), "secret").unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).unwrap());
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = DocumentIngestTool::new(workspace.path().to_path_buf(), memory, security);
        let path = outside.path().join("secret.md").to_string_lossy().to_string();

        let error = tool
            .execute(approved_args("document_ingest", json!({"path": path})))
            .await
            .expect_err("outside path must fail");
        assert!(error.to_string().contains("blocked") || error.to_string().contains("escapes"));
    }
}
