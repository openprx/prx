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

#[async_trait]
impl Tool for DocumentGetChunkTool {
    fn name(&self) -> &str {
        "document_get_chunk"
    }

    fn description(&self) -> &str {
        "Retrieve one durable source document chunk by chunk_id when visible to the current owner/workspace scope."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "chunk_id": {
                    "type": "string",
                    "description": "Stable document chunk id returned by document_search"
                },
                "chunkId": {
                    "type": "string",
                    "description": "Alias of chunk_id"
                }
            },
            "required": ["chunk_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
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

fn chunk_visible_to_principal(workspace_id: &str, owner_id: Option<&str>, principal: &MemoryPrincipal) -> bool {
    if workspace_id != principal.workspace_id {
        return false;
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
