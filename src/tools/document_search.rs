use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::{DocumentSearchResult, Memory, MemoryPrincipal, RetrievalTraceInput, RetrievedContextItem};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

// Behavior-limits Phase 1: DEFAULT 5 -> 10, MAX 50 -> 200 (fuller recall).
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 200;

pub struct DocumentSearchTool {
    workspace_dir: PathBuf,
    memory: Arc<dyn Memory>,
}

impl DocumentSearchTool {
    pub fn new(workspace_dir: PathBuf, memory: Arc<dyn Memory>) -> Self {
        Self { workspace_dir, memory }
    }

    fn workspace_id(&self) -> String {
        self.workspace_dir.to_string_lossy().to_string()
    }
}

pub(crate) fn owner_id_for_document_principal(principal: &MemoryPrincipal) -> Option<String> {
    if principal
        .owner_id
        .as_deref()
        .is_some_and(|owner| !owner.trim().is_empty())
    {
        return principal.owner_id.clone();
    }
    let channel = principal.channel.as_deref()?.trim();
    let sender = principal.sender.as_deref()?.trim();
    if channel.is_empty() || sender.is_empty() {
        return None;
    }
    Some(
        crate::memory::principal::OwnerPrincipal::new(
            principal.workspace_id.clone(),
            channel,
            sender,
            principal.session_key.clone().unwrap_or_default(),
            vec![crate::memory::principal::Role::Anonymous],
        )
        .owner_id,
    )
}

pub(crate) fn parse_scope_principal(args: &serde_json::Value, workspace_id: String) -> MemoryPrincipal {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let scope = args.get("_zc_scope").and_then(serde_json::Value::as_object);

    let read_scope = |key: &str| {
        if !trusted {
            return None;
        }
        scope?
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    MemoryPrincipal {
        workspace_id,
        agent_id: None,
        persona_id: None,
        session_key: read_scope("chat_id"),
        channel: read_scope("channel"),
        sender: read_scope("sender"),
        owner_id: read_scope("owner_id"),
        legacy_session_key: None,
    }
}

pub(crate) fn document_result_to_context_item(result: &DocumentSearchResult) -> RetrievedContextItem {
    let chunk = &result.chunk;
    RetrievedContextItem {
        source: "document_chunk".to_string(),
        document_id: Some(chunk.document_id.clone()),
        chunk_id: Some(chunk.chunk_id.clone()),
        source_anchor: Some(chunk.source_anchor.clone()),
        score: Some(result.score),
        token_estimate: Some(chunk.token_estimate),
        payload_json: Some(
            json!({
                "workspace_id": chunk.workspace_id,
                "owner_id": chunk.owner_id,
                "topic_id": chunk.topic_id,
                "task_id": chunk.task_id,
                "chunk_index": chunk.chunk_index,
            })
            .to_string(),
        ),
    }
}

pub(crate) async fn append_document_tool_retrieval_trace(
    memory: &dyn Memory,
    principal: &MemoryPrincipal,
    source: &str,
    query: &str,
    candidate_count: usize,
    selected: &[RetrievedContextItem],
    dropped: &[RetrievedContextItem],
    payload: serde_json::Value,
) -> Option<String> {
    let selected_json = serde_json::to_string(selected).ok();
    let dropped_json = serde_json::to_string(dropped).ok();
    match memory
        .append_retrieval_trace(RetrievalTraceInput {
            trace_id: None,
            workspace_id: principal.workspace_id.clone(),
            owner_id: owner_id_for_document_principal(principal),
            session_key: principal.session_key.clone(),
            agent_id: principal.agent_id.clone(),
            persona_id: principal.persona_id.clone(),
            source: source.to_string(),
            query: query.to_string(),
            candidate_count,
            selected_count: selected.len(),
            dropped_count: dropped.len(),
            budget_tokens: None,
            selected_json,
            dropped_json,
            payload_json: Some(payload.to_string()),
        })
        .await
    {
        Ok(trace) => Some(trace.trace_id),
        Err(error) => {
            tracing::debug!(source, error = %error, "failed to append document tool retrieval trace");
            None
        }
    }
}

fn parse_limit(args: &serde_json::Value) -> usize {
    #[allow(clippy::cast_possible_truncation)]
    args.get("limit")
        .or_else(|| args.get("max_results"))
        .or_else(|| args.get("maxResults"))
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_LIMIT, |value| value as usize)
        .clamp(1, MAX_LIMIT)
}

