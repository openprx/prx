//! sessions_list — list active and completed sub-agent sessions.
//!
//! Wraps the shared active_runs registry from SessionsSpawnTool,
//! exposing a dedicated tool that aligns with OpenClaw's `sessions_list`.

use super::sessions_read_model::{self, RecoveredTaskRun, RecoveredTaskStatus};
use super::sessions_spawn::{SubAgentRun, SubAgentStatus};
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::memory::Memory;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Tool to list active and recently completed sub-agent sessions.
pub struct SessionsListTool {
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    memory: Option<Arc<dyn Memory>>,
    workspace_id: String,
}

impl SessionsListTool {
    pub fn new(active_runs: Arc<RwLock<Vec<SubAgentRun>>>) -> Self {
        Self {
            active_runs,
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
}

#[async_trait]
impl Tool for SessionsListTool {
    fn name(&self) -> &str {
        "sessions_list"
    }

    fn description(&self) -> &str {
        "List active and recently completed sub-agent sessions. \
         Shows run_id, source/manageability, origin, agent_index_hint, status, age, usage, and task for each session spawned via sessions_spawn. \
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

        let active_ids = filtered.iter().map(|run| run.id.as_str()).collect::<HashSet<_>>();
        let remaining = limit.saturating_sub(filtered.len());
        let recovered = if remaining > 0 {
            sessions_read_model::recover_task_runs(self.memory.as_ref(), self.workspace_id(), &args, limit)
                .await?
                .into_iter()
                .filter(|run| !active_ids.contains(run.run_id.as_str()))
                .filter(|run| recovered_matches_filter(run, status_filter))
                .take(remaining)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        if filtered.is_empty() && recovered.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No sessions found (filter: {status_filter})."),
                error: None,
            });
        }

        let mut lines: Vec<String> = filtered
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                let status = match &r.status {
                    SubAgentStatus::Running => "🔄 running".to_string(),
                    SubAgentStatus::AwaitingInput { prompt } => {
                        format!("❓ awaiting approval: {prompt}")
                    }
                    SubAgentStatus::Completed(msg) => {
                        let preview = msg.chars().take(60).collect::<String>();
                        let ellipsis = if msg.len() > 60 { "…" } else { "" };
                        format!("✅ completed: {preview}{ellipsis}")
                    }
                    SubAgentStatus::Failed(e) => format!("❌ failed: {e}"),
                };
                let age = (Utc::now() - r.started_at).num_seconds();
                let origin = if r.parent_run_id.is_some() { "model" } else { "user" };
                let agent_index_hint = idx.saturating_add(1);
                let usage = format_run_usage(&r.token_usage_records);
                format!(
                    "• `{}` [agent_index_hint=#{agent_index_hint}, source=runtime, manageable=true, origin={origin}, usage={usage}, {age}s ago] {status}\n  task: {}",
                    r.id, r.task
                )
            })
            .collect();

        let mut recovered_lines = recovered.iter().map(format_recovered_run).collect::<Vec<_>>();
        lines.append(&mut recovered_lines);

        Ok(ToolResult {
            success: true,
            output: format!(
                "Sessions ({} shown, filter: {}):\n\n{}",
                lines.len(),
                status_filter,
                lines.join("\n\n")
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

fn recovered_matches_filter(run: &RecoveredTaskRun, status_filter: &str) -> bool {
    match status_filter {
        "running" => matches!(run.status, RecoveredTaskStatus::Running),
        "completed" => matches!(run.status, RecoveredTaskStatus::Completed),
        "failed" => matches!(run.status, RecoveredTaskStatus::Failed),
        _ => true,
    }
}

fn format_recovered_run(run: &RecoveredTaskRun) -> String {
    let status = match run.status {
        RecoveredTaskStatus::Running => format!("🔄 running (memory: {})", run.last_event_type),
        RecoveredTaskStatus::Completed => {
            let detail = run.status_detail.as_deref().unwrap_or("completed");
            let preview = detail.chars().take(60).collect::<String>();
            let ellipsis = if detail.len() > 60 { "…" } else { "" };
            format!("✅ completed (memory): {preview}{ellipsis}")
        }
        RecoveredTaskStatus::Failed => {
            let detail = run.status_detail.as_deref().unwrap_or(run.last_event_type.as_str());
            format!("❌ failed (memory): {detail}")
        }
    };
    let task = run.task.as_deref().unwrap_or("(task unavailable)");
    let owner = run
        .owner_id
        .as_deref()
        .map(|owner| format!("\n  owner: {owner}"))
        .unwrap_or_default();
    format!(
        "• `{}` [source=memory, manageable=false, usage=unknown, memory at {}] {status}\n  task: {task}{owner}\n  note: recovered from memory only; not killable/steerable in the current runtime registry",
        run.run_id, run.last_event_at
    )
}

pub(crate) fn format_run_usage(records: &[crate::llm::route_decision::MeteredTokenUsageRecord]) -> String {
    let mut total_tokens = 0u64;
    let mut estimated_tokens = 0u64;
    let mut known_cost_usd = 0.0f64;
    let mut unknown_cost_requests = 0u64;
    for record in records {
        total_tokens = total_tokens.saturating_add(record.total_tokens);
        if record.source == crate::llm::route_decision::TokenUsageSource::Estimated {
            estimated_tokens = estimated_tokens.saturating_add(record.total_tokens);
        }
        if let Some(cost) = record.cost_usd.filter(|cost| cost.is_finite() && *cost >= 0.0) {
            known_cost_usd += cost;
        } else {
            unknown_cost_requests = unknown_cost_requests.saturating_add(1);
        }
    }
    if total_tokens == 0 {
        return "unknown".to_string();
    }
    let prefix = if estimated_tokens > 0 { "~" } else { "" };
    let mut out = format!("{prefix}{} tok", format_token_count_compact(total_tokens));
    if unknown_cost_requests > 0 {
        out.push_str(" | cost unknown");
    } else {
        out.push_str(" | ");
        out.push_str(&format_cost_usd(known_cost_usd));
    }
    out
}

fn format_token_count_compact(tokens: u64) -> String {
    if tokens >= 10_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_cost_usd(cost: f64) -> String {
    if !cost.is_finite() || cost <= 0.0 {
        "$0.0000".to_string()
    } else if cost >= 1.0 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::route_decision::TokenUsageSource;
    use crate::memory::{MemoryEventInput, MemoryPrincipal, MemoryVisibility, SqliteMemory};
    use crate::tools::sessions_spawn::{SubAgentRun, SubAgentStatus};
    use chrono::Utc;

    fn make_run(id: &str, status: SubAgentStatus, task: &str) -> SubAgentRun {
        SubAgentRun {
            id: id.to_string(),
            task: task.to_string(),
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
    async fn lists_origin_and_agent_index_hint() {
        let mut run = make_run("model-run", SubAgentStatus::Running, "model task");
        run.parent_run_id = Some("turn-root".to_string());
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsListTool::new(runs);

        let result = tool.execute(json!({})).await.unwrap();

        assert!(result.success);
        assert!(
            result.output.contains("agent_index_hint=#1") && result.output.contains("origin=model"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("source=runtime, manageable=true"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn lists_runtime_usage_when_reported() {
        let mut run = make_run("usage-run", SubAgentStatus::Completed("done".into()), "usage task");
        run.token_usage_records
            .push(crate::llm::route_decision::MeteredTokenUsageRecord {
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                prompt_tokens: 1000,
                completion_tokens: 500,
                total_tokens: 1500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                source: TokenUsageSource::Reported,
                cost_usd: Some(0.0042),
            });
        let runs = Arc::new(RwLock::new(vec![run]));
        let tool = SessionsListTool::new(runs);

        let result = tool.execute(json!({"status": "completed"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("usage=1.5k tok | $0.0042"), "{}", result.output);
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

    #[tokio::test]
    async fn lists_memory_backed_runs_when_runtime_registry_is_empty() {
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
                payload_json: Some(
                    json!({
                        "task": "recover me",
                        "owner_id": "owner-a",
                        "topic_id": "topic-a"
                    })
                    .to_string(),
                ),
            })
            .await
            .unwrap();
        memory
            .append_memory_event(MemoryEventInput {
                event_id: None,
                workspace_id: "/tmp".to_string(),
                event_type: "task.completed".to_string(),
                subject_table: "tasks".to_string(),
                subject_id: "mem-run-1".to_string(),
                session_key: Some("test-session".to_string()),
                run_id: None,
                parent_run_id: None,
                agent_id: None,
                persona_id: None,
                visibility: MemoryVisibility::Workspace,
                payload_json: Some(json!({"result_preview": "done"}).to_string()),
            })
            .await
            .unwrap();

        let tool = SessionsListTool::new(Arc::new(RwLock::new(Vec::new()))).with_shared_memory(memory.clone(), "/tmp");
        let result = tool.execute(json!({"status": "completed"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("mem-run-1"));
        assert!(result.output.contains("recover me"));
        assert!(result.output.contains("owner-a"));
        assert!(
            result.output.contains("source=memory, manageable=false"),
            "{}",
            result.output
        );
        assert!(
            result
                .output
                .contains("not killable/steerable in the current runtime registry"),
            "{}",
            result.output
        );

        let visible = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: "/tmp".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: None,
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
        assert_eq!(visible.len(), 2);
    }
}
