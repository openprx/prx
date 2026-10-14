#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;
use crate::agent::context_paging::recall_context_pages;
use crate::agent::turn_tests::paging_test_config;
use crate::memory::principal::{OwnerPrincipal, Role};
use crate::memory::{DocumentIngestInput, Memory, MemoryPrincipal, MemoryVisibility, SqliteMemory};
use crate::runtime::envelope::RuntimeEnvelope;
use std::sync::Arc;
use tempfile::TempDir;

#[test]
fn document_ingest_runtime_inherits_envelope_lineage() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent(tmp.path().to_string_lossy().to_string(), "run-child")
        .with_owner_id("owner:workspace:telegram:alice")
        .with_topic_id("topic-a")
        .with_task_id("task-a")
        .with_source_message_event_id("msg-a")
        .with_agent_id("agent-a")
        .with_persona_id("persona-a")
        .with_owner_isolation();

    let runtime = DocumentIngestRuntime::from_envelope(memory, &envelope);

    assert_eq!(runtime.owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
    assert_eq!(runtime.topic_id.as_deref(), Some("topic-a"));
    assert_eq!(runtime.task_id.as_deref(), Some("task-a"));
    assert_eq!(runtime.source_message_event_id.as_deref(), Some("msg-a"));
    assert_eq!(runtime.session_key.as_deref(), Some("agent:run-child"));
    assert_eq!(runtime.agent_id.as_deref(), Some("agent-a"));
    assert_eq!(runtime.persona_id.as_deref(), Some("persona-a"));
    assert!(runtime.require_owner_match);
    assert!(runtime.memory_principal().require_owner_match);
}

#[tokio::test]
async fn os_paging_recall_disabled_returns_none() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent(workspace, "run-paging-2");
    let runtime = DocumentIngestRuntime::from_envelope(mem, &envelope);

    let mut config = paging_test_config(true);
    config.os_paging.proactive_retrieval = false;
    let recalled = recall_context_pages("anything", &config, Some(&runtime)).await;
    assert!(
        recalled.is_none(),
        "recall must be skipped when proactive_retrieval is off"
    );
}

/// Build a DocumentIngestRuntime over `memory` scoped to the given
/// channel/sender so its owner_id matches a search principal built the same
/// way (needed for Private-visibility document retrieval).
fn untrusted_test_runtime(memory: Arc<dyn Memory>) -> DocumentIngestRuntime {
    let workspace_id = "workspace-untrusted".to_string();
    let owner_id = OwnerPrincipal::new(
        workspace_id.clone(),
        "terminal",
        "local-user",
        "chat-1".to_string(),
        vec![Role::Anonymous],
    )
    .owner_id;
    DocumentIngestRuntime {
        memory,
        workspace_id,
        owner_id: Some(owner_id),
        require_owner_match: false,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        session_key: Some("chat-1".to_string()),
        legacy_session_key: None,
        turn_key: "turn:test-untrusted".to_string(),
        degradation_recorded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        agent_id: None,
        persona_id: None,
        channel: Some("terminal".to_string()),
        sender: Some("local-user".to_string()),
        config_generation_id: Some(0),
        config_source_revision: None,
        visibility: MemoryVisibility::Workspace,
    }
}

fn untrusted_search_principal() -> MemoryPrincipal {
    MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-untrusted".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("chat-1".to_string()),
        channel: Some("terminal".to_string()),
        sender: Some("local-user".to_string()),
        owner_id: None,
        legacy_session_key: None,
    }
}

#[tokio::test]
async fn persist_untrusted_external_copy_ingests_web_fetch_verbatim() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let runtime = untrusted_test_runtime(Arc::clone(&memory));

    let content = "untrusted remote page about vector retrieval anchors";
    persist_untrusted_external_copy(Some(&runtime), "web_fetch", content).await;

    let results = memory
        .search_document_chunks(&untrusted_search_principal(), "vector retrieval anchors", 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1, "untrusted web_fetch copy should be retrievable");
    let chunk = &results[0].chunk;
    assert!(chunk.document_id.starts_with("untrusted:tool-output:web_fetch:"));
    assert!(chunk.content.contains("untrusted remote page"));
}

