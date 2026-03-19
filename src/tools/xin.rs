//! LLM tool for the xin (心) autonomous task heartbeat engine.
//!
//! Actions:
//!  - list — list all xin tasks
//!  - get — get details of a single task
//!  - add — create a new user task
//!  - remove — delete a task
//!  - pause / resume — disable/enable a task
//!  - status — show xin subsystem status

use super::traits::{Tool, ToolResult};
use crate::config::{Config, SharedConfig};
use crate::security::SecurityPolicy;
use crate::xin::store;
use crate::xin::types::{
    ExecutionMode, NewXinTask, TaskKind, TaskPriority, XinTask, XinTaskPatch,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct XinTool {
    config: SharedConfig,
    security: Arc<SecurityPolicy>,
}

impl XinTool {
    pub fn new(config: SharedConfig, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn check_enabled(&self, cfg: &Config) -> Option<ToolResult> {
        if !cfg.xin.enabled {
            Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("xin is disabled by config (xin.enabled=false)".to_string()),
            })
        } else {
            None
        }
    }

    fn enforce_mutation(&self, action: &str, cfg: &Config) -> Option<ToolResult> {
        if let Some(r) = self.check_enabled(cfg) {
            return Some(r);
        }
        if !self.security.can_act() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Security policy: read-only mode, cannot perform '{action}'"
                )),
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

#[async_trait]
impl Tool for XinTool {
    fn name(&self) -> &str {
        "xin"
    }

    fn description(&self) -> &str {
        "Xin (心) autonomous task heartbeat engine. \
         Actions: list, get, add, remove, pause, resume, status."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "get", "add", "remove", "pause", "resume", "status"],
                    "description": "Action to perform."
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID for get/remove/pause/resume actions."
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
                            output: format!(
                                "Xin tasks ({}):\n{}",
                                tasks.len(),
                                lines.join("\n")
                            ),
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
                let system = tasks
                    .iter()
                    .filter(|t| t.kind == TaskKind::System)
                    .count();
                let user = tasks.iter().filter(|t| t.kind == TaskKind::User).count();
                let agent = tasks.iter().filter(|t| t.kind == TaskKind::Agent).count();

                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Xin Status\n\
                         ──────────\n\
                         Enabled:     true\n\
                         Interval:    {} min\n\
                         Tasks:       {} total ({active} active, {paused} paused)\n\
                         By kind:     {system} system, {user} user, {agent} agent\n\
                         Concurrency: {} max\n\
                         Evolution:   {}",
                        cfg.xin.interval_minutes,
                        tasks.len(),
                        cfg.xin.max_concurrent,
                        if cfg.xin.evolution_integration {
                            "integrated"
                        } else {
                            "standalone"
                        }
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
                let description = args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let execution_mode = match args
                    .get("execution_mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent_session")
                {
                    "shell" => ExecutionMode::Shell,
                    _ => ExecutionMode::AgentSession,
                };
                let priority = TaskPriority::from_str_lossy(
                    args.get("priority")
                        .and_then(|v| v.as_str())
                        .unwrap_or("normal"),
                );
                let recurring = args
                    .get("recurring")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let interval_secs = args
                    .get("interval_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                // Prevent busy-loop: recurring tasks must have ≥60s interval.
                if recurring && interval_secs < 60 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "Recurring tasks require interval_secs >= 60 to prevent busy-loops"
                                .to_string(),
                        ),
                    });
                }

                let new = NewXinTask {
                    name,
                    description,
                    kind: TaskKind::User,
                    priority,
                    execution_mode,
                    payload,
                    recurring,
                    interval_secs,
                    max_failures: 3,
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
                    "Unknown action '{other}'. Use: list, get, add, remove, pause, resume, status."
                )),
            }),
        }
    }
}
