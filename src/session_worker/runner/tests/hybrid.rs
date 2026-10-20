#![cfg(test)]

use crate::config::Config;
use crate::memory::{Memory, MemoryFabric, MessageEventScope};
use crate::security::SecurityPolicy;
use crate::session_worker::protocol::{WorkerManifest, WorkerResult};
use crate::session_worker::runner::context::{load_worker_shared_context, record_hybrid_worker_draft_if_needed};
use std::sync::Arc;

#[tokio::test]
async fn hybrid_worker_shared_context_reads_parent_fabric() {
    let parent = tempfile::TempDir::new().unwrap();
    let worker = tempfile::TempDir::new().unwrap();
    let shared_db = parent.path().join("memory").join("brain.db");
    std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();
    let shared_memory = crate::memory::SqliteMemory::new_with_path_and_acl(shared_db.clone(), false).unwrap();
    shared_memory
        .append_message_event(crate::memory::MessageEventInput {
            event_id: None,
            idempotency_key: None,
            workspace_id: parent.path().to_string_lossy().to_string(),
            // OWNER-VIS OV-D: the feed carries the worker's own owner's
            // events only, so the parent event belongs to that owner.
            owner_id: Some("owner-a".to_string()),
            source: "gateway".into(),
            channel: Some("webhook".to_string()),
            session_key: Some("gateway:external".to_string()),
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
            content: "parent shared context".to_string(),
            raw_payload_json: None,
            visibility: crate::memory::MemoryVisibility::Workspace,
        })
        .await
        .unwrap();
    let manifest = WorkerManifest {
        parent_capability: Some("capability".to_string()),
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "run-hybrid".to_string(),
        task: "use context".to_string(),
        provider_name: "provider".to_string(),
        model: "model".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        temperature_override: None,
        config_dir: parent.path().join("config"),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: worker.path().to_path_buf(),
        memory_db_path: worker.path().join("brain.db"),
        memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
        memory_strategy: Some("hybrid".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(shared_db),
        worker_memory_db_path: Some(worker.path().join("brain.db")),
        agent_id: None,
        persona_id: None,
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
        owner_isolated: false,
        topic_id: Some("topic-a".to_string()),
        parent_task_id: Some("run-parent".to_string()),
        source_message_event_id: Some("msg-a".to_string()),
        spawn_depth: 1,
        session_scope_key: "telegram:chat-1:alice".to_string(),
        parent_run_id: Some("run-parent".to_string()),
        compaction_config: None,
    };
    let context = load_worker_shared_context(&manifest, &Config::default(), &shared_memory).await;

    assert!(context.contains("parent shared context"));
}

/// OWNER-VIS OV-D (D-019 Q-4): a process-mode sub-agent worker's prompt
/// used to carry "[Recent shared workspace events]" from the whole
/// workspace — another owner's spawn task text and sub-agent result
/// included. The feed now carries the spawning owner's events only.
///
/// MUTATION GUARD: drop the manifest owner from the worker's principal,
/// or put `{owner_fragment}` back on the shared-context `workspace` arm,
/// and this goes red.
#[tokio::test]
async fn process_mode_worker_prompt_carries_no_other_owners_spawn_text() {
    let parent = tempfile::TempDir::new().unwrap();
    let worker = tempfile::TempDir::new().unwrap();
    let shared_db = parent.path().join("memory").join("brain.db");
    std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();
    let shared_memory = crate::memory::SqliteMemory::new_with_path_and_acl(shared_db.clone(), false).unwrap();
    for (owner, session, role, content) in [
        (
            "owner-other",
            "telegram:chat-9:bob",
            "user",
            "bob private spawn task text",
        ),
        (
            "owner-other",
            "telegram:chat-9:bob",
            "event",
            "bob sub-agent result text",
        ),
        ("owner-a", "telegram:chat-2:alice", "user", "alice earlier shared event"),
    ] {
        shared_memory
            .append_message_event(crate::memory::MessageEventInput {
                event_id: None,
                idempotency_key: None,
                workspace_id: parent.path().to_string_lossy().to_string(),
                owner_id: Some(owner.to_string()),
                source: "sessions_spawn".into(),
                channel: Some("telegram".to_string()),
                session_key: Some(session.to_string()),
                parent_session_key: None,
                run_id: Some(format!("run-{owner}")),
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                sender: None,
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
                raw_payload_json: None,
                visibility: crate::memory::MemoryVisibility::Workspace,
            })
            .await
            .expect("test: seed a workspace event");
    }
    let manifest = WorkerManifest {
        parent_capability: Some("capability".to_string()),
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "run-hybrid".to_string(),
        task: "use context".to_string(),
        provider_name: "provider".to_string(),
        model: "model".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        temperature_override: None,
        config_dir: parent.path().join("config"),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: worker.path().to_path_buf(),
        memory_db_path: worker.path().join("brain.db"),
        memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
        memory_strategy: Some("hybrid".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(shared_db),
        worker_memory_db_path: Some(worker.path().join("brain.db")),
        agent_id: None,
        persona_id: None,
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
        owner_isolated: false,
        topic_id: Some("topic-a".to_string()),
        parent_task_id: Some("run-parent".to_string()),
        source_message_event_id: Some("msg-a".to_string()),
        spawn_depth: 1,
        session_scope_key: "telegram:chat-1:alice".to_string(),
        parent_run_id: Some("run-parent".to_string()),
        compaction_config: None,
    };
    let context = load_worker_shared_context(&manifest, &Config::default(), &shared_memory).await;

    assert!(
        context.contains("alice earlier shared event"),
        "the spawning owner's own events still reach the worker: {context}"
    );
    assert!(
        !context.contains("bob private spawn task text") && !context.contains("bob sub-agent result text"),
        "another owner's spawn and result text must not reach the worker prompt: {context}"
    );
}

#[tokio::test]
async fn hybrid_worker_result_creates_private_draft_and_parent_merge_request() {
    let parent = tempfile::TempDir::new().unwrap();
    let worker = tempfile::TempDir::new().unwrap();
    let shared_db = parent.path().join("memory").join("brain.db");
    let worker_db = worker.path().join("brain.db");
    std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();

    let worker_memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new_with_path_and_acl(worker_db.clone(), false).unwrap());
    let worker_fabric = MemoryFabric::new(worker_memory.clone(), worker.path().to_string_lossy().to_string());
    let scope = MessageEventScope::new("session_worker", crate::memory::MemoryVisibility::Workspace)
        .with_owner_id("owner-a")
        .with_session_key("telegram:chat-1:alice")
        .with_run_id("run-hybrid")
        .with_parent_run_id("run-parent")
        .with_agent_id("agent-a")
        .with_persona_id("persona-a");
    let result = WorkerResult {
        completion_evidence: None,
        ordinary_completion: None,
        operator_completion: None,
        success: true,
        output: "worker draft content".to_string(),
        error: None,
        tokens_used: None,
    };
    let result_event = worker_fabric
        .record_worker_result(scope.clone(), result.output.clone(), None)
        .await
        .unwrap();
    let manifest = WorkerManifest {
        parent_capability: Some("capability".to_string()),
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "run-hybrid".to_string(),
        task: "produce draft".to_string(),
        provider_name: "provider".to_string(),
        model: "model".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        temperature_override: None,
        config_dir: parent.path().join("config"),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: worker.path().to_path_buf(),
        memory_db_path: worker_db,
        memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
        memory_strategy: Some("hybrid".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(shared_db.clone()),
        worker_memory_db_path: Some(worker.path().join("brain.db")),
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
        owner_isolated: false,
        topic_id: Some("topic-a".to_string()),
        parent_task_id: Some("run-parent".to_string()),
        source_message_event_id: Some("msg-a".to_string()),
        spawn_depth: 1,
        session_scope_key: "telegram:chat-1:alice".to_string(),
        parent_run_id: Some("run-parent".to_string()),
        compaction_config: None,
    };

    record_hybrid_worker_draft_if_needed(
        &manifest,
        &Config::default(),
        &worker_fabric,
        &scope,
        &result,
        Some(&result_event),
        &result.output,
        &SecurityPolicy::default(),
    )
    .await;

    let drafts = worker_memory
        .list_memory_drafts_for_run(
            &crate::memory::traits::MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace".to_string(),
                agent_id: Some("system".to_string()),
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            "run-hybrid",
        )
        .await
        .unwrap();
    assert_eq!(drafts.len(), 1);
    let draft = drafts.first();
    assert_eq!(draft.map(|draft| draft.status.as_str()), Some("pending"));
    assert_eq!(draft.and_then(|draft| draft.owner_id.as_deref()), Some("owner-a"));
    assert_eq!(draft.map(|draft| draft.content.as_str()), Some("worker draft content"));
    assert_eq!(
        draft.and_then(|draft| draft.source_event_id.as_deref()),
        Some(result_event.event_id.as_str())
    );

    let parent_memory = crate::memory::SqliteMemory::new_with_path_and_acl(shared_db, false).unwrap();
    let parent_events = parent_memory
        .list_memory_events_since(
            &crate::memory::MemoryPrincipal {
                require_owner_match: false,
                workspace_id: parent.path().to_string_lossy().to_string(),
                agent_id: Some("agent-a".to_string()),
                persona_id: Some("persona-a".to_string()),
                session_key: Some("telegram:chat-1:alice".to_string()),
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(parent_events.len(), 1);
    let parent_event = parent_events.first();
    assert_eq!(
        parent_event.map(|event| event.event_type.as_str()),
        Some("memory.draft.merge_requested")
    );
    assert_eq!(
        parent_event.map(|event| event.subject_id.as_str()),
        draft.map(|draft| draft.draft_id.as_str())
    );
    let draft_key = draft.map(|draft| draft.key.as_str()).unwrap_or_default();
    assert!(parent_memory.get(draft_key).await.unwrap().is_none());
}

#[tokio::test]
async fn hybrid_worker_draft_obeys_readonly_resource_gate() {
    let parent = tempfile::TempDir::new().unwrap();
    let worker = tempfile::TempDir::new().unwrap();
    let shared_db = parent.path().join("memory").join("brain.db");
    let worker_db = worker.path().join("brain.db");
    std::fs::create_dir_all(shared_db.parent().unwrap()).unwrap();

    let worker_memory: Arc<dyn Memory> =
        Arc::new(crate::memory::SqliteMemory::new_with_path_and_acl(worker_db.clone(), false).unwrap());
    let worker_fabric = MemoryFabric::new(worker_memory.clone(), worker.path().to_string_lossy().to_string());
    let scope = MessageEventScope::new("session_worker", crate::memory::MemoryVisibility::Workspace)
        .with_owner_id("owner-a")
        .with_session_key("telegram:chat-1:alice")
        .with_run_id("run-hybrid");
    let result = WorkerResult {
        completion_evidence: None,
        ordinary_completion: None,
        operator_completion: None,
        success: true,
        output: "worker draft content".to_string(),
        error: None,
        tokens_used: None,
    };
    let manifest = WorkerManifest {
        parent_capability: Some("capability".to_string()),
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "run-hybrid".to_string(),
        task: "produce draft".to_string(),
        provider_name: "provider".to_string(),
        model: "model".to_string(),
        api_key: None,
        route_hint: None,
        route_source_provider: None,
        route_credential: None,
        route_uses_root_connection: false,
        direct_uses_root_connection: false,
        temperature: 0.7,
        temperature_override: None,
        config_dir: parent.path().join("config"),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: worker.path().to_path_buf(),
        memory_db_path: worker_db,
        memory_workspace_id: Some(worker.path().to_string_lossy().to_string()),
        memory_strategy: Some("hybrid".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(shared_db),
        worker_memory_db_path: Some(worker.path().join("brain.db")),
        agent_id: None,
        persona_id: None,
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
        owner_isolated: false,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
        spawn_depth: 1,
        session_scope_key: "telegram:chat-1:alice".to_string(),
        parent_run_id: None,
        compaction_config: None,
    };
    let readonly = SecurityPolicy {
        autonomy: crate::security::policy::AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    };

    record_hybrid_worker_draft_if_needed(
        &manifest,
        &Config::default(),
        &worker_fabric,
        &scope,
        &result,
        None,
        &result.output,
        &readonly,
    )
    .await;

    let drafts = worker_memory
        .list_memory_drafts_for_run(
            &crate::memory::traits::MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace".to_string(),
                agent_id: Some("system".to_string()),
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            "run-hybrid",
        )
        .await
        .unwrap();
    assert!(drafts.is_empty());
}
