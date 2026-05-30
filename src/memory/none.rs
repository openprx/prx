use super::traits::{Memory, MemoryCategory, MemoryEntry};
use async_trait::async_trait;

/// Explicit no-op memory backend.
///
/// This backend is used when `memory.backend = "none"` to disable persistence
/// while keeping the runtime wiring stable.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoneMemory;

impl NoneMemory {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Memory for NoneMemory {
    fn name(&self) -> &str {
        "none"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    async fn increment_useful_count(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        CompactionRunInput, DocumentIngestInput, MemoryEventInput, MemoryPrincipal, MemoryVisibility,
        MessageEventInput, RetrievalTraceInput,
    };

    #[tokio::test]
    async fn none_memory_is_noop() {
        let memory = NoneMemory::new();

        memory.store("k", "v", MemoryCategory::Core, None).await.unwrap();

        assert!(memory.get("k").await.unwrap().is_none());
        assert!(memory.recall("k", 10, None).await.unwrap().is_empty());
        assert!(memory.list(None, None).await.unwrap().is_empty());
        assert!(!memory.forget("k").await.unwrap());
        assert_eq!(memory.count().await.unwrap(), 0);
        assert!(memory.health_check().await);
    }

    #[tokio::test]
    async fn memory_fail_fast_fabric_document_retrieval_compaction_defaults() {
        let memory = NoneMemory::new();
        let err = memory
            .append_message_event(MessageEventInput {
                event_id: None,
                idempotency_key: None,
                workspace_id: "workspace".to_string(),
                owner_id: None,
                source: "test".to_string(),
                channel: None,
                session_key: None,
                parent_session_key: None,
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                sender: None,
                recipient: None,
                role: "event".to_string(),
                content: "router.route_decision decision_id=test".to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fabric::append_message_event"));
        assert!(err.to_string().contains("fail_fast"));

        let err = memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: "workspace".to_string(),
                event_type: "memory.test".to_string(),
                subject_table: "message_events".to_string(),
                subject_id: "subject".to_string(),
                session_key: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fabric::append_memory_event"));

        let err = memory
            .ingest_document(DocumentIngestInput {
                document_id: None,
                workspace_id: "workspace".to_string(),
                owner_id: None,
                topic_id: None,
                task_id: None,
                source_message_event_id: None,
                source_kind: "test".to_string(),
                source_uri: None,
                title: None,
                content: "document".to_string(),
                mime_type: Some("text/plain".to_string()),
                visibility: MemoryVisibility::Workspace,
                metadata_json: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("document::ingest_document"));

        let principal = MemoryPrincipal {
            workspace_id: "workspace".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: None,
            channel: None,
            sender: None,
            owner_id: None,
        };
        let err = memory
            .search_document_chunks(&principal, "document", 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("document::search_document_chunks"));

        let err = memory
            .append_retrieval_trace(RetrievalTraceInput {
                trace_id: None,
                workspace_id: "workspace".to_string(),
                owner_id: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
                source: "test".to_string(),
                query: "document".to_string(),
                candidate_count: 0,
                selected_count: 0,
                dropped_count: 0,
                budget_tokens: Some(0),
                selected_json: Some("[]".to_string()),
                dropped_json: Some("[]".to_string()),
                payload_json: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("retrieval::append_retrieval_trace"));

        let err = memory
            .append_compaction_run(CompactionRunInput {
                run_id: None,
                workspace_id: "workspace".to_string(),
                owner_id: None,
                session_key: None,
                agent_id: None,
                persona_id: None,
                trigger: "test".to_string(),
                mode: "test".to_string(),
                source_message_count: 0,
                source_token_estimate: 0,
                summary: "summary".to_string(),
                summary_memory_key: None,
                source_event_ids_json: None,
                source_document_refs_json: None,
                fidelity_status: "unchecked".to_string(),
                payload_json: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("compaction::append_compaction_run"));
    }
}
