use crate::memory::{
    Memory, MemoryCategory, MemoryDraft, MemoryDraftInput, MemoryEvent, MemoryEventInput, MemoryLinkInput,
    MemoryPrincipal, MemoryStoreMetadata, MemoryVisibility, MessageEvent, MessageEventInput,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub fn fail_fast(backend: &str, method: &str) -> anyhow::Error {
    anyhow::anyhow!("memory backend {backend} does not implement fabric::{method} (fail_fast)")
}

/// FIX-P1-20: extract the `document_id` of every `[document_ingest_ref] ...
/// [/document_ingest_ref]` block embedded in `content`.
///
/// The agent loop injects these markers when a large tool output is offloaded to
/// the document store. A summary/semantic memory derived from such content is
/// linked back to those documents via `memory_links`. Returns the distinct
/// document ids in first-seen order; an empty vector when no marker is present.
fn parse_document_ingest_refs(content: &str) -> Vec<String> {
    const OPEN: &str = "[document_ingest_ref]";
    const CLOSE: &str = "[/document_ingest_ref]";
    let mut ids = Vec::new();
    let mut remaining = content;
    while let Some(start) = remaining.find(OPEN) {
        let after_open = &remaining[start + OPEN.len()..];
        let Some(end) = after_open.find(CLOSE) else {
            break;
        };
        let block = &after_open[..end];
        for line in block.lines() {
            if let Some(value) = line.trim().strip_prefix("document_id:") {
                let id = value.trim();
                if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
                    ids.push(id.to_string());
                }
                break;
            }
        }
        remaining = &after_open[end + CLOSE.len()..];
    }
    ids
}

pub fn fail_fast_result<T>(backend: &str, method: &str) -> anyhow::Result<T> {
    return Err(fail_fast(backend, method));
}

pub fn fail_fast_optional<T>(backend: &str, method: &str) -> anyhow::Result<Option<T>> {
    return Err(fail_fast(backend, method));
}

/// Shared memory fabric facade used by ingress points to record normalized
/// runtime events without depending on backend-specific details.
#[derive(Clone)]
pub struct MemoryFabric {
    memory: Arc<dyn Memory>,
    workspace_id: String,
    event_recording: MemoryEventRecording,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MemoryEventRecording {
    pub enabled: bool,
    pub record_user_messages: bool,
    pub record_assistant_messages: bool,
    pub record_tool_events: bool,
}

impl Default for MemoryEventRecording {
    fn default() -> Self {
        Self {
            enabled: true,
            record_user_messages: true,
            record_assistant_messages: true,
            record_tool_events: false,
        }
    }
}

/// Cursor-based SQLite-first live watcher. Postgres can keep the same public
/// contract and replace the internals with LISTEN/NOTIFY later.
pub struct MemoryEventWatcher {
    fabric: MemoryFabric,
    principal: MemoryPrincipal,
    cursor: i64,
    limit: usize,
}

/// Common routing and ownership metadata for message-fabric writes.
#[derive(Debug, Clone)]
pub struct MessageEventScope {
    pub source: String,
    pub owner_id: Option<String>,
    pub channel: Option<String>,
    pub session_key: Option<String>,
    pub parent_session_key: Option<String>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub visibility: MemoryVisibility,
}

impl MessageEventScope {
    #[must_use]
    pub fn new(source: impl Into<String>, visibility: MemoryVisibility) -> Self {
        Self {
            source: source.into(),
            owner_id: None,
            channel: None,
            session_key: None,
            parent_session_key: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: None,
            recipient: None,
            visibility,
        }
    }

    #[must_use]
    pub fn with_channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }

    #[must_use]
    pub fn with_owner_id(mut self, owner_id: impl Into<String>) -> Self {
        self.owner_id = Some(owner_id.into());
        self
    }

    #[must_use]
    pub fn with_session_key(mut self, session_key: impl Into<String>) -> Self {
        self.session_key = Some(session_key.into());
        self
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    #[must_use]
    pub fn with_persona_id(mut self, persona_id: impl Into<String>) -> Self {
        self.persona_id = Some(persona_id.into());
        self
    }

    #[must_use]
    pub fn with_sender(mut self, sender: impl Into<String>) -> Self {
        self.sender = Some(sender.into());
        self
    }

    #[must_use]
    pub fn with_recipient(mut self, recipient: impl Into<String>) -> Self {
        self.recipient = Some(recipient.into());
        self
    }

    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    #[must_use]
    pub fn with_parent_run_id(mut self, parent_run_id: impl Into<String>) -> Self {
        self.parent_run_id = Some(parent_run_id.into());
        self
    }
}

impl MemoryFabric {
    #[must_use]
    pub fn new(memory: Arc<dyn Memory>, workspace_id: impl Into<String>) -> Self {
        Self {
            memory,
            workspace_id: workspace_id.into(),
            event_recording: MemoryEventRecording::default(),
        }
    }

