use super::{SpawnLineage, SpawnScope, SubAgentStatus};
use crate::memory::{MemoryFabric, MessageEventScope};
use crate::runtime::envelope::RuntimeEnvelope;
use serde_json::json;
use std::sync::Arc;

pub(super) fn spawn_event_scope(
    workspace_id: &str,
    run_id: &str,
    session_scope_key: &str,
    parent_run_id: Option<&str>,
    agent_name: Option<&str>,
    scope: Option<&SpawnScope>,
) -> MessageEventScope {
    let mut envelope = RuntimeEnvelope::sessions_spawn(workspace_id, session_scope_key, run_id)
        .with_channel(scope.map_or("sessions_spawn", |scope| scope.channel.as_str()));
    if let Some(parent_run_id) = parent_run_id {
        envelope = envelope.with_parent_run_id(parent_run_id);
    }
    if let Some(agent_name) = agent_name {
        envelope = envelope.with_agent_id(agent_name);
    }
    if let Some(scope) = scope {
        envelope = envelope
            .with_sender(scope.sender.as_str())
            .with_recipient(scope.chat_id.as_str());
        envelope.config_generation_id = scope.config_generation_id;
        envelope.config_source_revision = scope.config_source_revision.clone();
    }
    let mut event_scope = envelope.message_scope();
    if let Some(owner_id) = scope.and_then(|scope| scope.owner_id.as_deref()) {
        event_scope.owner_id = Some(owner_id.to_string());
    }
    event_scope
}

pub(super) async fn record_spawn_request_event(
    fabric: Option<&MemoryFabric>,
    scope: MessageEventScope,
    task: &str,
    mode: &str,
    provider_name: &str,
    model: &str,
    lineage: &SpawnLineage,
) -> anyhow::Result<Option<Arc<crate::runtime::child_source::ChildInputReceipt>>> {
    let Some(fabric) = fabric else {
        return Ok(None);
    };
    // Durable records keep the endpoint, never a URL's `user:password@`.
    let provider_name = crate::providers::redact_endpoint_userinfo(provider_name);
    let task_event_scope = scope.clone();
    let task_id = task_event_scope
        .run_id
        .clone()
        .unwrap_or_else(|| "sessions_spawn:unknown".to_string());
    let grant = crate::runtime::child_source::outgoing_grant();
    let receipt = crate::runtime::child_source::record_child_input(
        fabric,
        scope,
        task,
        None,
        Some(
            json!({
                "mode": mode,
                "provider": provider_name,
                "model": model,
                "owner_id": lineage.owner_id,
                "topic_id": lineage.topic_id,
                "parent_task_id": lineage.parent_task_id,
                "source_message_event_id": lineage.source_message_event_id
            })
            .to_string(),
        ),
        grant.as_deref(),
    )
    .await?;
    if let Err(error) = fabric
        .record_task_event(
            task_event_scope,
            task_id,
            "task.spawned",
            Some(
                json!({
                    "task": task,
                    "mode": mode,
                    "provider": provider_name,
                    "model": model,
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                })
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn task.spawned event: {error}");
    }
    Ok(receipt)
}

pub(super) async fn record_spawn_result_event(
    fabric: Option<&MemoryFabric>,
    scope: MessageEventScope,
    result_text: &str,
    status: &SubAgentStatus,
    lineage: &SpawnLineage,
    terminal_event_type: Option<&str>,
) {
    let Some(fabric) = fabric else {
        return;
    };
    let task_event_scope = scope.clone();
    let task_id = task_event_scope
        .run_id
        .clone()
        .unwrap_or_else(|| "sessions_spawn:unknown".to_string());
    let (success, error) = match status {
        SubAgentStatus::Completed(_) => (true, None),
        SubAgentStatus::Running => (false, Some("still running".to_string())),
        SubAgentStatus::AwaitingInput { prompt } => (false, Some(format!("awaiting approval: {prompt}"))),
        SubAgentStatus::Failed(error) => (false, Some(error.clone())),
    };
    // The result and task records carry the sub-agent's reply or error text:
    // redacted like the terminal commit (D-053).
    if let Err(error) = fabric
        .record_worker_result(
            scope,
            crate::agent::terminal::redact_persisted_text(result_text),
            Some(
                crate::agent::terminal::redact_persisted_json(json!({
                    "success": success,
                    "error": error,
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                }))
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn result event: {error}");
    }
    let task_event_type = terminal_event_type.unwrap_or(if success { "task.completed" } else { "task.failed" });
    if let Err(error) = fabric
        .record_task_event(
            task_event_scope,
            task_id,
            task_event_type,
            Some(
                crate::agent::terminal::redact_persisted_json(json!({
                    "success": success,
                    "error": error,
                    "result_preview": result_text.chars().take(500).collect::<String>(),
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                }))
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn {task_event_type} event: {error}");
    }
}
