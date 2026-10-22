use super::document_search::{
    append_document_tool_retrieval_trace, document_result_to_context_item, parse_scope_principal,
};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::{DocumentSearchResult, Memory, MemoryPrincipal, RetrievedContextItem};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

pub struct DocumentGetChunkTool {
    workspace_dir: PathBuf,
    memory: Arc<dyn Memory>,
}

impl DocumentGetChunkTool {
    pub fn new(workspace_dir: PathBuf, memory: Arc<dyn Memory>) -> Self {
        Self { workspace_dir, memory }
    }

    fn workspace_id(&self) -> String {
        self.workspace_dir.to_string_lossy().to_string()
    }
}

impl DocumentGetChunkTool {
    fn execution_schema(&self) -> &'static std::sync::Arc<serde_json::Value> {
        static SCHEMA: std::sync::LazyLock<std::sync::Arc<serde_json::Value>> = std::sync::LazyLock::new(|| {
            std::sync::Arc::new({
                crate::tools::schema::with_required_alternatives(
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "chunk_id": {
                                "type": "string",
                                "description": "Stable document chunk id returned by document_search"
                            },
                            "chunkId": {
                                "type": "string",
                                "description": "Alias of chunk_id"
                            }
                        }
                    }),
                    &[&["chunk_id"], &["chunkId"]],
                )
            })
        });
        &SCHEMA
    }
}

#[async_trait]
impl Tool for DocumentGetChunkTool {
    fn name(&self) -> &str {
        "document_get_chunk"
    }

    fn description(&self) -> &str {
        "Retrieve one durable source document chunk by chunk_id when visible to the current owner/workspace scope. Provide at least one of chunk_id or chunkId."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::publish_flat_schema(self.execution_schema())
    }

    fn execution_parameters_schema(&self) -> Option<std::sync::Arc<serde_json::Value>> {
        Some(std::sync::Arc::clone(self.execution_schema()))
    }

    async fn execute(&self, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        crate::tools::schema::validate_execution_contract(self.name(), self.execution_schema(), &mut args)?;
        let chunk_id = args
            .get("chunk_id")
            .or_else(|| args.get("chunkId"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty 'chunk_id' parameter"))?;
        let principal = parse_scope_principal(&args, self.workspace_id());
        let Some(chunk) = self.memory.get_document_chunk(chunk_id).await? else {
            let dropped = vec![dropped_chunk_item(chunk_id, "not_found")];
            let trace_id = append_document_tool_retrieval_trace(
                self.memory.as_ref(),
                &principal,
                "tool.document_get_chunk",
                chunk_id,
                1,
                &[],
                &dropped,
                json!({
                    "tool": self.name(),
                    "chunk_id": chunk_id,
                    "result": "not_found"
                }),
            )
            .await;
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "chunk_id": chunk_id,
                    "trace_id": trace_id,
                    "found": false,
                }))?,
                error: None,
            });
        };

        if !chunk_visible_to_principal(&chunk.workspace_id, chunk.owner_id.as_deref(), &principal) {
            let dropped = vec![RetrievedContextItem {
                source: "document_chunk".to_string(),
                document_id: Some(chunk.document_id.clone()),
                chunk_id: Some(chunk.chunk_id.clone()),
                source_anchor: Some(chunk.source_anchor.clone()),
                score: None,
                token_estimate: Some(chunk.token_estimate),
                payload_json: Some(
                    json!({
                        "workspace_id": chunk.workspace_id,
                        "owner_id": chunk.owner_id,
                        "reason": "acl_hidden"
                    })
                    .to_string(),
                ),
            }];
            let trace_id = append_document_tool_retrieval_trace(
                self.memory.as_ref(),
                &principal,
                "tool.document_get_chunk",
                chunk_id,
                1,
                &[],
                &dropped,
                json!({
                    "tool": self.name(),
                    "chunk_id": chunk_id,
                    "result": "acl_hidden"
                }),
            )
            .await;
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "chunk_id": chunk_id,
                    "trace_id": trace_id,
                    "found": false,
                }))?,
                error: None,
            });
        }

        let selected_result = DocumentSearchResult {
            chunk: chunk.clone(),
            score: 1.0,
            // Direct chunk fetch by id (not a search): the owning document's
            // source_kind is not resolved here. The explicit get-chunk tool is an
            // intentional provenance lookup, not the agent recall path, so leaving
            // this `None` is correct.
            source_kind: None,
        };
        let selected = vec![document_result_to_context_item(&selected_result)];
        let trace_id = append_document_tool_retrieval_trace(
            self.memory.as_ref(),
            &principal,
            "tool.document_get_chunk",
            chunk_id,
            1,
            &selected,
            &[],
            json!({
                "tool": self.name(),
                "chunk_id": chunk_id,
                "result": "selected"
            }),
        )
        .await;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "document_id": chunk.document_id,
                "chunk_id": chunk.chunk_id,
                "trace_id": trace_id,
                "workspace_id": chunk.workspace_id,
                "owner_id": chunk.owner_id,
                "topic_id": chunk.topic_id,
                "task_id": chunk.task_id,
                "chunk_index": chunk.chunk_index,
                "heading": chunk.heading,
                "content_sha256": chunk.content_sha256,
                "source_anchor": chunk.source_anchor,
                "token_estimate": chunk.token_estimate,
                "content": chunk.content,
            }))?,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Memory]
    }
}