    #[must_use]
    pub const fn with_event_recording(mut self, event_recording: MemoryEventRecording) -> Self {
        self.event_recording = event_recording;
        self
    }

    #[must_use]
    pub fn memory(&self) -> Arc<dyn Memory> {
        self.memory.clone()
    }

    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub async fn record_inbound_user_message(
        &self,
        scope: MessageEventScope,
        content: impl Into<String>,
        idempotency_key: Option<String>,
        raw_payload_json: Option<String>,
    ) -> anyhow::Result<MessageEvent> {
        self.record_message_event(scope, "user", content, idempotency_key, raw_payload_json)
            .await
    }

    pub async fn record_assistant_message(
        &self,
        scope: MessageEventScope,
        content: impl Into<String>,
    ) -> anyhow::Result<MessageEvent> {
        self.record_message_event(scope, "assistant", content, None, None).await
    }

    pub async fn record_tool_event(
        &self,
        scope: MessageEventScope,
        content: impl Into<String>,
        raw_payload_json: Option<String>,
    ) -> anyhow::Result<MessageEvent> {
        self.record_message_event(scope, "tool", content, None, raw_payload_json)
            .await
    }

    pub async fn record_worker_result(
        &self,
        scope: MessageEventScope,
        content: impl Into<String>,
        raw_payload_json: Option<String>,
    ) -> anyhow::Result<MessageEvent> {
        self.record_message_event(scope, "event", content, None, raw_payload_json)
            .await
    }

    /// Append structured runtime timeline events such as RouteDecision and
    /// ProviderExecutionOutcome records into the shared message fabric.
    pub async fn record_runtime_event(
        &self,
        scope: MessageEventScope,
        event_type: &str,
        content: impl Into<String>,
        raw_payload_json: Option<String>,
    ) -> anyhow::Result<MessageEvent> {
        let payload = raw_payload_json.map_or_else(
            || Some(serde_json::json!({ "event_type": event_type }).to_string()),
            Some,
        );
        let content = format!("{} {}", event_type, content.into());
        self.record_message_event(scope, "event", content, None, payload).await
    }

