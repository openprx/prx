//! The `schedule.result.recorded` row of a scheduled agent run.
//!
//! One row is written per scheduled run, at its end, in the run's own session
//! and agent partition, so the agent erasure and purge passes cover it. A
//! failure to write it is logged by its error class and never fails the run.
use crate::agent::diagnostics::error_class;
use crate::config::Config;
use crate::cron::ScheduledDeliver;
use crate::memory::{Memory, MemoryFabric};
use crate::runtime::envelope::RuntimeEnvelope;
use std::sync::Arc;

/// The event type of a scheduled run's result row.
pub(crate) const SCHEDULE_RESULT_EVENT_TYPE: &str = "schedule.result.recorded";
const SCHEMA_VERSION: u32 = 1;
/// Characters of the reply kept in the row, after redaction.
const EXCERPT_CHARS: usize = 400;

/// Writes the result row of the scheduled invocation this call runs inside.
///
/// Nothing is written outside a scheduled invocation or for a job or task whose
/// deliver mode is `none`. The owner is chosen from the task's stored owner: a
/// task with an owner writes under the run's envelope scope. A task with no
/// owner writes only when the local operator created it: its row goes to the
/// terminal owner's Main partition, so `prx chat` shows it. The row carries the
/// reply excerpt on success and only the error class on failure.
pub(crate) async fn record(
    memory: &Arc<dyn Memory>,
    config: &Config,
    envelope: &RuntimeEnvelope,
    outcome: &anyhow::Result<String>,
) {
    let Some(meta) = super::scheduled_source::current_result_meta() else {
        return;
    };
    // A nested run that inherits the scope has another task id; it writes nothing.
    if meta.deliver == ScheduledDeliver::None || envelope.task_id.as_deref() != Some(meta.task_id.as_str()) {
        return;
    }
    let scope = if meta.has_owner {
        envelope.message_scope()
    } else if meta.local_operator {
        terminal_result_envelope(envelope).message_scope()
    } else {
        return;
    };
    let (status, excerpt, failure) = match outcome {
        Ok(reply) => ("ok", Some(excerpt(reply)), None),
        Err(error) => ("error", None, Some(error_class(error).to_string())),
    };
    let payload = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "kind": envelope.source.as_str(),
        "id": meta.task_id,
        "name": meta.name,
        "status": status,
        "finished_at": chrono::Utc::now().to_rfc3339(),
        "excerpt": excerpt,
        "transcript_session": envelope.session_key,
        "error_class": failure,
    });
    let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
        .with_event_recording(config.memory.event_recording_config());
    if let Err(error) = fabric
        .record_tool_round_event(
            scope,
            SCHEDULE_RESULT_EVENT_TYPE,
            "scheduled run result",
            Some(payload.to_string()),
        )
        .await
    {
        tracing::warn!(error = %error_class(&error), "Failed to record scheduled run result");
    }
}

/// The run's envelope bound to the terminal owner and its Main partition, the
/// owner and partition `prx chat` reads its scheduled-result note from.
fn terminal_result_envelope(envelope: &RuntimeEnvelope) -> RuntimeEnvelope {
    let owner_id = RuntimeEnvelope::terminal_owner_id(&envelope.workspace_id);
    let partition = crate::memory::AgentPartition {
        workspace_id: envelope.workspace_id.clone(),
        owner_id: owner_id.clone(),
        key: crate::memory::PartitionKey::Main,
    };
    envelope.clone().with_owner_id(owner_id).with_agent_partition(partition)
}

/// The first characters of the reply, after the persisted-text redaction.
fn excerpt(reply: &str) -> String {
    crate::agent::terminal::redact_persisted_text(reply)
        .chars()
        .take(EXCERPT_CHARS)
        .collect()
}
