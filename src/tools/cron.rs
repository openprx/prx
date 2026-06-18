//! Unified cron management tool — single entry point for all cron operations.
//!
//! Consolidates the seven previously-separate cron tools (cron_add, cron_list,
//! cron_remove, cron_update, cron_run, cron_runs, schedule) — now removed — into a
//! single `cron` tool with an `action` parameter dispatcher, aligning with the
//! OpenClaw unified interface.
//!
//! The individual tools have been removed; `cron` is the sole scheduler tool.
//!
//! Actions:
//!  - add / schedule — create a recurring (or `at`/`every`) job. Accepts either
//!    the simple shell form (`expression` + `command`) or the full form
//!    (`schedule` object + `payload`/`job_type` for shell or agent jobs, with
//!    optional `name`/`session_target`/`model`/`delivery`/`delete_after_run`).
//!  - once — create a one-shot shell job (cron expr, delay, or run_at)
//!  - list — list all scheduled jobs
//!  - remove / cancel — delete a job by id
//!  - update / patch — patch fields of an existing job
//!  - run — force-run a job immediately
//!  - runs / history — list run history for a job
//!  - get — fetch details of a single job
//!  - events — list append-only lifecycle events for a job
//!  - pause / resume — enable/disable a job without removing it
//!  - status — show cron subsystem status

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::{Config, SharedConfig};
use crate::cron::{self, CronJobPatch, DeliveryConfig, JobType, Schedule, SessionTarget};
use crate::security::policy::{ApprovalGrant, PERSISTED_APPROVAL_GRANT_TTL_SECS};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

const MAX_RUN_OUTPUT_CHARS: usize = 500;

pub struct CronTool {
    config: SharedConfig,
    security: Arc<SecurityPolicy>,
}

impl CronTool {
    pub const fn new(config: SharedConfig, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    /// A human-readable warning to append when scheduled jobs will NOT fire in
    /// the background because the scheduler module is disabled. The `cron` tool
    /// is always registered (so the model can manage jobs), but the background
    /// scheduler loop is only started when the scheduler module is enabled — so
    /// without it, newly created jobs are persisted but never triggered.
    const fn scheduler_inactive_notice(cfg: &Config) -> &'static str {
        if cfg.modules.scheduler {
            ""
        } else {
            "\n⚠ Scheduler module is disabled (modules.scheduler=false): this job is saved \
             but will NOT run in the background until the scheduler module is enabled. \
             You can still force-run it now with action='run'."
        }
    }

