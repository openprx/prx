//! sessions_send — send a steering message to a running sub-agent session.
//!
//! Exposes cross-session message injection as a dedicated tool,
//! aligning with OpenClaw's `sessions_send` capability.
//! Internally this invokes the same steer-channel mechanism as
//! the 'steer' action in sessions_spawn.

use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Tool that sends a message to a running sub-agent session to redirect it.
pub struct SessionsSendTool {
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
}

impl SessionsSendTool {
    pub const fn new(active_runs: Arc<RwLock<Vec<SubAgentRun>>>) -> Self {
        Self { active_runs }
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

        let runs = self.active_runs.read().await;
        let Some(run) = runs.iter().find(|r| r.id == run_id) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No session found with ID `{run_id}`.")),
            });
        };

        match &run.status {
            SubAgentStatus::Running => {
                if let Some(ref tx) = run.steer_tx {
                    tx.send(message.to_string())
                        .map_err(|_| anyhow::anyhow!("Session message channel closed unexpectedly"))?;
                    Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Message sent to session `{run_id}`. \
                             The sub-agent will incorporate it at the next opportunity."
                        ),
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Session `{run_id}` is running but has no message channel available \
                             (it may be a legacy run without steer support)."
                        )),
                    })
                }
            }
            SubAgentStatus::Completed(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Session `{run_id}` already completed; cannot send message.")),
            }),
            SubAgentStatus::Failed(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Session `{run_id}` already failed ({e}); cannot send message.")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};
    use chrono::Utc;

    fn make_running_run(id: &str) -> (SubAgentRun, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let run = SubAgentRun {
            id: id.to_string(),
            task: "some task".to_string(),
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
    async fn completed_session_returns_failure() {
        let run = SubAgentRun {
            id: "done-run".to_string(),
            task: "finished task".to_string(),
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