/// Whether `principal` may read a chunk owned by `owner_id`.
///
/// Three cases, matching [`crate::memory::owner_predicate::owner_match_suffix`]
/// so this default-exposed MCP tool and the event read paths cannot drift:
///
/// - **owner-isolated principal, chunk owner equals the authenticated owner id**
///   → visible;
/// - **owner-isolated principal, anything else** (a different owner, *and* a
///   chunk with no owner at all) → hidden. An isolated principal's channel and
///   sender are the caller-chosen external subject alone, so the subject-derived
///   `OwnerPrincipal` below would admit a chunk belonging to a different issuer;
///   and an unattributed chunk cannot be proved to be the caller's. Both fail
///   closed, and a principal with no owner id reaches no owned chunk at all.
/// - **not owner-isolated** → the historical behaviour, unchanged: an
///   unattributed chunk is workspace-visible and the owner may be matched either
///   directly or through the channel/sender pair.
fn chunk_visible_to_principal(workspace_id: &str, owner_id: Option<&str>, principal: &MemoryPrincipal) -> bool {
    if workspace_id != principal.workspace_id {
        return false;
    }
    if principal.require_owner_match {
        let (Some(chunk_owner), Some(caller_owner)) = (
            owner_id,
            principal
                .owner_id
                .as_deref()
                .map(str::trim)
                .filter(|owner| !owner.is_empty()),
        ) else {
            return false;
        };
        return chunk_owner == caller_owner;
    }
    let Some(owner_id) = owner_id else {
        return true;
    };
    if principal.owner_id.as_deref() == Some(owner_id) {
        return true;
    }
    let (Some(channel), Some(sender)) = (principal.channel.as_deref(), principal.sender.as_deref()) else {
        return false;
    };
    let principal_owner = crate::memory::principal::OwnerPrincipal::new(
        principal.workspace_id.clone(),
        channel,
        sender,
        principal.session_key.clone().unwrap_or_default(),
        vec![crate::memory::principal::Role::Anonymous],
    );
    owner_id == principal_owner.owner_id
}

