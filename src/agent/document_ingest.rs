use crate::agent::turn_contracts::ScopeContext;
use crate::memory::{DocumentIngestInput, DocumentSearchResult, Memory, MemoryPrincipal, MemoryVisibility};
use crate::runtime::envelope::RuntimeEnvelope;
use sha2::Digest;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct DocumentIngestRuntime {
    pub(super) memory: Arc<dyn Memory>,
    pub(super) workspace_id: String,
    pub(super) owner_id: Option<String>,
    /// Whether document reads for this turn must match `owner_id` exactly.
    ///
    /// Authenticated external entrypoints set this on their runtime envelope;
    /// retaining it here keeps ingest provenance and context-page recall on
    /// the same owner boundary as the rest of the turn.
    pub(super) require_owner_match: bool,
    pub(super) topic_id: Option<String>,
    pub(super) task_id: Option<String>,
    pub(super) source_message_event_id: Option<String>,
    pub(super) session_key: Option<String>,
    pub(super) legacy_session_key: Option<String>,
    /// Stable identity of the turn this runtime serves.
    ///
    /// The runtime is built once per turn, so the incoming message event id
    /// identifies it; entry points without one (agent runs, cron) mint a value
    /// here instead, which is per-turn for the same reason.
    pub(super) turn_key: String,
    /// Set the first time this turn records a `context.compaction.degraded`
    /// event. The pre-provider budget check runs on every tool-loop iteration
    /// and the overflow retry adds four more call sites, so a session that
    /// stays degraded used to open one synchronous write transaction per
    /// iteration, forever.
    pub(super) degradation_recorded: Arc<std::sync::atomic::AtomicBool>,
    pub(super) agent_id: Option<String>,
    pub(super) persona_id: Option<String>,
    pub(super) channel: Option<String>,
    /// Originating sender. Retained so OS-paging recall (FIX-P3-05) can rebuild
    /// the exact owner-scoped `MemoryPrincipal` used when pages were ingested.
    pub(super) sender: Option<String>,
    pub(super) config_generation_id: Option<u64>,
    pub(super) config_source_revision: Option<String>,
    pub(super) visibility: MemoryVisibility,
}

impl DocumentIngestRuntime {
    pub(crate) fn from_envelope(memory: Arc<dyn Memory>, envelope: &RuntimeEnvelope) -> Self {
        Self {
            memory,
            workspace_id: envelope.workspace_id.clone(),
            owner_id: Some(envelope.resolved_owner_id()),
            require_owner_match: envelope.owner_isolated,
            topic_id: envelope.topic_id.clone(),
            task_id: envelope.resolved_task_id().map(ToString::to_string),
            source_message_event_id: envelope.source_message_event_id.clone(),
            session_key: Some(envelope.session_key.clone()),
            legacy_session_key: envelope.legacy_session_key.clone(),
            turn_key: envelope
                .source_message_event_id
                .clone()
                .unwrap_or_else(|| format!("turn:{}", Uuid::new_v4())),
            degradation_recorded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: envelope.agent_id.clone(),
            persona_id: envelope.persona_id.clone(),
            channel: envelope.channel.clone(),
            sender: envelope.sender.clone(),
            config_generation_id: envelope.config_generation_id,
            config_source_revision: envelope.config_source_revision.clone(),
            visibility: envelope.visibility.clone(),
        }
    }

