//! sessions_list — list active and completed sub-agent sessions.
//!
//! Wraps the shared active_runs registry from SessionsSpawnTool,
//! exposing a dedicated tool that aligns with OpenClaw's `sessions_list`.

use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Tool to list active and recently completed sub-agent sessions.
pub struct SessionsListTool {
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
}

impl SessionsListTool {
    pub fn new(active_runs: Arc<RwLock<Vec<SubAgentRun>>>) -> Self {
        Self { active_runs }
    }
}

#[async_trait]
impl Tool for SessionsListTool {
    fn name(&self) -> &str {
        "sessions_list"
    }

    fn description(&self) -> &str {
        "List active and recently completed sub-agent sessions. \
         Shows run_id, status, age, and task for each session spawned via sessions_spawn. \
         Use this to check what sub-agents are running or have recently finished."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["all", "running", "completed", "failed"],
                    "default": "all",
                    "description": "Filter by session status. Defaults to 'all'."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Maximum number of sessions to return. Defaults to 20."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let status_filter = args.get("status").and_then(|v| v.as_str()).unwrap_or("all");
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

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
                output: format!("No sessions found (filter: {status_filter})."),
                error: None,
            });
        }

        let lines: Vec<String> = filtered
            .iter()
            .map(|r| {
                let status = match &r.status {
                    SubAgentStatus::Running => "🔄 running".to_string(),
                    SubAgentStatus::Completed(msg) => {
                        let preview = msg.chars().take(60).collect::<String>();
                        let ellipsis = if msg.len() > 60 { "…" } else { "" };
                        format!("✅ completed: {preview}{ellipsis}")
                    }
                    SubAgentStatus::Failed(e) => format!("❌ failed: {e}"),
                };
                let age = (Utc::now() - r.started_at).num_seconds();
                format!("• `{}` [{age}s ago] {status}\n  task: {}", r.id, r.task)
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: format!(
                "Sessions ({} shown, filter: {}):\n\n{}",
                filtered.len(),
                status_filter,
                lines.join("\n\n")
            ),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};
    use chrono::Utc;

    fn make_run(id: &str, status: SubAgentStatus, task: &str) -> SubAgentRun {
        SubAgentRun {
            id: id.to_string(),
            task: task.to_string(),
            started_at: Utc::now(),
            status,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
        }
    }

    #[test]
    fn name_and_description() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsListTool::new(runs);
        assert_eq!(tool.name(), "sessions_list");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn empty_returns_success() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No sessions"));
    }

    #[tokio::test]
    async fn lists_runs() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("aaa", SubAgentStatus::Running, "task A"),
            make_run("bbb", SubAgentStatus::Completed("done".into()), "task B"),
        ]));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("aaa"));
        assert!(result.output.contains("bbb"));
        assert!(result.output.contains("task A"));
    }

    #[tokio::test]
    async fn filter_running_only() {
        let runs = Arc::new(RwLock::new(vec![
            make_run("run1", SubAgentStatus::Running, "task1"),
            make_run("run2", SubAgentStatus::Completed("done".into()), "task2"),
        ]));
        let tool = SessionsListTool::new(runs);
        let result = tool.execute(json!({"status": "running"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("run1"));
        assert!(!result.output.contains("run2"));
    }
}
