//! Unified cron management tool — single entry point for all cron operations.
//!
//! Consolidates the seven individual cron tools (cron_add, cron_list, cron_remove,
//! cron_update, cron_run, cron_runs, schedule) into a single `cron` tool with an
//! `action` parameter dispatcher, aligning with the OpenClaw unified interface.
//!
//! Actions:
//!  - add / schedule / once — create a new job (cron expr, delay, or run_at)
//!  - list — list all scheduled jobs
//!  - remove / cancel — delete a job by id
//!  - update / patch — patch fields of an existing job
//!  - run — force-run a job immediately
//!  - runs / history — list run history for a job
//!  - get — fetch details of a single job
//!  - pause / resume — enable/disable a job without removing it
//!  - status — show cron subsystem status

use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron::{self, CronJobPatch, JobType, Schedule};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

const MAX_RUN_OUTPUT_CHARS: usize = 500;

pub struct CronTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl CronTool {
    pub fn new(config: Arc<Config>, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn check_enabled(&self) -> Option<ToolResult> {
        if !self.config.cron.enabled {
            Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cron is disabled by config (cron.enabled=false)".to_string()),
            })
        } else {
            None
        }
    }

    fn enforce_mutation(&self, action: &str) -> Option<ToolResult> {
        if let Some(r) = self.check_enabled() {
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

#[derive(Serialize)]
struct RunView {
    id: i64,
    job_id: String,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: String,
    output: Option<String>,
    duration_ms: Option<i64>,
}

fn truncate_str(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out: String = input.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Unified cron/scheduler management. \
         Actions: add/schedule/once (create job), list, get, remove/cancel, update/patch, \
         run (force-run now), runs/history (run log), pause, resume, status."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "add", "schedule", "once",
                        "list",
                        "get",
                        "remove", "cancel",
                        "update", "patch",
                        "run",
                        "runs", "history",
                        "pause", "resume",
                        "status"
                    ],
                    "description": "Action to perform."
                },
                "job_id": {
                    "type": "string",
                    "description": "Job ID for get/remove/update/run/runs/pause/resume actions."
                },
                "expression": {
                    "type": "string",
                    "description": "Cron expression (e.g. '*/5 * * * *') for recurring jobs."
                },
                "command": {
                    "type": "string",
                    "description": "Shell command to execute."
                },
                "delay": {
                    "type": "string",
                    "description": "Delay for one-shot jobs (e.g. '30m', '2h')."
                },
                "run_at": {
                    "type": "string",
                    "description": "Absolute RFC3339 timestamp for one-shot jobs."
                },
                "patch": {
                    "type": "object",
                    "description": "Fields to update for the 'update/patch' action."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max entries for 'runs' action (default 10)."
                },
                "approved": {
                    "type": "boolean",
                    "description": "Explicitly approve medium/high-risk shell commands in supervised mode.",
                    "default": false
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
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
            // ── Read-only ──────────────────────────────────────────────────────────
            "list" => {
                if let Some(r) = self.check_enabled() {
                    return Ok(r);
                }
                match cron::list_jobs(&self.config) {
                    Ok(jobs) => {
                        if jobs.is_empty() {
                            return Ok(ToolResult {
                                success: true,
                                output: "No scheduled cron jobs.".to_string(),
                                error: None,
                            });
                        }
                        let mut lines = Vec::with_capacity(jobs.len());
                        for job in &jobs {
                            let paused = !job.enabled;
                            let one_shot = matches!(job.schedule, Schedule::At { .. });
                            let flags = match (paused, one_shot) {
                                (true, true) => " [disabled, one-shot]",
                                (true, false) => " [disabled]",
                                (false, true) => " [one-shot]",
                                (false, false) => "",
                            };
                            let last_run = job
                                .last_run
                                .map_or_else(|| "never".to_string(), |v| v.to_rfc3339());
                            let last_status = job.last_status.as_deref().unwrap_or("n/a");
                            lines.push(format!(
                                "- {} | {} | next={} | last={} ({}){} | cmd: {}",
                                job.id,
                                job.expression,
                                job.next_run.to_rfc3339(),
                                last_run,
                                last_status,
                                flags,
                                job.command
                            ));
                        }
                        Ok(ToolResult {
                            success: true,
                            output: format!(
                                "Scheduled jobs ({}):\n{}",
                                jobs.len(),
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
                if let Some(r) = self.check_enabled() {
                    return Ok(r);
                }
                let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'job_id' parameter".to_string()),
                        });
                    }
                };
                match cron::get_job(&self.config, job_id) {
                    Ok(job) => {
                        let detail = json!({
                            "id": job.id,
                            "expression": job.expression,
                            "command": job.command,
                            "next_run": job.next_run.to_rfc3339(),
                            "last_run": job.last_run.map(|v| v.to_rfc3339()),
                            "last_status": job.last_status,
                            "enabled": job.enabled,
                            "one_shot": matches!(job.schedule, Schedule::At { .. }),
                        });
                        Ok(ToolResult {
                            success: true,
                            output: serde_json::to_string_pretty(&detail)?,
                            error: None,
                        })
                    }
                    Err(_) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Job '{job_id}' not found")),
                    }),
                }
            }

            "status" => {
                if let Some(r) = self.check_enabled() {
                    return Ok(r);
                }
                let jobs = cron::list_jobs(&self.config).unwrap_or_default();
                let enabled_count = jobs.iter().filter(|j| j.enabled).count();
                let disabled_count = jobs.len() - enabled_count;
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "⏰ Cron Status\n\
                         ─────────────\n\
                         Enabled:  true\n\
                         Jobs:     {} total ({} active, {} paused)",
                        jobs.len(),
                        enabled_count,
                        disabled_count,
                    ),
                    error: None,
                })
            }

            "runs" | "history" => {
                if let Some(r) = self.check_enabled() {
                    return Ok(r);
                }
                let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'job_id' parameter".to_string()),
                        });
                    }
                };
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map_or(10, |v| usize::try_from(v).unwrap_or(10));

                match cron::list_runs(&self.config, job_id, limit) {
                    Ok(runs) => {
                        let views: Vec<RunView> = runs
                            .into_iter()
                            .map(|run| RunView {
                                id: run.id,
                                job_id: run.job_id,
                                started_at: run.started_at,
                                finished_at: run.finished_at,
                                status: run.status,
                                output: run
                                    .output
                                    .map(|out| truncate_str(&out, MAX_RUN_OUTPUT_CHARS)),
                                duration_ms: run.duration_ms,
                            })
                            .collect();
                        Ok(ToolResult {
                            success: true,
                            output: serde_json::to_string_pretty(&views)?,
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

            // ── Mutating ───────────────────────────────────────────────────────────
            "add" | "schedule" => {
                if let Some(r) = self.enforce_mutation(action) {
                    return Ok(r);
                }
                let expression = match args.get("expression").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'expression' parameter for add action".to_string()),
                        });
                    }
                };
                let command = match args.get("command").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'command' parameter".to_string()),
                        });
                    }
                };
                let approved = args
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Err(reason) = self.security.validate_command_execution(command, approved) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(reason),
                    });
                }
                match cron::add_job(&self.config, expression, command) {
                    Ok(job) => Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Created recurring job {} (expr: {}, next: {}, cmd: {})",
                            job.id,
                            job.expression,
                            job.next_run.to_rfc3339(),
                            job.command
                        ),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "once" => {
                if let Some(r) = self.enforce_mutation(action) {
                    return Ok(r);
                }
                let command = match args.get("command").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'command' parameter".to_string()),
                        });
                    }
                };
                let approved = args
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Err(reason) = self.security.validate_command_execution(command, approved) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(reason),
                    });
                }

                let delay = args.get("delay").and_then(|v| v.as_str());
                let run_at = args.get("run_at").and_then(|v| v.as_str());

                match (delay, run_at) {
                    (Some(d), None) => match cron::add_once(&self.config, d, command) {
                        Ok(job) => Ok(ToolResult {
                            success: true,
                            output: format!(
                                "Created one-shot job {} (runs at: {}, cmd: {})",
                                job.id,
                                job.next_run.to_rfc3339(),
                                job.command
                            ),
                            error: None,
                        }),
                        Err(e) => Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(e.to_string()),
                        }),
                    },
                    (None, Some(at)) => {
                        let run_at_parsed: DateTime<Utc> =
                            match DateTime::parse_from_rfc3339(at) {
                                Ok(v) => v.with_timezone(&Utc),
                                Err(e) => {
                                    return Ok(ToolResult {
                                        success: false,
                                        output: String::new(),
                                        error: Some(format!(
                                            "Invalid run_at timestamp: {e}"
                                        )),
                                    });
                                }
                            };
                        match cron::add_once_at(&self.config, run_at_parsed, command) {
                            Ok(job) => Ok(ToolResult {
                                success: true,
                                output: format!(
                                    "Created one-shot job {} (runs at: {}, cmd: {})",
                                    job.id,
                                    job.next_run.to_rfc3339(),
                                    job.command
                                ),
                                error: None,
                            }),
                            Err(e) => Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(e.to_string()),
                            }),
                        }
                    }
                    _ => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "'once' requires exactly one of 'delay' or 'run_at'".to_string(),
                        ),
                    }),
                }
            }

            "remove" | "cancel" => {
                if let Some(r) = self.enforce_mutation(action) {
                    return Ok(r);
                }
                let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'job_id' parameter".to_string()),
                        });
                    }
                };
                match cron::remove_job(&self.config, job_id) {
                    Ok(()) => Ok(ToolResult {
                        success: true,
                        output: format!("Removed cron job {job_id}"),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "update" | "patch" => {
                if let Some(r) = self.enforce_mutation(action) {
                    return Ok(r);
                }
                let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'job_id' parameter".to_string()),
                        });
                    }
                };
                let patch_val = match args.get("patch") {
                    Some(v) => v.clone(),
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'patch' parameter".to_string()),
                        });
                    }
                };
                let patch = match serde_json::from_value::<CronJobPatch>(patch_val) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Invalid patch payload: {e}")),
                        });
                    }
                };
                let approved = args
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(command) = &patch.command {
                    if let Err(reason) =
                        self.security.validate_command_execution(command, approved)
                    {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(reason),
                        });
                    }
                }
                match cron::update_job(&self.config, job_id, patch) {
                    Ok(job) => Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&job)?,
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            "run" => {
                // Inline force-run logic (same as CronRunTool)
                if let Some(r) = self.check_enabled() {
                    return Ok(r);
                }
                let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'job_id' parameter".to_string()),
                        });
                    }
                };
                let approved = args
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !self.security.can_act() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "Security policy: read-only mode, cannot perform 'cron run'".into(),
                        ),
                    });
                }
                if self.security.is_rate_limited() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "Rate limit exceeded: too many actions in the last hour".into(),
                        ),
                    });
                }
                let job = match cron::get_job(&self.config, job_id) {
                    Ok(j) => j,
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(e.to_string()),
                        });
                    }
                };
                if matches!(job.job_type, JobType::Shell) {
                    if let Err(reason) =
                        self.security.validate_command_execution(&job.command, approved)
                    {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(reason),
                        });
                    }
                }
                if !self.security.record_action() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Rate limit exceeded: action budget exhausted".into()),
                    });
                }
                let started_at = Utc::now();
                let (success, output) =
                    cron::scheduler::execute_job_now(&self.config, &job).await;
                let finished_at = Utc::now();
                let duration_ms = (finished_at - started_at).num_milliseconds();
                let status = if success { "ok" } else { "error" };
                let _ = cron::record_run(
                    &self.config,
                    &job.id,
                    started_at,
                    finished_at,
                    status,
                    Some(&output),
                    duration_ms,
                );
                let _ = cron::record_last_run(
                    &self.config,
                    &job.id,
                    finished_at,
                    success,
                    &output,
                );
                Ok(ToolResult {
                    success,
                    output: serde_json::to_string_pretty(&json!({
                        "job_id": job.id,
                        "status": status,
                        "duration_ms": duration_ms,
                        "output": output
                    }))?,
                    error: if success {
                        None
                    } else {
                        Some("cron job execution failed".to_string())
                    },
                })
            }

            "pause" => {
                if let Some(r) = self.enforce_mutation(action) {
                    return Ok(r);
                }
                let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'job_id' parameter".to_string()),
                        });
                    }
                };
                match cron::pause_job(&self.config, job_id) {
                    Ok(_) => Ok(ToolResult {
                        success: true,
                        output: format!("Paused job {job_id}"),
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
                if let Some(r) = self.enforce_mutation(action) {
                    return Ok(r);
                }
                let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'job_id' parameter".to_string()),
                        });
                    }
                };
                match cron::resume_job(&self.config, job_id) {
                    Ok(_) => Ok(ToolResult {
                        success: true,
                        output: format!("Resumed job {job_id}"),
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
                    "Unknown action '{other}'. Use: add, schedule, once, list, get, remove, cancel, update, patch, run, runs, history, pause, resume, status."
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::security::AutonomyLevel;
    use tempfile::TempDir;

    async fn test_config(tmp: &TempDir) -> Arc<Config> {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        Arc::new(config)
    }

    fn test_security(cfg: &Config) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::from_config(
            &cfg.autonomy,
            &cfg.workspace_dir,
        ))
    }

    #[tokio::test]
    async fn list_empty() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));
        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No scheduled cron jobs"));
    }

    #[tokio::test]
    async fn add_and_list_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));

        let add = tool
            .execute(json!({"action": "add", "expression": "*/5 * * * *", "command": "echo hi"}))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let list = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(list.success);
        assert!(list.output.contains("echo hi"));
    }

    #[tokio::test]
    async fn status_reports_job_counts() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        cron::add_job(&cfg, "*/5 * * * *", "echo status-test").unwrap();
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));
        let result = tool.execute(json!({"action": "status"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 total"));
    }

    #[tokio::test]
    async fn get_job_details() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo get-test").unwrap();
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({"action": "get", "job_id": job.id}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("echo get-test"));
    }

    #[tokio::test]
    async fn remove_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo remove-test").unwrap();
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({"action": "remove", "job_id": job.id}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(cron::list_jobs(&cfg).unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_job_enabled() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo upd-test").unwrap();
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({"action": "update", "job_id": job.id, "patch": {"enabled": false}}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("\"enabled\": false"));
    }

    #[tokio::test]
    async fn disabled_cron_returns_error() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.cron.enabled = false;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));
        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("cron is disabled"));
    }

    #[tokio::test]
    async fn readonly_blocks_add() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({"action": "add", "expression": "*/5 * * * *", "command": "echo x"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only"));
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({"action": "explode"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Unknown action"));
    }
}
