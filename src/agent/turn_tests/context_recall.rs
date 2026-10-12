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
use crate::memory::{
    ConversationTurn, DocumentIngestInput, DocumentSearchResult, MemoryCategory, MemoryPrincipal, MemoryVisibility,
    MessageEvent, MessageEventInput, SqliteMemory,
};
use crate::providers::ChatMessage;
use tempfile::TempDir;

/// The same incident one layer up: the poisoned text is not typed by the
/// user, it is *assembled* from the shared event log into the live user
/// turn. Quoted content must arrive with its media markers already defanged.
#[tokio::test]
async fn quoted_events_reach_the_prompt_without_live_media_markers() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    let poisoned = "Media markers (`[IMAGE:...]` etc.) are not mapped; \
                        an older shot lived at [IMAGE:/gone/artifacts/deleted.jpg] \
                        and a clip at [VOICE:/gone/artifacts/deleted.m4a]";
    mem.append_message_event(MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        source: "channel".into(),
        channel: Some("wacli".to_string()),
        session_key: Some("wacli:chat-1".to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: Some("prx".to_string()),
        recipient: None,
        role: "assistant".to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: poisoned.to_string(),
        raw_payload_json: None,
        visibility: MemoryVisibility::Workspace,
    })
    .await
    .unwrap();

    let context = build_context_with_shared_events_and_scope(
        &mem,
        MemoryPrincipal {
            require_owner_match: false,
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("wacli:chat-1".to_string()),
            channel: Some("wacli".to_string()),
            sender: Some("prx".to_string()),
            owner_id: None,
            legacy_session_key: None,
        },
        "does wacli support sending files?",
        0.0,
        None,
    )
    .await;

    assert!(
        context.preamble.contains("deleted.jpg"),
        "the quoted reference must stay readable: {}",
        context.preamble
    );
    for kind in ["IMAGE", "VOICE", "DOCUMENT", "VIDEO", "AUDIO"] {
        assert!(
            !context.preamble.contains(&format!("[{kind}:")),
            "quoted {kind} marker leaked into the prompt preamble: {}",
            context.preamble
        );
    }

    // The preamble is prepended to the live user turn; that composed message
    // is what the provider layer normalizes.
    let composed = vec![ChatMessage::user(format!(
        "{}does wacli support sending files?",
        context.preamble
    ))];
    let artifacts = crate::media::MediaArtifactOwner::for_workspace(tmp.path());
    let prepared = crate::multimodal::prepare_messages_for_provider(
        &composed,
        &crate::config::MultimodalConfig::default(),
        artifacts.as_ref(),
    )
    .await
    .expect("a preamble built from history must never fail the provider call");
    assert!(!prepared.contains_images);
}

#[test]
fn autosave_memory_key_has_prefix_and_uniqueness() {
    let key1 = autosave_memory_key("user_msg");
    let key2 = autosave_memory_key("user_msg");

    assert!(key1.starts_with("user_msg_"));
    assert!(key2.starts_with("user_msg_"));
    assert_ne!(key1, key2);
}

#[tokio::test]
async fn autosave_memory_keys_preserve_multiple_turns() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    let key1 = autosave_memory_key("user_msg");
    let key2 = autosave_memory_key("user_msg");

    mem.store(&key1, "I'm Paul", MemoryCategory::Conversation, None)
        .await
        .unwrap();
    mem.store(&key2, "I'm 45", MemoryCategory::Conversation, None)
        .await
        .unwrap();

    assert_eq!(mem.count().await.unwrap(), 2);

    let recalled = mem.recall("45", 5, None).await.unwrap();
    assert!(recalled.iter().any(|entry| entry.content.contains("45")));
}

#[tokio::test]
async fn build_context_ignores_legacy_assistant_autosave_entries() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    mem.store(
        "assistant_resp_poisoned",
        "User suffered a fabricated event",
        MemoryCategory::Daily,
        None,
    )
    .await
    .unwrap();
    mem.store(
        "user_msg_real",
        "User asked for concise status updates",
        MemoryCategory::Conversation,
        None,
    )
    .await
    .unwrap();

    let context = build_context_with_scope(&mem, "status updates", 0.0, None).await;
    assert!(context.preamble.contains("user_msg_real"));
    assert!(!context.preamble.contains("assistant_resp_poisoned"));
    assert!(!context.preamble.contains("fabricated event"));
    assert_eq!(context.ids.len(), 1);
}