#[async_trait]
impl Tool for DocumentSearchTool {
    fn name(&self) -> &str {
        "document_search"
    }

    fn description(&self) -> &str {
        "Search durable source document chunks visible to the current owner/workspace scope."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keyword query to search source document chunks"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum chunks to return (default: 5, max: 50)"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Alias of limit"
                },
                "maxResults": {
                    "type": "integer",
                    "description": "Alias of limit"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty 'query' parameter"))?;
        let principal = parse_scope_principal(&args, self.workspace_id());
        let results = self
            .memory
            .search_document_chunks(&principal, query, parse_limit(&args))
            .await?;
        let selected = results.iter().map(document_result_to_context_item).collect::<Vec<_>>();
        let trace_id = append_document_tool_retrieval_trace(
            self.memory.as_ref(),
            &principal,
            "tool.document_search",
            query,
            results.len(),
            &selected,
            &[],
            json!({
                "tool": self.name(),
                "limit": parse_limit(&args),
                "scope_trusted": args.get("_zc_scope_trusted").and_then(serde_json::Value::as_bool).unwrap_or(false),
            }),
        )
        .await;

        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "query": query,
                    "trace_id": trace_id,
                    "matches": [],
                }))?,
                error: None,
            });
        }

        let records = results
            .into_iter()
            .map(|result| {
                let chunk = result.chunk;
                json!({
                    "document_id": chunk.document_id,
                    "chunk_id": chunk.chunk_id,
                    "topic_id": chunk.topic_id,
                    "task_id": chunk.task_id,
                    "chunk_index": chunk.chunk_index,
                    "source_anchor": chunk.source_anchor,
                    "score": result.score,
                    "heading": chunk.heading,
                    "snippet": snippet(&chunk.content),
                })
            })
            .collect::<Vec<_>>();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "query": query,
                "trace_id": trace_id,
                "matches": records,
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

fn snippet(content: &str) -> String {
    const MAX_CHARS: usize = 320;
    let flattened = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= MAX_CHARS {
        return flattened;
    }
    format!("{}...", flattened.chars().take(MAX_CHARS).collect::<String>())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::memory::{DocumentIngestInput, Memory, MemoryVisibility, SqliteMemory};
    use tempfile::TempDir;

    #[tokio::test]
    async fn document_search_returns_scoped_source_anchors() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .ingest_document(DocumentIngestInput {
                document_id: Some("doc-search-1".into()),
                workspace_id: tmp.path().to_string_lossy().to_string(),
                owner_id: None,
                topic_id: Some("topic-a".into()),
                task_id: Some("task-a".into()),
                source_message_event_id: Some("msg-a".into()),
                source_kind: "test".into(),
                source_uri: Some("test://doc-search-1".into()),
                title: Some("Search Fixture".into()),
                content: "alpha beta source evidence for document retrieval".into(),
                mime_type: Some("text/plain".into()),
                visibility: MemoryVisibility::Workspace,
                metadata_json: None,
            })
            .await
            .unwrap();

        let tool = DocumentSearchTool::new(tmp.path().to_path_buf(), memory);
        let result = tool.execute(json!({"query": "source evidence"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("\"document_id\": \"doc-search-1\""));
        assert!(result.output.contains("\"source_anchor\": \"doc-search-1#chunk-0\""));
        assert!(result.output.contains("\"trace_id\":"));
    }
}