fn dropped_chunk_item(chunk_id: &str, reason: &str) -> RetrievedContextItem {
    RetrievedContextItem {
        source: "document_chunk".to_string(),
        document_id: None,
        chunk_id: Some(chunk_id.to_string()),
        source_anchor: None,
        score: None,
        token_estimate: None,
        payload_json: Some(json!({ "reason": reason }).to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::memory::principal::{OwnerPrincipal, Role};
    use crate::memory::{DocumentIngestInput, Memory, MemoryVisibility, SqliteMemory};
    use tempfile::TempDir;

    #[tokio::test]
    async fn schema_accepts_both_chunk_id_spellings() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let tool = DocumentGetChunkTool::new(tmp.path().to_path_buf(), memory);
        crate::tools::schema::assert_flat_execution_contract(&tool, tool.execution_schema()).await;
        let schema = tool.execution_schema();

        assert!(
            !crate::tools::schema::validate_tool_arguments(schema, &json!({})).is_empty(),
            "expected a non-empty value"
        );
        assert!(
            crate::tools::schema::validate_tool_arguments(schema, &json!({"chunk_id": "id"})).is_empty(),
            "expected an empty value"
        );
        assert!(
            crate::tools::schema::validate_tool_arguments(schema, &json!({"chunkId": "id"})).is_empty(),
            "expected an empty value"
        );
    }

    #[tokio::test]
    async fn document_get_chunk_returns_content_for_visible_owner() {
        let tmp = TempDir::new().unwrap();
        let workspace_id = tmp.path().to_string_lossy().to_string();
        let owner = OwnerPrincipal::new(
            workspace_id.clone(),
            "telegram",
            "alice",
            "chat-a",
            vec![Role::Anonymous],
        )
        .owner_id;
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .ingest_document(DocumentIngestInput {
                document_id: Some("doc-get-1".into()),
                workspace_id,
                owner_id: Some(owner),
                topic_id: Some("topic-a".into()),
                task_id: Some("task-a".into()),
                source_message_event_id: Some("msg-a".into()),
                source_kind: "test".into(),
                source_uri: None,
                title: None,
                content: "private owner chunk content".into(),
                mime_type: Some("text/plain".into()),
                visibility: MemoryVisibility::Private,
                metadata_json: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();

        let tool = DocumentGetChunkTool::new(tmp.path().to_path_buf(), memory);
        let result = tool
            .execute(json!({
                "chunk_id": "doc-get-1:chunk:0",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_id": "chat-a",
                    "sender": "alice"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("\"content\": \"private owner chunk content\""));
        assert!(result.output.contains("\"trace_id\":"));
    }

    /// C03 / V02-R4 (P2-2): `document_get_chunk` is default-exposed over MCP.
    /// An owner-isolated caller's channel/sender pair is the external subject
    /// alone, so resolving the chunk owner from it hands a private chunk owned
    /// by one issuer to a different issuer presenting the same subject.
    #[tokio::test]
    async fn c03_document_get_chunk_does_not_leak_a_subject_derived_chunk_to_another_issuer() {
        let tmp = TempDir::new().unwrap();
        let workspace_id = tmp.path().to_string_lossy().to_string();
        // The owner an MCP ingest would derive from channel + sender: subject
        // only, no issuer.
        let subject_derived_owner = OwnerPrincipal::new(
            workspace_id.clone(),
            "mcp",
            "shared-subject",
            "mcp:shared-subject",
            vec![Role::Anonymous],
        )
        .owner_id;
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .ingest_document(DocumentIngestInput {
                document_id: Some("doc-mcp-1".into()),
                workspace_id: workspace_id.clone(),
                owner_id: Some(subject_derived_owner),
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "test".into(),
                source_uri: None,
                title: None,
                content: "subject-derived-secret marker body".into(),
                mime_type: Some("text/plain".into()),
                visibility: MemoryVisibility::Private,
                metadata_json: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();
        // A second document with no owner at all: unattributed, and therefore
        // equally unprovable as the caller's.
        memory
            .ingest_document(DocumentIngestInput {
                document_id: Some("doc-unowned-1".into()),
                workspace_id,
                owner_id: None,
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "test".into(),
                source_uri: None,
                title: None,
                content: "unattributed-secret marker body".into(),
                mime_type: Some("text/plain".into()),
                visibility: MemoryVisibility::Private,
                metadata_json: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();

        let tool = DocumentGetChunkTool::new(tmp.path().to_path_buf(), memory);
        let isolated_scope = json!({
            "channel": "mcp",
            "chat_id": "mcp:shared-subject",
            "sender": "shared-subject",
            "owner_id": "owner:ws:agent:issuer-b:shared-subject",
            "owner_isolated": true,
        });

        for (chunk_id, secret) in [
            ("doc-mcp-1:chunk:0", "subject-derived-secret"),
            ("doc-unowned-1:chunk:0", "unattributed-secret"),
        ] {
            let result = tool
                .execute(json!({
                    "chunk_id": chunk_id,
                    "_zc_scope_trusted": true,
                    "_zc_scope": isolated_scope,
                }))
                .await
                .unwrap();
            assert!(
                !result.output.contains(secret),
                "`{chunk_id}` must not reach an owner-isolated caller of another issuer: {}",
                result.output
            );
            assert!(
                result.output.contains("\"found\": false"),
                "`{chunk_id}` must be reported as not found: {}",
                result.output
            );
        }
    }

    /// The complement: the owner-isolated caller that actually owns the chunk
    /// still reads it, so the rule is isolation and not a blanket denial.
    #[tokio::test]
    async fn c03_document_get_chunk_still_serves_the_owner_isolated_caller_its_own_chunk() {
        let tmp = TempDir::new().unwrap();
        let workspace_id = tmp.path().to_string_lossy().to_string();
        const ISSUER_B_OWNER: &str = "owner:ws:agent:issuer-b:shared-subject";
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .ingest_document(DocumentIngestInput {
                document_id: Some("doc-owned-by-b".into()),
                workspace_id,
                owner_id: Some(ISSUER_B_OWNER.to_string()),
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "test".into(),
                source_uri: None,
                title: None,
                content: "issuer-b own chunk body".into(),
                mime_type: Some("text/plain".into()),
                visibility: MemoryVisibility::Private,
                metadata_json: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();

        let tool = DocumentGetChunkTool::new(tmp.path().to_path_buf(), memory);
        let result = tool
            .execute(json!({
                "chunk_id": "doc-owned-by-b:chunk:0",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "mcp",
                    "chat_id": "mcp:shared-subject",
                    "sender": "shared-subject",
                    "owner_id": ISSUER_B_OWNER,
                    "owner_isolated": true,
                },
            }))
            .await
            .unwrap();
        assert!(
            result.output.contains("issuer-b own chunk body"),
            "the owner must still read its own chunk: {}",
            result.output
        );
    }

    /// Fail closed: an owner-isolated caller with no usable owner id reaches no
    /// owner-scoped chunk, rather than falling back to the channel/sender pair.
    #[tokio::test]
    async fn c03_document_get_chunk_denies_an_owner_isolated_caller_without_an_owner_id() {
        let tmp = TempDir::new().unwrap();
        let workspace_id = tmp.path().to_string_lossy().to_string();
        let subject_derived_owner = OwnerPrincipal::new(
            workspace_id.clone(),
            "mcp",
            "shared-subject",
            "mcp:shared-subject",
            vec![Role::Anonymous],
        )
        .owner_id;
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .ingest_document(DocumentIngestInput {
                document_id: Some("doc-no-owner-caller".into()),
                workspace_id,
                owner_id: Some(subject_derived_owner),
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "test".into(),
                source_uri: None,
                title: None,
                content: "no-owner-caller-secret body".into(),
                mime_type: Some("text/plain".into()),
                visibility: MemoryVisibility::Private,
                metadata_json: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();

        let tool = DocumentGetChunkTool::new(tmp.path().to_path_buf(), memory);
        for scope in [
            json!({
                "channel": "mcp",
                "chat_id": "mcp:shared-subject",
                "sender": "shared-subject",
                "owner_isolated": true,
            }),
            json!({
                "channel": "mcp",
                "chat_id": "mcp:shared-subject",
                "sender": "shared-subject",
                "owner_id": "   ",
                "owner_isolated": true,
            }),
        ] {
            let result = tool
                .execute(json!({
                    "chunk_id": "doc-no-owner-caller:chunk:0",
                    "_zc_scope_trusted": true,
                    "_zc_scope": scope,
                }))
                .await
                .unwrap();
            assert!(
                !result.output.contains("no-owner-caller-secret"),
                "an isolated caller with no owner id must reach no owner-scoped chunk: {}",
                result.output
            );
        }
    }

    #[tokio::test]
    async fn document_get_chunk_returns_trace_for_hidden_owner() {
        let tmp = TempDir::new().unwrap();
        let workspace_id = tmp.path().to_string_lossy().to_string();
        let owner = OwnerPrincipal::new(
            workspace_id.clone(),
            "telegram",
            "alice",
            "chat-a",
            vec![Role::Anonymous],
        )
        .owner_id;
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .ingest_document(DocumentIngestInput {
                document_id: Some("doc-hidden-1".into()),
                workspace_id,
                owner_id: Some(owner),
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "test".into(),
                source_uri: None,
                title: None,
                content: "hidden owner chunk content".into(),
                mime_type: Some("text/plain".into()),
                visibility: MemoryVisibility::Private,
                metadata_json: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
            })
            .await
            .unwrap();

        let tool = DocumentGetChunkTool::new(tmp.path().to_path_buf(), memory);
        let result = tool
            .execute(json!({
                "chunk_id": "doc-hidden-1:chunk:0",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_id": "chat-b",
                    "sender": "bob"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("\"found\": false"));
        assert!(result.output.contains("\"trace_id\":"));
    }
}