#[tokio::test]
async fn build_context_with_shared_events_includes_external_visible_events() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    mem.append_message_event(MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: "workspace-a".to_string(),
        // OWNER-VIS OV-D: the feed carries the reader's own owner's events.
        owner_id: Some("owner-a".to_string()),
        source: "channel".into(),
        channel: Some("telegram".to_string()),
        session_key: Some("telegram:chat-1".to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: Some("tg-user".to_string()),
        recipient: None,
        role: "user".to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: "telegram status update".to_string(),
        raw_payload_json: None,
        visibility: MemoryVisibility::Workspace,
    })
    .await
    .unwrap();
    mem.append_message_event(MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        source: "chat".into(),
        channel: Some("terminal".to_string()),
        session_key: Some("chat:current".to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: Some("local-user".to_string()),
        recipient: None,
        role: "user".to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: "what changed".to_string(),
        raw_payload_json: None,
        visibility: MemoryVisibility::Workspace,
    })
    .await
    .unwrap();

    let context = build_context_with_shared_events_and_scope(
        &mem,
        MemoryPrincipal {
            require_owner_match: false,
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("chat:current".to_string()),
            channel: Some("terminal".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: Some("owner-a".to_string()),
            legacy_session_key: None,
        },
        "what changed",
        0.0,
        None,
    )
    .await;

    assert!(context.preamble.contains("[Recent shared workspace events]"));
    assert!(context.preamble.contains("telegram status update"));
    assert!(!context.preamble.contains("[Current session context]"));
}

