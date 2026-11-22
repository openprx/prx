use super::delegation;
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

/// The codes the `task.spawned` record states about the issued child.
pub(super) struct SpawnRecordCodes<'a> {
    /// Whether the child can spawn ([`crate::runtime::child_source::publication::ChildDelegation`]).
    pub(super) child_delegation: Option<&'a str>,
    /// The level the child runs at.
    pub(super) child_level: crate::security::AutonomyLevel,
}

pub(super) async fn record_spawn_request_event(
    fabric: Option<&MemoryFabric>,
    scope: MessageEventScope,
    task: &str,
    mode: &str,
    selection: (&str, &str),
    lineage: &SpawnLineage,
    codes: SpawnRecordCodes<'_>,
) -> anyhow::Result<Option<Arc<crate::runtime::child_source::ChildInputReceipt>>> {
    let Some(fabric) = fabric else {
        return Ok(None);
    };
    // Durable records keep the endpoint, never a URL's `user:password@`.
    let provider_name = crate::providers::redact_endpoint_userinfo(selection.0);
    let task_event_scope = scope.clone();
    let task_id = task_event_scope
        .run_id
        .clone()
        .unwrap_or_else(|| "sessions_spawn:unknown".to_string());
    let mut metadata = json!({
        "mode": mode,
        "provider": provider_name, "model": selection.1,
        "owner_id": lineage.owner_id,
        "topic_id": lineage.topic_id,
        "parent_task_id": lineage.parent_task_id,
        "source_message_event_id": lineage.source_message_event_id
    });
    let receipt =
        crate::runtime::child_source::record_child_input(fabric, scope, task, None, Some(metadata.to_string())).await?;
    delegation::append_task(&mut metadata, task)?;
    // Only the task record states it. The child-input payload is read back
    // with `deny_unknown_fields` and owns the `delegation` key.
    if let Some(object) = metadata.as_object_mut() {
        if let Some(code) = codes.child_delegation {
            object.insert("child_delegation".to_owned(), serde_json::Value::from(code));
        }
        object.insert(
            "child_level".to_owned(),
            serde_json::Value::from(crate::security::agent_level::stored_level(codes.child_level)),
        );
    }
    if let Err(error) = fabric
        .record_task_event(task_event_scope, task_id, "task.spawned", Some(metadata.to_string()))
        .await
    {
        tracing::warn!("failed to record sessions_spawn task.spawned event: {error}");
    }
    Ok(receipt)
}

/// The terminal row of a process worker that ended without its own result
/// because its owner went away. It shares the run's single result key
/// ([`crate::session_worker::protocol::worker_result_key`]), so if the worker
/// (or its monitor) already recorded a result, the store keeps that row and
/// this write adds nothing. Returns whether this call created the row.
pub(super) async fn record_owner_loss_terminal(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    run_id: &str,
    reason: super::OwnerLossReason,
) -> anyhow::Result<bool> {
    let key = crate::session_worker::protocol::worker_result_key(run_id);
    let code = reason.code();
    let payload = json!({
        "success": false,
        "error": code,
        "reason": code,
        "unconfirmed": true,
    })
    .to_string();
    let event = fabric
        .record_worker_result_idempotent(
            scope,
            format!("Sub-agent process ended without a result: {code}; cleanup and provider usage remain unconfirmed"),
            Some(payload.clone()),
            key.clone(),
        )
        .await?;
    // A disabled event recorder returns a synthetic row (id 0): nothing durable.
    anyhow::ensure!(
        event.id == 0
            || (event.idempotency_key.as_deref() == Some(key.as_str()) && event.run_id.as_deref() == Some(run_id)),
        "process worker result row differs from its run"
    );
    Ok(event.id > 0 && event.raw_payload_json.as_deref() == Some(payload.as_str()))
}

pub(super) async fn record_spawn_result_event(
    fabric: Option<&MemoryFabric>,
    scope: MessageEventScope,
    result_text: &str,
    status: &SubAgentStatus,
    lineage: &SpawnLineage,
    terminal_event_type: Option<&str>,
) {
    record_spawn_result_event_keyed(fabric, scope, result_text, status, lineage, terminal_event_type, None).await;
}

/// [`record_spawn_result_event`] with the result row under `result_key`. A
/// process-mode monitor passes its run's single result key, so its row and
/// the worker's own row collapse into one (the first writer wins).
pub(super) async fn record_spawn_result_event_keyed(
    fabric: Option<&MemoryFabric>,
    scope: MessageEventScope,
    result_text: &str,
    status: &SubAgentStatus,
    lineage: &SpawnLineage,
    terminal_event_type: Option<&str>,
    result_key: Option<String>,
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
    let content = crate::agent::terminal::redact_persisted_text(result_text);
    let payload = Some(
        crate::agent::terminal::redact_persisted_json(json!({
            "success": success,
            "error": error,
            "owner_id": lineage.owner_id,
            "topic_id": lineage.topic_id,
            "parent_task_id": lineage.parent_task_id,
            "source_message_event_id": lineage.source_message_event_id
        }))
        .to_string(),
    );
    let recorded = match result_key {
        Some(key) => {
            fabric
                .record_worker_result_idempotent(scope, content, payload, key)
                .await
        }
        None => fabric.record_worker_result(scope, content, payload).await,
    };
    if let Err(error) = recorded {
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
