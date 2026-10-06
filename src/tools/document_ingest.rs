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

        // V02-R9: an owner-isolated caller (external MCP) is confined to its own
        // namespace. Refused before any file is read or any row is written.
        let isolated = crate::tools::execution::scope_requires_owner_match(&args);
        if isolated && let Some(refusal) = isolated_ingest_refusal(tool_name, &args, require_path) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(refusal),
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
        if isolated && owner_id.is_none() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "{tool_name} requires an authenticated owner id for an owner-isolated caller"
                )),
            });
        }
        let requested_document_id = args
            .get("document_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| source_uri.as_deref().map(stable_source_document_id));
        // R-MCPB F-5: the owner namespace belongs to owner-isolated callers
        // alone. A writer that is not isolated must not name an id inside it,
        // or it could take such a document over through the same upsert.
        if !isolated
            && requested_document_id
                .as_deref()
                .is_some_and(|id| id.starts_with(OWNER_DOCUMENT_ID_PREFIX))
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "{tool_name}: document ids starting with '{OWNER_DOCUMENT_ID_PREFIX}' are reserved for owner-isolated callers"
                )),
            });
        }
        // Both adapters upsert on `document_id` and move the row's owner to the
        // writer, so an id two owners can both name is a takeover primitive.
        // An isolated caller's id therefore lives in its own namespace: stable
        // for that owner, unreachable for every other.
        let document_id = match owner_id.as_deref() {
            Some(owner) if isolated => requested_document_id.map(|id| owner_namespaced_document_id(owner, &id)),
            _ => requested_document_id,
        };
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
                // V02-R6: the trusted scope's chat id is the turn's session key.
                // A tool principal resolves no agent or persona
                // (`parse_scope_principal`), so those stay unset here.
                session_key: principal.session_key,
                agent_id: None,
                persona_id: None,
            })
            .await?;

        let mut rendered = serde_json::to_value(&record)?;
        if isolated && let Some(object) = rendered.as_object_mut() {
            // The adapter's workspace id is the host workspace path: nothing an
            // external caller needs, and not something to hand it.
            object.remove("workspace_id");
        }
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&rendered)?,
            error: None,
        })
    }
}

/// Why an owner-isolated caller may not make this ingestion call, if it may
/// not (V02-R9).
///
/// - A `path` reads a file of the host workspace, which belongs to the
///   operator rather than to an external owner; owner scoping cannot make that
///   safe, so it is refused outright. `document_sync` always takes a path, so
///   it is refused as a whole.
/// - The stored document is `private` to its owner. An explicit shared
///   visibility would publish the caller's text into the operator's own
///   document recall, so any value other than `private` is refused rather
///   than silently rewritten.
fn isolated_ingest_refusal(tool_name: &str, args: &serde_json::Value, require_path: bool) -> Option<String> {
    let has_path = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|path| !path.trim().is_empty());
    if require_path || has_path {
        return Some(format!(
            "{tool_name} cannot read workspace files for an owner-isolated caller; send the document as inline 'content'"
        ));
    }
    match args.get("visibility").and_then(serde_json::Value::as_str) {
        Some(visibility) if visibility != "private" => Some(format!(
            "{tool_name} stores an owner-isolated caller's documents as 'private'; visibility '{visibility}' is not available"
        )),
        _ => None,
    }
}

/// The reserved prefix of every owner-isolated caller's document id.
const OWNER_DOCUMENT_ID_PREFIX: &str = "owner-doc:";

/// Hex digits of `sha256(owner_id)` in an owner-namespaced document id: 128
/// bits, so a caller that chooses its own subject cannot search its way into
/// another owner's namespace (R-MCPB F-5).
const OWNER_DOCUMENT_ID_HASH_HEX: usize = 32;