/// V02-R6: the default-on write path. `persist_untrusted_external_copy`
/// fires on every successful external fetch and writes at `Private`
/// visibility, so its event row must carry the turn's scoping keys instead
/// of the NULLs no principal could ever match.
#[tokio::test]
async fn persist_untrusted_external_copy_stamps_the_runtime_session_key() {
    let tmp = TempDir::new().expect("test: temp dir");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let mut runtime = untrusted_test_runtime(Arc::clone(&memory));
    runtime.agent_id = Some("agent-1".to_string());
    runtime.persona_id = Some("persona-1".to_string());

    persist_untrusted_external_copy(Some(&runtime), "web_fetch", "untrusted remote page body").await;

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-untrusted".to_string(),
        agent_id: Some("agent-1".to_string()),
        persona_id: Some("persona-1".to_string()),
        session_key: Some("chat-1".to_string()),
        channel: Some("terminal".to_string()),
        sender: Some("local-user".to_string()),
        // OWNER-VIS OV-E: a `private` event is readable only by its owner.
        owner_id: runtime.owner_id.clone(),
        legacy_session_key: None,
    };
    let event = memory
        .list_memory_events_recent(&principal, 50)
        .await
        .expect("test: recent memory events")
        .into_iter()
        .find(|event| event.event_type == "document.ingested")
        .expect("test: the untrusted copy's ingest event must reach its own turn");
    assert_eq!(event.session_key.as_deref(), Some("chat-1"));
    assert_eq!(event.agent_id.as_deref(), Some("agent-1"));
    assert_eq!(event.persona_id.as_deref(), Some("persona-1"));
    assert_eq!(event.visibility, MemoryVisibility::Private);
}

#[tokio::test]
async fn persist_untrusted_external_copy_skips_non_external_tool() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let runtime = untrusted_test_runtime(Arc::clone(&memory));

    // `shell` is not an external-content tool — no copy should be persisted.
    persist_untrusted_external_copy(Some(&runtime), "shell", "ls -la output here").await;

    let results = memory
        .search_document_chunks(&untrusted_search_principal(), "output here", 10)
        .await
        .unwrap();
    assert!(results.is_empty(), "non-external tool output must not be ingested");
}

#[tokio::test]
async fn persist_untrusted_external_copy_skips_empty_and_missing_runtime() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let runtime = untrusted_test_runtime(Arc::clone(&memory));

    // Empty content is skipped.
    persist_untrusted_external_copy(Some(&runtime), "file_read", "   ").await;
    // No runtime → no-op (must not panic).
    persist_untrusted_external_copy(None, "http_request", "some body").await;

    let results = memory
        .search_document_chunks(&untrusted_search_principal(), "some body", 10)
        .await
        .unwrap();
    assert!(results.is_empty());
}

/// FIX-P2-02: the search result for an untrusted external copy must carry the
/// `untrusted_external` source_kind so the recall filter can identify it.
#[tokio::test]
async fn search_result_tags_untrusted_external_source_kind() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let runtime = untrusted_test_runtime(Arc::clone(&memory));

    persist_untrusted_external_copy(Some(&runtime), "web_fetch", "untrusted poison context payload").await;

    let results = memory
        .search_document_chunks(&untrusted_search_principal(), "poison context payload", 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].source_kind.as_deref(), Some("untrusted_external"));
}

/// FIX-P2-02: the recall hard-filter removes untrusted external copies but
/// keeps ordinary documents, so attacker-controlled fetched content is never
/// re-injected as trusted context while genuine memory still recalls.
#[tokio::test]
async fn drop_untrusted_external_results_filters_untrusted_only() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let runtime = untrusted_test_runtime(Arc::clone(&memory));

    // 1. An untrusted external copy (web_fetch) — must be filtered out.
    persist_untrusted_external_copy(Some(&runtime), "web_fetch", "shared marker token untrusted half").await;

    // 2. A normal, trusted document with the same query-matching token.
    memory
        .ingest_document(DocumentIngestInput {
            document_id: Some("trusted-doc-1".to_string()),
            workspace_id: "workspace-untrusted".to_string(),
            owner_id: runtime.owner_id.clone(),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "user_note".to_string(),
            source_uri: None,
            title: Some("trusted note".to_string()),
            content: "shared marker token trusted half".to_string(),
            mime_type: Some("text/plain".to_string()),
            visibility: MemoryVisibility::Workspace,
            metadata_json: None,
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .unwrap();

    let mut results = memory
        .search_document_chunks(&untrusted_search_principal(), "shared marker token", 10)
        .await
        .unwrap();
    // Both copies are stored and retrievable at the store level.
    assert_eq!(results.len(), 2, "both trusted and untrusted copies are in the store");

    // After the recall hard-filter, only the trusted document remains.
    drop_untrusted_external_results(&mut results);
    assert_eq!(
        results.len(),
        1,
        "untrusted external copy must be filtered out of recall"
    );
    assert_eq!(results[0].source_kind.as_deref(), Some("user_note"));
    assert!(results[0].chunk.content.contains("trusted half"));
    assert!(!results[0].chunk.content.contains("untrusted half"));
}
