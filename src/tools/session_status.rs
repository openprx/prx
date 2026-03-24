//! session_status — show current agent session/runtime status.
//!
//! Returns information about the current agent session: model, provider,
//! channel(s), uptime, and counts of active/completed/failed sub-agents.
//! Aligns with OpenClaw's `session_status` tool (📊 session_status).

use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Tool that reports the current session's runtime status.
pub struct SessionStatusTool {
    /// Shared active-runs registry (from sessions_spawn).
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    /// Name of the current provider (e.g. "openai", "anthropic").
    provider_name: String,
    /// Current model identifier (e.g. "gpt-4o", "claude-3-5-sonnet").
    model: String,
    /// Names of active channels (e.g. ["signal"]).
    channels: Vec<String>,
    /// Server startup time for uptime calculation.
    started_at: Instant,
}

impl SessionStatusTool {
    pub fn new(
        active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        channels: Vec<String>,
    ) -> Self {
        Self {
            active_runs,
            provider_name: provider_name.into(),
            model: model.into(),
            channels,
            started_at: Instant::now(),
        }
    }

    fn format_uptime(elapsed_secs: u64) -> String {
        if elapsed_secs < 60 {
            format!("{elapsed_secs}s")
        } else if elapsed_secs < 3600 {
            format!("{}m {}s", elapsed_secs / 60, elapsed_secs % 60)
        } else {
            format!("{}h {}m", elapsed_secs / 3600, (elapsed_secs % 3600) / 60)
        }
    }
}

#[async_trait]
impl Tool for SessionStatusTool {
    fn name(&self) -> &str {
        "session_status"
    }

    fn description(&self) -> &str {
        "Show current agent session status: model, provider, active channels, uptime, \
         and sub-agent run counts. Use for session-level diagnostics (📊 session_status)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let uptime_secs = self.started_at.elapsed().as_secs();
        let uptime_str = Self::format_uptime(uptime_secs);

        let runs = self.active_runs.read().await;
        let running_count = runs
            .iter()
            .filter(|r| matches!(r.status, SubAgentStatus::Running))
            .count();
        let completed_count = runs
            .iter()
            .filter(|r| matches!(r.status, SubAgentStatus::Completed(_)))
            .count();
        let failed_count = runs
            .iter()
            .filter(|r| matches!(r.status, SubAgentStatus::Failed(_)))
            .count();
        let total_count = runs.len();
        drop(runs);

        let channels_str = if self.channels.is_empty() {
            "none".to_string()
        } else {
            self.channels.join(", ")
        };

        let output = format!(
            "📊 Session Status\n\
             ─────────────────\n\
             Model:     {}\n\
             Provider:  {}\n\
             Channels:  {}\n\
             Uptime:    {}\n\
             \n\
             Sub-agents: {} total ({} running, {} completed, {} failed)",
            self.model,
            self.provider_name,
            channels_str,
            uptime_str,
            total_count,
            running_count,
            completed_count,
            failed_count,
        );

        Ok(ToolResult {
            success: true,
            output,
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
    use crate::tools::sessions_spawn::SubAgentStatus;
    use chrono::Utc;

    fn make_tool_with_runs(runs: Vec<SubAgentRun>) -> SessionStatusTool {
        SessionStatusTool::new(
            Arc::new(RwLock::new(runs)),
            "anthropic",
            "claude-3-5-sonnet",
            vec!["signal".to_string()],
        )
    }

    #[test]
    fn name_and_description() {
        let tool = make_tool_with_runs(vec![]);
        assert_eq!(tool.name(), "session_status");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn format_uptime_seconds() {
        assert_eq!(SessionStatusTool::format_uptime(45), "45s");
    }

    #[test]
    fn format_uptime_minutes() {
        assert_eq!(SessionStatusTool::format_uptime(125), "2m 5s");
    }

    #[test]
    fn format_uptime_hours() {
        assert_eq!(SessionStatusTool::format_uptime(7200), "2h 0m");
    }

    #[tokio::test]
    async fn returns_status_with_no_runs() {
        let tool = make_tool_with_runs(vec![]);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("claude-3-5-sonnet"));
        assert!(result.output.contains("anthropic"));
        assert!(result.output.contains("signal"));
        assert!(result.output.contains("0 total"));
    }

    #[tokio::test]
    async fn counts_runs_by_status() {
        let runs = vec![
            SubAgentRun {
                id: "r1".into(),
                task: "t1".into(),
                started_at: Utc::now(),
                status: SubAgentStatus::Running,
                recipient: None,
                abort_handle: None,
                history: Arc::new(RwLock::new(vec![])),
                steer_tx: None,
                parent_run_id: None,
                session_scope_key: "test-session".to_string(),
                spawn_depth: 0,
            },
            SubAgentRun {
                id: "r2".into(),
                task: "t2".into(),
                started_at: Utc::now(),
                status: SubAgentStatus::Completed("done".into()),
                recipient: None,
                abort_handle: None,
                history: Arc::new(RwLock::new(vec![])),
                steer_tx: None,
                parent_run_id: None,
                session_scope_key: "test-session".to_string(),
                spawn_depth: 0,
            },
            SubAgentRun {
                id: "r3".into(),
                task: "t3".into(),
                started_at: Utc::now(),
                status: SubAgentStatus::Failed("oops".into()),
                recipient: None,
                abort_handle: None,
                history: Arc::new(RwLock::new(vec![])),
                steer_tx: None,
                parent_run_id: None,
                session_scope_key: "test-session".to_string(),
                spawn_depth: 0,
            },
        ];
        let tool = make_tool_with_runs(runs);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("3 total"));
        assert!(result.output.contains("1 running"));
        assert!(result.output.contains("1 completed"));
        assert!(result.output.contains("1 failed"));
    }
}
