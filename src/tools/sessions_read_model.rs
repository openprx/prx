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
    let owner_scope = RunOwnerScope::from_args(args);
    let principal = principal_from_args(args, workspace_id);
    let scan_limit = MEMORY_EVENT_SCAN_LIMIT.max(limit.saturating_mul(20)).max(1);
    let events = memory.list_memory_events_recent(&principal, scan_limit).await?;
    let message_events = memory
        .list_message_events_recent(&principal, MESSAGE_EVENT_SCAN_LIMIT.max(limit.saturating_mul(10)))
        .await?;
    Ok(task_runs_from_events_and_messages(
        owner_scope.retain_admitted(events, |event| event.owner_id.as_deref()),
        owner_scope.retain_admitted(message_events, |event| event.owner_id.as_deref()),
        limit,
    ))
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
    let owner_scope = RunOwnerScope::from_args(args);
    let principal = principal_from_args(args, workspace_id);
    let events = memory
        .list_memory_events_recent(&principal, MEMORY_EVENT_SCAN_LIMIT)
        .await?;
    let message_events = memory
        .list_message_events_recent(&principal, MESSAGE_EVENT_SCAN_LIMIT.max(limit))
        .await?;
    let Some(run) = task_runs_from_events_and_messages(
        owner_scope.retain_admitted(events, |event| event.owner_id.as_deref()),
        owner_scope.retain_admitted(message_events, |event| event.owner_id.as_deref()),
        MEMORY_EVENT_SCAN_LIMIT,
    )
    .into_iter()
    .find(|run| run.run_id == run_id) else {
        return Ok(None);
    };

    let mut message_principal = principal;
    message_principal.session_key = run.session_key.clone().or(message_principal.session_key);
    let transcript = memory
        .list_message_events_recent(&message_principal, MESSAGE_EVENT_SCAN_LIMIT.max(limit))
        .await?;
    let mut messages = owner_scope
        .retain_admitted(transcript, |event| event.owner_id.as_deref())
        .into_iter()
        .filter(|event| event.run_id.as_deref() == Some(run_id) || event.parent_run_id.as_deref() == Some(run_id))
        .collect::<Vec<_>>();
    messages.sort_by_key(|a| a.id);
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
        // C03 / V02-R4: for an externally authenticated origin the session key
        // (`mcp:{subject}`) and the sender are the caller-chosen subject alone,
        // so `session`/`private` admission must also match the owner. The marker
        // comes from the service-authored scope, never from caller arguments.
        require_owner_match: crate::tools::execution::scope_requires_owner_match(args),
        legacy_session_key: None,
    }
}

/// The confinement a caller runs under when it reads or acts on sub-agent runs:
/// the shared in-process run registry and the recovered projection behind it.
///
/// `sessions_spawn` records a run's task string, its request message and its
/// sub-agent result message at `workspace` visibility, and the registry is a
/// process-global `Vec<SubAgentRun>` with no query layer, so neither SQL nor
/// the registry separates owners. This is the one rule every run surface
/// applies instead (OWNER-VIS; D-016 OQ-19, D-019 Q-5): a run is visible to
/// the owner that started it, and the local-process operator sees them all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunOwnerScope {
    /// Every run: the local-process operator (terminal UI and `prx agent`
    /// CLI), and in-process calls that bypass the execution service and so
    /// carry no trusted scope at all.
    Unrestricted,
    /// Only runs registered under exactly this owner. `None` means the caller
    /// has no resolvable owner, so nothing is admitted.
    Owner(Option<String>),
}

