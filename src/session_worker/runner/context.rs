use crate::agent::context_recall::build_context_with_shared_events_and_scope;
use crate::config::Config;
use crate::memory::{Memory, MemoryCategory, MemoryFabric, MessageEvent, MessageEventScope};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::policy::ResourceRiskLevel;
use crate::security::{SecurityPolicy, SideEffectGate};
use crate::session_worker::protocol::{WorkerManifest, WorkerResult};
use crate::tools::sessions_spawn::{SPAWN_EXECUTION_CONTEXT, SpawnExecutionContext};
use std::future::Future;
use std::sync::Arc;

pub(super) async fn record_hybrid_worker_draft_if_needed(
    manifest: &WorkerManifest,
    config: &Config,
    memory_fabric: &MemoryFabric,
    worker_event_scope: &MessageEventScope,
    worker_result: &WorkerResult,
    worker_result_event: Option<&MessageEvent>,
    event_content: &str,
    security: &SecurityPolicy,
) {
    if manifest.memory_strategy.as_deref() != Some("hybrid") || !worker_result.success {
        return;
    }

    if let Err(error) = SideEffectGate::new(security).authorize_resource_operation(
        "session_worker",
        &format!("session_worker:hybrid_draft:{}", manifest.run_id),
        ResourceRiskLevel::Low,
        None,
    ) {
        tracing::warn!(run_id = %manifest.run_id, "hybrid worker draft blocked by SideEffectGate: {error}");
        return;
    }

    let draft_key = format!("worker_result:{}", manifest.run_id);
    match memory_fabric
        .create_worker_memory_draft(
            worker_event_scope,
            &manifest.run_id,
            &draft_key,
            event_content,
            MemoryCategory::Conversation,
            worker_result_event.map(|event| event.event_id.as_str()),
            Some(
                crate::agent::terminal::redact_persisted_json(serde_json::json!({
                    "success": worker_result.success,
                    "error": worker_result.error,
                    "merge_policy": "parent_decides",
                    "owner_id": manifest.owner_id.as_deref(),
                    "topic_id": manifest.topic_id.as_deref(),
                    "parent_task_id": manifest.parent_task_id.as_deref(),
                    "source_message_event_id": manifest.source_message_event_id.as_deref()
                }))
                .to_string(),
            ),
        )
        .await
    {
        Ok(draft) => {
            if let Some(shared_db_path) = manifest.shared_memory_db_path.as_ref() {
                match crate::memory::SqliteMemory::new_with_path_and_acl(
                    shared_db_path.clone(),
                    config.memory.acl_enabled,
                ) {
                    Ok(shared_memory) => {
                        let shared_workspace_id = shared_worker_workspace_id(manifest);
                        let shared_fabric = MemoryFabric::new(Arc::new(shared_memory), shared_workspace_id);
                        if let Err(error) = shared_fabric
                            .record_draft_merge_requested(&draft, Some(shared_fabric.workspace_id()))
                            .await
                        {
                            tracing::warn!(
                                run_id = %manifest.run_id,
                                draft_id = %draft.draft_id,
                                "failed to record hybrid draft merge request: {error}"
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            run_id = %manifest.run_id,
                            "failed to open parent shared memory for hybrid draft request: {error}"
                        );
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(run_id = %manifest.run_id, "failed to create hybrid worker memory draft: {error}");
        }
    }
}

fn shared_worker_workspace_id(manifest: &WorkerManifest) -> String {
    manifest
        .shared_memory_db_path
        .as_ref()
        .and_then(|path| path.parent().and_then(std::path::Path::parent))
        .map(|path| path.to_string_lossy().to_string())
        .or_else(|| manifest.memory_workspace_id.clone())
        .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string())
}

pub(super) async fn load_worker_shared_context(
    manifest: &WorkerManifest,
    config: &Config,
    shared_memory: &dyn Memory,
) -> String {
    let strategy = manifest.memory_strategy.as_deref().unwrap_or("shared_fabric");
    if strategy == "isolated_private" {
        return String::new();
    }

    let workspace_id = if strategy == "hybrid" {
        shared_worker_workspace_id(manifest)
    } else {
        manifest
            .memory_workspace_id
            .clone()
            .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string())
    };

    let runtime_envelope = worker_runtime_envelope_for_workspace(manifest, workspace_id);
    let semantic_scope = match manifest.scope_chat_type.as_deref() {
        Some(chat_type)
            if !chat_type.is_empty()
                && manifest.scope_sender.as_deref().is_some_and(|value| !value.is_empty())
                && manifest.scope_channel.as_deref().is_some_and(|value| !value.is_empty())
                && manifest.scope_chat_id.as_deref().is_some_and(|value| !value.is_empty()) =>
        {
            Some(runtime_envelope.memory_write_context(chat_type))
        }
        _ => None,
    };

    // OWNER-VIS OV-D: the shared-events feed is limited to the reader's own
    // owner. A worker reads for the owner that spawned it — the owner its
    // manifest carries and its result events are recorded under — not for
    // the channel/sender composition of its own envelope.
    let mut principal = runtime_envelope.memory_principal();
    if let Some(owner_id) = manifest
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
    {
        principal.owner_id = Some(owner_id.to_string());
    }
    build_context_with_shared_events_and_scope(
        shared_memory,
        principal,
        &manifest.task,
        config.memory.min_relevance_score,
        semantic_scope.as_ref(),
    )
    .await
    .preamble
}

fn worker_session_scope_key(manifest: &WorkerManifest) -> &str {
    if manifest.session_scope_key.trim().is_empty() {
        "sessions_spawn:global"
    } else {
        manifest.session_scope_key.as_str()
    }
}

pub(super) fn worker_runtime_envelope(manifest: &WorkerManifest) -> RuntimeEnvelope {
    let workspace_id = manifest
        .memory_workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string());
    worker_runtime_envelope_for_workspace(manifest, workspace_id)
}

fn worker_runtime_envelope_for_workspace(manifest: &WorkerManifest, workspace_id: String) -> RuntimeEnvelope {
    let mut envelope = RuntimeEnvelope::session_worker(
        workspace_id,
        worker_session_scope_key(manifest),
        manifest.run_id.clone(),
    )
    .with_channel(manifest.scope_channel.as_deref().unwrap_or("session_worker"));

    if let Some(agent_id) = manifest.agent_id.as_deref() {
        envelope = envelope.with_agent_id(agent_id);
    }
    if let Some(persona_id) = manifest.persona_id.as_deref() {
        envelope = envelope.with_persona_id(persona_id);
    }
    if let Some(parent_run_id) = manifest.parent_run_id.as_deref() {
        envelope = envelope.with_parent_run_id(parent_run_id);
    }
    if let Some(sender) = manifest.scope_sender.as_deref() {
        envelope = envelope.with_sender(sender);
    }
    if let Some(chat_id) = manifest.scope_chat_id.as_deref() {
        envelope = envelope.with_recipient(chat_id);
    }
    if let Some(owner_id) = manifest.owner_id.as_deref().filter(|owner_id| !owner_id.is_empty()) {
        envelope = envelope.with_owner_id(owner_id);
    }
    if manifest.owner_isolated {
        envelope = envelope.with_owner_isolation();
    }
    envelope.config_generation_id = manifest.runtime_config_generation_id;
    envelope.config_source_revision = manifest.runtime_config_source_revision.clone();
    envelope
}

pub(super) fn worker_message_event_scope(manifest: &WorkerManifest) -> MessageEventScope {
    let mut scope = worker_runtime_envelope(manifest).message_scope();
    if let Some(owner_id) = manifest.owner_id.as_deref().filter(|value| !value.is_empty()) {
        scope.owner_id = Some(owner_id.to_string());
    }
    scope
}

pub(super) fn worker_lineage_payload(manifest: &WorkerManifest) -> serde_json::Value {
    serde_json::json!({
        "owner_id": manifest.owner_id.as_deref(),
        "topic_id": manifest.topic_id.as_deref(),
        "parent_task_id": manifest.parent_task_id.as_deref(),
        "source_message_event_id": manifest.source_message_event_id.as_deref(),
        "parent_run_id": manifest.parent_run_id.as_deref(),
        "session_scope_key": manifest.session_scope_key.as_str(),
        "spawn_depth": manifest.spawn_depth
    })
}

#[inline(never)]
pub(super) fn boxed_worker_segments<F, Fut>(make_work: F) -> std::pin::Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: Future + Send,
{
    Box::pin(make_work())
}

pub(super) async fn with_manifest_spawn_context<T, Fut>(
    manifest: &WorkerManifest,
    input_receipt: Option<Arc<crate::runtime::child_source::ChildInputReceipt>>,
    incoming_grant: Option<Arc<crate::runtime::child_source::IssuedChildGrant>>,
    provider_binding: Option<Arc<crate::runtime::child_source::ChildProviderBinding>>,
    terminal_usage: Option<Arc<crate::runtime::turn_terminal::TerminalUsageCollector>>,
    fut: Fut,
) -> T
where
    Fut: Future<Output = T>,
{
    if !manifest.session_scope_key.trim().is_empty() {
        let source_message_event_id = if incoming_grant.is_some() {
            input_receipt.as_ref().map(|receipt| receipt.event().event_id.clone())
        } else {
            manifest.source_message_event_id.clone()
        };
        SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext {
                    operator_authority: None,
                    ordinary_authority: None,
                    incoming_grant,
                    provider_binding,
                    terminal_usage,
                    input_receipt,
                    run_id: manifest.run_id.clone(),
                    session_scope_key: manifest.session_scope_key.clone(),
                    spawn_depth: manifest.spawn_depth,
                    owner_id: manifest.owner_id.clone(),
                    owner_isolated: manifest.owner_isolated,
                    topic_id: manifest.topic_id.clone(),
                    source_message_event_id,
                    // A resumed spawn-run process (not a turn root): children
                    // compute spawn_depth + 1, preserving the manifest depth chain.
                    is_turn_root: false,
                    // The spawning surface's exclusions travel in the sealed
                    // manifest, so everything this worker spawns in turn — task
                    // or process — stays inside the same boundary.
                    channel_excluded_tools: manifest.channel_excluded_tools.clone(),
                },
                fut,
            )
            .await
    } else {
        fut.await
    }
}
