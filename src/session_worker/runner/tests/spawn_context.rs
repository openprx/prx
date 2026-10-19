#![cfg(test)]

use crate::session_worker::protocol::WorkerManifest;
use crate::session_worker::runner::context::{
    with_manifest_spawn_context, worker_lineage_payload, worker_message_event_scope, worker_runtime_envelope,
};

#[tokio::test]
async fn process_mode_restores_spawn_context_for_nested_runs() {
    let manifest = WorkerManifest {
        parent_capability: Some("capability".to_string()),
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "run-child".to_string(),
        task: "noop".to_string(),
        provider_name: "provider".to_string(),
        model: "model".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        config_dir: std::path::PathBuf::from("/tmp/openprx"),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: std::path::PathBuf::from("/tmp/ws"),
        memory_db_path: std::path::PathBuf::from("/tmp/ws/brain.db"),
        memory_workspace_id: Some("/tmp/ws".to_string()),
        memory_strategy: Some("shared_fabric".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(std::path::PathBuf::from("/tmp/ws/memory/brain.db")),
        worker_memory_db_path: Some(std::path::PathBuf::from("/tmp/worker/brain.db")),
        agent_id: Some("agent-a".to_string()),
        persona_id: Some("persona-a".to_string()),
        memory_event_recording: crate::memory::MemoryEventRecording::default(),
        allowed_tools: Vec::new(),
        channel_excluded_tools: Vec::new(),
        system_prompt: None,
        identity_dir: None,
        scope_sender: None,
        scope_channel: None,
        scope_chat_type: None,
        scope_chat_id: None,
        owner_id: Some("owner-a".to_string()),
        owner_isolated: false,
        topic_id: Some("topic-a".to_string()),
        parent_task_id: Some("run-parent".to_string()),
        source_message_event_id: Some("msg-a".to_string()),
        spawn_depth: 1,
        session_scope_key: "signal:group:test".to_string(),
        parent_run_id: Some("run-parent".to_string()),
        compaction_config: None,
    };

    let snapshot = with_manifest_spawn_context(&manifest, None, None, None, None, async {
        crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
            .try_with(|ctx| {
                (
                    ctx.run_id.clone(),
                    ctx.session_scope_key.clone(),
                    ctx.spawn_depth,
                    ctx.owner_id.clone(),
                    ctx.topic_id.clone(),
                    ctx.source_message_event_id.clone(),
                )
            })
            .ok()
    })
    .await;
    assert_eq!(
        snapshot,
        Some((
            "run-child".to_string(),
            "signal:group:test".to_string(),
            1usize,
            Some("owner-a".to_string()),
            Some("topic-a".to_string()),
            Some("msg-a".to_string())
        ))
    );
}

#[test]
fn worker_event_scope_preserves_spawn_lineage_and_owner_isolation() {
    let manifest = WorkerManifest {
        parent_capability: Some("capability".to_string()),
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "run-child".to_string(),
        task: "noop".to_string(),
        provider_name: "provider".to_string(),
        model: "model".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        config_dir: std::path::PathBuf::from("/tmp/openprx"),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: Some(17),
        runtime_config_source_revision: Some("revision-17".to_string()),
        workspace_dir: std::path::PathBuf::from("/tmp/worker"),
        memory_db_path: std::path::PathBuf::from("/tmp/parent/memory/brain.db"),
        memory_workspace_id: Some("/tmp/parent".to_string()),
        memory_strategy: Some("shared_fabric".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(std::path::PathBuf::from("/tmp/parent/memory/brain.db")),
        worker_memory_db_path: Some(std::path::PathBuf::from("/tmp/worker/brain.db")),
        agent_id: Some("agent-a".to_string()),
        persona_id: Some("persona-a".to_string()),
        memory_event_recording: crate::memory::MemoryEventRecording::default(),
        allowed_tools: Vec::new(),
        channel_excluded_tools: Vec::new(),
        system_prompt: None,
        identity_dir: None,
        scope_sender: Some("alice".to_string()),
        scope_channel: Some("telegram".to_string()),
        scope_chat_type: Some("direct".to_string()),
        scope_chat_id: Some("chat-1".to_string()),
        owner_id: Some("owner-a".to_string()),
        owner_isolated: true,
        topic_id: Some("topic-a".to_string()),
        parent_task_id: Some("run-parent".to_string()),
        source_message_event_id: Some("msg-a".to_string()),
        spawn_depth: 1,
        session_scope_key: "telegram:chat-1:alice".to_string(),
        parent_run_id: Some("run-parent".to_string()),
        compaction_config: None,
    };

    let scope = worker_message_event_scope(&manifest);
    assert_eq!(scope.source, "session_worker");
    assert_eq!(scope.channel.as_deref(), Some("telegram"));
    assert_eq!(scope.session_key.as_deref(), Some("telegram:chat-1:alice"));
    assert_eq!(scope.run_id.as_deref(), Some("run-child"));
    assert_eq!(scope.config_generation_id, Some(17));
    assert_eq!(scope.config_source_revision.as_deref(), Some("revision-17"));
    assert_eq!(scope.parent_run_id.as_deref(), Some("run-parent"));
    assert_eq!(scope.owner_id.as_deref(), Some("owner-a"));
    assert_eq!(scope.agent_id.as_deref(), Some("agent-a"));
    assert_eq!(scope.persona_id.as_deref(), Some("persona-a"));
    assert_eq!(scope.sender.as_deref(), Some("alice"));
    assert_eq!(scope.recipient.as_deref(), Some("chat-1"));

    let envelope = worker_runtime_envelope(&manifest);
    let principal = envelope.memory_principal();
    assert_eq!(principal.workspace_id, "/tmp/parent");
    assert_eq!(principal.session_key.as_deref(), Some("telegram:chat-1:alice"));
    assert_eq!(principal.channel.as_deref(), Some("telegram"));
    assert_eq!(principal.sender.as_deref(), Some("alice"));
    assert_eq!(principal.owner_id.as_deref(), Some("owner-a"));
    assert!(principal.require_owner_match);
    assert!(envelope.owner_isolated);

    let write_context = envelope.memory_write_context("direct");
    assert_eq!(write_context.channel.as_deref(), Some("telegram"));
    assert_eq!(write_context.chat_id.as_deref(), Some("chat-1"));
    assert_eq!(write_context.raw_sender.as_deref(), Some("alice"));
    let payload = worker_lineage_payload(&manifest);
    assert_eq!(
        payload.get("owner_id").and_then(serde_json::Value::as_str),
        Some("owner-a")
    );
    assert_eq!(
        payload.get("topic_id").and_then(serde_json::Value::as_str),
        Some("topic-a")
    );
    assert_eq!(
        payload.get("parent_task_id").and_then(serde_json::Value::as_str),
        Some("run-parent")
    );
    assert_eq!(
        payload
            .get("source_message_event_id")
            .and_then(serde_json::Value::as_str),
        Some("msg-a")
    );
}