    pub(crate) fn from_scope(memory: Arc<dyn Memory>, scope: &ScopeContext<'_>) -> Self {
        let workspace_id = scope.policy.workspace_dir.to_string_lossy().to_string();
        let owner_id = Some(
            crate::memory::principal::OwnerPrincipal::new(
                workspace_id.clone(),
                scope.channel,
                scope.sender,
                scope.chat_id,
                vec![crate::memory::principal::Role::Anonymous],
            )
            .owner_id,
        );
        Self {
            memory,
            workspace_id,
            owner_id,
            // ScopeContext is used by in-process callers, whose historical
            // workspace/session sharing semantics are intentionally retained.
            require_owner_match: false,
            topic_id: scope.topic_id.map(ToString::to_string),
            task_id: scope.task_id.map(ToString::to_string),
            source_message_event_id: scope.source_message_event_id.map(ToString::to_string),
            session_key: Some(scope.chat_id.to_string()),
            legacy_session_key: None,
            turn_key: scope
                .source_message_event_id
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("turn:{}", Uuid::new_v4())),
            degradation_recorded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            persona_id: None,
            // Persist the originating channel so anonymous principals can
            // resolve channel scope on later recall (FIX-P1-08).
            channel: Some(scope.channel.to_string()),
            sender: Some(scope.sender.to_string()),
            config_generation_id: scope.config_generation_id,
            config_source_revision: scope.config_source_revision.map(ToString::to_string),
            visibility: MemoryVisibility::Session,
        }
    }

    /// Re-key this runtime as a nested turn of the one that built its scope.
    ///
    /// A `delegate` or `sessions_spawn` sub-loop inherits the parent's
    /// `source_message_event_id` through [`ScopeContext`], so parent and child
    /// would otherwise carry the identical `turn_key`. Every per-turn
    /// idempotency key is derived from it, so the parent's degradation event
    /// and the child's collapse into one row and the child's degradation
    /// disappears from the audit trail. The suffix separates them while leaving
    /// the parent's id readable in the key. It is applied once, where the child
    /// runtime is built, so the key is still constant for the whole child loop
    /// and a child that degrades twice in its turn still records one event.
    #[must_use]
    pub(crate) fn into_nested_turn(mut self) -> Self {
        self.turn_key = format!("{}:sub:{}", self.turn_key, Uuid::new_v4());
        self
    }

    #[must_use]
    pub(crate) fn with_source_message_event_id(mut self, source_message_event_id: Option<String>) -> Self {
        if let Some(event_id) = source_message_event_id.as_ref() {
            self.turn_key.clone_from(event_id);
        }
        self.source_message_event_id = source_message_event_id;
        self
    }

    pub(super) fn memory_principal(&self) -> MemoryPrincipal {
        MemoryPrincipal {
            workspace_id: self.workspace_id.clone(),
            agent_id: self.agent_id.clone(),
            persona_id: self.persona_id.clone(),
            session_key: self.session_key.clone(),
            channel: self.channel.clone(),
            sender: self.sender.clone(),
            owner_id: self.owner_id.clone(),
            require_owner_match: self.require_owner_match,
            legacy_session_key: self.legacy_session_key.clone(),
        }
    }
}

/// Approximate chunk size for large tool-output document references.
/// 🟡 Behavior-limits Phase 1: raised 2048 -> 4096 (fewer chunks per big output).
pub(super) const TOOL_OUTPUT_DOCUMENT_CHUNK_TOKENS: usize = 4_096;

pub(super) fn tool_output_document_id(tool_name: &str, content: &str) -> (String, String) {
    let mut hasher = sha2::Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    let sha256 = hex::encode(digest);
    let short = sha256.get(..16).unwrap_or(&sha256);
    (format!("tool-output:{tool_name}:{short}"), sha256)
}

pub(super) async fn persist_tool_output_document(
    runtime: Option<&DocumentIngestRuntime>,
    tool_name: &str,
    content: &str,
    max_inline_chars: usize,
) -> Option<&'static str> {
    if content.len() <= max_inline_chars {
        return None;
    }
    let Some(runtime) = runtime else {
        return None;
    };
    // FIX-P0-28: backends without durable document ingest (NoneMemory /
    // MarkdownMemory) must skip persistence and degrade to plain history
    // compaction instead of emitting a fail-fast error on every large tool output.
    if !runtime.memory.supports_document_ingest() {
        tracing::debug!(
            tool = %tool_name,
            backend = runtime.memory.name(),
            "memory backend does not support document ingest; skipping tool output persistence (status=backend_not_supported)"
        );
        return Some("backend_not_supported");
    }
    let (document_id, sha256) = tool_output_document_id(tool_name, content);
    let metadata_json = serde_json::json!({
        "tool": tool_name,
        "content_sha256": sha256,
        "bytes": content.len(),
    })
    .to_string();
    let input = DocumentIngestInput {
        document_id: Some(document_id.clone()),
        workspace_id: runtime.workspace_id.clone(),
        owner_id: runtime.owner_id.clone(),
        topic_id: runtime.topic_id.clone(),
        task_id: runtime.task_id.clone(),
        source_message_event_id: runtime.source_message_event_id.clone(),
        source_kind: "tool_output".to_string(),
        source_uri: Some(format!("tool:{tool_name}")),
        title: Some(format!("Tool output: {tool_name}")),
        content: content.to_string(),
        mime_type: Some("text/plain".to_string()),
        visibility: runtime.visibility.clone(),
        metadata_json: Some(metadata_json),
        // V02-R6: scope the `document.ingested` event to this turn.
        session_key: runtime.session_key.clone(),
        agent_id: runtime.agent_id.clone(),
        persona_id: runtime.persona_id.clone(),
    };
    match runtime.memory.ingest_document(input).await {
        Ok(document) => {
            tracing::info!(
                document_id = %document.document_id,
                tool = %tool_name,
                chunks = document.chunk_count,
                "Persisted oversized tool output into document store"
            );
            Some("persisted_document_backend")
        }
        Err(error) => {
            tracing::warn!(
                document_id = %document_id,
                tool = %tool_name,
                error = %error,
                "Failed to persist oversized tool output into document store"
            );
            Some("document_ingest_failed")
        }
    }
}

/// FIX-P2-02: tools whose output is *externally controlled, untrusted* content
/// (remote web pages, arbitrary HTTP responses, on-disk files). Their results
/// are ingested verbatim as read-only, untrusted document copies so the exact
/// fetched bytes are preserved and cannot later be silently rewritten in history
/// and re-presented to the model as trusted context.
const UNTRUSTED_EXTERNAL_CONTENT_TOOLS: &[&str] = &["web_fetch", "http_request", "file_read"];

