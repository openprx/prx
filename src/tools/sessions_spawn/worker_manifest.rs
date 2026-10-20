use super::{
    PROCESS_MEMORY_STRATEGY_HYBRID, PROCESS_MEMORY_STRATEGY_ISOLATED, PROCESS_MEMORY_STRATEGY_SHARED,
    SESSION_WORKER_CAP_TTL_SECS, SpawnLineage, SpawnScope, capability_now_unix, compute_worker_capability_with_key,
    manifest_signing_payload, seal_worker_capability,
};
use crate::config::AgentCompactionConfig;
use crate::memory::MemoryEventRecording;
use crate::providers;
use crate::session_worker::protocol::WorkerManifest;
use anyhow::Context as _;

pub(super) fn copy_dir_recursive(source: &std::path::Path, destination: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = destination.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if metadata.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

pub(super) fn build_session_worker_cli_args(manifest: &WorkerManifest) -> anyhow::Result<Vec<String>> {
    let tools_json = serde_json::to_string(&manifest.allowed_tools)?;
    let config_dir = manifest
        .config_dir
        .to_str()
        .context("session-worker config directory must be valid UTF-8")?;
    Ok(vec![
        "--config-dir".to_string(),
        config_dir.to_string(),
        "session-worker".to_string(),
        "--task".to_string(),
        manifest.task.clone(),
        "--workspace".to_string(),
        manifest.workspace_dir.display().to_string(),
        "--memory-db".to_string(),
        manifest.memory_db_path.display().to_string(),
        "--tools".to_string(),
        tools_json,
    ])
}

pub(super) fn shared_worker_memory_db_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root.join("memory").join("brain.db")
}

pub(super) fn private_worker_memory_db_path(worker_workspace: &std::path::Path) -> std::path::PathBuf {
    worker_workspace.join("brain.db")
}

pub(super) fn normalize_process_memory_strategy(strategy: &str) -> anyhow::Result<&'static str> {
    match strategy.trim() {
        "" | PROCESS_MEMORY_STRATEGY_SHARED => Ok(PROCESS_MEMORY_STRATEGY_SHARED),
        PROCESS_MEMORY_STRATEGY_ISOLATED => Ok(PROCESS_MEMORY_STRATEGY_ISOLATED),
        PROCESS_MEMORY_STRATEGY_HYBRID => anyhow::bail!(crate::config::HYBRID_PROCESS_MEMORY_UNAVAILABLE),
        other => anyhow::bail!(
            "Invalid sessions_spawn.process_memory_strategy '{other}'. Expected 'shared_fabric' or 'isolated_private'."
        ),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_session_worker_manifest(
    run_id: &str,
    task: &str,
    provider_name: &str,
    model: &str,
    api_key: Option<&str>,
    route_hint: Option<&str>,
    route_source_provider: Option<&str>,
    route_credential: Option<&providers::credentials::CredentialRef>,
    route_uses_root_connection: bool,
    direct_uses_root_connection: bool,
    temperature: f64,
    temperature_override: Option<f64>,
    worker_workspace: std::path::PathBuf,
    memory_db_path: std::path::PathBuf,
    memory_workspace_id: String,
    normalized_memory_strategy: &str,
    memory_backend: &str,
    shared_memory_db_path: std::path::PathBuf,
    worker_memory_db_path: std::path::PathBuf,
    config_dir: std::path::PathBuf,
    config_generation: &str,
    agent_id: Option<&str>,
    agent_system_prompt: Option<&str>,
    event_recording: MemoryEventRecording,
    allowed_tools: &[String],
    identity_dir: Option<String>,
    scope: Option<&SpawnScope>,
    lineage: &SpawnLineage,
    spawn_depth: usize,
    session_scope_key: &str,
    parent_run_id: Option<&str>,
    compaction_config: &AgentCompactionConfig,
    channel_excluded_tools: &[String],
    child_source: Option<(
        &crate::runtime::child_source::IssuedChildGrant,
        &crate::runtime::child_source::ChildInputReceipt,
        &crate::session_worker::protocol::WorkerProviderSelection,
    )>,
    source_launch: Option<(
        crate::runtime::operator_spawn::process::ProcessSourceLaunch,
        &ring::hmac::Key,
    )>,
) -> anyhow::Result<(WorkerManifest, String, u64)> {
    let mut manifest = WorkerManifest {
        parent_capability: None,
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: run_id.to_string(),
        task: task.to_string(),
        provider_name: provider_name.to_string(),
        model: model.to_string(),
        api_key: api_key.map(str::to_string),
        route_hint: route_hint.map(str::to_string),
        route_source_provider: route_source_provider.map(str::to_string),
        route_credential: route_credential.cloned(),
        route_uses_root_connection,
        direct_uses_root_connection,
        temperature,
        temperature_override,
        config_dir,
        config_generation: config_generation.to_string(),
        runtime_config_generation_id: scope.and_then(|ctx| ctx.config_generation_id),
        runtime_config_source_revision: scope.and_then(|ctx| ctx.config_source_revision.clone()),
        workspace_dir: worker_workspace,
        memory_db_path,
        memory_workspace_id: Some(memory_workspace_id),
        memory_strategy: Some(normalized_memory_strategy.to_string()),
        memory_backend: memory_backend.to_string(),
        shared_memory_db_path: Some(shared_memory_db_path),
        worker_memory_db_path: Some(worker_memory_db_path),
        agent_id: agent_id.map(str::to_string),
        persona_id: source_launch
            .as_ref()
            .and_then(|(launch, _)| launch.persona_id().map(str::to_owned)),
        memory_event_recording: event_recording,
        allowed_tools: allowed_tools.to_vec(),
        channel_excluded_tools: channel_excluded_tools.to_vec(),
        system_prompt: agent_system_prompt.map(str::to_string),
        identity_dir,
        scope_sender: scope.map(|ctx| ctx.sender.clone()),
        scope_channel: scope.map(|ctx| ctx.channel.clone()),
        scope_chat_type: scope.map(|ctx| ctx.chat_type.clone()),
        scope_chat_id: scope.map(|ctx| ctx.chat_id.clone()),
        owner_id: lineage.owner_id.clone(),
        owner_isolated: lineage.owner_isolated,
        topic_id: lineage.topic_id.clone(),
        parent_task_id: lineage.parent_task_id.clone(),
        source_message_event_id: lineage.source_message_event_id.clone(),
        spawn_depth,
        session_scope_key: session_scope_key.to_string(),
        parent_run_id: parent_run_id.map(str::to_string),
        compaction_config: Some(compaction_config.clone()),
    };

    if let Some((grant, receipt, selection)) = child_source {
        grant.bind_worker_manifest(&mut manifest, receipt, selection)?;
    }

    let capability_expiry = capability_now_unix().saturating_add(SESSION_WORKER_CAP_TTL_SECS);
    let sealed_capability = if let Some((launch, key)) = source_launch {
        launch.bind_worker_manifest(key, &mut manifest)?;
        compute_worker_capability_with_key(
            key,
            &manifest.run_id,
            capability_expiry,
            &manifest_signing_payload(&manifest)?,
        )
    } else {
        seal_worker_capability(&manifest, capability_expiry)?
    };
    manifest.parent_capability = Some(sealed_capability.clone());
    Ok((manifest, sealed_capability, capability_expiry))
}
