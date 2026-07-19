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
use crate::xin::types::{ExecutionMode, NewXinTask, TaskKind, TaskPriority, XinTask, XinTaskEvent, XinTaskPatch};
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
        if self.security.is_rate_limited() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".to_string()),
            });
        }
        if !self.security.record_action() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".to_string()),
            });
        }
        None
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
    lines.push(format!("Max Fails:   {}", task.max_failures));
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
         Actions: list, get, events, add, remove, pause, resume, status."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "get", "events", "add", "remove", "pause", "resume", "status"],
                    "description": "Action to perform."
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID for get/events/remove/pause/resume actions."
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

        match action {
            // ── Read-only ──────────────────────────────────────────────
            "list" => {
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                match store::list_tasks(&cfg) {
                    Ok(tasks) => {
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

            "status" => {
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                let tasks = match store::list_tasks(&cfg) {
                    Ok(t) => t,
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to query xin tasks: {e}")),
                        });
                    }
                };
                let active = tasks.iter().filter(|t| t.enabled).count();
                let paused = tasks.len() - active;
                let system = tasks.iter().filter(|t| t.kind == TaskKind::System).count();
                let user = tasks.iter().filter(|t| t.kind == TaskKind::User).count();
                let agent = tasks.iter().filter(|t| t.kind == TaskKind::Agent).count();

                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Xin Status\n\
                         ──────────\n\
                         Interval:    {} min\n\
                         Tasks:       {} total ({active} active, {paused} paused)\n\
                         By kind:     {system} system, {user} user, {agent} agent\n\
                         Concurrency: {} max\n\
                         Evolution:   integrated",
                        cfg.xin.interval_minutes,
                        tasks.len(),
                        cfg.xin.max_concurrent,
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
                    max_failures: 3,
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
                let patch = XinTaskPatch {
                    enabled: Some(false),
                    ..XinTaskPatch::default()
                };
                match store::update_task(&cfg, task_id, &patch) {
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
                let patch = XinTaskPatch {
                    enabled: Some(true),
                    ..XinTaskPatch::default()
                };
                match store::update_task(&cfg, task_id, &patch) {
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

            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{other}'. Use: list, get, events, add, remove, pause, resume, status."
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
