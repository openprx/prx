#![cfg(test)]

mod credentials;
mod hybrid;
#[cfg(unix)]
mod manifest_reference_tests;
mod manifest_validation;
mod probe;
mod spawn_context;
mod steering;
mod tool_selection;

use crate::session_worker::protocol::WorkerManifest;
use std::path::Path;

pub(super) fn base_manifest(workspace: &Path, capability: &str) -> WorkerManifest {
    WorkerManifest {
        parent_capability: Some(capability.to_string()),
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: "run-worker".to_string(),
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
        temperature_override: None,
        config_dir: workspace.join("config"),
        config_generation: "0".repeat(64),
        runtime_config_generation_id: None,
        runtime_config_source_revision: None,
        workspace_dir: workspace.to_path_buf(),
        memory_db_path: workspace.join("memory").join("brain.db"),
        memory_workspace_id: Some(workspace.to_string_lossy().to_string()),
        memory_strategy: Some("shared_fabric".to_string()),
        memory_backend: "sqlite".to_string(),
        shared_memory_db_path: Some(workspace.join("memory").join("brain.db")),
        worker_memory_db_path: Some(workspace.join("worker.db")),
        agent_id: None,
        persona_id: None,
        memory_event_recording: crate::memory::MemoryEventRecording::default(),
        allowed_tools: vec!["file_read".to_string()],
        channel_excluded_tools: Vec::new(),
        system_prompt: None,
        identity_dir: Some("identity/worker".to_string()),
        scope_sender: None,
        scope_channel: None,
        scope_chat_type: None,
        scope_chat_id: None,
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
        spawn_depth: 0,
        session_scope_key: "sessions_spawn:global".to_string(),
        parent_run_id: None,
        compaction_config: None,
    }
}