    fn check_enabled(&self, cfg: &Config) -> Option<ToolResult> {
        if !cfg.cron.enabled {
            Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cron is disabled by config (cron.enabled=false)".to_string()),
            })
        } else {
            None
        }
    }

    /// Mutation pre-checks WITHOUT consuming the action budget: module enabled,
    /// read-only mode, and the hourly rate-limit window. Callers that defer the
    /// budget charge until after parameter validation (e.g. `add`/`schedule`,
    /// which validate params + run the shell approval gate before touching the
    /// DB) use this and then call [`Self::consume_action_budget`] right before
    /// the DB write — matching the legacy `cron_add` semantics where invalid
    /// requests never burned a budget slot.
    fn enforce_mutation_no_budget(&self, action: &str, cfg: &Config) -> Option<ToolResult> {
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
        None
    }

    /// Consume one action from the security budget. Returns an error `ToolResult`
    /// when the budget is exhausted, otherwise `None`.
    fn consume_action_budget(&self) -> Option<ToolResult> {
        if self.security.record_action() {
            None
        } else {
            Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".to_string()),
            })
        }
    }

    /// Full mutation pre-check: [`Self::enforce_mutation_no_budget`] plus an
    /// immediate budget charge. Used by mutating actions that don't have
    /// significant post-validation gating before the DB write.
    fn enforce_mutation(&self, action: &str, cfg: &Config) -> Option<ToolResult> {
        if let Some(r) = self.enforce_mutation_no_budget(action, cfg) {
            return Some(r);
        }
        self.consume_action_budget()
    }

    /// Create a recurring (or `at`/`every`) job from the `add`/`schedule` action.
    ///
    /// Accepts both the simple shell form (`expression` string + `command`) and
    /// the full job form (`schedule` object + `payload`/`job_type` for shell or
    /// agent jobs, plus `name`/`session_target`/`model`/`delivery`/
    /// `delete_after_run`). Callers must already have passed `enforce_mutation`.
    fn handle_add(&self, cfg: &Arc<Config>, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        // Resolve the schedule: prefer an explicit `schedule` object, else fall
        // back to the plain `expression` string (treated as a cron expression).
        let schedule = if let Some(v) = args.get("schedule") {
            match serde_json::from_value::<Schedule>(v.clone()) {
                Ok(schedule) => schedule,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Invalid schedule: {e}")),
                    });
                }
            }
        } else if let Some(expr) = args.get("expression").and_then(|v| v.as_str()) {
            if expr.trim().is_empty() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Empty 'expression' for add action".to_string()),
                });
            }
            Schedule::Cron {
                expr: expr.to_string(),
                tz: None,
            }
        } else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing 'expression' or 'schedule' parameter for add action".to_string()),
            });
        };

        let name = args.get("name").and_then(serde_json::Value::as_str).map(str::to_string);

        // OpenClaw-compatible payload.kind API as an alias for job_type + prompt/command.
        let payload = args.get("payload");
        let payload_kind = payload.and_then(|p| p.get("kind")).and_then(serde_json::Value::as_str);
        let job_type = if let Some(kind) = payload_kind {
            match kind {
                "agentTurn" | "systemEvent" => JobType::Agent,
                other => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Invalid payload.kind: {other}. Use 'agentTurn' or 'systemEvent'"
                        )),
                    });
                }
            }
        } else {
            match args.get("job_type").and_then(serde_json::Value::as_str) {
                Some("agent") => JobType::Agent,
                Some("shell") => JobType::Shell,
                Some(other) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Invalid job_type: {other}")),
                    });
                }
                None => {
                    if args.get("prompt").is_some() {
                        JobType::Agent
                    } else {
                        JobType::Shell
                    }
                }
            }
        };

        let default_delete_after_run = matches!(schedule, Schedule::At { .. });
        let delete_after_run = args
            .get("delete_after_run")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(default_delete_after_run);
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), args);
        let lineage = cron::lineage_from_trusted_scope(cfg, args);

        let result = match job_type {
            JobType::Shell => {
                let command = match args.get("command").and_then(serde_json::Value::as_str) {
                    Some(command) if !command.trim().is_empty() => command,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'command' for shell job".to_string()),
                        });
                    }
                };
                if let Err(reason) = SideEffectGate::new(self.security.as_ref()).authorize_command_execution(
                    self.name(),
                    command,
                    approval_grant.as_ref(),
                ) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(reason),
                    });
                }
                // All shell-job validation + approval passed: charge the budget now,
                // immediately before the DB write (legacy cron_add parity).
                if let Some(r) = self.consume_action_budget() {
                    return Ok(r);
                }
                let persisted_grant = ApprovalGrant::persisted_runner_grant(
                    "cron_scheduler",
                    command,
                    approval_grant.as_ref(),
                    PERSISTED_APPROVAL_GRANT_TTL_SECS,
                )
                .map(|grant| serde_json::to_string(&grant))
                .transpose()?;
                cron::add_shell_job_with_lineage_and_approval_grant(
                    cfg,
                    name,
                    schedule,
                    command,
                    persisted_grant,
                    lineage,
                )
            }
            JobType::Agent => {
                // Resolve prompt: payload.message > payload.text > prompt field.
                let prompt_str = payload
                    .and_then(|p| p.get("message").or_else(|| p.get("text")))
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| {
                        args.get("prompt")
                            .and_then(serde_json::Value::as_str)
                            .filter(|s| !s.trim().is_empty())
                    });
                let prompt = match prompt_str {
                    Some(p) => p,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Missing prompt for agent job. Use payload.message, payload.text, or the 'prompt' field."
                                    .to_string(),
                            ),
                        });
                    }
                };
                // payload.kind='systemEvent' → main session; otherwise isolated unless overridden.
                let session_target = if payload_kind == Some("systemEvent") {
                    SessionTarget::Main
                } else {
                    match args.get("session_target") {
                        Some(v) => match serde_json::from_value::<SessionTarget>(v.clone()) {
                            Ok(target) => target,
                            Err(e) => {
                                return Ok(ToolResult {
                                    success: false,
                                    output: String::new(),
                                    error: Some(format!("Invalid session_target: {e}")),
                                });
                            }
                        },
                        None => SessionTarget::Isolated,
                    }
                };
                let model = args
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                let delivery = match args.get("delivery") {
                    Some(v) => match serde_json::from_value::<DeliveryConfig>(v.clone()) {
                        Ok(delivery_cfg) => Some(delivery_cfg),
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid delivery config: {e}")),
                            });
                        }
                    },
                    None => None,
                };
                // All agent-job validation passed (prompt/session/model/delivery):
                // charge the budget now, immediately before the DB write (legacy
                // cron_add parity — invalid requests never burn a budget slot).
                if let Some(r) = self.consume_action_budget() {
                    return Ok(r);
                }
                cron::add_agent_job_with_lineage(
                    cfg,
                    name,
                    schedule,
                    prompt,
                    session_target,
                    model,
                    delivery,
                    delete_after_run,
                    lineage,
                )
            }
        };

        match result {
            Ok(job) => Ok(ToolResult {
                success: true,
                output: format!(
                    "Created job {} (expr: {}, next: {}, cmd: {}){}",
                    job.id,
                    job.expression,
                    job.next_run.to_rfc3339(),
                    job.command,
                    Self::scheduler_inactive_notice(cfg)
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
        "Unified cron/scheduler management — the single entry point for ALL scheduled-task \
         operations. Actions: \
         add/schedule (create recurring job: pass `expression` + `command` for a shell job, \
         or a `schedule` object {kind:'cron'|'at'|'every'} with `payload`/`job_type` for shell \
         or agent jobs, optionally `name`, `session_target`, `model`, `delivery`, \
         `delete_after_run`); \
         once (one-shot shell job via `delay` or `run_at`); \
         list; get; remove/cancel; update/patch; run (force-run now); runs/history (run log); \
         events; pause; resume; status."
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
                        "events",
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
                    "description": "Cron expression (e.g. '*/5 * * * *') for recurring shell jobs (add/schedule)."
                },
                "command": {
                    "type": "string",
                    "description": "Shell command to execute (shell jobs)."
                },
                "delay": {
                    "type": "string",
                    "description": "Delay for one-shot jobs (e.g. '30m', '2h')."
                },
                "run_at": {
                    "type": "string",
                    "description": "Absolute RFC3339 timestamp for one-shot jobs."
                },
                "schedule": {
                    "type": "object",
                    "description": "Schedule object for add/schedule: {kind:'cron',expr:'0 9 * * *',tz?:'America/New_York'} | {kind:'at',at:'ISO-8601'} | {kind:'every',every_ms:30000}. Alternative to the plain 'expression' string."
                },
                "name": {
                    "type": "string",
                    "description": "Human-readable job name (add/schedule)."
                },
                "payload": {
                    "type": "object",
                    "description": "Job payload for add/schedule: {kind:'agentTurn',message:'task prompt'} runs an isolated LLM turn; {kind:'systemEvent',text:'message text'} injects text into the main session.",
                    "properties": {
                        "kind": { "type": "string", "enum": ["agentTurn", "systemEvent"] },
                        "message": { "type": "string", "description": "Task for agentTurn" },
                        "text": { "type": "string", "description": "Text for systemEvent" }
                    }
                },
                "job_type": {
                    "type": "string",
                    "enum": ["shell", "agent"],
                    "description": "Legacy alternative to 'payload' for add/schedule. 'agent' runs an LLM turn."
                },
                "prompt": {
                    "type": "string",
                    "description": "LLM prompt for agent jobs (add/schedule). Overridden by payload.message/payload.text."
                },
                "session_target": {
                    "type": "string",
                    "enum": ["isolated", "main"],
                    "description": "Agent-job target session: isolated=new context (default), main=inject into main session."
                },
                "model": {
                    "type": "string",
                    "description": "Override model for agent jobs (add/schedule)."
                },
                "delivery": {
                    "type": "object",
                    "description": "Result delivery for agent jobs: {mode:'announce',channel:'signal',to:'<phone|uuid|group:ID>'} or {mode:'none'}.",
                    "properties": {
                        "mode": { "type": "string", "enum": ["none", "announce"] },
                        "channel": { "type": "string", "enum": ["signal", "telegram", "discord", "slack", "mattermost"] },
                        "to": { "type": "string", "description": "Recipient: E.164 phone, UUID, or group:<groupId>" },
                        "best_effort": { "type": "boolean", "default": true }
                    }
                },
                "delete_after_run": {
                    "type": "boolean",
                    "description": "Auto-delete one-shot jobs after success (default true for 'at' schedule)."
                },
                "patch": {
                    "type": "object",
                    "description": "Fields to update for the 'update/patch' action."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max entries for 'runs' action (default 10)."
                },
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
            // ── Read-only ──────────────────────────────────────────────────────────
            "list" => {
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                match cron::list_jobs(&cfg) {
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
                            let last_run = job.last_run.map_or_else(|| "never".to_string(), |v| v.to_rfc3339());
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
                            output: format!("Scheduled jobs ({}):\n{}", jobs.len(), lines.join("\n")),
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
                match cron::get_job(&cfg, job_id) {
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
                if let Some(r) = self.check_enabled(&cfg) {
                    return Ok(r);
                }
                let jobs = cron::list_jobs(&cfg).unwrap_or_default();
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
                if let Some(r) = self.check_enabled(&cfg) {
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

                match cron::list_runs(&cfg, job_id, limit) {
                    Ok(runs) => {
                        let views: Vec<RunView> = runs
                            .into_iter()
                            .map(|run| RunView {
                                id: run.id,
                                job_id: run.job_id,
                                started_at: run.started_at,
                                finished_at: run.finished_at,
                                status: run.status,
                                output: run.output.map(|out| truncate_str(&out, MAX_RUN_OUTPUT_CHARS)),
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

            "events" => {
                if let Some(r) = self.check_enabled(&cfg) {
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
                match cron::list_job_events(&cfg, job_id) {
                    Ok(events) => Ok(ToolResult {
                        success: true,
                        output: serde_json::to_string_pretty(&events)?,
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    }),
                }
            }

            // ── Mutating ───────────────────────────────────────────────────────────
            "add" | "schedule" => {
                // Budget is NOT charged here: handle_add validates all params and
                // runs the shell approval gate first, and only consumes the action
                // budget immediately before the DB write (legacy cron_add parity).
                if let Some(r) = self.enforce_mutation_no_budget(action, &cfg) {
                    return Ok(r);
                }
                self.handle_add(&cfg, &args)
            }

            "once" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
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
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                if let Err(reason) = SideEffectGate::new(self.security.as_ref()).authorize_command_execution(
                    self.name(),
                    command,
                    approval_grant.as_ref(),
                ) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(reason),
                    });
                }
                let persisted_grant = ApprovalGrant::persisted_runner_grant(
                    "cron_scheduler",
                    command,
                    approval_grant.as_ref(),
                    PERSISTED_APPROVAL_GRANT_TTL_SECS,
                )
                .map(|grant| serde_json::to_string(&grant))
                .transpose()?;

                let delay = args.get("delay").and_then(|v| v.as_str());
                let run_at = args.get("run_at").and_then(|v| v.as_str());
                let lineage = cron::lineage_from_trusted_scope(&cfg, &args);

                match (delay, run_at) {
                    (Some(d), None) => {
                        match cron::add_once_with_lineage_and_approval_grant(&cfg, d, command, persisted_grant, lineage)
                        {
                            Ok(job) => Ok(ToolResult {
                                success: true,
                                output: format!(
                                    "Created one-shot job {} (runs at: {}, cmd: {}){}",
                                    job.id,
                                    job.next_run.to_rfc3339(),
                                    job.command,
                                    Self::scheduler_inactive_notice(&cfg)
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
                    (None, Some(at)) => {
                        let run_at_parsed: DateTime<Utc> = match DateTime::parse_from_rfc3339(at) {
                            Ok(v) => v.with_timezone(&Utc),
                            Err(e) => {
                                return Ok(ToolResult {
                                    success: false,
                                    output: String::new(),
                                    error: Some(format!("Invalid run_at timestamp: {e}")),
                                });
                            }
                        };
                        match cron::add_once_at_with_lineage_and_approval_grant(
                            &cfg,
                            run_at_parsed,
                            command,
                            persisted_grant,
                            lineage,
                        ) {
                            Ok(job) => Ok(ToolResult {
                                success: true,
                                output: format!(
                                    "Created one-shot job {} (runs at: {}, cmd: {}){}",
                                    job.id,
                                    job.next_run.to_rfc3339(),
                                    job.command,
                                    Self::scheduler_inactive_notice(&cfg)
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
                        error: Some("'once' requires exactly one of 'delay' or 'run_at'".to_string()),
                    }),
                }
            }

            "remove" | "cancel" => {
                if let Some(r) = self.enforce_mutation(action, &cfg) {
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
                match cron::remove_job(&cfg, job_id) {
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
                if let Some(r) = self.enforce_mutation(action, &cfg) {
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
                let mut patch = match serde_json::from_value::<CronJobPatch>(patch_val) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Invalid patch payload: {e}")),
                        });
                    }
                };
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                if let Some(command) = &patch.command {
                    if let Err(reason) = SideEffectGate::new(self.security.as_ref()).authorize_command_execution(
                        self.name(),
                        command,
                        approval_grant.as_ref(),
                    ) {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(reason),
                        });
                    }
                    patch.approval_grant_json = ApprovalGrant::persisted_runner_grant(
                        "cron_scheduler",
                        command,
                        approval_grant.as_ref(),
                        PERSISTED_APPROVAL_GRANT_TTL_SECS,
                    )
                    .map(|grant| serde_json::to_string(&grant))
                    .transpose()?;
                }
                match cron::update_job(&cfg, job_id, patch) {
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
                if let Some(r) = self.check_enabled(&cfg) {
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
                let mut approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                if !self.security.can_act() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Security policy: read-only mode, cannot perform 'cron run'".into()),
                    });
                }
                if self.security.is_rate_limited() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Rate limit exceeded: too many actions in the last hour".into()),
                    });
                }
                let job = match cron::get_job(&cfg, job_id) {
                    Ok(j) => j,
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(e.to_string()),
                        });
                    }
                };
                if approval_grant.is_none()
                    && args
                        .get(crate::security::policy::RUNTIME_APPROVAL_GRANTED_ARG)
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                {
                    approval_grant = Some(ApprovalGrant::for_command(self.name(), &job.command, "runtime", None));
                }
                if matches!(job.job_type, JobType::Shell) {
                    if let Err(reason) = SideEffectGate::new(self.security.as_ref()).authorize_command_execution(
                        self.name(),
                        &job.command,
                        approval_grant.as_ref(),
                    ) {
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
                let (success, output) = cron::scheduler::execute_job_now_with_runtime_approval_for_tool(
                    &cfg,
                    &job,
                    self.name(),
                    approval_grant,
                )
                .await;
                let finished_at = Utc::now();
                let duration_ms = (finished_at - started_at).num_milliseconds();
                let status = if success { "ok" } else { "error" };
                let _ = cron::record_run(
                    &cfg,
                    &job.id,
                    started_at,
                    finished_at,
                    status,
                    Some(&output),
                    duration_ms,
                );
                let _ = cron::record_last_run(&cfg, &job.id, finished_at, success, &output);
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
                if let Some(r) = self.enforce_mutation(action, &cfg) {
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
                match cron::pause_job(&cfg, job_id) {
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
                if let Some(r) = self.enforce_mutation(action, &cfg) {
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
                match cron::resume_job(&cfg, job_id) {
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
                    "Unknown action '{other}'. Use: add, schedule, once, list, get, remove, cancel, update, patch, run, runs, history, events, pause, resume, status."
                )),
            }),
        }
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Scheduling]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, SharedConfig, new_shared};
    use crate::security::AutonomyLevel;
    use tempfile::TempDir;

    async fn test_config(tmp: &TempDir) -> SharedConfig {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        tokio::fs::create_dir_all(&config.workspace_dir).await.unwrap();
        new_shared(config)
    }

    fn test_security(cfg: &Config) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir))
    }

    #[tokio::test]
    async fn list_empty() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
        let result = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No scheduled cron jobs"));
    }

    #[tokio::test]
    async fn add_and_list_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

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
    async fn add_with_schedule_object_creates_shell_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let add = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "echo sched-obj"
            }))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let list = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(list.output.contains("echo sched-obj"));
    }

    #[tokio::test]
    async fn add_agent_job_via_payload() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let add = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                "payload": { "kind": "agentTurn", "message": "summarize my day" }
            }))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let jobs = cron::list_jobs(&cfg_snap).unwrap();
        assert_eq!(jobs.len(), 1);
        let job = jobs.first().expect("test: one job present");
        assert!(matches!(job.job_type, JobType::Agent));
    }

    #[tokio::test]
    async fn add_agent_job_requires_prompt() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "agent"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Missing prompt"));
    }

    #[tokio::test]
    async fn status_reports_job_counts() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        cron::add_job(&cfg_snap, "*/5 * * * *", "echo status-test").unwrap();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
        let result = tool.execute(json!({"action": "status"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 total"));
    }

    #[tokio::test]
    async fn get_job_details() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo get-test").unwrap();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
        let result = tool.execute(json!({"action": "get", "job_id": job.id})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("echo get-test"));
    }

    #[tokio::test]
    async fn events_lists_job_lifecycle_events() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo events-test").unwrap();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool
            .execute(json!({"action": "events", "job_id": job.id}))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("cron.job.created"));
    }

    #[tokio::test]
    async fn remove_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo remove-test").unwrap();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
        let result = tool
            .execute(json!({"action": "remove", "job_id": job.id}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(cron::list_jobs(&cfg_snap).unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_job_enabled() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo upd-test").unwrap();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
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
        let cfg_snap = Arc::new(config.clone());
        let cfg = new_shared(config);
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
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
        let cfg_snap = Arc::new(config.clone());
        let cfg = new_shared(config);
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
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
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
        let result = tool.execute(json!({"action": "explode"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Unknown action"));
    }

    // ── Focused agent-add coverage (fields easily lost in the consolidation) ──

    /// `payload.kind=systemEvent` must route to the MAIN session (vs agentTurn's
    /// isolated default), and `payload.text` must be accepted as the prompt.
    #[tokio::test]
    async fn add_agent_system_event_targets_main_and_uses_text() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let add = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                "payload": { "kind": "systemEvent", "text": "daily standup ping" }
            }))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let job = cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .next()
            .expect("test: one job");
        assert!(matches!(job.job_type, JobType::Agent));
        assert_eq!(job.session_target, SessionTarget::Main);
        assert_eq!(job.prompt.as_deref(), Some("daily standup ping"));
    }

    /// agentTurn defaults to the isolated session and accepts `payload.message`.
    #[tokio::test]
    async fn add_agent_turn_defaults_isolated() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let add = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                "payload": { "kind": "agentTurn", "message": "summarize inbox" }
            }))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let job = cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .next()
            .expect("test: one job");
        assert_eq!(job.session_target, SessionTarget::Isolated);
        assert_eq!(job.prompt.as_deref(), Some("summarize inbox"));
    }

    /// `payload.message` takes precedence over the top-level `prompt` field.
    #[tokio::test]
    async fn add_agent_payload_message_overrides_prompt_field() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let add = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                "payload": { "kind": "agentTurn", "message": "from-payload" },
                "prompt": "from-prompt-field"
            }))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let job = cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .next()
            .expect("test: one job");
        assert_eq!(job.prompt.as_deref(), Some("from-payload"));
    }

    /// `prompt` is used when no `payload` is given (job_type=agent), and `name`,
    /// `model` and `delivery` are all carried through onto the persisted job.
    #[tokio::test]
    async fn add_agent_named_with_model_and_delivery() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let add = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                "job_type": "agent",
                "name": "morning-brief",
                "prompt": "brief me",
                "model": "claude-test-model",
                "delivery": { "mode": "announce", "channel": "signal", "to": "+15551234567" }
            }))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let job = cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .next()
            .expect("test: one job");
        assert_eq!(job.name.as_deref(), Some("morning-brief"));
        assert_eq!(job.prompt.as_deref(), Some("brief me"));
        assert_eq!(job.model.as_deref(), Some("claude-test-model"));
        assert_eq!(job.delivery.mode, "announce");
        assert_eq!(job.delivery.channel.as_deref(), Some("signal"));
        assert_eq!(job.delivery.to.as_deref(), Some("+15551234567"));
    }

    /// A malformed `delivery` object must surface a parse error and create no job.
    #[tokio::test]
    async fn add_agent_invalid_delivery_errors() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                "job_type": "agent",
                "prompt": "x",
                // `mode` must be a string; an integer fails DeliveryConfig parsing.
                "delivery": { "mode": 123 }
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Invalid delivery config"));
        assert!(cron::list_jobs(&cfg_snap).unwrap().is_empty());
    }

    /// `delete_after_run` defaults to false for recurring (cron) agent jobs.
    #[tokio::test]
    async fn add_agent_delete_after_run_defaults_false_for_cron() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        tool.execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "recurring"
        }))
        .await
        .unwrap();

        let job = cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .next()
            .expect("test: one job");
        assert!(!job.delete_after_run);
    }

    /// An explicit `delete_after_run=true` overrides the default.
    #[tokio::test]
    async fn add_agent_delete_after_run_explicit_true() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        tool.execute(json!({
            "action": "add",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "job_type": "agent",
            "prompt": "recurring",
            "delete_after_run": true
        }))
        .await
        .unwrap();

        let job = cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .next()
            .expect("test: one job");
        assert!(job.delete_after_run);
    }

    /// `Schedule::At` agent jobs persist the `At` schedule and default
    /// `delete_after_run` to true (one-shot semantics).
    #[tokio::test]
    async fn add_agent_schedule_at_one_shot_defaults() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let at = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let add = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "at", "at": at },
                "job_type": "agent",
                "prompt": "once-at"
            }))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let job = cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .next()
            .expect("test: one job");
        assert!(matches!(job.schedule, Schedule::At { .. }));
        assert!(job.delete_after_run, "At-scheduled agent jobs are one-shot by default");
    }

    /// `Schedule::Every` agent jobs persist the `Every` schedule (every_ms).
    #[tokio::test]
    async fn add_agent_schedule_every_persists() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let add = tool
            .execute(json!({
                "action": "add",
                "schedule": { "kind": "every", "every_ms": 30000 },
                "job_type": "agent",
                "prompt": "every-30s"
            }))
            .await
            .unwrap();
        assert!(add.success, "{:?}", add.error);

        let job = cron::list_jobs(&cfg_snap)
            .unwrap()
            .into_iter()
            .next()
            .expect("test: one job");
        assert!(matches!(job.schedule, Schedule::Every { every_ms: 30000 }));
    }
}
