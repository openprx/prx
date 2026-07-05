//! subagents — manage spawned sub-agent runs.
//!
//! Aligns with OpenClaw's `subagents` interface:
//! - list: list active/recent sub-agent runs
//! - kill: terminate a running sub-agent
//! - steer: send a message to a running sub-agent

use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric, MessageEventScope};
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

const DEFAULT_LIMIT: usize = 20;

/// Manage running/recent sub-agent sessions created by `sessions_spawn`.
pub struct SubagentsTool {
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    security: Arc<SecurityPolicy>,
    memory: Option<Arc<dyn Memory>>,
    event_recording: MemoryEventRecording,
}

impl SubagentsTool {
    pub fn new(active_runs: Arc<RwLock<Vec<SubAgentRun>>>) -> Self {
        Self::with_security(active_runs, Arc::new(SecurityPolicy::default()))
    }

    pub fn with_security(active_runs: Arc<RwLock<Vec<SubAgentRun>>>, security: Arc<SecurityPolicy>) -> Self {
        Self {
            active_runs,
            security,
            memory: None,
            event_recording: MemoryEventRecording::default(),
        }
    }

    pub fn with_shared_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub const fn with_event_recording(mut self, event_recording: MemoryEventRecording) -> Self {
        self.event_recording = event_recording;
        self
    }

    fn memory_fabric(&self) -> Option<MemoryFabric> {
        self.memory.as_ref().map(|memory| {
            MemoryFabric::new(memory.clone(), self.security.workspace_dir.to_string_lossy())
                .with_event_recording(self.event_recording)
        })
    }

    async fn record_run_task_event(&self, run: &SubAgentRun, event_type: &str, detail: serde_json::Value) {
        let Some(fabric) = self.memory_fabric() else {
            return;
        };
        let mut scope = MessageEventScope::new("subagents", crate::memory::MemoryVisibility::Workspace)
            .with_session_key(run.session_scope_key.clone())
            .with_run_id(run.id.clone());
        if let Some(owner_id) = run.owner_id.as_deref() {
            scope = scope.with_owner_id(owner_id);
        }
        if let Some(parent_run_id) = run.parent_run_id.as_deref() {
            scope = scope.with_parent_run_id(parent_run_id);
        }
        let payload = json!({
            "task": run.task,
            "status": status_label(&run.status),
            "owner_id": run.owner_id,
            "topic_id": run.topic_id,
            "parent_task_id": run.parent_run_id,
            "source_message_event_id": run.source_message_event_id,
            "detail": detail
        });
        if let Err(error) = fabric
            .record_task_event(scope, run.id.clone(), event_type.to_string(), Some(payload.to_string()))
            .await
        {
            tracing::warn!(run_id = %run.id, event_type, "failed to record subagents task event: {error}");
        }
    }

