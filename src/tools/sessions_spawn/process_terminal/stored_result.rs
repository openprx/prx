//! The run's own stored result row, read first when recovery finds no signed
//! completion journal.
use crate::memory::MemoryFabric;
use anyhow::Context;

/// What a finished process child recorded under its single result key.
pub(in crate::tools::sessions_spawn) struct StoredWorkerResult {
    pub(in crate::tools::sessions_spawn) success: bool,
    /// The reply on success, the failure text otherwise (already redacted).
    pub(in crate::tools::sessions_spawn) content: String,
    pub(in crate::tools::sessions_spawn) finished_at: chrono::DateTime<chrono::Utc>,
}

/// Read `worker:<run>:result`. `None` when no row exists, or when the row is
/// an owner-loss terminal, which records that no result was written.
pub(super) async fn read(fabric: &MemoryFabric, owner: &str, run: &str) -> anyhow::Result<Option<StoredWorkerResult>> {
    let key = crate::session_worker::protocol::worker_result_key(run);
    let Some(event) = fabric
        .memory()
        .find_owned_message_event_by_idempotency_key(fabric.workspace_id(), owner, &key)
        .await?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        event.id > 0
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id.as_deref() == Some(owner)
            && event.run_id.as_deref() == Some(run)
            && event.role == "event"
            && event.event_type == "worker.result.created"
            && event.idempotency_key.as_deref() == Some(key.as_str()),
        "stored worker result differs from its run"
    );
    let payload: serde_json::Value = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("stored worker result payload missing")?,
    )?;
    if payload.get("unconfirmed").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let success = payload
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .context("stored worker result success flag missing")?;
    let content = if success {
        event.content
    } else {
        payload
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map_or(event.content, str::to_owned)
    };
    let finished_at = chrono::DateTime::parse_from_rfc3339(&event.created_at)?.with_timezone(&chrono::Utc);
    Ok(Some(StoredWorkerResult {
        success,
        content,
        finished_at,
    }))
}