/// The document id an owner-isolated caller's `id` resolves to:
/// `owner-doc:{first 32 hex digits of sha256(owner_id)}:{id}`.
fn owner_namespaced_document_id(owner_id: &str, id: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(owner_id.as_bytes()));
    let prefix = digest.get(..OWNER_DOCUMENT_ID_HASH_HEX).unwrap_or(digest.as_str());
    format!("{OWNER_DOCUMENT_ID_PREFIX}{prefix}:{id}")
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
    } else {
        schema = crate::tools::schema::with_required_alternatives(schema, &[&["path"], &["content"]]);
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
    use crate::memory::{MemoryPrincipal, SqliteMemory};
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

    #[test]
    fn ingest_schema_requires_path_or_content() {
        let schema = common_schema(false);
        assert!(!crate::tools::schema::validate_tool_arguments(&schema, &json!({})).is_empty());
        assert!(crate::tools::schema::validate_tool_arguments(&schema, &json!({"path": "doc.md"})).is_empty());
        assert!(crate::tools::schema::validate_tool_arguments(&schema, &json!({"content": "body"})).is_empty());
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

    fn ingest_event_principal(
        workspace_id: &str,
        session_key: Option<&str>,
        owner_id: Option<&str>,
    ) -> MemoryPrincipal {
        MemoryPrincipal {
            workspace_id: workspace_id.to_string(),
            agent_id: None,
            persona_id: None,
            session_key: session_key.map(str::to_string),
            channel: None,
            sender: None,
            owner_id: owner_id.map(str::to_string),
            require_owner_match: owner_id.is_some(),
            legacy_session_key: None,
        }
    }

    fn ingest_tool_fixture(temp: &TempDir) -> (Arc<dyn Memory>, DocumentIngestTool) {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite memory"));
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: temp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = DocumentIngestTool::new(temp.path().to_path_buf(), Arc::clone(&memory), security);
        (memory, tool)
    }

    /// V02-R6: the only test that proves the key travels from the trusted
    /// `_zc_scope.chat_id` to the `memory_events.session_key` column.
    ///
    /// The scope is not owner-isolated: since V02-R9 an isolated caller may
    /// only store `private` documents, and the key this test follows is the
    /// `session` one. The owner id still reaches the event.
    #[tokio::test]
    async fn ingest_stamps_the_turn_session_key_on_the_memory_event() {
        let temp = TempDir::new().expect("test: temp dir");
        let (memory, tool) = ingest_tool_fixture(&temp);
        let workspace_id = temp.path().to_string_lossy().to_string();
        let owner = "owner:ws:agent:issuer-a:subject-a";

        let result = tool
            .execute(approved_args(
                "document_ingest",
                json!({
                    "content": "scoped ingest body",
                    "visibility": "session",
                    "_zc_scope_trusted": true,
                    "_zc_scope": {"chat_id": "chat-1", "owner_id": owner},
                }),
            ))
            .await
            .expect("test: ingest executes");
        assert!(result.success, "{:?}", result.error);

        let events = memory
            .list_memory_events_recent(&ingest_event_principal(&workspace_id, Some("chat-1"), Some(owner)), 50)
            .await
            .expect("test: recent memory events");
        let event = events
            .iter()
            .find(|event| event.event_type == "document.ingested")
            .expect("test: the session ingest event must reach its own session");
        assert_eq!(event.session_key.as_deref(), Some("chat-1"));
        assert_eq!(event.owner_id.as_deref(), Some(owner));
    }

    /// An untrusted scope resolves no key at all, so the visibility default
    /// stays `workspace` and the event stays readable with no key — unchanged.
    #[tokio::test]
    async fn ingest_from_an_untrusted_scope_still_defaults_to_workspace() {
        let temp = TempDir::new().expect("test: temp dir");
        let (memory, tool) = ingest_tool_fixture(&temp);
        let workspace_id = temp.path().to_string_lossy().to_string();

        let result = tool
            .execute(approved_args(
                "document_ingest",
                json!({
                    "content": "unscoped ingest body",
                    "_zc_scope": {"chat_id": "chat-1", "owner_id": "owner:ws:agent:issuer-a:subject-a"},
                }),
            ))
            .await
            .expect("test: ingest executes");
        assert!(result.success, "{:?}", result.error);

        let events = memory
            .list_memory_events_recent(&ingest_event_principal(&workspace_id, None, None), 50)
            .await
            .expect("test: recent memory events");
        let event = events
            .iter()
            .find(|event| event.event_type == "document.ingested")
            .expect("test: a workspace ingest event stays readable without any key");
        assert_eq!(event.visibility, MemoryVisibility::Workspace);
        assert_eq!(event.session_key, None);
        assert_eq!(event.agent_id, None);
        assert_eq!(event.persona_id, None);
    }

    // ── V02-R9: owner-isolated ingestion stays inside the caller's namespace ──

    const R9_OWNER_A: &str = "owner:ws-r9:agent:issuer-a.example:shared-subject";
    const R9_OWNER_B: &str = "owner:ws-r9:agent:issuer-b.example:shared-subject";
    const R9_SOURCE_URI: &str = "https://docs.example/design";

    /// The arguments the execution service authors for an externally
    /// authenticated MCP caller: two issuers share the subject, so they differ
    /// in `owner_id` alone, and `owner_isolated` marks the isolated origin.
    fn r9_args(tool_name: &str, owner_id: &str, isolated: bool, mut fields: serde_json::Value) -> serde_json::Value {
        let mut scope = json!({
            "owner_id": owner_id,
            "sender": "shared-subject",
            "channel": "mcp",
            "chat_type": "gateway",
            "chat_id": "mcp:shared-subject",
        });
        if isolated && let Some(scope) = scope.as_object_mut() {
            scope.insert("owner_isolated".to_string(), json!(true));
        }
        let object = fields.as_object_mut().expect("test: ingest fields are an object");
        object.insert("_zc_scope_trusted".to_string(), json!(true));
        object.insert("_zc_scope".to_string(), scope);
        approved_args(tool_name, fields)
    }

    async fn r9_ingest(tool: &DocumentIngestTool, owner_id: &str, fields: serde_json::Value) -> ToolResult {
        tool.execute(r9_args("document_ingest", owner_id, true, fields))
            .await
            .expect("test: document_ingest executes")
    }

    fn r9_record(result: &ToolResult) -> serde_json::Value {
        assert!(result.success, "{:?}", result.error);
        serde_json::from_str(&result.output).expect("test: the ingest output is the document record")
    }

    fn r9_document_id(result: &ToolResult) -> String {
        r9_record(result)
            .get("document_id")
            .and_then(serde_json::Value::as_str)
            .expect("test: the record names its document id")
            .to_string()
    }

    async fn r9_first_chunk(memory: &Arc<dyn Memory>, document_id: &str) -> Option<crate::memory::DocumentChunkRecord> {
        memory
            .get_document_chunk(&format!("{document_id}:chunk:0"))
            .await
            .expect("test: read a document chunk")
    }

    /// SP-MCP test 12: the stable `source_uri` id used to be global, so a
    /// second owner re-ingesting the same URI replaced the first owner's
    /// document, deleted its chunks and took the document over.
    ///
    /// MUTATION GUARD: drop the owner namespace from the isolated document id
    /// and both the id and the chunk assertions go red.
    #[tokio::test]
    async fn owner_isolated_ingest_cannot_overwrite_another_owners_document_by_source_uri() {
        let temp = TempDir::new().expect("test: temp dir");
        let (memory, tool) = ingest_tool_fixture(&temp);

        let first = r9_ingest(
            &tool,
            R9_OWNER_A,
            json!({"content": "alice kestrel design notes", "source_uri": R9_SOURCE_URI}),
        )
        .await;
        let first_id = r9_document_id(&first);
        let second = r9_ingest(
            &tool,
            R9_OWNER_B,
            json!({"content": "mallory replacement body", "source_uri": R9_SOURCE_URI}),
        )
        .await;
        let second_id = r9_document_id(&second);

        assert_ne!(first_id, second_id, "two owners must never share a document id");
        let chunk = r9_first_chunk(&memory, &first_id)
            .await
            .expect("test: the first owner's chunk must survive");
        assert!(chunk.content.contains("alice kestrel design notes"), "{chunk:?}");
        assert_eq!(chunk.owner_id.as_deref(), Some(R9_OWNER_A));
    }

    /// The same, with an explicit `document_id` equal to the other owner's.
    #[tokio::test]
    async fn owner_isolated_ingest_cannot_overwrite_another_owners_document_by_explicit_id() {
        let temp = TempDir::new().expect("test: temp dir");
        let (memory, tool) = ingest_tool_fixture(&temp);

        let first = r9_ingest(
            &tool,
            R9_OWNER_A,
            json!({"content": "alice kestrel design notes", "document_id": "design-doc"}),
        )
        .await;
        let first_id = r9_document_id(&first);
        let second = r9_ingest(
            &tool,
            R9_OWNER_B,
            json!({"content": "mallory replacement body", "document_id": first_id.clone()}),
        )
        .await;
        let second_id = r9_document_id(&second);

        assert_ne!(first_id, second_id, "naming another owner's id must not reach it");
        let chunk = r9_first_chunk(&memory, &first_id)
            .await
            .expect("test: the first owner's chunk must survive");
        assert!(chunk.content.contains("alice kestrel design notes"), "{chunk:?}");
        assert_eq!(chunk.owner_id.as_deref(), Some(R9_OWNER_A));
    }

    /// The owner keeps a stable id of its own: re-ingesting its source URI
    /// updates its document in place.
    #[tokio::test]
    async fn owner_isolated_reingest_of_the_callers_own_source_updates_its_document() {
        let temp = TempDir::new().expect("test: temp dir");
        let (memory, tool) = ingest_tool_fixture(&temp);

        let first = r9_ingest(
            &tool,
            R9_OWNER_B,
            json!({"content": "first body", "source_uri": R9_SOURCE_URI}),
        )
        .await;
        let second = r9_ingest(
            &tool,
            R9_OWNER_B,
            json!({"content": "second body", "source_uri": R9_SOURCE_URI}),
        )
        .await;
        let document_id = r9_document_id(&first);
        assert_eq!(document_id, r9_document_id(&second));
        let chunk = r9_first_chunk(&memory, &document_id)
            .await
            .expect("test: the document has a chunk");
        assert!(chunk.content.contains("second body"), "{chunk:?}");
    }

    /// SP-MCP test 13: the host workspace filesystem is the operator's, not an
    /// external owner's, so a path is refused before any file is read, on both
    /// `document_ingest` and `document_sync`.
    #[tokio::test]
    async fn owner_isolated_ingest_refuses_a_workspace_path() {
        let temp = TempDir::new().expect("test: temp dir");
        std::fs::write(temp.path().join("operator-notes.md"), "# Operator\n\nhost-only secret").expect("test: write");
        let (_memory, tool) = ingest_tool_fixture(&temp);
        let refused = tool
            .execute(r9_args(
                "document_ingest",
                R9_OWNER_B,
                true,
                json!({"path": "operator-notes.md"}),
            ))
            .await
            .expect("test: document_ingest executes");
        assert!(
            !refused.success,
            "an isolated caller must not ingest a host file: {}",
            refused.output
        );
        assert!(!refused.output.contains("host-only secret"), "{}", refused.output);

        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite memory"));
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: temp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let sync = DocumentSyncTool::new(temp.path().to_path_buf(), memory, security);
        let refused = sync
            .execute(r9_args(
                "document_sync",
                R9_OWNER_B,
                true,
                json!({"path": "operator-notes.md"}),
            ))
            .await
            .expect("test: document_sync executes");
        assert!(
            !refused.success,
            "document_sync must refuse an isolated caller: {}",
            refused.output
        );
        assert!(!refused.output.contains("host-only secret"), "{}", refused.output);
    }

    /// An isolated caller stores privately: an explicit shared visibility is
    /// refused rather than published into the operator's document recall, and
    /// `private` (explicit or default) is kept.
    #[tokio::test]
    async fn owner_isolated_ingest_refuses_a_shared_visibility() {
        let temp = TempDir::new().expect("test: temp dir");
        let (_memory, tool) = ingest_tool_fixture(&temp);
        for shared in ["global", "workspace", "agent", "session", "system"] {
            let refused = r9_ingest(
                &tool,
                R9_OWNER_B,
                json!({"content": "published body", "visibility": shared}),
            )
            .await;
            assert!(
                !refused.success,
                "visibility {shared} must be refused: {}",
                refused.output
            );
        }
        for private in [
            json!({"content": "kept private"}),
            json!({"content": "kept private", "visibility": "private"}),
        ] {
            let stored = r9_ingest(&tool, R9_OWNER_B, private).await;
            assert_eq!(
                r9_record(&stored).get("visibility").and_then(serde_json::Value::as_str),
                Some("private")
            );
        }
    }

    /// The record an isolated caller gets back leaves out the adapter's
    /// workspace id, which is the host workspace path. (The caller's owner id
    /// is composed with the same path by the MCP identity format, so this is
    /// not a claim that the path is secret from the caller.)
    #[tokio::test]
    async fn owner_isolated_ingest_output_omits_the_adapter_workspace_id() {
        let temp = TempDir::new().expect("test: temp dir");
        let (_memory, tool) = ingest_tool_fixture(&temp);
        let stored = r9_ingest(&tool, R9_OWNER_B, json!({"content": "path free body"})).await;
        assert!(r9_record(&stored).get("workspace_id").is_none(), "{}", stored.output);
        assert!(
            !stored.output.contains(&*temp.path().to_string_lossy()),
            "{}",
            stored.output
        );
    }

    /// SP-MCP test 14 (control): a caller that is not owner-isolated keeps the
    /// global `source:{sha256}` id, its explicit visibility and path support.
    #[tokio::test]
    async fn a_non_isolated_ingest_keeps_the_global_source_id_and_visibility() {
        let temp = TempDir::new().expect("test: temp dir");
        let (_memory, tool) = ingest_tool_fixture(&temp);
        let stored = tool
            .execute(r9_args(
                "document_ingest",
                R9_OWNER_A,
                false,
                json!({"content": "shared body", "source_uri": R9_SOURCE_URI, "visibility": "workspace"}),
            ))
            .await
            .expect("test: document_ingest executes");
        let record = r9_record(&stored);
        assert_eq!(
            record.get("document_id").and_then(serde_json::Value::as_str),
            Some(stable_source_document_id(R9_SOURCE_URI).as_str())
        );
        assert_eq!(
            record.get("visibility").and_then(serde_json::Value::as_str),
            Some("workspace")
        );

        std::fs::write(temp.path().join("notes.md"), "# Notes\n\nbody").expect("test: write");
        let from_path = tool
            .execute(r9_args(
                "document_ingest",
                R9_OWNER_A,
                false,
                json!({"path": "notes.md"}),
            ))
            .await
            .expect("test: document_ingest executes");
        assert!(from_path.success, "{:?}", from_path.error);
    }

    /// R-MCPB F-5: the owner namespace of an isolated caller's document id is
    /// at least 128 bits of `sha256(owner_id)`; 64 bits left a targeted
    /// second preimage within reach of a caller that picks its own subject.
    #[tokio::test]
    async fn owner_namespaced_document_ids_carry_at_least_128_bits_of_the_owner_hash() {
        let temp = TempDir::new().expect("test: temp dir");
        let (_memory, tool) = ingest_tool_fixture(&temp);
        let stored = r9_ingest(
            &tool,
            R9_OWNER_A,
            json!({"content": "body", "document_id": "design-doc"}),
        )
        .await;
        let document_id = r9_document_id(&stored);
        let prefix = document_id
            .strip_prefix("owner-doc:")
            .and_then(|rest| rest.split_once(':'))
            .map(|(prefix, _)| prefix)
            .unwrap_or_else(|| panic!("test: an owner-namespaced id: {document_id}"));
        assert!(
            prefix.len() >= 32 && prefix.chars().all(|character| character.is_ascii_hexdigit()),
            "the owner prefix must carry at least 128 bits: {document_id}"
        );
    }

    /// R-MCPB F-5: the `owner-doc:` namespace is reserved for owner-isolated
    /// callers. A writer that is not isolated (a webhook, channel or sub-agent
    /// turn) used to be able to name an isolated caller's id — derivable from
    /// its owner id — and take the document over through the adapters'
    /// ownership-transferring upsert.
    ///
    /// MUTATION GUARD: drop the reserved-prefix refusal and this goes red.
    #[tokio::test]
    async fn a_non_isolated_writer_cannot_name_an_owner_namespaced_document_id() {
        let temp = TempDir::new().expect("test: temp dir");
        let (memory, tool) = ingest_tool_fixture(&temp);
        let first = r9_ingest(&tool, R9_OWNER_A, json!({"content": "alice kestrel design notes"})).await;
        let isolated = r9_ingest(
            &tool,
            R9_OWNER_A,
            json!({"content": "alice kestrel design notes", "document_id": "design-doc"}),
        )
        .await;
        assert!(first.success, "{:?}", first.error);
        let alice_document = r9_document_id(&isolated);

        let taken = tool
            .execute(r9_args(
                "document_ingest",
                R9_OWNER_B,
                false,
                json!({"content": "replacement body", "document_id": alice_document.clone()}),
            ))
            .await
            .expect("test: document_ingest executes");
        assert!(!taken.success, "a reserved id must be refused: {}", taken.output);
        let chunk = r9_first_chunk(&memory, &alice_document)
            .await
            .expect("test: alice's chunk survives");
        assert!(chunk.content.contains("alice kestrel design notes"), "{chunk:?}");
        assert_eq!(chunk.owner_id.as_deref(), Some(R9_OWNER_A));
    }
}