    pub async fn record_task_event(
        &self,
        scope: MessageEventScope,
        task_id: impl Into<String>,
        event_type: impl Into<String>,
        payload_json: Option<String>,
    ) -> anyhow::Result<MemoryEvent> {
        self.memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: self.workspace_id.clone(),
                event_type: event_type.into(),
                subject_table: "tasks".to_string(),
                subject_id: task_id.into(),
                session_key: scope.session_key,
                // FIX-P1-16 (#60): thread task lineage through dedicated columns so
                // child runs are queryable by parent_run_id without parsing payload_json.
                run_id: scope.run_id,
                parent_run_id: scope.parent_run_id,
                agent_id: scope.agent_id,
                persona_id: scope.persona_id,
                visibility: scope.visibility,
                payload_json,
            })
            .await
    }

    pub async fn record_xin_task_event(
        &self,
        scope: MessageEventScope,
        task_id: impl Into<String>,
        event_type: impl Into<String>,
        payload_json: Option<String>,
    ) -> anyhow::Result<MemoryEvent> {
        self.record_task_event(scope, task_id, event_type, payload_json).await
    }

    pub async fn record_cron_job_event(
        &self,
        scope: MessageEventScope,
        job_id: impl Into<String>,
        event_type: impl Into<String>,
        payload_json: Option<String>,
    ) -> anyhow::Result<MemoryEvent> {
        self.record_task_event(scope, job_id, event_type, payload_json).await
    }

    pub async fn record_semantic_memory(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.record_semantic_memory_from_event(key, content, category, session_id, None, None, None)
            .await
    }

    pub async fn record_semantic_memory_from_event(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        source_event_id: Option<&str>,
        agent_id: Option<&str>,
        persona_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let category_label = category.to_string();
        self.memory
            .store_with_metadata(
                key,
                content,
                category,
                session_id,
                MemoryStoreMetadata {
                    workspace_id: Some(self.workspace_id.clone()),
                    owner_id: None,
                    agent_id: agent_id.map(str::to_string),
                    persona_id: persona_id.map(str::to_string),
                    source_event_id: source_event_id.map(str::to_string),
                    source: Some("semantic_promotion".to_string()),
                    topic_id: None,
                    channel: None,
                },
            )
            .await?;
        let _event = self
            .memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: self.workspace_id.clone(),
                event_type: "memory.stored".to_string(),
                subject_table: "memories".to_string(),
                subject_id: key.to_string(),
                session_key: session_id.map(str::to_string),
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: Some(
                    serde_json::json!({
                        "key": key,
                        "category": category_label,
                        "source_event_id": source_event_id
                    })
                    .to_string(),
                ),
            })
            .await?;

        // FIX-P1-20: a promoted memory whose content carries one or more
        // `[document_ingest_ref]` markers is derived from those source documents.
        // Record the back-references in `memory_links` so the (previously dead)
        // table captures provenance from the semantic-memory path. Linking is
        // best-effort: a backend that does not implement `link_memory_source`
        // (or a transient failure) must not fail the memory write.
        for document_id in parse_document_ingest_refs(content) {
            let link = self
                .memory
                .link_memory_source(MemoryLinkInput {
                    link_id: None,
                    workspace_id: self.workspace_id.clone(),
                    owner_id: None,
                    memory_key: Some(key.to_string()),
                    memory_event_id: None,
                    message_event_id: source_event_id.map(str::to_string),
                    document_id: document_id.clone(),
                    chunk_id: None,
                    link_type: "derived_from".to_string(),
                    payload_json: None,
                })
                .await;
            if let Err(error) = link {
                tracing::debug!(
                    %document_id,
                    key,
                    "memory source link skipped (backend unsupported or transient): {error}"
                );
            }
        }
        Ok(())
    }

    pub async fn create_worker_memory_draft(
        &self,
        scope: &MessageEventScope,
        worker_run_id: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        source_event_id: Option<&str>,
        payload_json: Option<String>,
    ) -> anyhow::Result<MemoryDraft> {
        self.memory
            .create_memory_draft(MemoryDraftInput {
                draft_id: None,
                workspace_id: self.workspace_id.clone(),
                owner_id: scope.owner_id.clone(),
                worker_run_id: worker_run_id.to_string(),
                parent_run_id: scope.parent_run_id.clone(),
                session_key: scope.session_key.clone(),
                agent_id: scope.agent_id.clone(),
                persona_id: scope.persona_id.clone(),
                key: key.to_string(),
                content: content.to_string(),
                category,
                source_event_id: source_event_id.map(str::to_string),
                visibility: scope.visibility.clone(),
                payload_json,
            })
            .await
    }

    pub async fn record_draft_merge_requested(
        &self,
        draft: &MemoryDraft,
        target_workspace_id: Option<&str>,
    ) -> anyhow::Result<MemoryEvent> {
        self.memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: target_workspace_id.unwrap_or_else(|| self.workspace_id()).to_string(),
                event_type: "memory.draft.merge_requested".to_string(),
                subject_table: "memory_drafts".to_string(),
                subject_id: draft.draft_id.clone(),
                session_key: draft.session_key.clone(),
                run_id: None,
                parent_run_id: Some(draft.worker_run_id.clone()),
                agent_id: draft.agent_id.clone(),
                persona_id: draft.persona_id.clone(),
                visibility: draft.visibility.clone(),
                payload_json: Some(
                    serde_json::json!({
                        "draft_id": draft.draft_id,
                        "owner_id": draft.owner_id,
                        "worker_run_id": draft.worker_run_id,
                        "parent_run_id": draft.parent_run_id,
                        "key": draft.key,
                        "draft_workspace_id": draft.workspace_id
                    })
                    .to_string(),
                ),
            })
            .await
    }

    pub async fn merge_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        self.memory.merge_memory_draft(principal, draft_id).await
    }

    pub async fn reject_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        self.memory.reject_memory_draft(principal, draft_id, reason).await
    }

    pub async fn poll_memory_events(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        self.memory.list_memory_events_since(principal, after_id, limit).await
    }

    pub async fn poll_recent_memory_events(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        self.memory.list_memory_events_recent(principal, limit).await
    }

    #[must_use]
    pub fn watch_memory_events(
        &self,
        principal: MemoryPrincipal,
        start_after_id: i64,
        limit: usize,
    ) -> MemoryEventWatcher {
        MemoryEventWatcher {
            fabric: self.clone(),
            principal,
            cursor: start_after_id,
            limit: limit.max(1),
        }
    }

    async fn record_message_event(
        &self,
        scope: MessageEventScope,
        role: &str,
        content: impl Into<String>,
        idempotency_key: Option<String>,
        raw_payload_json: Option<String>,
    ) -> anyhow::Result<MessageEvent> {
        let content = content.into();
        if !self.should_record_role(role) {
            return Ok(self.synthetic_message_event(scope, role, content, idempotency_key, raw_payload_json));
        }
        self.memory
            .append_message_event(MessageEventInput {
                event_id: None,
                idempotency_key,
                workspace_id: self.workspace_id.clone(),
                owner_id: scope.owner_id,
                source: scope.source,
                channel: scope.channel,
                session_key: scope.session_key,
                parent_session_key: scope.parent_session_key,
                run_id: scope.run_id,
                parent_run_id: scope.parent_run_id,
                agent_id: scope.agent_id,
                persona_id: scope.persona_id,
                sender: scope.sender,
                recipient: scope.recipient,
                role: role.to_string(),
                content,
                raw_payload_json,
                visibility: scope.visibility,
            })
            .await
    }

    fn should_record_role(&self, role: &str) -> bool {
        if !self.event_recording.enabled {
            return false;
        }
        match role {
            "user" => self.event_recording.record_user_messages,
            "assistant" => self.event_recording.record_assistant_messages,
            "event" => true,
            "tool" | "system" => self.event_recording.record_tool_events,
            _ => true,
        }
    }

    fn synthetic_message_event(
        &self,
        scope: MessageEventScope,
        role: &str,
        content: String,
        idempotency_key: Option<String>,
        raw_payload_json: Option<String>,
    ) -> MessageEvent {
        let now = chrono::Utc::now().to_rfc3339();
        MessageEvent {
            id: 0,
            event_id: uuid::Uuid::new_v4().to_string(),
            idempotency_key,
            workspace_id: self.workspace_id.clone(),
            owner_id: scope.owner_id,
            source: scope.source,
            channel: scope.channel,
            session_key: scope.session_key,
            parent_session_key: scope.parent_session_key,
            run_id: scope.run_id,
            parent_run_id: scope.parent_run_id,
            agent_id: scope.agent_id,
            persona_id: scope.persona_id,
            sender: scope.sender,
            recipient: scope.recipient,
            role: role.to_string(),
            content,
            content_hash: None,
            raw_payload_json,
            visibility: scope.visibility,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

impl MemoryEventWatcher {
    #[must_use]
    pub const fn cursor(&self) -> i64 {
        self.cursor
    }

    pub async fn poll_next(&mut self) -> anyhow::Result<Vec<MemoryEvent>> {
        let events = self
            .fabric
            .poll_memory_events(&self.principal, self.cursor, self.limit)
            .await?;
        if let Some(last) = events.last() {
            self.cursor = last.id;
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::SqliteMemory;
    use tempfile::TempDir;

    #[tokio::test]
    async fn fabric_records_inbound_user_message_to_sqlite() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a");

        let event = fabric
            .record_inbound_user_message(
                MessageEventScope::new("chat", MemoryVisibility::Workspace)
                    .with_owner_id("owner:workspace-a:terminal:local-user")
                    .with_channel("terminal")
                    .with_session_key("chat:1")
                    .with_sender("local-user"),
                "hello fabric",
                Some("chat:1:msg:1".to_string()),
                None,
            )
            .await
            .unwrap();

        assert_eq!(event.workspace_id, "workspace-a");
        assert_eq!(event.owner_id.as_deref(), Some("owner:workspace-a:terminal:local-user"));
        assert_eq!(event.source, "chat");
        assert_eq!(event.role, "user");
        assert_eq!(event.content, "hello fabric");

        let visible = memory
            .list_message_events_since(
                &crate::memory::MemoryPrincipal {
                    workspace_id: "workspace-a".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("chat:1".to_string()),
                    channel: Some("terminal".to_string()),
                    sender: Some("local-user".to_string()),
                    owner_id: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(
            visible.first().map(|event| event.content.as_str()),
            Some("hello fabric")
        );
        assert_eq!(
            visible.first().and_then(|event| event.owner_id.as_deref()),
            Some("owner:workspace-a:terminal:local-user")
        );
    }

    #[tokio::test]
    async fn fabric_semantic_memory_uses_existing_store_path() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a");

        fabric
            .record_semantic_memory("fact-key", "durable fact", MemoryCategory::Core, None)
            .await
            .unwrap();

        let entry = memory.get("fact-key").await.unwrap().unwrap();
        assert_eq!(entry.content, "durable fact");

        let events = memory
            .list_memory_events_since(
                &crate::memory::MemoryPrincipal {
                    workspace_id: "workspace-a".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
                    channel: None,
                    sender: None,
                    owner_id: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events.first().map(|event| event.event_type.as_str()),
            Some("memory.stored")
        );
        assert_eq!(events.first().map(|event| event.subject_id.as_str()), Some("fact-key"));
    }

    #[tokio::test]
    async fn fabric_semantic_memory_records_source_event_payload() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a");

        fabric
            .record_semantic_memory_from_event(
                "fact-with-source",
                "durable fact",
                MemoryCategory::Core,
                Some("chat:1"),
                Some("message-event-1"),
                Some("agent-a"),
                Some("persona-a"),
            )
            .await
            .unwrap();

        let events = memory
            .list_memory_events_since(
                &crate::memory::MemoryPrincipal {
                    workspace_id: "workspace-a".to_string(),
                    agent_id: Some("agent-a".to_string()),
                    persona_id: Some("persona-a".to_string()),
                    session_key: Some("chat:1".to_string()),
                    channel: None,
                    sender: None,
                    owner_id: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events.first().map(|event| event.event_type.as_str()),
            Some("memory.stored")
        );
        assert!(
            events
                .first()
                .and_then(|event| event.payload_json.as_deref())
                .unwrap_or_default()
                .contains("\"source_event_id\":\"message-event-1\"")
        );
    }

    #[tokio::test]
    async fn fabric_worker_draft_records_merge_request_event() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a");
        let scope = MessageEventScope::new("session_worker", MemoryVisibility::Workspace)
            .with_session_key("session-a")
            .with_run_id("run-worker")
            .with_parent_run_id("run-parent")
            .with_agent_id("agent-a")
            .with_persona_id("persona-a");

        let draft = fabric
            .create_worker_memory_draft(
                &scope,
                "run-worker",
                "draft-key",
                "draft content",
                MemoryCategory::Conversation,
                Some("event-1"),
                None,
            )
            .await
            .unwrap();
        fabric
            .record_draft_merge_requested(&draft, Some("workspace-a"))
            .await
            .unwrap();

        let events = memory
            .list_memory_events_since(
                &crate::memory::MemoryPrincipal {
                    workspace_id: "workspace-a".to_string(),
                    agent_id: Some("agent-a".to_string()),
                    persona_id: Some("persona-a".to_string()),
                    session_key: Some("session-a".to_string()),
                    channel: None,
                    sender: None,
                    owner_id: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events.first().map(|event| event.event_type.as_str()),
            Some("memory.draft.created")
        );
        assert_eq!(
            events.get(1).map(|event| event.event_type.as_str()),
            Some("memory.draft.merge_requested")
        );
        assert_eq!(
            events.get(1).map(|event| event.subject_id.as_str()),
            Some(draft.draft_id.as_str())
        );
    }

    #[tokio::test]
    async fn memory_event_watcher_polls_and_advances_cursor() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a");
        let principal = crate::memory::MemoryPrincipal {
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("chat:1".to_string()),
            channel: Some("terminal".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: None,
        };
        let mut watcher = fabric.watch_memory_events(principal, 0, 10);

        fabric
            .record_inbound_user_message(
                MessageEventScope::new("chat", MemoryVisibility::Workspace)
                    .with_channel("terminal")
                    .with_session_key("chat:1")
                    .with_sender("local-user"),
                "watch this",
                None,
                None,
            )
            .await
            .unwrap();

        let first = watcher.poll_next().await.unwrap();
        assert_eq!(first.len(), 1);
        let first_event_id = first.first().map(|event| event.id).unwrap_or_default();
        assert_eq!(
            first.first().map(|event| event.event_type.as_str()),
            Some("message.created")
        );
        assert_eq!(watcher.cursor(), first_event_id);

        let second = watcher.poll_next().await.unwrap();
        assert!(second.is_empty());
        assert_eq!(watcher.cursor(), first_event_id);
    }

    #[tokio::test]
    async fn fabric_event_recording_can_disable_message_log_writes() {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "workspace-a").with_event_recording(MemoryEventRecording {
            enabled: false,
            ..MemoryEventRecording::default()
        });

        let event = fabric
            .record_inbound_user_message(
                MessageEventScope::new("chat", MemoryVisibility::Workspace)
                    .with_channel("terminal")
                    .with_session_key("chat:1"),
                "should not persist",
                Some("chat:1:msg:1".to_string()),
                None,
            )
            .await
            .unwrap();

        assert_eq!(event.id, 0);
        assert_eq!(event.content, "should not persist");

        let visible = memory
            .list_message_events_since(
                &crate::memory::MemoryPrincipal {
                    workspace_id: "workspace-a".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("chat:1".to_string()),
                    channel: Some("terminal".to_string()),
                    sender: None,
                    owner_id: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert!(visible.is_empty());
    }
}
