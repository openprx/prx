use crate::memory::{Memory, MemoryEvent, MemoryPrincipal, MessageEvent};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

const MEMORY_EVENT_SCAN_LIMIT: usize = 1000;
const MESSAGE_EVENT_SCAN_LIMIT: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveredTaskStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveredTaskRun {
    pub(crate) run_id: String,
    pub(crate) task: Option<String>,
    pub(crate) status: RecoveredTaskStatus,
    pub(crate) status_detail: Option<String>,
    pub(crate) session_key: Option<String>,
    pub(crate) owner_id: Option<String>,
    pub(crate) topic_id: Option<String>,
    pub(crate) parent_task_id: Option<String>,
    pub(crate) source_message_event_id: Option<String>,
    pub(crate) last_event_id: i64,
    pub(crate) last_event_type: String,
    pub(crate) last_event_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveredRunHistory {
    pub(crate) run: RecoveredTaskRun,
    pub(crate) messages: Vec<MessageEvent>,
}

pub(crate) async fn recover_task_runs(
    memory: Option<&Arc<dyn Memory>>,
    workspace_id: &str,
    args: &Value,
    limit: usize,
) -> anyhow::Result<Vec<RecoveredTaskRun>> {
    let Some(memory) = memory else {
        return Ok(Vec::new());
    };
    let principal = principal_from_args(args, workspace_id);
    let scan_limit = MEMORY_EVENT_SCAN_LIMIT.max(limit.saturating_mul(20)).max(1);
    let events = memory.list_memory_events_recent(&principal, scan_limit).await?;
    let message_events = memory
        .list_message_events_recent(&principal, MESSAGE_EVENT_SCAN_LIMIT.max(limit.saturating_mul(10)))
        .await?;
    Ok(task_runs_from_events_and_messages(events, message_events, limit))
}

pub(crate) async fn recover_run_history(
    memory: Option<&Arc<dyn Memory>>,
    workspace_id: &str,
    args: &Value,
    run_id: &str,
    limit: usize,
) -> anyhow::Result<Option<RecoveredRunHistory>> {
    let Some(memory) = memory else {
        return Ok(None);
    };
    let principal = principal_from_args(args, workspace_id);
    let events = memory
        .list_memory_events_recent(&principal, MEMORY_EVENT_SCAN_LIMIT)
        .await?;
    let message_events = memory
        .list_message_events_recent(&principal, MESSAGE_EVENT_SCAN_LIMIT.max(limit))
        .await?;
    let Some(run) = task_runs_from_events_and_messages(events, message_events, MEMORY_EVENT_SCAN_LIMIT)
        .into_iter()
        .find(|run| run.run_id == run_id)
    else {
        return Ok(None);
    };

    let mut message_principal = principal;
    message_principal.session_key = run.session_key.clone().or(message_principal.session_key);
    let mut messages = memory
        .list_message_events_recent(&message_principal, MESSAGE_EVENT_SCAN_LIMIT.max(limit))
        .await?
        .into_iter()
        .filter(|event| event.run_id.as_deref() == Some(run_id) || event.parent_run_id.as_deref() == Some(run_id))
        .collect::<Vec<_>>();
    messages.sort_by(|a, b| a.id.cmp(&b.id));
    if messages.len() > limit {
        let start = messages.len().saturating_sub(limit);
        messages = messages.split_off(start);
    }

    Ok(Some(RecoveredRunHistory { run, messages }))
}

pub(crate) fn principal_from_args(args: &Value, workspace_id: &str) -> MemoryPrincipal {
    let trusted = args.get("_zc_scope_trusted").and_then(Value::as_bool).unwrap_or(false);
    let scope = args.get("_zc_scope").and_then(Value::as_object);
    let read_scope = |key: &str| {
        if !trusted {
            return None;
        }
        scope?
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let channel = read_scope("channel");
    let sender = read_scope("sender");
    let session_key = read_scope("session_key").or_else(|| {
        let chat_id = read_scope("chat_id")?;
        match (channel.as_deref(), sender.as_deref()) {
            (Some(channel), Some(sender)) => Some(format!("{channel}:{chat_id}:{sender}")),
            _ => Some(chat_id),
        }
    });

    MemoryPrincipal {
        workspace_id: workspace_id.to_string(),
        agent_id: read_scope("agent_id"),
        persona_id: read_scope("persona_id"),
        session_key,
        channel,
        sender,
        owner_id: read_scope("owner_id"),
        legacy_session_key: None,
    }
}

fn task_runs_from_events(events: Vec<MemoryEvent>, limit: usize) -> Vec<RecoveredTaskRun> {
    let mut by_run: HashMap<String, RecoveredTaskRun> = HashMap::new();
    for event in events {
        if event.subject_table != "tasks" || !is_task_lifecycle_event(&event.event_type) {
            continue;
        }
        let payload = event
            .payload_json
            .as_deref()
            .and_then(|payload| serde_json::from_str::<Value>(payload).ok());
        let next = recovered_run_from_event(&event, payload.as_ref());
        by_run
            .entry(next.run_id.clone())
            .and_modify(|current| merge_recovered_run(current, &next))
            .or_insert(next);
    }
    let mut runs = by_run.into_values().collect::<Vec<_>>();
    sort_recovered_runs(&mut runs);
    runs.truncate(limit);
    runs
}

fn task_runs_from_events_and_messages(
    events: Vec<MemoryEvent>,
    message_events: Vec<MessageEvent>,
    limit: usize,
) -> Vec<RecoveredTaskRun> {
    let mut by_run = task_runs_from_events(events, usize::MAX)
        .into_iter()
        .map(|run| (run.run_id.clone(), run))
        .collect::<HashMap<_, _>>();

    for message_event in message_events {
        let Some(next) = recovered_run_from_message_event(&message_event) else {
            continue;
        };
        by_run
            .entry(next.run_id.clone())
            .and_modify(|current| merge_message_backfill(current, &next))
            .or_insert(next);
    }

    let mut runs = by_run.into_values().collect::<Vec<_>>();
    sort_recovered_runs(&mut runs);
    runs.truncate(limit);
    runs
}

fn sort_recovered_runs(runs: &mut [RecoveredTaskRun]) {
    runs.sort_by(|a, b| {
        b.last_event_at
            .cmp(&a.last_event_at)
            .then_with(|| b.last_event_id.cmp(&a.last_event_id))
    });
}

fn recovered_run_from_event(event: &MemoryEvent, payload: Option<&Value>) -> RecoveredTaskRun {
    RecoveredTaskRun {
        run_id: event.subject_id.clone(),
        task: payload
            .and_then(|payload| payload.get("task"))
            .and_then(Value::as_str)
            .map(str::to_string),
        status: status_from_event_type(&event.event_type),
        status_detail: status_detail_from_payload(payload),
        session_key: event.session_key.clone(),
        owner_id: payload
            .and_then(|payload| payload.get("owner_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        topic_id: payload
            .and_then(|payload| payload.get("topic_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_task_id: payload
            .and_then(|payload| payload.get("parent_task_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        source_message_event_id: payload
            .and_then(|payload| payload.get("source_message_event_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        last_event_id: event.id,
        last_event_type: event.event_type.clone(),
        last_event_at: event.created_at.clone(),
    }
}

fn recovered_run_from_message_event(event: &MessageEvent) -> Option<RecoveredTaskRun> {
    if !is_task_lineage_message_event(event) {
        return None;
    }
    let run_id = event.run_id.clone()?;
    let payload = event
        .raw_payload_json
        .as_deref()
        .and_then(|payload| serde_json::from_str::<Value>(payload).ok());
    let payload_ref = payload.as_ref();
    let status = status_from_message_event(event, payload_ref);
    let task = payload_ref
        .and_then(|payload| payload.get("task").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| (event.role == "user").then(|| event.content.clone()));

    Some(RecoveredTaskRun {
        run_id,
        task,
        status,
        status_detail: status_detail_from_payload(payload_ref).or_else(|| {
            (event.role == "event" || event.role == "assistant").then(|| truncate_status_detail(&event.content))
        }),
        session_key: event.session_key.clone(),
        owner_id: event.owner_id.clone().or_else(|| {
            payload_ref
                .and_then(|payload| payload.get("owner_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        topic_id: payload_ref
            .and_then(|payload| payload.get("topic_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_task_id: event.parent_run_id.clone().or_else(|| {
            payload_ref
                .and_then(|payload| payload.get("parent_task_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        source_message_event_id: payload_ref
            .and_then(|payload| payload.get("source_message_event_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(event.event_id.clone())),
        last_event_id: event.id,
        last_event_type: format!("message.{}", event.role),
        last_event_at: event.created_at.clone(),
    })
}

fn merge_recovered_run(current: &mut RecoveredTaskRun, next: &RecoveredTaskRun) {
    if next.task.is_some() {
        current.task = next.task.clone();
    }
    if next.session_key.is_some() {
        current.session_key = next.session_key.clone();
    }
    if next.owner_id.is_some() {
        current.owner_id = next.owner_id.clone();
    }
    if next.topic_id.is_some() {
        current.topic_id = next.topic_id.clone();
    }
    if next.parent_task_id.is_some() {
        current.parent_task_id = next.parent_task_id.clone();
    }
    if next.source_message_event_id.is_some() {
        current.source_message_event_id = next.source_message_event_id.clone();
    }
    if next.status_detail.is_some() {
        current.status_detail = next.status_detail.clone();
    }
    if next.last_event_id >= current.last_event_id {
        current.status = next.status;
        current.last_event_id = next.last_event_id;
        current.last_event_type = next.last_event_type.clone();
        current.last_event_at = next.last_event_at.clone();
    }
}

fn merge_message_backfill(current: &mut RecoveredTaskRun, next: &RecoveredTaskRun) {
    if current.task.is_none() && next.task.is_some() {
        current.task = next.task.clone();
    }
    if current.session_key.is_none() && next.session_key.is_some() {
        current.session_key = next.session_key.clone();
    }
    if current.owner_id.is_none() && next.owner_id.is_some() {
        current.owner_id = next.owner_id.clone();
    }
    if current.topic_id.is_none() && next.topic_id.is_some() {
        current.topic_id = next.topic_id.clone();
    }
    if current.parent_task_id.is_none() && next.parent_task_id.is_some() {
        current.parent_task_id = next.parent_task_id.clone();
    }
    if current.source_message_event_id.is_none() && next.source_message_event_id.is_some() {
        current.source_message_event_id = next.source_message_event_id.clone();
    }
    if current.status_detail.is_none() && next.status_detail.is_some() {
        current.status_detail = next.status_detail.clone();
    }
}

fn is_task_lifecycle_event(event_type: &str) -> bool {
    event_type == "task.spawned"
        || event_type == "task.steered"
        || event_type == "task.completed"
        || event_type == "task.failed"
        || event_type == "task.killed"
        || event_type == "xin.task.created"
        || event_type == "xin.task.spawned"
        || event_type == "xin.task.updated"
        || event_type == "xin.task.claimed"
        || event_type == "xin.task.completed"
        || event_type == "xin.task.failed"
        || event_type == "xin.task.stale"
        || event_type == "xin.task.rescheduled"
        || event_type == "xin.task.removed"
        || event_type == "xin.task.run_recorded"
        || event_type == "cron.job.created"
        || event_type == "cron.job.updated"
        || event_type == "cron.job.claimed"
        || event_type == "cron.job.completed"
        || event_type == "cron.job.failed"
        || event_type == "cron.job.removed"
        || event_type == "cron.job.run_recorded"
        || event_type.ends_with(".task.started")
        || event_type.ends_with(".task.completed")
        || event_type.ends_with(".task.failed")
        || event_type.ends_with(".task.timeout")
        || event_type.ends_with(".task.cancel_requested")
}

fn status_from_event_type(event_type: &str) -> RecoveredTaskStatus {
    if event_type == "task.completed"
        || event_type == "xin.task.completed"
        || event_type == "cron.job.completed"
        || event_type.ends_with(".task.completed")
    {
        RecoveredTaskStatus::Completed
    } else if event_type == "task.failed"
        || event_type == "task.killed"
        || event_type == "xin.task.failed"
        || event_type == "xin.task.removed"
        || event_type == "cron.job.failed"
        || event_type == "cron.job.removed"
        || event_type.ends_with(".task.failed")
        || event_type.ends_with(".task.timeout")
    {
        RecoveredTaskStatus::Failed
    } else {
        RecoveredTaskStatus::Running
    }
}

fn status_detail_from_payload(payload: Option<&Value>) -> Option<String> {
    let payload = payload?;
    payload
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| payload.get("result_preview").and_then(Value::as_str))
        .or_else(|| payload.get("status").and_then(Value::as_str))
        .map(str::to_string)
}

fn is_task_lineage_message_event(event: &MessageEvent) -> bool {
    if event.run_id.as_deref().is_none_or(str::is_empty) {
        return false;
    }
    matches!(
        event.source.as_str(),
        "sessions_spawn" | "delegate" | "session_worker" | "subagents" | "nodes"
    )
}

fn status_from_message_event(event: &MessageEvent, payload: Option<&Value>) -> RecoveredTaskStatus {
    if let Some(success) = payload
        .and_then(|payload| payload.get("success"))
        .and_then(Value::as_bool)
    {
        return if success {
            RecoveredTaskStatus::Completed
        } else {
            RecoveredTaskStatus::Failed
        };
    }
    match event.role.as_str() {
        "event" | "assistant" => RecoveredTaskStatus::Completed,
        _ => RecoveredTaskStatus::Running,
    }
}

fn truncate_status_detail(content: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut detail = content.chars().take(MAX_CHARS).collect::<String>();
    if content.chars().count() > MAX_CHARS {
        detail.push_str("...");
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryEventInput, MemoryVisibility, MessageEventInput, SqliteMemory};
    use serde_json::json;

    fn principal_args() -> Value {
        json!({})
    }

    fn task_event(workspace_id: &str, event_type: &str, subject_id: &str, task: &str) -> MemoryEventInput {
        MemoryEventInput {
            event_id: None,
            workspace_id: workspace_id.to_string(),
            event_type: event_type.to_string(),
            subject_table: "tasks".to_string(),
            subject_id: subject_id.to_string(),
            session_key: Some("test-session".to_string()),
            run_id: Some(subject_id.to_string()),
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            visibility: MemoryVisibility::Workspace,
            payload_json: Some(json!({ "task": task }).to_string()),
        }
    }

    #[tokio::test]
    async fn recover_task_runs_returns_most_recent_events_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        for i in 0..2000 {
            memory
                .append_memory_event(task_event("/tmp", "task.spawned", &format!("old-run-{i}"), "old task"))
                .await
                .unwrap();
        }
        for i in 0..5 {
            memory
                .append_memory_event(task_event(
                    "/tmp",
                    "task.spawned",
                    &format!("tail-run-{i}"),
                    &format!("tail task {i}"),
                ))
                .await
                .unwrap();
        }

        let runs = recover_task_runs(Some(&memory), "/tmp", &principal_args(), 100)
            .await
            .unwrap();
        assert!(runs.iter().any(|run| run.run_id == "tail-run-4"));
        assert!(runs.iter().any(|run| run.run_id == "tail-run-0"));
        assert!(!runs.iter().any(|run| run.run_id == "old-run-0"));
    }

    #[tokio::test]
    async fn recover_task_runs_recovers_from_message_events_without_task_events() {
        let tmp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .append_message_event(MessageEventInput {
                event_id: None,
                idempotency_key: None,
                workspace_id: "/tmp".to_string(),
                owner_id: Some("owner-a".to_string()),
                source: "sessions_spawn".to_string(),
                channel: Some("telegram".to_string()),
                session_key: Some("telegram:chat:alice".to_string()),
                parent_session_key: None,
                run_id: Some("message-only-run".to_string()),
                parent_run_id: Some("parent-run".to_string()),
                agent_id: None,
                persona_id: None,
                sender: Some("alice".to_string()),
                recipient: None,
                role: "user".to_string(),
                content: "message-only task".to_string(),
                raw_payload_json: Some(
                    json!({
                        "topic_id": "topic-a",
                        "parent_task_id": "parent-run",
                        "source_message_event_id": "msg-a"
                    })
                    .to_string(),
                ),
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .unwrap();

        let runs = recover_task_runs(Some(&memory), "/tmp", &principal_args(), 10)
            .await
            .unwrap();
        let run = runs
            .iter()
            .find(|run| run.run_id == "message-only-run")
            .expect("message-only run should be recovered");
        assert_eq!(run.task.as_deref(), Some("message-only task"));
        assert_eq!(run.owner_id.as_deref(), Some("owner-a"));
        assert_eq!(run.topic_id.as_deref(), Some("topic-a"));
        assert_eq!(run.parent_task_id.as_deref(), Some("parent-run"));
    }

    #[test]
    fn recognizes_xin_and_cron_lifecycle_events() {
        assert!(is_task_lifecycle_event("xin.task.spawned"));
        assert!(is_task_lifecycle_event("xin.task.completed"));
        assert!(is_task_lifecycle_event("cron.job.created"));
        assert!(is_task_lifecycle_event("cron.job.failed"));
        assert_eq!(
            status_from_event_type("xin.task.completed"),
            RecoveredTaskStatus::Completed
        );
        assert_eq!(status_from_event_type("cron.job.failed"), RecoveredTaskStatus::Failed);
    }
}
