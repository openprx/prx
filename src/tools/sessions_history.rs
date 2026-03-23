//! sessions_history — view conversation log of a sub-agent run.
//!
//! Exposes the `history` action from `sessions_spawn` as a dedicated standalone tool,
//! aligning with OpenClaw's `sessions_history` tool interface.
//!
//! Usage: sessions_history(run_id="...", limit=50)

use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

const DEFAULT_LIMIT: usize = 50;

/// Tool that retrieves the conversation history of a spawned sub-agent run.
pub struct SessionsHistoryTool {
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
}

impl SessionsHistoryTool {
    pub const fn new(active_runs: Arc<RwLock<Vec<SubAgentRun>>>) -> Self {
        Self { active_runs }
    }
}

#[async_trait]
impl Tool for SessionsHistoryTool {
    fn name(&self) -> &str {
        "sessions_history"
    }

    fn description(&self) -> &str {
        "View the conversation history (message log) of a sub-agent run. \
         Returns timestamped role/content entries for the given run_id. \
         Use sessions_list to find run IDs."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "The run ID of the sub-agent to inspect (from sessions_spawn or sessions_list)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 500,
                    "description": "Maximum number of history entries to return (default 50)."
                }
            },
            "required": ["run_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let run_id = match args.get("run_id").and_then(|v| v.as_str()) {
            Some(v) if !v.trim().is_empty() => v,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'run_id' parameter".to_string()),
                });
            }
        };

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map_or(DEFAULT_LIMIT, |v| usize::try_from(v).unwrap_or(DEFAULT_LIMIT));

        let runs = self.active_runs.read().await;
        let Some(run) = runs.iter().find(|r| r.id == run_id) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No run found with ID `{run_id}`.")),
            });
        };

        let entries = run.history.read().await;

        if entries.is_empty() {
            let status = match &run.status {
                SubAgentStatus::Running => "still running, no history captured yet",
                SubAgentStatus::Completed(_) => "completed but history is empty",
                SubAgentStatus::Failed(_) => "failed, history may be incomplete",
            };
            return Ok(ToolResult {
                success: true,
                output: format!("No history entries for run `{run_id}` ({status})."),
                error: None,
            });
        }

        let total = entries.len();
        let lines: Vec<String> = entries
            .iter()
            .take(limit)
            .map(|e| {
                let ts = e.timestamp.format("%H:%M:%S").to_string();
                let preview: String = e.content.chars().take(200).collect();
                let ellipsis = if e.content.len() > 200 { "…" } else { "" };
                format!("[{ts}] **{}**: {}{}", e.role, preview, ellipsis)
            })
            .collect();

        let shown = lines.len();
        let truncated_note = if shown < total {
            format!(" (showing {shown}/{total}; increase `limit` to see more)")
        } else {
            String::new()
        };

        Ok(ToolResult {
            success: true,
            output: format!(
                "Conversation history for sub-agent `{run_id}` ({total} entries){truncated_note}:\n\n{}",
                lines.join("\n\n")
            ),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sessions_spawn::{HistoryEntry, SubAgentRun, SubAgentStatus};
    use chrono::Utc;

    fn make_run(id: &str, status: SubAgentStatus, entries: Vec<HistoryEntry>) -> SubAgentRun {
        SubAgentRun {
            id: id.to_string(),
            task: "test task".to_string(),
            started_at: Utc::now(),
            status,
            recipient: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(entries)),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "test-session".to_string(),
            spawn_depth: 0,
        }
    }

    fn make_entry(role: &str, content: &str) -> HistoryEntry {
        HistoryEntry {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn name_and_description() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsHistoryTool::new(runs);
        assert_eq!(tool.name(), "sessions_history");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn missing_run_id_returns_error() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsHistoryTool::new(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Missing 'run_id'"));
    }

    #[tokio::test]
    async fn unknown_run_id_returns_error() {
        let runs = Arc::new(RwLock::new(Vec::new()));
        let tool = SessionsHistoryTool::new(runs);
        let result = tool.execute(json!({"run_id": "nonexistent"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("No run found"));
    }

    #[tokio::test]
    async fn empty_history_returns_status_message() {
        let run = make_run("run-1", SubAgentStatus::Running, vec![]);
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsHistoryTool::new(runs);
        let result = tool.execute(json!({"run_id": "run-1"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No history entries"));
        assert!(result.output.contains("still running"));
    }

    #[tokio::test]
    async fn returns_history_entries() {
        let entries = vec![
            make_entry("user", "Hello, what is 2+2?"),
            make_entry("assistant", "The answer is 4."),
        ];
        let run = make_run("run-2", SubAgentStatus::Completed("done".into()), entries);
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsHistoryTool::new(runs);
        let result = tool.execute(json!({"run_id": "run-2"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("user"));
        assert!(result.output.contains("Hello, what is 2+2?"));
        assert!(result.output.contains("assistant"));
        assert!(result.output.contains("The answer is 4"));
        assert!(result.output.contains("2 entries"));
    }

    #[tokio::test]
    async fn limit_truncates_output() {
        let entries: Vec<HistoryEntry> = (0..20).map(|i| make_entry("user", &format!("message {i}"))).collect();
        let run = make_run("run-3", SubAgentStatus::Completed("done".into()), entries);
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsHistoryTool::new(runs);
        let result = tool.execute(json!({"run_id": "run-3", "limit": 5})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("showing 5/20"));
    }
}
