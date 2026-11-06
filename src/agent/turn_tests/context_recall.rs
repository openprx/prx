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
fn current_input_memory_key_is_per_run() {
    let key1 = current_input_memory_key("run-a");
    let key2 = current_input_memory_key("run-b");

    assert!(key1.starts_with("user_msg:"));
    assert!(key2.starts_with("user_msg:"));
    assert_ne!(key1, key2);
}

#[tokio::test]
async fn current_input_memory_keys_preserve_multiple_turns() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    let key1 = current_input_memory_key("run-a");
    let key2 = current_input_memory_key("run-b");

    mem.store(&key1, "prx_user prefers Rust", MemoryCategory::Conversation, None)
        .await
        .unwrap();
    mem.store(&key2, "prx_user uses port 45", MemoryCategory::Conversation, None)
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

fn recall_test_scope(owner: &str, workspace: &str) -> RecallScope {
    let mut envelope = crate::runtime::envelope::RuntimeEnvelope::channel(
        workspace,
        "mcp",
        "shared-subject",
        Some("shared-session".into()),
    );
    envelope.owner_id = Some(owner.to_owned());
    envelope.owner_isolated = true;
    RecallScope::from_envelope(
        &envelope,
        &crate::config::MemoryConfig {
            min_relevance_score: 0.0,
            ..Default::default()
        },
        "private",
    )
}

async fn seed_recall_memory(mem: &SqliteMemory, scope: &RecallScope, key: &str, content: &str) {
    mem.store_with_context(key, content, MemoryCategory::Conversation, None, Some(&scope.semantic))
        .await
        .unwrap();
}