#[tokio::test]
async fn build_agent_context_returns_structured_bundle() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    mem.store(
        "user_msg_pref",
        "User prefers terse operational summaries",
        MemoryCategory::Conversation,
        None,
    )
    .await
    .unwrap();
    mem.append_message_event(MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        source: "chat".into(),
        channel: Some("terminal".to_string()),
        session_key: Some("chat:current".to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: Some("local-user".to_string()),
        recipient: None,
        role: "user".to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: "previous current-session request".to_string(),
        raw_payload_json: None,
        visibility: MemoryVisibility::Workspace,
    })
    .await
    .unwrap();
    mem.append_message_event(MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        source: "chat".into(),
        channel: Some("terminal".to_string()),
        session_key: Some("chat:current".to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: None,
        recipient: None,
        role: "assistant".to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: "previous current-session answer".to_string(),
        raw_payload_json: None,
        visibility: MemoryVisibility::Workspace,
    })
    .await
    .unwrap();
    mem.append_conversation_turn(
        "chat:current",
        "terminal",
        "local-user",
        "user",
        "previous current-session request",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    mem.append_conversation_turn(
        "chat:current",
        "terminal",
        "local-user",
        "assistant",
        "previous current-session answer",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    mem.append_message_event(MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: "workspace-a".to_string(),
        // OWNER-VIS OV-D: the feed carries the reader's own owner's events.
        owner_id: Some("owner-a".to_string()),
        source: "gateway".into(),
        channel: Some("webhook".to_string()),
        session_key: Some("gateway:webhook:client-a".to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: Some("client-a".to_string()),
        recipient: None,
        role: "user".to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: "gateway shared status".to_string(),
        raw_payload_json: None,
        visibility: MemoryVisibility::Workspace,
    })
    .await
    .unwrap();

    let bundle = build_agent_context_with_semantic_scope(
        &mem,
        MemoryPrincipal {
            require_owner_match: false,
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("chat:current".to_string()),
            channel: Some("terminal".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: Some("owner-a".to_string()),
            legacy_session_key: None,
        },
        "operational summaries",
        0.0,
        None,
    )
    .await;

    assert!(bundle.current_session_context.contains("[Current session context]"));
    assert!(
        bundle
            .current_session_context
            .contains("previous current-session request")
    );
    assert!(bundle.recent_shared_context.contains("gateway shared status"));
    assert!(bundle.semantic_memory_context.contains("user_msg_pref"));
    assert!(!bundle.recalled_memory_ids.is_empty(), "expected a non-empty value");
    assert!(bundle.preamble().contains("[Recent shared workspace events]"));
    assert!(bundle.preamble().contains("[Memory context]"));
}

// D4 C3: agent keeps its per-turn run boundary on the write side; recall
// read-merges only when an explicit legacy key is supplied, otherwise stays
// isolated per run (single-key degradation).
#[tokio::test]
async fn d4_agent_recall_keeps_run_boundary_and_read_merges_only_with_legacy_key() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    // Two unrelated agent runs, each with its own per-turn run boundary.
    mem.append_conversation_turn(
        "agent:run-A",
        "cli",
        "local-user",
        "user",
        "history from run A",
        Some("2026-03-05T00:00:00Z"),
        None,
        None,
    )
    .await
    .unwrap();
    mem.append_conversation_turn(
        "agent:run-B",
        "cli",
        "local-user",
        "user",
        "history from run B",
        Some("2026-03-05T00:00:01Z"),
        None,
        None,
    )
    .await
    .unwrap();

    // Run B with NO legacy key: must see only its own turn (run boundary
    // preserved, unrelated run A NOT pulled in).
    let isolated = build_agent_context_with_semantic_scope(
        &mem,
        MemoryPrincipal {
            require_owner_match: false,
            workspace_id: tmp.path().to_string_lossy().to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("agent:run-B".to_string()),
            channel: Some("cli".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: None,
            legacy_session_key: None,
        },
        "anything",
        0.0,
        None,
    )
    .await;
    assert!(isolated.current_session_context.contains("history from run B"));
    assert!(
        !isolated.current_session_context.contains("history from run A"),
        "run boundary must isolate unrelated runs: {}",
        isolated.current_session_context
    );

    // Resume run B but explicitly read-merge legacy run A history: both visible.
    let merged = build_agent_context_with_semantic_scope(
        &mem,
        MemoryPrincipal {
            require_owner_match: false,
            workspace_id: tmp.path().to_string_lossy().to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("agent:run-B".to_string()),
            channel: Some("cli".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: None,
            legacy_session_key: Some("agent:run-A".to_string()),
        },
        "anything",
        0.0,
        None,
    )
    .await;
    assert!(merged.current_session_context.contains("history from run B"));
    assert!(
        merged.current_session_context.contains("history from run A"),
        "explicit legacy key must read-merge: {}",
        merged.current_session_context
    );
}

#[tokio::test]
async fn build_agent_context_includes_document_evidence_with_source_ids() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    mem.ingest_document(DocumentIngestInput {
        document_id: Some("doc-context-1".to_string()),
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        topic_id: Some("topic-context".to_string()),
        task_id: Some("task-context".to_string()),
        source_message_event_id: Some("msg-context".to_string()),
        source_kind: "test".to_string(),
        source_uri: Some("test://doc-context-1".to_string()),
        title: Some("Context Evidence".to_string()),
        content: "retrieval context document evidence with stable source anchors".to_string(),
        mime_type: Some("text/plain".to_string()),
        visibility: MemoryVisibility::Workspace,
        metadata_json: None,
        session_key: None,
        agent_id: None,
        persona_id: None,
    })
    .await
    .unwrap();

    let bundle = build_agent_context_with_semantic_scope(
        &mem,
        MemoryPrincipal {
            require_owner_match: false,
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("chat:current".to_string()),
            channel: Some("terminal".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: None,
            legacy_session_key: None,
        },
        "stable source anchors",
        0.0,
        None,
    )
    .await;

    assert!(bundle.document_context.contains("[Document evidence]"));
    assert!(bundle.document_context.contains("document_id=doc-context-1"));
    assert!(bundle.document_context.contains("chunk_id=doc-context-1:chunk:0"));
    assert!(bundle.document_context.contains("source_anchor=doc-context-1#chunk-0"));
    assert!(bundle.preamble().contains("[Document evidence]"));

    let conn = rusqlite::Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
    let (selected_count, selected_json): (i64, String) = conn
        .query_row(
            "SELECT selected_count, selected_json
                 FROM retrieval_traces
                 WHERE workspace_id = 'workspace-a'
                   AND source = 'agent_context.document_evidence'
                 ORDER BY id DESC
                 LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(selected_count, 1);
    assert!(selected_json.contains("doc-context-1:chunk:0"));
    assert!(selected_json.contains("doc-context-1#chunk-0"));
}

#[tokio::test]
async fn build_agent_context_keeps_current_session_when_shared_events_are_noisy() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    mem.append_message_event(MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        source: "chat".into(),
        channel: Some("terminal".to_string()),
        session_key: Some("chat:current".to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: Some("local-user".to_string()),
        recipient: None,
        role: "user".to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: "important current-session fact".to_string(),
        raw_payload_json: None,
        visibility: MemoryVisibility::Workspace,
    })
    .await
    .unwrap();
    for index in 0..40 {
        mem.append_message_event(MessageEventInput {
            event_id: None,
            idempotency_key: None,
            workspace_id: "workspace-a".to_string(),
            owner_id: None,
            source: "gateway".into(),
            channel: Some("webhook".to_string()),
            session_key: Some(format!("gateway:external:{index}")),
            parent_session_key: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: Some(format!("client-{index}")),
            recipient: None,
            role: "user".to_string(),
            event_type: "message.created".to_string(),
            subject: None,
            goal_id: None,
            causation_event_id: None,
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: Some(0),
            config_source_revision: None,
            content: format!("external noisy event {index}"),
            raw_payload_json: None,
            visibility: MemoryVisibility::Workspace,
        })
        .await
        .unwrap();
    }

    let bundle = build_agent_context_with_semantic_scope(
        &mem,
        MemoryPrincipal {
            require_owner_match: false,
            workspace_id: "workspace-a".to_string(),
            agent_id: None,
            persona_id: None,
            session_key: Some("chat:current".to_string()),
            channel: Some("terminal".to_string()),
            sender: Some("local-user".to_string()),
            owner_id: None,
            legacy_session_key: None,
        },
        "what is current",
        0.0,
        None,
    )
    .await;

    assert!(
        bundle
            .current_session_context
            .contains("important current-session fact")
    );
}

/// Minimal `MessageEvent` builder for preamble classification tests: only the
/// fields the two preamble builders inspect (`session_key`, `role`, `content`)
/// carry meaning; the rest are filled with neutral defaults.
fn preamble_event(session_key: Option<&str>, role: &str, content: &str) -> MessageEvent {
    MessageEvent {
        id: 0,
        event_id: format!("evt-{role}-{content}"),
        idempotency_key: None,
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        source: "chat".into(),
        channel: Some("terminal".to_string()),
        session_key: session_key.map(str::to_string),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: Some("local-user".to_string()),
        recipient: None,
        role: role.to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: content.to_string(),
        content_hash: None,
        raw_payload_json: None,
        visibility: MemoryVisibility::Workspace,
        created_at: "2026-03-05T00:00:00Z".to_string(),
        updated_at: "2026-03-05T00:00:00Z".to_string(),
    }
}

#[test]
fn preamble_builders_read_merge_legacy_current_session_key() {
    // D4 review: the prompt assembly layer must classify "current session"
    // by the full candidate key set (canonical + legacy), matching the
    // read-merge already done in the memory store layer.
    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("chat:canonical".to_string()),
        channel: Some("terminal".to_string()),
        sender: Some("local-user".to_string()),
        owner_id: None,
        legacy_session_key: Some("chat:legacy".to_string()),
    };

    let events = vec![
        // Canonical current-session event.
        preamble_event(Some("chat:canonical"), "user", "canonical current fact"),
        // Legacy current-session event (pre-cutover key).
        preamble_event(Some("chat:legacy"), "assistant", "legacy current fact"),
        // Genuinely cross-session shared event (neither candidate key).
        preamble_event(Some("gateway:external:1"), "user", "external shared fact"),
    ];

    let current = build_current_session_events_preamble(&principal, &events, "what is current");
    // (1) Both canonical AND legacy current-session events land in the
    // current-session preamble.
    assert!(
        current.contains("canonical current fact"),
        "canonical current-session event missing from current preamble: {current}"
    );
    assert!(
        current.contains("legacy current fact"),
        "legacy current-session event must read-merge into current preamble: {current}"
    );
    // The cross-session event must NOT be treated as current session.
    assert!(
        !current.contains("external shared fact"),
        "cross-session event must not enter current preamble: {current}"
    );

    let shared = build_shared_events_preamble(&principal, &events);
    // (2) Only the genuinely cross-session event reaches the shared preamble.
    assert!(
        shared.contains("external shared fact"),
        "cross-session event must appear in shared preamble: {shared}"
    );
    // (3) The legacy current-session event must NOT leak into shared events.
    assert!(
        !shared.contains("legacy current fact"),
        "legacy current-session event must not leak into shared preamble: {shared}"
    );
    assert!(
        !shared.contains("canonical current fact"),
        "canonical current-session event must not leak into shared preamble: {shared}"
    );
}

/// The 150 persisted assistant turns that started the fabrication are still
/// in the message fabric, and the session preamble quotes them into every
/// prompt. A marker that reaches the model that way is the same claim about
/// work that never happened as one the model just wrote, so it must not
/// survive the quoting — in either preamble, and in either grain (the
/// preamble flattens content to one line, which turns a whole marker line
/// into an in-line fragment on the way in).
///
/// MUTATION GUARD: drop the strip from `quoted_event_content` and both
/// assistant assertions fail with `[Used tools:` in the preamble.
#[test]
fn a_persisted_fabricated_marker_never_reaches_the_prompt_through_a_preamble() {
    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("chat:canonical".to_string()),
        channel: Some("terminal".to_string()),
        sender: Some("local-user".to_string()),
        owner_id: None,
        legacy_session_key: None,
    };
    let events = vec![
        preamble_event(
            Some("chat:canonical"),
            "assistant",
            "Root is at 41%.\n[Used tools: shell, session_status]",
        ),
        preamble_event(
            Some("chat:canonical"),
            "assistant",
            "Disk looks fine. [Used tools: shell] Nothing else to report.",
        ),
        preamble_event(Some("chat:canonical"), "user", "stop writing [Used tools: shell] at me"),
        preamble_event(Some("gateway:external:1"), "assistant", "Deployed. [Used tools: shell]"),
    ];

    let current = build_current_session_events_preamble(&principal, &events, "and the logs?");
    let assistant_lines = |preamble: &str| {
        preamble
            .lines()
            .filter(|line| line.starts_with("- [assistant"))
            .map(str::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !assistant_lines(&current).contains("[Used tools:"),
        "a persisted fabricated marker must not reach the prompt through the session preamble: {current}"
    );
    assert!(
        current.contains("Root is at 41%.") && current.contains("Nothing else to report."),
        "only the marker is dropped; the answer the model really gave stays: {current}"
    );
    assert!(
        current.contains("stop writing [Used tools: shell] at me"),
        "an operator quoting the marker back is real conversation content: {current}"
    );

    let shared = build_shared_events_preamble(&principal, &events);
    assert!(
        !shared.contains("[Used tools:"),
        "the shared-events preamble replays other sessions' assistant turns too: {shared}"
    );
    assert!(
        shared.contains("Deployed."),
        "the content itself must survive: {shared}"
    );
}

/// The same guarantee for the other two quoting surfaces: a turn replayed
/// from `conversation_turns`, and a document chunk (an evicted context page
/// is a verbatim transcript of earlier assistant turns).
#[test]
fn quoted_turns_and_document_chunks_drop_fabricated_markers_too() {
    let turn = |role: &str, content: &str| ConversationTurn {
        id: 0,
        session_key: "chat:canonical".to_string(),
        role: role.to_string(),
        content: content.to_string(),
        timestamp: "2026-03-05T00:00:00Z".to_string(),
        message_id: None,
    };
    let turns = vec![
        turn("assistant", "Cleaned up. [Used tools: shell, file_write]"),
        turn("user", "why do you keep writing [Used tools: shell]"),
    ];
    let preamble = build_current_session_preamble(&turns, "what now?");
    assert!(
        preamble.contains("Cleaned up.") && !preamble.contains("[Used tools: shell, file_write]"),
        "a replayed assistant turn must lose its marker: {preamble}"
    );
    assert!(
        preamble.contains("why do you keep writing [Used tools: shell]"),
        "a user turn is never rewritten: {preamble}"
    );

    let chunk = DocumentSearchResult {
        chunk: crate::memory::traits::DocumentChunkRecord {
            id: 1,
            chunk_id: "chunk-1".to_string(),
            document_id: "context-page:session:1".to_string(),
            workspace_id: "workspace-a".to_string(),
            owner_id: None,
            topic_id: None,
            task_id: None,
            chunk_index: 0,
            heading: None,
            content: "assistant: Rebuilt the index.\n[Used tools: shell]".to_string(),
            content_sha256: String::new(),
            source_anchor: "page-1".to_string(),
            token_estimate: 8,
            created_at: "2026-03-05T00:00:00Z".to_string(),
        },
        score: 1.0,
        source_kind: Some("context_page".to_string()),
    };
    let line = format_document_result_line(&chunk);
    assert!(
        line.contains("Rebuilt the index.") && !line.contains("[Used tools:"),
        "an evicted context page quotes earlier assistant turns verbatim: {line}"
    );
}

#[test]
fn constants_bounds_are_compile_time_checked() {
    // Bounds are enforced by the const assertions above.
}