/// The owner a trusted scope acts for: the authenticated `owner_id` the
/// runtime attached, else the `owner:{workspace}:{channel}:{sender}` identity
/// [`crate::memory::principal::OwnerPrincipal::new`] composes from the same
/// scope — the identity a run started from such a scope has always been
/// tagged with through its spawn envelope. The writer (`sessions_spawn` run
/// tagging) and the reader ([`RunOwnerScope::from_args`]) both call this, so
/// they cannot disagree about who owns a run.
///
/// `None` for an untrusted scope, or one without the workspace, channel or
/// sender to compose from.
pub(crate) fn effective_run_owner(args: &Value) -> Option<String> {
    if let Some(context) = crate::tools::execution::current_child_call_context() {
        let context = context.context();
        return effective_run_owner_from_context(context);
    }
    if !args.get("_zc_scope_trusted").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let scope = args.get("_zc_scope").and_then(Value::as_object)?;
    let field = |key: &str| {
        scope
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    if let Some(owner) = field("owner_id") {
        return Some(owner.to_string());
    }
    Some(
        crate::memory::principal::OwnerPrincipal::new(
            field("workspace_id")?,
            field("channel")?,
            field("sender")?,
            String::new(),
            vec![crate::memory::principal::Role::Anonymous],
        )
        .owner_id,
    )
}

pub(crate) fn effective_run_owner_from_context(context: &crate::tools::ToolExecutionContext) -> Option<String> {
    if let Some(owner) = context
        .envelope
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
    {
        return Some(owner.to_owned());
    }
    if context.envelope.owner_isolated {
        return None;
    }
    let workspace = context.envelope.workspace_id.trim();
    let channel = context.channel().trim();
    let sender = context.sender().trim();
    if workspace.is_empty() || channel.is_empty() || sender.is_empty() {
        return None;
    }
    Some(
        crate::memory::principal::OwnerPrincipal::new(
            workspace,
            channel,
            sender,
            String::new(),
            vec![crate::memory::principal::Role::Anonymous],
        )
        .owner_id,
    )
}

impl RunOwnerScope {
    /// Derive the confinement from the service-authored scope. The isolation
    /// marker, the operator tuple and the owner id are all read from
    /// `_zc_scope` / `_prx_scope_trusted`, which the execution service
    /// rewrites from the authenticated envelope, so caller-supplied arguments
    /// cannot widen (or narrow) this.
    ///
    /// - owner-isolated (external MCP): its authenticated owner, unchanged;
    /// - no trusted scope: [`Self::Unrestricted`] — only an in-process call
    ///   that bypasses `ToolExecutionService` gets here;
    /// - the local-process operator
    ///   ([`crate::tools::execution::scope_is_local_process_operator`]): every
    ///   run;
    /// - every other trusted origin — gateway and webhook turns, the Web
    ///   Console, IM channels, sub-agent workers — its [`effective_run_owner`].
    pub(crate) fn from_args(args: &Value) -> Self {
        if let Some(context) = crate::tools::execution::current_child_call_context() {
            let context = context.context();
            if !context.envelope.owner_isolated && context.is_local_operator() {
                return Self::Unrestricted;
            }
            return Self::Owner(effective_run_owner_from_context(context));
        }
        if crate::tools::execution::scope_requires_owner_match(args) {
            return Self::Owner(
                args.get("_zc_scope")
                    .and_then(Value::as_object)
                    .and_then(|scope| scope.get("owner_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|owner| !owner.is_empty())
                    .map(str::to_string),
            );
        }
        if !args.get("_zc_scope_trusted").and_then(Value::as_bool).unwrap_or(false)
            || crate::tools::execution::scope_is_local_process_operator(args)
        {
            return Self::Unrestricted;
        }
        Self::Owner(effective_run_owner(args))
    }

    /// Whether a registry entry owned by `run_owner_id` is visible.
    ///
    /// A confined caller never sees an unowned run: the registry cannot prove
    /// such a run is the caller's, so it fails closed. Runs recorded before
    /// OWNER-VIS without an owner are therefore visible to the operator only.
    pub(crate) fn admits(&self, run_owner_id: Option<&str>) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Owner(Some(caller)) => run_owner_id.is_some_and(|owner| owner == caller),
            Self::Owner(None) => false,
        }
    }

    /// Owner-qualify already-fetched projection rows.
    ///
    /// The read model is handed whole rows by the `Memory` trait, not a query
    /// it can extend, so the confinement is applied to the rows' own `owner_id`
    /// column here — the one seam `sessions_list` and `sessions_history` share.
    /// The rule is [`Self::admits`] unchanged: a caller with no owner id admits
    /// nothing, and a row with no owner is unattributable and admits to nobody
    /// isolated.
    ///
    /// An unrestricted caller keeps the vector it was given, so the operator's
    /// view renders byte-identical output.
    fn retain_admitted<T>(&self, rows: Vec<T>, owner_of: impl Fn(&T) -> Option<&str>) -> Vec<T> {
        match self {
            Self::Unrestricted => rows,
            Self::Owner(_) => rows.into_iter().filter(|row| self.admits(owner_of(row))).collect(),
        }
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
    if event_type.starts_with("xin.task.") || event_type.starts_with("cron.job.") {
        return false;
    }
    event_type == "task.spawned"
        || event_type == "task.steered"
        || event_type == "task.completed"
        || event_type == "task.failed"
        || event_type == "task.killed"
        || event_type.ends_with(".task.started")
        || event_type.ends_with(".task.completed")
        || event_type.ends_with(".task.failed")
        || event_type.ends_with(".task.timeout")
        || event_type.ends_with(".task.cancel_requested")
}

fn status_from_event_type(event_type: &str) -> RecoveredTaskStatus {
    if event_type == "task.completed" || event_type.ends_with(".task.completed") {
        RecoveredTaskStatus::Completed
    } else if event_type == "task.failed"
        || event_type == "task.killed"
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
            owner_id: None,
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
                source: "sessions_spawn".into(),
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
                event_type: "message.created".to_string(),
                subject: None,
                goal_id: None,
                causation_event_id: None,
                correlation_id: None,
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: Some(0),
                config_source_revision: None,
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

    const OWNER_A: &str = "owner:/tmp:agent:issuer-a.example:shared-subject";
    const OWNER_B: &str = "owner:/tmp:agent:issuer-b.example:shared-subject";

    /// The scope an owner-isolated caller reaches this module with. The marker
    /// and the owner id are both service-authored: `_zc_scope_trusted` is set
    /// by the execution service, never by the model.
    fn isolated_args(owner_id: Option<&str>) -> Value {
        let mut scope = serde_json::Map::new();
        scope.insert("owner_isolated".to_string(), Value::Bool(true));
        if let Some(owner_id) = owner_id {
            scope.insert("owner_id".to_string(), Value::String(owner_id.to_string()));
        }
        json!({ "_zc_scope_trusted": true, "_zc_scope": Value::Object(scope) })
    }

    /// One `workspace`-visibility sub-agent run, in the shapes `sessions_spawn`
    /// writes: a `task.spawned` lifecycle event and a role-`user` request
    /// message carrying the full task string, both attributed to `owner_id`.
    async fn seed_workspace_run(memory: &Arc<dyn Memory>, run_id: &str, owner_id: Option<&str>, task: &str) {
        memory
            .append_memory_event(MemoryEventInput {
                owner_id: owner_id.map(str::to_string),
                event_id: None,
                workspace_id: "/tmp".to_string(),
                event_type: "task.spawned".to_string(),
                subject_table: "tasks".to_string(),
                subject_id: run_id.to_string(),
                session_key: Some("test-session".to_string()),
                run_id: Some(run_id.to_string()),
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: Some(json!({ "task": task, "owner_id": owner_id }).to_string()),
            })
            .await
            .expect("test: append the task.spawned event");
        memory
            .append_message_event(MessageEventInput {
                event_id: None,
                idempotency_key: None,
                workspace_id: "/tmp".to_string(),
                owner_id: owner_id.map(str::to_string),
                source: "sessions_spawn".into(),
                channel: Some("gateway".to_string()),
                session_key: Some("test-session".to_string()),
                parent_session_key: None,
                run_id: Some(run_id.to_string()),
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                sender: Some("shared-subject".to_string()),
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
                content: task.to_string(),
                raw_payload_json: None,
                visibility: MemoryVisibility::Workspace,
            })
            .await
            .expect("test: append the request message event");
    }

    async fn seeded_memory() -> (tempfile::TempDir, Arc<dyn Memory>) {
        let tmp = tempfile::tempdir().expect("test: temp dir");
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
        seed_workspace_run(&memory, "run-a", Some(OWNER_A), "task owned by caller a").await;
        seed_workspace_run(&memory, "run-b", Some(OWNER_B), "task owned by caller b").await;
        seed_workspace_run(&memory, "run-null", None, "task with no owner").await;
        (tmp, memory)
    }

    /// F-1: `workspace`-visibility rows carry no owner predicate in SQL, so the
    /// read model owner-qualifies the rows it is handed.
    ///
    /// MUTATION GUARD: drop the `retain_admitted` calls in `recover_task_runs`
    /// and this goes red.
    #[tokio::test]
    async fn an_isolated_caller_recovers_only_the_runs_it_owns() {
        let (_tmp, memory) = seeded_memory().await;

        let runs = recover_task_runs(Some(&memory), "/tmp", &isolated_args(Some(OWNER_A)), 10)
            .await
            .expect("test: recover task runs");
        let ids = runs.iter().map(|run| run.run_id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["run-a"], "an isolated caller sees only its own run: {ids:?}");
        assert_eq!(
            runs.first().and_then(|run| run.task.as_deref()),
            Some("task owned by caller a"),
            "its own run is still fully recovered"
        );
    }

    /// A caller that is owner-isolated but carries no usable owner id reaches
    /// nothing at all, rather than falling back to the shared view.
    #[tokio::test]
    async fn an_isolated_caller_without_an_owner_id_recovers_nothing() {
        let (_tmp, memory) = seeded_memory().await;

        for scope in [isolated_args(None), isolated_args(Some("   "))] {
            let runs = recover_task_runs(Some(&memory), "/tmp", &scope, 10)
                .await
                .expect("test: recover task runs");
            assert!(runs.is_empty(), "fail closed on a missing owner id: {runs:?}");
        }
    }

    /// A non-isolated caller keeps the workspace-shared view, unchanged.
    #[tokio::test]
    async fn a_non_isolated_caller_recovers_every_run_in_the_workspace() {
        let (_tmp, memory) = seeded_memory().await;

        let runs = recover_task_runs(Some(&memory), "/tmp", &principal_args(), 10)
            .await
            .expect("test: recover task runs");
        let mut ids = runs.iter().map(|run| run.run_id.as_str()).collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(
            ids,
            ["run-a", "run-b", "run-null"],
            "a non-isolated caller is not confined: {ids:?}"
        );
    }

    /// The transcript half: `recover_run_history` must not hand another
    /// caller's run back, and must not hand back its messages.
    ///
    /// MUTATION GUARD: drop either `retain_admitted` call in
    /// `recover_run_history` and this goes red.
    #[tokio::test]
    async fn an_isolated_caller_recovers_only_its_own_run_history() {
        let (_tmp, memory) = seeded_memory().await;
        let scope = isolated_args(Some(OWNER_A));

        for denied in ["run-b", "run-null"] {
            let history = recover_run_history(Some(&memory), "/tmp", &scope, denied, 50)
                .await
                .expect("test: recover run history");
            assert!(
                history.is_none(),
                "`{denied}` must be indistinguishable from a run that does not exist"
            );
        }

        let own = recover_run_history(Some(&memory), "/tmp", &scope, "run-a", 50)
            .await
            .expect("test: recover run history")
            .expect("test: caller a must still recover its own run");
        assert_eq!(own.run.run_id, "run-a");
        assert!(
            own.messages
                .iter()
                .any(|event| event.content == "task owned by caller a"),
            "caller a must still read its own transcript: {:?}",
            own.messages
        );
        assert!(
            own.messages
                .iter()
                .all(|event| event.owner_id.as_deref() == Some(OWNER_A)),
            "no message from another owner may enter the transcript: {:?}",
            own.messages
        );
    }

    // ── OWNER-VIS: every caller but the local-process operator is confined ──

    /// The scope a trusted, non-isolated origin reaches this module with, in
    /// the shape `normalize_arguments` authors it.
    fn trusted_args(channel: &str, sender: &str, chat_type: &str, chat_id: &str, operator: bool) -> Value {
        json!({
            "_zc_scope_trusted": true,
            "_prx_scope_trusted": operator,
            "_zc_scope": {
                "channel": channel,
                "sender": sender,
                "chat_type": chat_type,
                "chat_id": chat_id,
                "workspace_id": "/tmp",
                "runtime_source": "chat",
            },
        })
    }

    fn webhook_args(sender: &str) -> Value {
        trusted_args(
            "webhook",
            sender,
            "gateway",
            &format!("gateway:webhook:{sender}"),
            false,
        )
    }

    /// The owner `sessions_spawn` tags a run started from `webhook_args(sender)`
    /// with: the owner its spawn envelope resolves.
    fn webhook_owner(sender: &str) -> String {
        crate::runtime::envelope::RuntimeEnvelope::sessions_spawn("/tmp", "test-session", "run")
            .with_channel("webhook")
            .with_sender(sender)
            .resolved_owner_id()
    }

    async fn owner_vis_memory() -> (tempfile::TempDir, Arc<dyn Memory>) {
        let (tmp, memory) = seeded_memory().await;
        seed_workspace_run(
            &memory,
            "run-mallory",
            Some(&webhook_owner("mallory")),
            "task owned by mallory",
        )
        .await;
        (tmp, memory)
    }

    /// OWNER-VIS OV-A: a gateway webhook turn — trusted, not isolated, no
    /// explicit owner id — used to recover every run in the workspace.
    ///
    /// MUTATION GUARD: return `Unrestricted` for every trusted non-isolated
    /// scope in `RunOwnerScope::from_args` and this goes red.
    #[tokio::test]
    async fn a_network_caller_recovers_only_the_runs_its_owner_started() {
        let (_tmp, memory) = owner_vis_memory().await;
        let mallory = webhook_args("mallory");

        let runs = recover_task_runs(Some(&memory), "/tmp", &mallory, 10)
            .await
            .expect("test: recover task runs");
        let ids = runs.iter().map(|run| run.run_id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["run-mallory"], "a webhook caller sees only its own run: {ids:?}");

        for denied in ["run-a", "run-b", "run-null"] {
            let history = recover_run_history(Some(&memory), "/tmp", &mallory, denied, 50)
                .await
                .expect("test: recover run history");
            assert!(history.is_none(), "`{denied}` must read as a run that does not exist");
        }
    }

    /// D-019 Q-5: the terminal UI and the `prx agent` CLI keep the
    /// workspace-wide view.
    #[tokio::test]
    async fn the_local_process_operator_recovers_every_run() {
        let (_tmp, memory) = owner_vis_memory().await;
        for operator in [
            trusted_args("terminal", "user", "private", "terminal:user", true),
            trusted_args("cli", "agent", "private", "agent:local", false),
        ] {
            let runs = recover_task_runs(Some(&memory), "/tmp", &operator, 10)
                .await
                .expect("test: recover task runs");
            let mut ids = runs.iter().map(|run| run.run_id.as_str()).collect::<Vec<_>>();
            ids.sort_unstable();
            assert_eq!(ids, ["run-a", "run-b", "run-mallory", "run-null"], "{operator}");
        }
    }

    /// `args` with the object field at `path` set to `value`.
    fn with_field(mut args: Value, path: &[&str], value: Value) -> Value {
        let Some((last, parents)) = path.split_last() else {
            return args;
        };
        let mut object = args.as_object_mut();
        for parent in parents {
            object = object
                .and_then(|map| map.get_mut(*parent))
                .and_then(Value::as_object_mut);
        }
        object
            .expect("test: every parent on the path is an object")
            .insert((*last).to_string(), value);
        args
    }

    /// Writer and reader agree: the owner a trusted scope is confined to is
    /// the owner a run started from it is tagged with.
    #[test]
    fn a_run_is_tagged_with_the_owner_its_reader_is_confined_to() {
        assert_eq!(
            effective_run_owner(&webhook_args("mallory")).as_deref(),
            Some(webhook_owner("mallory").as_str())
        );
        assert_eq!(
            RunOwnerScope::from_args(&webhook_args("mallory")),
            RunOwnerScope::Owner(Some(webhook_owner("mallory")))
        );

        let explicit = with_field(
            webhook_args("mallory"),
            &["_zc_scope", "owner_id"],
            json!(" owner:/tmp:telegram:alice "),
        );
        assert_eq!(
            effective_run_owner(&explicit).as_deref(),
            Some("owner:/tmp:telegram:alice"),
            "an authenticated owner id wins over the composed one"
        );

        let untrusted = with_field(webhook_args("mallory"), &["_zc_scope_trusted"], json!(false));
        assert_eq!(effective_run_owner(&untrusted), None);
        assert_eq!(RunOwnerScope::from_args(&untrusted), RunOwnerScope::Unrestricted);

        let senderless = with_field(webhook_args("mallory"), &["_zc_scope", "sender"], json!("  "));
        assert_eq!(effective_run_owner(&senderless), None);
        assert_eq!(
            RunOwnerScope::from_args(&senderless),
            RunOwnerScope::Owner(None),
            "a trusted caller with no resolvable owner admits nothing"
        );
    }

    #[test]
    fn excludes_scheduled_work_from_subagent_sessions() {
        assert!(!is_task_lifecycle_event("xin.task.spawned"));
        assert!(!is_task_lifecycle_event("xin.task.completed"));
        assert!(!is_task_lifecycle_event("cron.job.created"));
        assert!(!is_task_lifecycle_event("cron.job.failed"));
        assert!(is_task_lifecycle_event("task.spawned"));
        assert!(is_task_lifecycle_event("delegate.task.completed"));
    }
}
