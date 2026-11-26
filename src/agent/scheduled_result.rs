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
/// Nothing is written outside a scheduled invocation, for a job or task whose
/// deliver mode is `none`, or for a run with no owner. The row carries the
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
    if meta.deliver == ScheduledDeliver::None
        || envelope.owner_id.is_none()
        || envelope.task_id.as_deref() != Some(meta.task_id.as_str())
    {
        return;
    }
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
            envelope.message_scope(),
            SCHEDULE_RESULT_EVENT_TYPE,
            "scheduled run result",
            Some(payload.to_string()),
        )
        .await
    {
        tracing::warn!(error = %error_class(&error), "Failed to record scheduled run result");
    }
}

/// The first characters of the reply, after the persisted-text redaction.
fn excerpt(reply: &str) -> String {
    crate::agent::terminal::redact_persisted_text(reply)
        .chars()
        .take(EXCERPT_CHARS)
        .collect()
}
