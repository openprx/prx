//! session_status — show current agent session/runtime status.
//!
//! Returns information about the current agent session: model, provider,
//! channel(s), uptime, and counts of active/completed/failed sub-agents.
//! Aligns with OpenClaw's `session_status` tool (📊 session_status).

use super::sessions_read_model::{self, RecoveredTaskStatus};
use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::Memory;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashSet;
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
    memory: Option<Arc<dyn Memory>>,
    workspace_id: String,
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
            memory: None,
            workspace_id: String::new(),
        }
    }

    pub fn with_shared_memory(mut self, memory: Arc<dyn Memory>, workspace_id: impl Into<String>) -> Self {
        self.memory = Some(memory);
        self.workspace_id = workspace_id.into();
        self
    }

    fn workspace_id(&self) -> &str {
        if self.workspace_id.is_empty() {
            "/tmp"
        } else {
            &self.workspace_id
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

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let uptime_secs = self.started_at.elapsed().as_secs();
        let uptime_str = Self::format_uptime(uptime_secs);

        let runs = self.active_runs.read().await;
        let active_ids = runs.iter().map(|run| run.id.clone()).collect::<HashSet<_>>();
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
        let runtime_count = runs.len();
        drop(runs);

        let recovered = sessions_read_model::recover_task_runs(self.memory.as_ref(), self.workspace_id(), &args, 100)
            .await?
            .into_iter()
            .filter(|run| !active_ids.contains(&run.run_id))
            .collect::<Vec<_>>();
        let recovered_running = recovered
            .iter()
            .filter(|run| matches!(run.status, RecoveredTaskStatus::Running))
            .count();
        let recovered_completed = recovered
            .iter()
            .filter(|run| matches!(run.status, RecoveredTaskStatus::Completed))
            .count();
        let recovered_failed = recovered
            .iter()
            .filter(|run| matches!(run.status, RecoveredTaskStatus::Failed))
            .count();
        let total_count = runtime_count + recovered.len();

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
             Sub-agents/tasks: {} total ({} running, {} completed, {} failed; {} memory-backed recovered)",
            self.model,
            self.provider_name,
            channels_str,
            uptime_str,
            total_count,
            running_count + recovered_running,
            completed_count + recovered_completed,
            failed_count + recovered_failed,
            recovered.len(),
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
    use crate::memory::{MemoryEventInput, MemoryVisibility, SqliteMemory};
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
                owner_id: None,
                topic_id: None,
                source_message_event_id: None,
                started_at: Utc::now(),
                finished_at: None,
                status: SubAgentStatus::Running,
                recipient: None,
                channel_name: None,
                abort_handle: None,
                history: Arc::new(RwLock::new(vec![])),
                steer_tx: None,
                parent_run_id: None,
                session_scope_key: "test-session".to_string(),
                spawn_depth: 0,
                token_usage_records: Vec::new(),
            },
            SubAgentRun {
                id: "r2".into(),
                task: "t2".into(),
                owner_id: None,
                topic_id: None,
                source_message_event_id: None,
                started_at: Utc::now(),
                finished_at: None,
                status: SubAgentStatus::Completed("done".into()),
                recipient: None,
                channel_name: None,
                abort_handle: None,
                history: Arc::new(RwLock::new(vec![])),
                steer_tx: None,
                parent_run_id: None,
                session_scope_key: "test-session".to_string(),
                spawn_depth: 0,
                token_usage_records: Vec::new(),
            },
            SubAgentRun {
                id: "r3".into(),
                task: "t3".into(),
                owner_id: None,
                topic_id: None,
                source_message_event_id: None,
                started_at: Utc::now(),
                finished_at: None,
                status: SubAgentStatus::Failed("oops".into()),
                recipient: None,
                channel_name: None,
                abort_handle: None,
                history: Arc::new(RwLock::new(vec![])),
                steer_tx: None,
                parent_run_id: None,
                session_scope_key: "test-session".to_string(),
                spawn_depth: 0,
                token_usage_records: Vec::new(),
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

    #[tokio::test]
    async fn counts_memory_backed_runs_after_runtime_registry_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: "/tmp".to_string(),
                event_type: "task.spawned".to_string(),
                subject_table: "tasks".to_string(),
                subject_id: "mem-run-1".to_string(),
                session_key: Some("test-session".to_string()),
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: Some(json!({"task": "durable task"}).to_string()),
            })
            .await
            .unwrap();
        memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: "/tmp".to_string(),
                event_type: "task.failed".to_string(),
                subject_table: "tasks".to_string(),
                subject_id: "mem-run-1".to_string(),
                session_key: Some("test-session".to_string()),
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: Some(json!({"error": "boom"}).to_string()),
            })
            .await
            .unwrap();

        let tool = make_tool_with_runs(vec![]).with_shared_memory(memory, "/tmp");
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 total"));
        assert!(result.output.contains("1 failed"));
        assert!(result.output.contains("1 memory-backed recovered"));
    }

    #[tokio::test]
    async fn counts_cron_job_events_from_memory_read_model() {
        let tmp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: "/tmp".to_string(),
                event_type: "cron.job.created".to_string(),
                subject_table: "tasks".to_string(),
                subject_id: "cron-job-1".to_string(),
                session_key: None,
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: Some(json!({"task": "cron lineaged task"}).to_string()),
            })
            .await
            .unwrap();

        let tool = make_tool_with_runs(vec![]).with_shared_memory(memory, "/tmp");
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 total"));
        assert!(result.output.contains("1 running"));
        assert!(result.output.contains("1 memory-backed recovered"));
    }
}