    async fn execute_list(&self, status_filter: &str, limit: usize) -> anyhow::Result<ToolResult> {
        let runs = self.active_runs.read().await;
        let filtered: Vec<&SubAgentRun> = runs
            .iter()
            .filter(|r| match status_filter {
                "running" => matches!(r.status, SubAgentStatus::Running),
                "completed" => matches!(r.status, SubAgentStatus::Completed(_)),
                "failed" => matches!(r.status, SubAgentStatus::Failed(_)),
                _ => true,
            })
            .take(limit)
            .collect();

        if filtered.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No subagents found (filter: {status_filter})."),
                error: None,
            });
        }

        let lines: Vec<String> = filtered
            .iter()
            .map(|r| {
                let status = match &r.status {
                    SubAgentStatus::Running => "running".to_string(),
                    SubAgentStatus::AwaitingInput { prompt } => {
                        format!("awaiting approval: {prompt}")
                    }
                    SubAgentStatus::Completed(msg) => {
                        let preview = msg.chars().take(60).collect::<String>();
                        let ellipsis = if msg.len() > 60 { "…" } else { "" };
                        format!("completed: {preview}{ellipsis}")
                    }
                    SubAgentStatus::Failed(e) => format!("failed: {e}"),
                };
                let age = (Utc::now() - r.started_at).num_seconds();
                format!("• `{}` [{age}s ago] {status}\n  task: {}", r.id, r.task)
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: format!(
                "Subagents ({} shown, filter: {}):\n\n{}",
                filtered.len(),
                status_filter,
                lines.join("\n\n")
            ),
            error: None,
        })
    }

    async fn execute_kill(&self, run_id: &str, approval_grant: Option<&ApprovalGrant>) -> anyhow::Result<ToolResult> {
        let mut runs = self.active_runs.write().await;
        let Some(run) = runs.iter_mut().find(|r| r.id == run_id) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No subagent found with ID `{run_id}`.")),
            });
        };

        match &run.status {
            SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {
                let operation_name = format!("subagents:kill:{run_id}");
                if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
                    self.name(),
                    &operation_name,
                    ResourceRiskLevel::Medium,
                    approval_grant,
                ) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(error),
                    });
                }
                if let Some(ref abort_handle) = run.abort_handle {
                    abort_handle.abort();
                }
                run.status = SubAgentStatus::Failed("killed by operator".into());
                run.steer_tx = None;
                let event_run = run.clone();
                drop(runs);
                self.record_run_task_event(&event_run, "task.killed", json!({ "operation": "subagents.kill" }))
                    .await;

                Ok(ToolResult {
                    success: true,
                    output: format!("Subagent `{run_id}` terminated."),
                    error: None,
                })
            }
            SubAgentStatus::Completed(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Subagent `{run_id}` already completed.")),
            }),
            SubAgentStatus::Failed(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Subagent `{run_id}` already failed: {e}")),
            }),
        }
    }

    async fn execute_steer(
        &self,
        run_id: &str,
        message: &str,
        approval_grant: Option<&ApprovalGrant>,
    ) -> anyhow::Result<ToolResult> {
        let steered_run = {
            let runs = self.active_runs.read().await;
            let Some(run) = runs.iter().find(|r| r.id == run_id) else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("No subagent found with ID `{run_id}`.")),
                });
            };

            match &run.status {
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {
                    if let Some(ref tx) = run.steer_tx {
                        let operation_name = format!("subagents:steer:{run_id}");
                        if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
                            self.name(),
                            &operation_name,
                            ResourceRiskLevel::Low,
                            approval_grant,
                        ) {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(error),
                            });
                        }
                        tx.send(message.to_string())
                            .map_err(|_| anyhow::anyhow!("Subagent message channel closed"))?;
                        run.clone()
                    } else {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Subagent `{run_id}` is running but cannot be steered.")),
                        });
                    }
                }
                SubAgentStatus::Completed(_) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Subagent `{run_id}` already completed; cannot steer.")),
                    });
                }
                SubAgentStatus::Failed(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Subagent `{run_id}` already failed ({e}); cannot steer.")),
                    });
                }
            }
        };
        self.record_run_task_event(
            &steered_run,
            "task.steered",
            json!({
                "operation": "subagents.steer",
                "message_preview": message.chars().take(500).collect::<String>()
            }),
        )
        .await;
        Ok(ToolResult {
            success: true,
            output: format!("Message sent to subagent `{run_id}`."),
            error: None,
        })
    }
}

const fn status_label(status: &SubAgentStatus) -> &'static str {
    match status {
        SubAgentStatus::Running => "running",
        SubAgentStatus::AwaitingInput { .. } => "awaiting-input",
        SubAgentStatus::Completed(_) => "completed",
        SubAgentStatus::Failed(_) => "failed",
    }
}

#[async_trait]
impl Tool for SubagentsTool {
    fn name(&self) -> &str {
        "subagents"
    }