async fn seed_recall_document(mem: &SqliteMemory, owner: &str, workspace: &str, content: &str) {
    mem.ingest_document(DocumentIngestInput {
        document_id: None,
        workspace_id: workspace.into(),
        owner_id: Some(owner.into()),
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        source_kind: "conversation_page".into(),
        source_uri: None,
        title: None,
        content: content.into(),
        mime_type: Some("text/plain".into()),
        visibility: MemoryVisibility::Private,
        metadata_json: None,
        session_key: None,
        agent_id: None,
        persona_id: None,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn scoped_recall_without_decision_context_returns_owner_bounded_entries() {
    let dir = TempDir::new().unwrap();
    let mem = SqliteMemory::new(dir.path()).unwrap();
    let scope = recall_test_scope("owner-a", "workspace-a");
    seed_recall_memory(&mem, &scope, "project-note", "recallkeyword owner fact").await;
    seed_recall_document(&mem, "owner-a", "workspace-a", "recallkeyword document-only fact").await;
    let recalled = scoped_recall(&mem, &scope, "recallkeyword", RECALL_CONTEXT_BUDGET).await;
    assert!(recalled.preamble.contains("owner fact"), "{}", recalled.preamble);
    assert_eq!(recalled.ids.len(), 1);
    assert!(recalled.preamble.contains("document-only fact"));
    let semantic = recalled.preamble.get(recalled.semantic_range).unwrap();
    assert!(semantic.contains("owner fact"));
    assert!(!semantic.contains("document-only fact"));
}

#[tokio::test]
async fn scoped_recall_excludes_foreign_owner_and_workspace() {
    let dir = TempDir::new().unwrap();
    let mem = SqliteMemory::new(dir.path()).unwrap();
    let scope = recall_test_scope("owner-a", "workspace-a");
    seed_recall_memory(&mem, &scope, "own-note", "recallkeyword own fact").await;
    seed_recall_memory(
        &mem,
        &recall_test_scope("owner-b", "workspace-a"),
        "foreign-note",
        "recallkeyword foreign owner",
    )
    .await;
    seed_recall_document(&mem, "owner-a", "workspace-a", "recallkeyword own document").await;
    seed_recall_document(&mem, "owner-b", "workspace-a", "recallkeyword foreign document").await;
    seed_recall_document(&mem, "owner-a", "workspace-b", "recallkeyword foreign workspace").await;
    let recalled = scoped_recall(&mem, &scope, "recallkeyword", RECALL_CONTEXT_BUDGET).await;
    assert!(recalled.preamble.contains("own fact"));
    assert!(recalled.preamble.contains("own document"), "{}", recalled.preamble);
    assert!(!recalled.preamble.contains("foreign"), "{}", recalled.preamble);
}

#[tokio::test]
async fn owner_isolated_origin_recall_is_private() {
    let dir = TempDir::new().unwrap();
    let mem = SqliteMemory::new(dir.path()).unwrap();
    let scope_a = recall_test_scope("issuer-a", "workspace-a");
    let mut scope_b = recall_test_scope("issuer-a", "workspace-a");
    scope_b.semantic.channel = Some("other-origin".into());
    scope_b.principal.channel = Some("other-origin".into());
    seed_recall_memory(&mem, &scope_a, "issuer-a-note", "recallkeyword issuer-a private").await;
    seed_recall_memory(&mem, &scope_b, "issuer-b-note", "recallkeyword issuer-b private").await;
    let recalled = scoped_recall(&mem, &scope_b, "recallkeyword", RECALL_CONTEXT_BUDGET).await;
    assert!(recalled.preamble.contains("issuer-b private"));
    assert!(!recalled.preamble.contains("issuer-a private"));
}

#[tokio::test]
async fn recall_respects_disabled_user_message_recording() {
    let dir = TempDir::new().unwrap();
    let mem = SqliteMemory::new(dir.path()).unwrap();
    let mut scope = recall_test_scope("owner-a", "workspace-a");
    seed_recall_memory(&mem, &scope, "fact-note", "recallkeyword semantic fact").await;
    seed_recall_document(&mem, "owner-a", "workspace-a", "recallkeyword document fact").await;
    mem.append_conversation_turn(
        scope.principal.session_key.as_deref().unwrap(),
        "mcp",
        "shared-subject",
        "user",
        "recallkeyword historical user text",
        None,
        None,
        Some("owner-a"),
    )
    .await
    .unwrap();
    let enabled = scoped_recall(&mem, &scope, "recallkeyword", RECALL_CONTEXT_BUDGET).await;
    assert!(
        enabled.preamble.contains("historical user text"),
        "{}",
        enabled.preamble
    );
    scope.record_user_messages = false;
    let recalled = scoped_recall(&mem, &scope, "recallkeyword", RECALL_CONTEXT_BUDGET).await;
    assert!(!recalled.preamble.contains("historical user text"));
    assert!(recalled.preamble.contains("semantic fact"));
    assert!(recalled.preamble.contains("document fact"));
    assert_ne!(recalled.ids, Vec::<String>::new());
}

#[tokio::test]
async fn recall_is_bounded_by_context_budget() {
    let dir = TempDir::new().unwrap();
    let mem = SqliteMemory::new(dir.path()).unwrap();
    let scope = recall_test_scope("owner-a", "workspace-a");
    seed_recall_memory(
        &mem,
        &scope,
        "large-note",
        &format!("recallkeyword {}", "é".repeat(3000)),
    )
    .await;
    let full = scoped_recall(&mem, &scope, "recallkeyword", RECALL_CONTEXT_BUDGET).await;
    assert!(full.preamble.len() > 66);
    let bounded = scoped_recall(&mem, &scope, "recallkeyword", 66).await;
    assert_ne!(bounded.preamble, "");
    assert!(!full.preamble.is_char_boundary(66));
    assert_eq!(bounded.preamble.len(), 65);
    assert_eq!(bounded.ids, Vec::<String>::new());
    let empty = scoped_recall(&mem, &scope, "recallkeyword", 0).await;
    assert_eq!(empty.preamble, "");
}

#[test]
fn scoped_recall_query_projects_images_and_bounds_utf8() {
    let input = format!("{}[IMAGE:/private/image.png]{}", "é".repeat(510), "é".repeat(40));
    let projected = crate::multimodal::project_image_markers_for_routing(&input);
    let query = recall_query(&input);
    assert_eq!(query, projected[..projected.floor_char_boundary(1024)]);
    assert!(query.len() <= 1024);
    assert!(!query.contains("/private/image.png"));
}

#[tokio::test]
async fn scoped_recall_excludes_stale_and_future_history() {
    let dir = TempDir::new().unwrap();
    let mem = SqliteMemory::new(dir.path()).unwrap();
    let scope = recall_test_scope("owner-a", "workspace-a");
    for (text, timestamp) in [
        ("stale history", "2000-01-01T00:00:00Z"),
        ("future history", "2099-01-01T00:00:00Z"),
    ] {
        mem.append_conversation_turn(
            scope.principal.session_key.as_deref().unwrap(),
            "mcp",
            "shared-subject",
            "user",
            text,
            Some(timestamp),
            None,
            Some("owner-a"),
        )
        .await
        .unwrap();
    }
    mem.append_conversation_turn(
        scope.principal.session_key.as_deref().unwrap(),
        "mcp",
        "shared-subject",
        "assistant",
        "fresh history",
        None,
        None,
        Some("owner-a"),
    )
    .await
    .unwrap();
    let recalled = scoped_recall(&mem, &scope, "history", RECALL_CONTEXT_BUDGET).await;
    assert!(recalled.preamble.contains("fresh history"));
    assert!(!recalled.preamble.contains("stale history"));
    assert!(!recalled.preamble.contains("future history"));
    assert!(!fresh_timestamp("invalid"));
}

#[tokio::test]
async fn scoped_recall_keyword_fallback_excludes_current_turn_and_foreign_owner() {
    let dir = TempDir::new().unwrap();
    let mem = SqliteMemory::new(dir.path()).unwrap();
    let mut scope = recall_test_scope("owner-a", "workspace-a");
    scope.min_relevance_score = crate::config::MemoryConfig::default().min_relevance_score;
    scope.current_input_key = Some("user_msg:prx_current_run".into());
    seed_recall_memory(
        &mem,
        &scope,
        scope.current_input_key.as_deref().unwrap(),
        "fallbackkeyword current turn",
    )
    .await;
    seed_recall_memory(
        &mem,
        &recall_test_scope("owner-b", "workspace-a"),
        "foreign-note",
        "fallbackkeyword foreign owner",
    )
    .await;
    let empty = scoped_recall(&mem, &scope, "fallbackkeyword", RECALL_CONTEXT_BUDGET).await;
    assert_eq!(empty.ids, Vec::<String>::new());
    seed_recall_memory(&mem, &scope, "prior-note", "fallbackkeyword prior fact").await;
    let scored = mem
        .recall_with_context("fallbackkeyword", 5, None, Some(&scope.semantic))
        .await
        .unwrap();
    assert!(!scored.is_empty());
    assert!(
        scored
            .iter()
            .all(|entry| entry.score.is_some_and(|score| score < scope.min_relevance_score))
    );
    let recalled = scoped_recall(&mem, &scope, "fallbackkeyword", RECALL_CONTEXT_BUDGET).await;
    assert_eq!(recalled.ids.len(), 1);
    assert!(recalled.preamble.contains("prior fact"));
    assert!(!recalled.preamble.contains("current turn"));
    assert!(!recalled.preamble.contains("foreign owner"));
}

#[tokio::test]
async fn cli_live_input_is_not_recalled_or_rewarded() {
    let envelope = crate::runtime::envelope::RuntimeEnvelope::agent("prx_workspace", "prx_run");
    live_input_is_not_recalled_or_rewarded(envelope).await;
}

#[tokio::test]
async fn chat_live_input_is_not_recalled_or_rewarded() {
    let envelope =
        crate::runtime::envelope::RuntimeEnvelope::chat("prx_workspace", "prx_session").with_run_id("prx_run");
    live_input_is_not_recalled_or_rewarded(envelope).await;
}

async fn live_input_is_not_recalled_or_rewarded(mut envelope: crate::runtime::envelope::RuntimeEnvelope) {
    envelope.owner_id = Some("prx_owner".into());
    envelope.owner_isolated = true;
    let dir = TempDir::new().unwrap();
    let mem = SqliteMemory::new(dir.path()).unwrap();
    let mut scope = RecallScope::from_envelope(&envelope, &crate::config::MemoryConfig::default(), "private");
    let key = current_input_memory_key(envelope.run_id.as_deref().unwrap());
    // CLI single-shot, interactive CLI and chat save with this shared identity
    // before constructing the prompt's authenticated recall scope.
    seed_recall_memory(&mem, &scope, &key, "prxquery live input").await;
    seed_recall_memory(&mem, &scope, "prior_fact", "prxquery prior fact").await;
    for threshold in [0.0, 1.0] {
        scope.min_relevance_score = threshold;
        let recalled = scoped_recall(&mem, &scope, "prxquery", RECALL_CONTEXT_BUDGET).await;
        assert!(recalled.preamble.contains("prior fact"));
        assert!(!recalled.preamble.contains(&key));
        assert!(!recalled.preamble.contains("live input"));
        let current = mem.get(&key).await.unwrap().unwrap();
        assert!(!recalled.ids.contains(&current.id));
        let before = current.useful_count;
        increment_recalled_useful_counts(&mem, &recalled.ids).await;
        assert_eq!(mem.get(&key).await.unwrap().unwrap().useful_count, before);
    }
}

struct EmptyKeywordMemory;

#[async_trait::async_trait]
impl Memory for EmptyKeywordMemory {
    fn name(&self) -> &str {
        "empty-keyword-memory"
    }
    async fn store(&self, _: &str, _: &str, _: MemoryCategory, _: Option<&str>) -> anyhow::Result<()> {
        panic!("unexpected mutation of empty keyword memory")
    }
    async fn recall(&self, _: &str, _: usize, _: Option<&str>) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn get(&self, _: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        Ok(None)
    }
    async fn list(
        &self,
        _: Option<&MemoryCategory>,
        _: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn forget(&self, _: &str) -> anyhow::Result<bool> {
        panic!("unexpected mutation of empty keyword memory")
    }
    async fn count(&self) -> anyhow::Result<usize> {
        panic!("empty check must be source-scoped")
    }
    async fn health_check(&self) -> bool {
        true
    }
    async fn count_rehearsal_hits(
        &self,
        _: &str,
        limit: u8,
        context: &MemoryWriteContext,
        _: std::time::Instant,
    ) -> anyhow::Result<u32> {
        assert_eq!(limit, 5);
        assert_eq!(context.owner_id.as_deref(), Some("owner-a"));
        assert!(context.require_owner_match);
        Ok(0)
    }
    async fn retrieve_decision_memory(
        &self,
        _: &str,
        _: u8,
        _: &MemoryWriteContext,
        _: Option<&str>,
        _: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionMemoryHit>> {
        panic!("empty source-scoped store must skip content fallback")
    }
}

#[tokio::test]
async fn scoped_recall_empty_keyword_store_skips_content_fallback() {
    let scope = recall_test_scope("owner-a", "workspace-a");
    let recalled = scoped_recall(&EmptyKeywordMemory, &scope, "emptykeyword", RECALL_CONTEXT_BUDGET).await;
    assert_eq!(recalled.preamble, "");
    assert_eq!(recalled.ids, Vec::<String>::new());
}

#[tokio::test]
async fn scoped_recall_excludes_source_event_with_arbitrary_save_key() {
    let dir = TempDir::new().unwrap();
    let memory = SqliteMemory::new(dir.path()).unwrap();
    let mut scope = recall_test_scope("prx_owner", "prx_workspace");
    scope.current_input_event = Some("prx_live_event".into());
    for (key, event, content) in [
        ("arbitrary-save-key", "prx_live_event", "prxquery current input"),
        ("prior-save-key", "prx_prior_event", "prxquery prior fact"),
    ] {
        memory
            .store_with_context_and_metadata(
                key,
                content,
                MemoryCategory::Conversation,
                None,
                Some(&scope.semantic),
                crate::memory::MemoryStoreMetadata {
                    source_event_id: Some(event.into()),
                    workspace_id: Some("prx_workspace".into()),
                    owner_id: Some("prx_owner".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    for relevance in [0.0, 1.1] {
        scope.min_relevance_score = relevance;
        let recalled = scoped_recall(&memory, &scope, "prxquery", RECALL_CONTEXT_BUDGET).await;
        assert!(recalled.preamble.contains("prior fact"));
        assert!(!recalled.preamble.contains("current input"));
        assert_eq!(recalled.ids.len(), 1);
    }
}