/// FIX-P2-02: `source_kind` tag stamped on the read-only untrusted external copies
/// persisted by [`persist_untrusted_external_copy`]. Document search results whose
/// owning document carries this `source_kind` are hard-filtered out of the agent's
/// trusted recall context (see [`drop_untrusted_external_results`]) so that
/// attacker-controlled fetched content can never be silently re-injected into the
/// prompt as if it were trusted memory. The copies remain in the store for
/// provenance/audit and are still reachable via explicit document tools.
const UNTRUSTED_EXTERNAL_SOURCE_KIND: &str = "untrusted_external";

/// FIX-P2-02: remove untrusted external content copies from a document search
/// result set before it is assembled into trusted agent context.
///
/// Untrusted external copies (`source_kind == "untrusted_external"`) are ingested
/// only as a tamper-evident provenance record of exactly what was fetched; they
/// must never be recalled and re-injected as trusted context, otherwise the
/// ingest would *amplify* the prompt-injection surface it was meant to contain.
/// Results whose `source_kind` is `None` (backends that cannot resolve it) or any
/// other value are retained unchanged.
pub(super) fn drop_untrusted_external_results(results: &mut Vec<DocumentSearchResult>) {
    results.retain(|result| result.source_kind.as_deref() != Some(UNTRUSTED_EXTERNAL_SOURCE_KIND));
}

/// FIX-P2-02: persist a read-only, untrusted copy of externally-sourced tool
/// output (web_fetch / http_request / file_read) into the document store.
///
/// Unlike [`persist_tool_output_document`] (which only fires for *oversized*
/// outputs and stores a generic `tool_output` document at the runtime
/// visibility), this captures the **verbatim** content of every successful
/// external fetch — regardless of size — at `MemoryVisibility::Private` and tags
/// it as untrusted in metadata. The copy is never mutated after ingest, giving a
/// tamper-evident reference for the originally retrieved bytes. Backends without
/// durable document ingest (NoneMemory / MarkdownMemory) skip silently; ingest
/// failures are logged and never surfaced as a tool error (best-effort sink).
pub(super) async fn persist_untrusted_external_copy(
    runtime: Option<&DocumentIngestRuntime>,
    tool_name: &str,
    content: &str,
) {
    if !UNTRUSTED_EXTERNAL_CONTENT_TOOLS.contains(&tool_name) {
        return;
    }
    if content.trim().is_empty() {
        return;
    }
    let Some(runtime) = runtime else {
        return;
    };
    if !runtime.memory.supports_document_ingest() {
        tracing::debug!(
            tool = %tool_name,
            backend = runtime.memory.name(),
            "memory backend does not support document ingest; skipping untrusted external copy"
        );
        return;
    }
    let (base_id, sha256) = tool_output_document_id(tool_name, content);
    // Namespace the document id so the untrusted copy never collides with the
    // oversized `tool_output` document for the same content.
    let document_id = format!("untrusted:{base_id}");
    let metadata_json = serde_json::json!({
        "tool": tool_name,
        "content_sha256": sha256,
        "bytes": content.len(),
        "trust": "untrusted",
        "read_only": true,
    })
    .to_string();
    let input = DocumentIngestInput {
        document_id: Some(document_id.clone()),
        workspace_id: runtime.workspace_id.clone(),
        owner_id: runtime.owner_id.clone(),
        topic_id: runtime.topic_id.clone(),
        task_id: runtime.task_id.clone(),
        source_message_event_id: runtime.source_message_event_id.clone(),
        source_kind: UNTRUSTED_EXTERNAL_SOURCE_KIND.to_string(),
        source_uri: Some(format!("untrusted:{tool_name}")),
        title: Some(format!("Untrusted external content: {tool_name}")),
        content: content.to_string(),
        mime_type: Some("text/plain".to_string()),
        // Most restrictive scope: untrusted external copies are private and are
        // not re-injected as trusted shared context.
        visibility: MemoryVisibility::Private,
        metadata_json: Some(metadata_json),
        // V02-R6: scope the `document.ingested` event to this turn.
        session_key: runtime.session_key.clone(),
        agent_id: runtime.agent_id.clone(),
        persona_id: runtime.persona_id.clone(),
    };
    match runtime.memory.ingest_document(input).await {
        Ok(document) => {
            tracing::info!(
                document_id = %document.document_id,
                tool = %tool_name,
                chunks = document.chunk_count,
                "Persisted read-only untrusted external content copy"
            );
        }
        Err(error) => {
            tracing::warn!(
                document_id = %document_id,
                tool = %tool_name,
                error = %error,
                "Failed to persist untrusted external content copy (non-fatal)"
            );
        }
    }
}

#[path = "turn_tests/document_ingest.rs"]
mod tests;