    fn description(&self) -> &str {
        "Manage sub-agents spawned by sessions_spawn. Actions: list, kill, steer."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "kill", "steer"],
                    "default": "list",
                    "description": "Action to perform."
                },
                "run_id": {
                    "type": "string",
                    "description": "Target run ID for kill/steer."
                },
                "message": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Message to send for steer action."
                },
                "status": {
                    "type": "string",
                    "enum": ["all", "running", "completed", "failed"],
                    "default": "all",
                    "description": "Optional status filter for list action."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Maximum number of results for list action (default 20)."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("list");
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
        match action {
            "list" => {
                let status = args.get("status").and_then(|v| v.as_str()).unwrap_or("all");
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map_or(DEFAULT_LIMIT, |v| usize::try_from(v).unwrap_or(DEFAULT_LIMIT));
                self.execute_list(status, limit).await
            }
            "kill" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for kill action"))?;
                self.execute_kill(run_id, approval_grant.as_ref()).await
            }
            "steer" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for steer action"))?;
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter for steer action"))?;
                self.execute_steer(run_id, message, approval_grant.as_ref()).await
            }
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unsupported action '{action}'. Valid actions: list, kill, steer."
                )),
            }),
        }
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryPrincipal, SqliteMemory};
    use chrono::Utc;

    fn make_run(id: &str, status: SubAgentStatus) -> SubAgentRun {
        SubAgentRun {
            id: id.to_string(),
            task: "test-task".to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            finished_at: None,
            status,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
            token_usage_records: Vec::new(),
        }
    }

    #[test]
    fn name_is_subagents() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        assert_eq!(tool.name(), "subagents");
    }

    #[tokio::test]
    async fn list_empty_succeeds() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let result = tool.execute(json!({"action":"list"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No subagents"));
    }

    #[tokio::test]
    async fn kill_running_marks_failed() {
        let run = make_run("run-1", SubAgentStatus::Running);
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SubagentsTool::new(runs.clone());

        let result = tool
            .execute(json!({
                "action":"kill",
                "run_id":"run-1",
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                    serde_json::to_value(ApprovalGrant::for_resource_operation(
                        "subagents",
                        "subagents:kill:run-1",
                        "test",
                        None
                    )).unwrap()
            }))
            .await
            .unwrap();
        assert!(result.success);

        let guard = runs.read().await;
        let run = guard.iter().find(|r| r.id == "run-1").unwrap();
        assert!(matches!(run.status, SubAgentStatus::Failed(_)));
    }

    #[tokio::test]
    async fn kill_running_records_task_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let mut run = make_run("run-kill-ledger", SubAgentStatus::Running);
        run.owner_id = Some("owner-a".to_string());
        run.topic_id = Some("topic-a".to_string());
        run.source_message_event_id = Some("msg-a".to_string());
        run.parent_run_id = Some("parent-a".to_string());
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SubagentsTool::with_security(
            runs.clone(),
            Arc::new(crate::security::SecurityPolicy {
                workspace_dir: tmp.path().to_path_buf(),
                ..crate::security::SecurityPolicy::default()
            }),
        )
        .with_shared_memory(memory.clone());

        let result = tool
            .execute(json!({
                "action":"kill",
                "run_id":"run-kill-ledger",
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                    serde_json::to_value(ApprovalGrant::for_resource_operation(
                        "subagents",
                        "subagents:kill:run-kill-ledger",
                        "test",
                        None
                    )).unwrap()
            }))
            .await
            .unwrap();
        assert!(result.success);

        let events = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("test-session".to_string()),
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "task.killed");
        assert_eq!(events[0].subject_table, "tasks");
        assert_eq!(events[0].subject_id, "run-kill-ledger");
        let payload = events[0].payload_json.as_deref().unwrap_or_default();
        assert!(payload.contains("\"operation\":\"subagents.kill\""));
        assert!(payload.contains("\"source_message_event_id\":\"msg-a\""));
    }

    #[tokio::test]
    async fn kill_running_requires_resource_grant() {
        let run = make_run("run-1", SubAgentStatus::Running);
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SubagentsTool::new(runs.clone());

        let result = tool.execute(json!({"action":"kill","run_id":"run-1"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("runtime approval grant"));

        let guard = runs.read().await;
        let run = guard.iter().find(|r| r.id == "run-1").unwrap();
        assert!(matches!(run.status, SubAgentStatus::Running));
    }

    #[tokio::test]
    async fn steer_running_sends_message() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let run = SubAgentRun {
            id: "run-2".into(),
            task: "task".into(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            finished_at: None,
            status: SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: Some(tx),
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
            token_usage_records: Vec::new(),
        };
        let tool = SubagentsTool::new(Arc::new(RwLock::new(vec![run])));

        let result = tool
            .execute(json!({"action":"steer","run_id":"run-2","message":"pivot"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(rx.try_recv().unwrap(), "pivot");
    }

    #[tokio::test]
    async fn steer_running_records_task_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let run = SubAgentRun {
            id: "run-steer-ledger".into(),
            task: "task".into(),
            owner_id: Some("owner-b".to_string()),
            topic_id: Some("topic-b".to_string()),
            source_message_event_id: Some("msg-b".to_string()),
            started_at: Utc::now(),
            finished_at: None,
            status: SubAgentStatus::Running,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: Some(tx),
            parent_run_id: Some("parent-b".to_string()),
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
            token_usage_records: Vec::new(),
        };
        let tool = SubagentsTool::with_security(
            Arc::new(RwLock::new(vec![run])),
            Arc::new(crate::security::SecurityPolicy {
                workspace_dir: tmp.path().to_path_buf(),
                ..crate::security::SecurityPolicy::default()
            }),
        )
        .with_shared_memory(memory.clone());

        let result = tool
            .execute(json!({"action":"steer","run_id":"run-steer-ledger","message":"pivot with ledger"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(rx.try_recv().unwrap(), "pivot with ledger");

        let events = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: tmp.path().to_string_lossy().to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("test-session".to_string()),
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "task.steered");
        assert_eq!(events[0].subject_table, "tasks");
        assert_eq!(events[0].subject_id, "run-steer-ledger");
        let payload = events[0].payload_json.as_deref().unwrap_or_default();
        assert!(payload.contains("\"operation\":\"subagents.steer\""));
        assert!(payload.contains("\"source_message_event_id\":\"msg-b\""));
    }

    // ── Metadata ────────────────────────────────────────────────

    #[test]
    fn description_non_empty() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn schema_has_action_enum() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let schema = tool.parameters_schema();
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("test: action enum");
        assert_eq!(actions.len(), 3);
    }

    // ── list with runs ──────────────────────────────────────────

    #[tokio::test]
    async fn list_with_runs_shows_count() {
        let runs = vec![
            make_run("r1", SubAgentStatus::Running),
            make_run("r2", SubAgentStatus::Completed("done".into())),
        ];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("2 shown"));
    }

    #[tokio::test]
    async fn list_filter_running() {
        let runs = vec![
            make_run("r1", SubAgentStatus::Running),
            make_run("r2", SubAgentStatus::Completed("done".into())),
            make_run("r3", SubAgentStatus::Failed("err".into())),
        ];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool
            .execute(json!({"action": "list", "status": "running"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 shown"));
        assert!(result.output.contains("r1"));
    }

    #[tokio::test]
    async fn list_filter_completed() {
        let runs = vec![
            make_run("r1", SubAgentStatus::Running),
            make_run("r2", SubAgentStatus::Completed("done".into())),
        ];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool
            .execute(json!({"action": "list", "status": "completed"}))
            .await
            .unwrap();
        assert!(result.output.contains("r2"));
        assert!(!result.output.contains("r1"));
    }

    #[tokio::test]
    async fn list_respects_limit() {
        let runs = vec![
            make_run("r1", SubAgentStatus::Running),
            make_run("r2", SubAgentStatus::Running),
            make_run("r3", SubAgentStatus::Running),
        ];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool.execute(json!({"action": "list", "limit": 2})).await.unwrap();
        assert!(result.output.contains("2 shown"));
    }

    // ── default action (no action param) → list ─────────────────

    #[tokio::test]
    async fn default_action_is_list() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No subagents"));
    }

    // ── kill edge cases ─────────────────────────────────────────

    #[tokio::test]
    async fn kill_nonexistent_fails() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let result = tool
            .execute(json!({"action": "kill", "run_id": "ghost"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("ghost"));
    }

    #[tokio::test]
    async fn kill_completed_fails() {
        let runs = vec![make_run("r1", SubAgentStatus::Completed("ok".into()))];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool.execute(json!({"action": "kill", "run_id": "r1"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("completed"));
    }

    #[tokio::test]
    async fn kill_failed_fails() {
        let runs = vec![make_run("r1", SubAgentStatus::Failed("err".into()))];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool.execute(json!({"action": "kill", "run_id": "r1"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("failed"));
    }

    #[tokio::test]
    async fn kill_missing_run_id_errors() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let err = tool.execute(json!({"action": "kill"})).await.unwrap_err();
        assert!(err.to_string().contains("run_id"));
    }

    // ── steer edge cases ────────────────────────────────────────

    #[tokio::test]
    async fn steer_nonexistent_fails() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let result = tool
            .execute(json!({"action": "steer", "run_id": "x", "message": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn steer_completed_fails() {
        let runs = vec![make_run("r1", SubAgentStatus::Completed("ok".into()))];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool
            .execute(json!({"action": "steer", "run_id": "r1", "message": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("completed"));
    }

    #[tokio::test]
    async fn steer_running_no_tx_fails() {
        // steer_tx is None
        let runs = vec![make_run("r1", SubAgentStatus::Running)];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool
            .execute(json!({"action": "steer", "run_id": "r1", "message": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("cannot be steered"));
    }

    #[tokio::test]
    async fn steer_missing_message_errors() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let err = tool
            .execute(json!({"action": "steer", "run_id": "r1"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("message"));
    }

    #[tokio::test]
    async fn steer_missing_run_id_errors() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let err = tool
            .execute(json!({"action": "steer", "message": "hi"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("run_id"));
    }

    // ── unknown action ──────────────────────────────────────────

    #[tokio::test]
    async fn unknown_action_fails() {
        let tool = SubagentsTool::new(Arc::new(RwLock::new(Vec::new())));
        let result = tool.execute(json!({"action": "nuke"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Unsupported"));
    }

    // ── Output formatting ───────────────────────────────────────

    #[tokio::test]
    async fn completed_run_shows_preview_with_ellipsis() {
        let long_msg = "a".repeat(100);
        let runs = vec![make_run("r1", SubAgentStatus::Completed(long_msg))];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(
            result.output.contains("…"),
            "long messages should be truncated with ellipsis"
        );
    }

    #[tokio::test]
    async fn failed_run_shows_error_reason() {
        let runs = vec![make_run("r1", SubAgentStatus::Failed("oom".into()))];
        let tool = SubagentsTool::new(Arc::new(RwLock::new(runs)));
        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(result.output.contains("failed: oom"));
    }
}
