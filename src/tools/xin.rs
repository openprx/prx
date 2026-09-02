//! LLM tool for the xin (心) autonomous task heartbeat engine.
//!
//! Actions:
//!  - list — list all xin tasks
//!  - get — get details of a single task
//!  - add — create a new user task
//!  - remove — delete a task
//!  - pause / resume — disable/enable a task
//!  - events — list lifecycle events for a task
//!  - status — show xin subsystem status

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::{Config, SharedConfig};
use crate::security::SecurityPolicy;
use crate::security::policy::{ApprovalGrant, PERSISTED_APPROVAL_GRANT_TTL_SECS};
use crate::xin::store;
use crate::xin::types::{
    ExecutionMode, GoalStatus, NewXinGoal, NewXinStep, NewXinTask, StepStatus, TaskKind, TaskPriority, TaskStatus,
    XinGoal, XinRun, XinStep, XinTask, XinTaskEvent, XinTaskPatch,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct XinTool {
    config: SharedConfig,
    security: Arc<SecurityPolicy>,
}

impl XinTool {
    pub const fn new(config: SharedConfig, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    const fn check_enabled(&self, _cfg: &Config) -> Option<ToolResult> {
        None
    }

    fn enforce_mutation(&self, action: &str, cfg: &Config) -> Option<ToolResult> {
        if let Some(r) = self.check_enabled(cfg) {
            return Some(r);
        }
        if !self.security.can_act() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Security policy: read-only mode, cannot perform '{action}'")),
            });
        }
        None
    }

    fn build_step(
        &self,
        args: &serde_json::Value,
        sequence: u32,
        approval_args: &serde_json::Value,
    ) -> anyhow::Result<NewXinStep> {
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing step 'name'"))?
            .to_string();
        let payload = args
            .get("payload")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing step 'payload'"))?
            .to_string();
        let execution_mode = match args
            .get("execution_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("agent_session")
        {
            "agent_session" => ExecutionMode::AgentSession,
            "shell" => ExecutionMode::Shell,
            other => anyhow::bail!("Unsupported user step execution_mode '{other}'"),
        };
        let approval_grant_json = if execution_mode == ExecutionMode::Shell {
            let approval = ApprovalGrant::from_runtime_args(self.name(), approval_args);
            ApprovalGrant::persisted_runner_grant(
                "xin_runner",
                &payload,
                approval.as_ref(),
                PERSISTED_APPROVAL_GRANT_TTL_SECS,
            )
            .map(|grant| serde_json::to_string(&grant))
            .transpose()?
        } else {
            None
        };
        Ok(NewXinStep {
            sequence,
            name,
            description: args
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            execution_mode,
            payload,
            max_retries: 0,
            approval_grant_json,
            lease_ttl_secs: args
                .get("lease_ttl_secs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        })
    }

    fn enforce_owner_scope(&self, cfg: &Config, args: &serde_json::Value, action: &str) -> Option<ToolResult> {
        let scope = parse_xin_lineage_scope(cfg, args);
        let Some(expected_owner) = scope.owner_id.as_deref() else {
            return None;
        };
        let denied = match action {
            "get" | "runs" | "update" | "remove" | "pause" | "resume" | "cancel" | "run" => args
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .map(|id| {
                    store::get_task(cfg, id).map_or(true, |task| task.owner_id.as_deref() != Some(expected_owner))
                })
                .unwrap_or(false),
            "goal_get" | "goal_pause" | "goal_resume" | "goal_cancel" | "goal_remove" | "step_list" | "step_add" => {
                args.get("goal_id")
                    .and_then(serde_json::Value::as_str)
                    .map(|id| {
                        store::get_goal(cfg, id).map_or(true, |goal| goal.owner_id.as_deref() != Some(expected_owner))
                    })
                    .unwrap_or(false)
            }
            "step_get" | "step_retry" => args
                .get("step_id")
                .and_then(serde_json::Value::as_str)
                .map(|id| {
                    store::get_step(cfg, id).map_or(true, |step| {
                        store::get_goal(cfg, &step.goal_id)
                            .map_or(true, |goal| goal.owner_id.as_deref() != Some(expected_owner))
                    })
                })
                .unwrap_or(false),
            "events" => args
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .map(|id| {
                    if let Ok(task) = store::get_task(cfg, id) {
                        return task.owner_id.as_deref() != Some(expected_owner);
                    }
                    store::get_goal(cfg, id).map_or(true, |goal| goal.owner_id.as_deref() != Some(expected_owner))
                })
                .unwrap_or(false),
            _ => false,
        };
        denied.then(|| tool_error("Xin subject not found in the caller's owner scope"))
    }
}

fn tool_error(message: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(message.into()),
    }
}

fn format_task_line(task: &XinTask) -> String {
    let enabled_marker = if task.enabled { "●" } else { "○" };
    format!(
        "{enabled_marker} {} | {} | {} | prio={} | mode={} | runs={} fails={}",
        task.id,
        task.name,
        task.status.as_str(),
        task.priority.as_i32(),
        task.execution_mode.as_str(),
        task.run_count,
        task.fail_count,
    )
}

fn format_task_detail(task: &XinTask) -> String {
    let mut lines = Vec::new();
    lines.push(format!("ID:          {}", task.id));
    if let Some(owner_id) = &task.owner_id {
        lines.push(format!("Owner:       {owner_id}"));
    }
    if let Some(topic_id) = &task.topic_id {
        lines.push(format!("Topic:       {topic_id}"));
    }
    if let Some(parent_task_id) = &task.parent_task_id {
        lines.push(format!("Parent Task: {parent_task_id}"));
    }
    if let Some(source_event_id) = &task.source_message_event_id {
        lines.push(format!("Source Msg:  {source_event_id}"));
    }
    lines.push(format!("Name:        {}", task.name));
    if let Some(ref desc) = task.description {
        lines.push(format!("Description: {desc}"));
    }
    lines.push(format!("Kind:        {}", task.kind.as_str()));
    lines.push(format!("Status:      {}", task.status.as_str()));
    lines.push(format!("Priority:    {}", task.priority.as_i32()));
    lines.push(format!("Mode:        {}", task.execution_mode.as_str()));
    lines.push(format!("Payload:     {}", task.payload));
    lines.push(format!("Recurring:   {}", task.recurring));
    if task.recurring {
        lines.push(format!("Interval:    {}s", task.interval_secs));
    }
    lines.push(format!("Enabled:     {}", task.enabled));
    lines.push(format!("Runs:        {}", task.run_count));
    lines.push(format!("Failures:    {}", task.fail_count));
    lines.push("Failure Cap: none (legacy value ignored)".to_string());
    lines.push(format!("Next Run:    {}", task.next_run_at.to_rfc3339()));
    if let Some(last) = &task.last_run_at {
        lines.push(format!("Last Run:    {}", last.to_rfc3339()));
    }
    if let Some(status) = &task.last_status {
        lines.push(format!("Last Status: {status}"));
    }
    if let Some(output) = &task.last_output {
        let truncated = if output.len() > 500 {
            // Find a valid UTF-8 char boundary at or before 500 bytes
            let mut cutoff = 500;
            while cutoff > 0 && !output.is_char_boundary(cutoff) {
                cutoff -= 1;
            }
            let mut s = output[..cutoff].to_string();
            s.push_str("...");
            s
        } else {
            output.clone()
        };
        lines.push(format!("Last Output: {truncated}"));
    }
    lines.join("\n")
}

fn format_task_event(event: &XinTaskEvent) -> String {
    let mut parts = vec![
        event.created_at.to_rfc3339(),
        event.event_type.clone(),
        format!("status={}", event.status.as_deref().unwrap_or("-")),
    ];
    if let Some(owner_id) = &event.owner_id {
        parts.push(format!("owner={owner_id}"));
    }
    if let Some(topic_id) = &event.topic_id {
        parts.push(format!("topic={topic_id}"));
    }
    if let Some(parent_task_id) = &event.parent_task_id {
        parts.push(format!("parent={parent_task_id}"));
    }
    parts.join(" | ")
}

fn format_run(run: &XinRun) -> String {
    let output = run.output.as_deref().unwrap_or("-");
    let output = if output.chars().count() > 500 {
        format!("{}...", output.chars().take(500).collect::<String>())
    } else {
        output.to_string()
    };
    format!(
        "{} | {} | {}ms | {}",
        run.started_at.to_rfc3339(),
        run.status,
        run.duration_ms,
        output
    )
}

fn format_goal(goal: &XinGoal) -> String {
    format!(
        "{} | {} | {} | steps={}/{} | enabled={}",
        goal.id,
        goal.name,
        goal.status.as_str(),
        goal.steps_completed,
        goal.steps_total,
        goal.enabled
    )
}

fn format_step(step: &XinStep) -> String {
    format!(
        "{} | seq={} | {} | {} | mode={} | retries={}",
        step.id,
        step.sequence,
        step.name,
        step.status.as_str(),
        step.execution_mode.as_str(),
        step.retry_count
    )
}

#[derive(Debug, Clone, Default)]
struct XinLineageScope {
    owner_id: Option<String>,
    topic_id: Option<String>,
    parent_task_id: Option<String>,
    source_message_event_id: Option<String>,
}

fn parse_xin_lineage_scope(cfg: &Config, args: &serde_json::Value) -> XinLineageScope {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return XinLineageScope::default();
    }
    let Some(scope) = args.get("_zc_scope").and_then(serde_json::Value::as_object) else {
        return XinLineageScope::default();
    };
    let channel = scope
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let sender = scope
        .get("sender")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let chat_id = scope
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("xin");
    let explicit_owner_id = scope
        .get("owner_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let owner_id = explicit_owner_id.or_else(|| match (channel, sender) {
        (Some(channel), Some(sender)) => Some(
            crate::memory::principal::OwnerPrincipal::new(
                cfg.workspace_dir.to_string_lossy().to_string(),
                channel,
                sender,
                chat_id,
                vec![crate::memory::principal::Role::Anonymous],
            )
            .owner_id,
        ),
        _ => None,
    });
    XinLineageScope {
        owner_id,
        topic_id: scope
            .get("topic_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        parent_task_id: scope
            .get("task_id")
            .or_else(|| scope.get("parent_task_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        source_message_event_id: scope
            .get("message_event_id")
            .or_else(|| scope.get("source_message_event_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    }
}

#[async_trait]
impl Tool for XinTool {
    fn name(&self) -> &str {
        "xin"
    }

    fn description(&self) -> &str {
        "Xin (心) autonomous task heartbeat engine. \
         Manage tasks and durable goal/step workflows, including execution and history."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "list", "get", "events", "runs", "add", "update", "remove", "pause", "resume", "cancel", "run", "status",
                        "goal_list", "goal_get", "goal_add", "goal_pause", "goal_resume", "goal_cancel", "goal_remove",
                        "step_list", "step_get", "step_add", "step_retry"
                    ],
                    "description": "Action to perform."
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID, or Goal ID for the generic events action."
                },
                "goal_id": {
                    "type": "string",
                    "description": "Goal ID for goal and step-list actions."
                },
                "step_id": {
                    "type": "string",
                    "description": "Step ID for step_get/step_retry actions."
                },
                "name": {
                    "type": "string",
                    "description": "Task name (for add action)."
                },
                "description": {
                    "type": "string",
                    "description": "Task description (for add action)."
                },
                "payload": {
                    "type": "string",
                    "description": "Task payload: prompt for agent_session, command for shell (for add action)."
                },
                "execution_mode": {
                    "type": "string",
                    "enum": ["agent_session", "shell"],
                    "description": "How the task runs: agent_session (LLM) or shell (command). Default: agent_session."
                },
                "priority": {
                    "type": "string",
                    "enum": ["low", "normal", "high", "critical"],
                    "description": "Task priority. Default: normal."
                },
                "recurring": {
                    "type": "boolean",
                    "description": "Whether the task repeats. Default: false."
                },
                "interval_secs": {
                    "type": "integer",
                    "description": "Repeat interval in seconds (only for recurring tasks)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum run-history entries to return (1-50, default 20)."
                },
                "sequence": {
                    "type": "integer",
                    "description": "One-based step sequence for step_add."
                },
                "lease_ttl_secs": {
                    "type": "integer",
                    "description": "Optional per-step lease TTL; zero uses the execution-mode default."
                },
                "target_completion_at": {
                    "type": "string",
                    "description": "Optional RFC3339 target time for goal_add."
                },
                "steps": {
                    "type": "array",
                    "description": "Optional initial ordered steps for goal_add.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "sequence": {"type": "integer"},
                            "name": {"type": "string"},
                            "description": {"type": "string"},
                            "payload": {"type": "string"},
                            "execution_mode": {"type": "string", "enum": ["agent_session", "shell"]},
                            "lease_ttl_secs": {"type": "integer"}
                        },
                        "required": ["name", "payload"]
                    }
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let cfg = self.config.load_full();
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'action' parameter".to_string()),
                });
            }
        };
        if let Some(denied) = self.enforce_owner_scope(&cfg, &args, action) {
            return Ok(denied);
        }
        let caller_scope = parse_xin_lineage_scope(&cfg, &args);

        match action {
            // ── Read-only ──────────────────────────────────────────────
            "list" => {
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                match store::list_tasks(&cfg) {
                    Ok(tasks) => {
                        let tasks = tasks
                            .into_iter()
                            .filter(|task| {
                                caller_scope
                                    .owner_id
                                    .as_deref()
                                    .is_none_or(|owner| task.owner_id.as_deref() == Some(owner))
                            })
                            .collect::<Vec<_>>();
                        if tasks.is_empty() {
                            return Ok(ToolResult {
                                success: true,
                                output: "No xin tasks.".to_string(),
                                error: None,
                            });
                        }
                        let lines: Vec<String> = tasks.iter().map(format_task_line).collect();
                        Ok(ToolResult {
                            success: true,
                            output: format!("Xin tasks ({}):\n{}", tasks.len(), lines.join("\n")),
                            error: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "get" => {
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'task_id' parameter".to_string()),
                        });
                    }
                };
                match store::get_task(&cfg, task_id) {
                    Ok(task) => Ok(ToolResult {
                        success: true,
                        output: format_task_detail(&task),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "events" => {
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'task_id' parameter".to_string()),
                        });
                    }
                };
                match store::list_task_events(&cfg, task_id) {
                    Ok(events) if events.is_empty() => Ok(ToolResult {
                        success: true,
                        output: format!("No xin task events for {task_id}."),
                        error: None,
                    }),
                    Ok(events) => {
                        let lines = events.iter().map(format_task_event).collect::<Vec<_>>();
                        Ok(ToolResult {
                            success: true,
                            output: format!("Xin task events ({task_id}):\n{}", lines.join("\n")),
                            error: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "runs" => {
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                let Some(task_id) = args
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'task_id' parameter"));
                };
                let limit = args.get("limit").and_then(serde_json::Value::as_u64).unwrap_or(20) as usize;
                match store::list_runs(&cfg, task_id, limit) {
                    Ok(runs) if runs.is_empty() => Ok(ToolResult {
                        success: true,
                        output: format!("No xin runs for {task_id}."),
                        error: None,
                    }),
                    Ok(runs) => Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Xin runs ({task_id}, newest first):\n{}",
                            runs.iter().map(format_run).collect::<Vec<_>>().join("\n")
                        ),
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "goal_list" => match store::list_goals(&cfg).map(|goals| {
                goals
                    .into_iter()
                    .filter(|goal| {
                        caller_scope
                            .owner_id
                            .as_deref()
                            .is_none_or(|owner| goal.owner_id.as_deref() == Some(owner))
                    })
                    .collect::<Vec<_>>()
            }) {
                Ok(goals) if goals.is_empty() => Ok(ToolResult {
                    success: true,
                    output: "No xin goals.".to_string(),
                    error: None,
                }),
                Ok(goals) => Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Xin goals ({}):\n{}",
                        goals.len(),
                        goals.iter().map(format_goal).collect::<Vec<_>>().join("\n")
                    ),
                    error: None,
                }),
                Err(error) => Ok(tool_error(error.to_string())),
            },

            "goal_get" => {
                let Some(goal_id) = args
                    .get("goal_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'goal_id' parameter"));
                };
                match store::get_goal(&cfg, goal_id) {
                    Ok(goal) => {
                        let steps = store::list_steps(&cfg, goal_id)?;
                        Ok(ToolResult {
                            success: true,
                            output: serde_json::to_string_pretty(&json!({"goal": goal, "steps": steps}))?,
                            error: None,
                        })
                    }
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "step_list" => {
                let Some(goal_id) = args
                    .get("goal_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'goal_id' parameter"));
                };
                match store::list_steps(&cfg, goal_id) {
                    Ok(steps) if steps.is_empty() => Ok(ToolResult {
                        success: true,
                        output: format!("No xin steps for goal {goal_id}."),
                        error: None,
                    }),
                    Ok(steps) => Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Xin steps ({goal_id}):\n{}",
                            steps.iter().map(format_step).collect::<Vec<_>>().join("\n")
                        ),
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "step_get" => {
                let Some(step_id) = args
                    .get("step_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'step_id' parameter"));
                };
                match store::get_step(&cfg, step_id) {
                    Ok(step) => Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&step)?,
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "status" => {
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                let tasks = match store::list_tasks(&cfg) {
                    Ok(tasks) => tasks
                        .into_iter()
                        .filter(|task| {
                            caller_scope
                                .owner_id
                                .as_deref()
                                .is_none_or(|owner| task.owner_id.as_deref() == Some(owner))
                        })
                        .collect::<Vec<_>>(),
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to query xin tasks: {e}")),
                        });
                    }
                };
                let active = tasks.iter().filter(|t| t.enabled).count();
                let cancelled = tasks.iter().filter(|t| t.status == TaskStatus::Cancelled).count();
                let paused = tasks
                    .iter()
                    .filter(|t| !t.enabled && t.status != TaskStatus::Cancelled)
                    .count();
                let system = tasks.iter().filter(|t| t.kind == TaskKind::System).count();
                let user = tasks.iter().filter(|t| t.kind == TaskKind::User).count();
                let agent = tasks.iter().filter(|t| t.kind == TaskKind::Agent).count();
                let goals = match store::list_goals(&cfg) {
                    Ok(goals) => goals
                        .into_iter()
                        .filter(|goal| {
                            caller_scope
                                .owner_id
                                .as_deref()
                                .is_none_or(|owner| goal.owner_id.as_deref() == Some(owner))
                        })
                        .collect::<Vec<_>>(),
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to query xin goals: {e}")),
                        });
                    }
                };
                let goals_active = goals
                    .iter()
                    .filter(|goal| goal.enabled && matches!(goal.status, GoalStatus::Pending | GoalStatus::Running))
                    .count();
                let goals_completed = goals.iter().filter(|goal| goal.status == GoalStatus::Completed).count();
                let goals_failed = goals.iter().filter(|goal| goal.status == GoalStatus::Failed).count();
                let goals_cancelled = goals.iter().filter(|goal| goal.status == GoalStatus::Cancelled).count();
                let goals_paused = goals.iter().filter(|goal| !goal.enabled).count();
                let mut steps_total = 0_usize;
                let mut steps_waiting = 0_usize;
                let mut steps_running = 0_usize;
                let mut steps_completed = 0_usize;
                let mut steps_failed = 0_usize;
                for goal in &goals {
                    let steps = match store::list_steps(&cfg, &goal.id) {
                        Ok(steps) => steps,
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Failed to query steps for xin goal {}: {e}", goal.id)),
                            });
                        }
                    };
                    steps_total += steps.len();
                    for step in steps {
                        match step.status {
                            StepStatus::Pending | StepStatus::Stale => steps_waiting += 1,
                            StepStatus::Claimed | StepStatus::Running => steps_running += 1,
                            StepStatus::Completed => steps_completed += 1,
                            StepStatus::Failed => steps_failed += 1,
                        }
                    }
                }

                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Xin Status\n\
                         ──────────\n\
                         Interval:    {} min\n\
                         Tasks:       {} total ({active} active, {paused} paused, {cancelled} cancelled)\n\
                         By kind:     {system} system, {user} user, {agent} agent\n\
                         Goals:       {} total ({goals_active} active, {goals_completed} completed, {goals_failed} legacy-failed, {goals_cancelled} cancelled, {goals_paused} paused)\n\
                         Steps:       {steps_total} total ({steps_waiting} waiting, {steps_running} running, {steps_completed} completed, {steps_failed} legacy-failed)\n\
                         Concurrency: unbounded\n\
                         Evolution:   integrated",
                        cfg.xin.interval_minutes,
                        tasks.len(),
                        goals.len(),
                    ),
                    error: None,
                })
            }

            // ── Mutating ──────────────────────────────────────────────
            "add" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let name = match args.get("name").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v.to_string(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'name' parameter".to_string()),
                        });
                    }
                };
                let payload = match args.get("payload").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v.to_string(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'payload' parameter".to_string()),
                        });
                    }
                };
                let description = args.get("description").and_then(|v| v.as_str()).map(String::from);
                let execution_mode = match args
                    .get("execution_mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent_session")
                {
                    "shell" => ExecutionMode::Shell,
                    _ => ExecutionMode::AgentSession,
                };
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                let approval_grant_json = if matches!(execution_mode, ExecutionMode::Shell) {
                    ApprovalGrant::persisted_runner_grant(
                        "xin_runner",
                        &payload,
                        approval_grant.as_ref(),
                        PERSISTED_APPROVAL_GRANT_TTL_SECS,
                    )
                    .map(|grant| serde_json::to_string(&grant))
                    .transpose()?
                } else {
                    None
                };
                let priority =
                    TaskPriority::from_str_lossy(args.get("priority").and_then(|v| v.as_str()).unwrap_or("normal"));
                let recurring = args.get("recurring").and_then(|v| v.as_bool()).unwrap_or(false);
                let interval_secs = args.get("interval_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                let lineage_scope = parse_xin_lineage_scope(&cfg, &args);

                // Prevent busy-loop: recurring tasks must have ≥60s interval.
                if recurring && interval_secs < 60 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Recurring tasks require interval_secs >= 60 to prevent busy-loops".to_string()),
                    });
                }

                let new = NewXinTask {
                    owner_id: lineage_scope.owner_id,
                    topic_id: lineage_scope.topic_id,
                    parent_task_id: lineage_scope.parent_task_id,
                    source_message_event_id: lineage_scope.source_message_event_id,
                    name,
                    description,
                    kind: TaskKind::User,
                    priority,
                    execution_mode,
                    payload,
                    recurring,
                    interval_secs,
                    max_failures: 0,
                    approval_grant_json,
                };

                match store::add_task(&cfg, &new) {
                    Ok(task) => Ok(ToolResult {
                        success: true,
                        output: format!("Created xin task: {} ({})", task.id, task.name),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "update" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let Some(task_id) = args
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'task_id' parameter"));
                };
                let existing = match store::get_task(&cfg, task_id) {
                    Ok(task) => task,
                    Err(error) => return Ok(tool_error(error.to_string())),
                };
                let payload = args
                    .get("payload")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                let approval_grant_json = if payload.is_some() && existing.execution_mode == ExecutionMode::Shell {
                    let approval = ApprovalGrant::from_runtime_args(self.name(), &args);
                    ApprovalGrant::persisted_runner_grant(
                        "xin_runner",
                        payload.as_deref().unwrap_or_default(),
                        approval.as_ref(),
                        PERSISTED_APPROVAL_GRANT_TTL_SECS,
                    )
                    .map(|grant| serde_json::to_string(&grant))
                    .transpose()?
                } else {
                    None
                };
                let patch = XinTaskPatch {
                    name: args.get("name").and_then(serde_json::Value::as_str).map(str::to_string),
                    description: args
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    priority: args
                        .get("priority")
                        .and_then(serde_json::Value::as_str)
                        .map(TaskPriority::from_str_lossy),
                    payload,
                    interval_secs: args.get("interval_secs").and_then(serde_json::Value::as_u64),
                    approval_grant_json,
                    ..XinTaskPatch::default()
                };
                if patch.name.is_none()
                    && patch.description.is_none()
                    && patch.priority.is_none()
                    && patch.payload.is_none()
                    && patch.interval_secs.is_none()
                {
                    return Ok(tool_error(
                        "At least one of name, description, priority, payload, or interval_secs is required",
                    ));
                }
                if existing.recurring && patch.interval_secs.is_some_and(|interval| interval < 60) {
                    return Ok(tool_error("Recurring tasks require interval_secs >= 60"));
                }
                match store::update_task(&cfg, task_id, &patch) {
                    Ok(task) => Ok(ToolResult {
                        success: true,
                        output: format!("Updated xin task: {} ({})", task.id, task.name),
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "run" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let Some(task_id) = args
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'task_id' parameter"));
                };
                match crate::xin::runner::run_task_now(&cfg, &self.security, task_id).await {
                    Ok(run) => Ok(ToolResult {
                        success: run.success,
                        output: serde_json::to_string_pretty(&json!({
                            "task_id": run.task_id,
                            "status": if run.success { "ok" } else { "error" },
                            "output": run.output,
                        }))?,
                        error: (!run.success).then(|| "xin task execution failed".to_string()),
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "remove" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'task_id' parameter".to_string()),
                        });
                    }
                };
                match store::remove_task(&cfg, task_id) {
                    Ok(()) => Ok(ToolResult {
                        success: true,
                        output: format!("Removed xin task {task_id}"),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "pause" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'task_id' parameter".to_string()),
                        });
                    }
                };
                match store::pause_task(&cfg, task_id) {
                    Ok(_) => Ok(ToolResult {
                        success: true,
                        output: format!("Paused xin task {task_id}"),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "resume" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'task_id' parameter".to_string()),
                        });
                    }
                };
                match store::resume_task(&cfg, task_id) {
                    Ok(_) => Ok(ToolResult {
                        success: true,
                        output: format!("Resumed xin task {task_id}"),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "cancel" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let Some(task_id) = args
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'task_id' parameter"));
                };
                match store::cancel_task(&cfg, task_id) {
                    Ok(_) => Ok(ToolResult {
                        success: true,
                        output: format!("Cancelled xin task {task_id}"),
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "goal_add" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let Some(name) = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'name' parameter"));
                };
                let mut initial_steps = Vec::new();
                if let Some(steps) = args.get("steps").and_then(serde_json::Value::as_array) {
                    for (index, step) in steps.iter().enumerate() {
                        let sequence = step
                            .get("sequence")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or((index + 1) as u64);
                        let sequence =
                            u32::try_from(sequence).map_err(|_| anyhow::anyhow!("step sequence exceeds u32"))?;
                        initial_steps.push(self.build_step(step, sequence, &args)?);
                    }
                }
                let target_completion_at = args
                    .get("target_completion_at")
                    .and_then(serde_json::Value::as_str)
                    .map(|value| {
                        chrono::DateTime::parse_from_rfc3339(value)
                            .map(|time| time.with_timezone(&chrono::Utc))
                            .map_err(|error| anyhow::anyhow!("Invalid target_completion_at: {error}"))
                    })
                    .transpose()?;
                let lineage = parse_xin_lineage_scope(&cfg, &args);
                let new = NewXinGoal {
                    owner_id: lineage.owner_id,
                    topic_id: lineage.topic_id,
                    parent_task_id: lineage.parent_task_id,
                    source_message_event_id: lineage.source_message_event_id,
                    name: name.to_string(),
                    description: args
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    kind: TaskKind::User,
                    priority: TaskPriority::from_str_lossy(
                        args.get("priority")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("normal"),
                    ),
                    target_completion_at,
                    initial_steps,
                };
                match store::add_goal(&cfg, &new) {
                    Ok(goal) => Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&goal)?,
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "goal_pause" | "goal_resume" | "goal_cancel" | "goal_remove" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let Some(goal_id) = args
                    .get("goal_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'goal_id' parameter"));
                };
                let result = match action {
                    "goal_pause" => store::pause_goal(&cfg, goal_id).map(|_| ()),
                    "goal_resume" => store::resume_goal(&cfg, goal_id).map(|_| ()),
                    "goal_cancel" => store::cancel_goal(&cfg, goal_id).map(|_| ()),
                    "goal_remove" => store::remove_goal(&cfg, goal_id),
                    _ => Err(anyhow::anyhow!("Unsupported goal action '{action}'")),
                };
                match result {
                    Ok(()) => Ok(ToolResult {
                        success: true,
                        output: format!("{} xin goal {goal_id}", action.trim_start_matches("goal_")),
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "step_add" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let Some(goal_id) = args
                    .get("goal_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'goal_id' parameter"));
                };
                let sequence = match args.get("sequence").and_then(serde_json::Value::as_u64) {
                    Some(value) => u32::try_from(value).map_err(|_| anyhow::anyhow!("sequence exceeds u32"))?,
                    None => store::list_steps(&cfg, goal_id)?
                        .iter()
                        .map(|step| step.sequence)
                        .max()
                        .unwrap_or(0)
                        .saturating_add(1),
                };
                let step = self.build_step(&args, sequence, &args)?;
                match store::add_step(&cfg, goal_id, &step) {
                    Ok(step) => Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&step)?,
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            "step_retry" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
                    return Ok(r);
                }
                let Some(step_id) = args
                    .get("step_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                else {
                    return Ok(tool_error("Missing 'step_id' parameter"));
                };
                match store::retry_step(&cfg, step_id) {
                    Ok(step) => Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&step)?,
                        error: None,
                    }),
                    Err(error) => Ok(tool_error(error.to_string())),
                }
            }

            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unknown action '{other}'. Consult the xin tool action schema.")),
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
    use crate::config::new_shared;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        }
    }

    #[test]
    fn parse_xin_lineage_scope_derives_owner_and_parent_task() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let args = serde_json::json!({
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "sender": "alice",
                "channel": "telegram",
                "chat_id": "chat-1",
                "topic_id": "topic-1",
                "task_id": "run-parent",
                "message_event_id": "msg-1"
            }
        });

        let scope = parse_xin_lineage_scope(&config, &args);
        let expected_owner = format!("owner:{}:telegram:alice", config.workspace_dir.to_string_lossy());

        assert_eq!(scope.owner_id.as_deref(), Some(expected_owner.as_str()));
        assert_eq!(scope.topic_id.as_deref(), Some("topic-1"));
        assert_eq!(scope.parent_task_id.as_deref(), Some("run-parent"));
        assert_eq!(scope.source_message_event_id.as_deref(), Some("msg-1"));
    }

    #[test]
    fn parse_xin_lineage_scope_ignores_untrusted_scope() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let args = serde_json::json!({
            "_zc_scope_trusted": false,
            "_zc_scope": {
                "owner_id": "owner-forged",
                "topic_id": "topic-forged"
            }
        });

        let scope = parse_xin_lineage_scope(&config, &args);

        assert!(scope.owner_id.is_none());
        assert!(scope.topic_id.is_none());
    }

    #[tokio::test]
    async fn status_reports_tasks_goals_and_steps_separately() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let tool = XinTool::new(new_shared(config), security);

        let result = tool.execute(json!({"action": "status"})).await.unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("Tasks:       0 total"));
        assert!(result.output.contains("Goals:       0 total"));
        assert!(result.output.contains("Steps:       0 total"));
        assert!(result.output.contains("Concurrency: unbounded"));
    }

    #[tokio::test]
    async fn task_management_actions_are_connected_to_store() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let tool = XinTool::new(new_shared(config.clone()), security);

        let added = tool
            .execute(json!({
                "action": "add",
                "name": "managed-task",
                "payload": "inspect health",
                "recurring": true,
                "interval_secs": 300
            }))
            .await
            .unwrap();
        assert!(added.success, "{:?}", added.error);
        let task = store::list_tasks(&config).unwrap().remove(0);

        let updated = tool
            .execute(json!({"action": "update", "task_id": task.id, "priority": "high"}))
            .await
            .unwrap();
        assert!(updated.success, "{:?}", updated.error);
        assert_eq!(store::get_task(&config, &task.id).unwrap().priority, TaskPriority::High);

        assert!(
            tool.execute(json!({"action": "pause", "task_id": task.id}))
                .await
                .unwrap()
                .success
        );
        assert!(!store::get_task(&config, &task.id).unwrap().enabled);
        assert!(
            tool.execute(json!({"action": "resume", "task_id": task.id}))
                .await
                .unwrap()
                .success
        );
        assert!(store::get_task(&config, &task.id).unwrap().enabled);

        let now = chrono::Utc::now();
        store::record_run(&config, &task.id, now, now, "ok", Some("observed"), 1).unwrap();
        let runs = tool
            .execute(json!({"action": "runs", "task_id": task.id}))
            .await
            .unwrap();
        assert!(runs.success);
        assert!(runs.output.contains("observed"));

        assert!(
            tool.execute(json!({"action": "cancel", "task_id": task.id}))
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            store::get_task(&config, &task.id).unwrap().status,
            TaskStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn goal_and_step_management_actions_are_connected_to_store() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let tool = XinTool::new(new_shared(config.clone()), security);

        let added = tool
            .execute(json!({
                "action": "goal_add",
                "name": "managed-goal",
                "steps": [{"name": "first", "payload": "do first"}]
            }))
            .await
            .unwrap();
        assert!(added.success, "{:?}", added.error);
        let goal = store::list_goals(&config).unwrap().remove(0);
        assert_eq!(store::list_steps(&config, &goal.id).unwrap().len(), 1);

        let step_added = tool
            .execute(json!({
                "action": "step_add",
                "goal_id": goal.id,
                "name": "second",
                "payload": "do second"
            }))
            .await
            .unwrap();
        assert!(step_added.success, "{:?}", step_added.error);
        assert_eq!(store::list_steps(&config, &goal.id).unwrap().len(), 2);

        for action in ["goal_pause", "goal_resume", "goal_cancel", "goal_resume"] {
            let result = tool
                .execute(json!({"action": action, "goal_id": goal.id}))
                .await
                .unwrap();
            assert!(result.success, "{action}: {:?}", result.error);
        }
        assert!(store::get_goal(&config, &goal.id).unwrap().enabled);
    }

    #[tokio::test]
    async fn trusted_owner_scope_cannot_read_or_mutate_another_owners_task() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let tool = XinTool::new(new_shared(config.clone()), security);
        let owner_a = json!({
            "channel": "telegram",
            "sender": "user_a",
            "chat_id": "chat-a"
        });
        let owner_b = json!({
            "channel": "telegram",
            "sender": "user_b",
            "chat_id": "chat-b"
        });
        assert!(
            tool.execute(json!({
                "action": "add",
                "name": "private-task",
                "payload": "inspect",
                "_zc_scope_trusted": true,
                "_zc_scope": owner_a
            }))
            .await
            .unwrap()
            .success
        );
        let task = store::list_tasks(&config).unwrap().remove(0);

        let denied = tool
            .execute(json!({
                "action": "get",
                "task_id": task.id,
                "_zc_scope_trusted": true,
                "_zc_scope": owner_b
            }))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied.error.unwrap().contains("owner scope"));
    }
}
