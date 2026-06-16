//! sessions_send — send a steering message to a running sub-agent session.
//!
//! Exposes cross-session message injection as a dedicated tool,
//! aligning with OpenClaw's `sessions_send` capability.
//! Internally this invokes the same steer-channel mechanism as
//! the 'steer' action in sessions_spawn.

use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric, MessageEventScope};
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Tool that sends a message to a running sub-agent session to redirect it.
pub struct SessionsSendTool {
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    security: Arc<SecurityPolicy>,
    memory: Option<Arc<dyn Memory>>,
    event_recording: MemoryEventRecording,
}

impl SessionsSendTool {
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

    async fn record_steer_event(&self, run: &SubAgentRun, message: &str) {
        let Some(fabric) = self.memory_fabric() else {
            return;
        };
        let mut scope = MessageEventScope::new("sessions_send", crate::memory::MemoryVisibility::Workspace)
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
            "status": "running",
            "owner_id": run.owner_id,
            "topic_id": run.topic_id,
            "parent_task_id": run.parent_run_id,
            "source_message_event_id": run.source_message_event_id,
            "message_preview": message.chars().take(500).collect::<String>(),
            "operation": "sessions_send.steer"
        });
        if let Err(error) = fabric
            .record_task_event(scope, run.id.clone(), "task.steered", Some(payload.to_string()))
            .await
        {
            tracing::warn!(run_id = %run.id, "failed to record sessions_send task.steered event: {error}");
        }
    }
}

#[async_trait]
impl Tool for SessionsSendTool {
    fn name(&self) -> &str {
        "sessions_send"
    }

    fn description(&self) -> &str {
        "Send a message to a running sub-agent session to redirect or update its instructions. \
         Use this for cross-session communication: inject new context, pivot the task, \
         or provide additional information to a running sub-agent. \
         Use sessions_list to find the run_id of the target session."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "The run ID of the target sub-agent session (from sessions_list or sessions_spawn output)."
                },
                "message": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The message/instruction to send to the running sub-agent."
                }
            },
            "required": ["run_id", "message"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let run_id = args
            .get("run_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter"))?;

        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?;

        let operation_name = format!("sessions_send:steer:{run_id}");
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
        if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
            self.name(),
            &operation_name,
            ResourceRiskLevel::Low,
            approval_grant.as_ref(),
        ) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let steered_run = {
            let runs = self.active_runs.read().await;
            let Some(run) = runs.iter().find(|r| r.id == run_id) else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("No session found with ID `{run_id}`.")),
                });
            };

            match &run.status {
                // A suspended (AwaitingInput) run still has a live message channel,
                // so an operator message is delivered the same way as for a running
                // session.
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {
                    if let Some(ref tx) = run.steer_tx {
                        tx.send(message.to_string())
                            .map_err(|_| anyhow::anyhow!("Session message channel closed unexpectedly"))?;
                        run.clone()
                    } else {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "Session `{run_id}` is running but has no message channel available \
                                 (it may be a legacy run without steer support)."
                            )),
                        });
                    }
                }
                SubAgentStatus::Completed(_) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Session `{run_id}` already completed; cannot send message.")),
                    });
                }
                SubAgentStatus::Failed(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Session `{run_id}` already failed ({e}); cannot send message.")),
                    });
                }
            }
        };

        self.record_steer_event(&steered_run, message).await;
        Ok(ToolResult {
            success: true,
            output: format!(
                "Message sent to session `{run_id}`. \
                 The sub-agent will incorporate it at the next opportunity."
            ),
            error: None,
        })
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryPrincipal, SqliteMemory};
    use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};
    use chrono::Utc;

    fn make_running_run(id: &str) -> (SubAgentRun, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let run = SubAgentRun {
            id: id.to_string(),
            task: "some task".to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            status: SubAgentStatus::Running,
            recipient: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: Some(tx),
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
        };
        (run, rx)
    }

    #[test]
    fn name_and_description() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsSendTool::new(runs);
        assert_eq!(tool.name(), "sessions_send");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn missing_run_id_returns_error() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsSendTool::new(runs);
        let result = tool.execute(json!({"message": "hello"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_message_returns_error() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsSendTool::new(runs);
        let result = tool.execute(json!({"run_id": "abc"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unknown_run_id_returns_failure() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsSendTool::new(runs);
        let result = tool
            .execute(json!({"run_id": "nonexistent", "message": "hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No session found"));
    }

    #[tokio::test]
    async fn sends_to_running_session() {
        let (run, mut rx) = make_running_run("test-run-123");
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsSendTool::new(runs);

        let result = tool
            .execute(json!({"run_id": "test-run-123", "message": "pivot to something else"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("test-run-123"));

        // Verify the message was actually sent
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg, "pivot to something else");
    }

    #[tokio::test]
    async fn sends_to_running_session_records_task_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let (mut run, mut rx) = make_running_run("test-run-ledger");
        run.owner_id = Some("owner-a".to_string());
        run.topic_id = Some("topic-a".to_string());
        run.source_message_event_id = Some("msg-a".to_string());
        run.parent_run_id = Some("parent-a".to_string());
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsSendTool::with_security(
            runs,
            Arc::new(crate::security::SecurityPolicy {
                workspace_dir: tmp.path().to_path_buf(),
                ..crate::security::SecurityPolicy::default()
            }),
        )
        .with_shared_memory(memory.clone());

        let result = tool
            .execute(json!({"run_id": "test-run-ledger", "message": "pivot with ledger"}))
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
        let event = events.first().expect("task event should be recorded");
        assert_eq!(event.event_type, "task.steered");
        assert_eq!(event.subject_table, "tasks");
        assert_eq!(event.subject_id, "test-run-ledger");
        let payload = event.payload_json.as_deref().unwrap_or_default();
        assert!(payload.contains("\"operation\":\"sessions_send.steer\""));
        assert!(payload.contains("\"source_message_event_id\":\"msg-a\""));
    }

    #[tokio::test]
    async fn send_obeys_readonly_resource_gate() {
        let (run, mut rx) = make_running_run("test-run-123");
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsSendTool::with_security(
            runs,
            Arc::new(crate::security::SecurityPolicy {
                autonomy: crate::security::policy::AutonomyLevel::ReadOnly,
                ..crate::security::SecurityPolicy::default()
            }),
        );

        let result = tool
            .execute(json!({"run_id": "test-run-123", "message": "pivot"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only mode"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn completed_session_returns_failure() {
        let run = SubAgentRun {
            id: "done-run".to_string(),
            task: "finished task".to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            status: SubAgentStatus::Completed("result".to_string()),
            recipient: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
        };
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsSendTool::new(runs);
        let result = tool
            .execute(json!({"run_id": "done-run", "message": "too late"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("already completed"));
    }
}
