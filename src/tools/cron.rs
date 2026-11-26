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

use super::execution::scope_is_local_process_operator;
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::{Config, SharedConfig};
use crate::cron::{self, CronJobPatch, DeliveryConfig, JobType, Schedule, SessionTarget};
use crate::security::SecurityPolicy;
use crate::security::policy::{ApprovalGrant, PERSISTED_APPROVAL_GRANT_TTL_SECS};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
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

    const fn scheduler_inactive_notice(_cfg: &Config) -> &'static str {
        ""
    }

    const fn check_enabled(&self, _cfg: &Config) -> Option<ToolResult> {
        None
    }

    /// Mutation pre-checks: module enabled and not read-only. Nothing rations
    /// how many mutations may run, so this is the whole gate.
    fn enforce_mutation(&self, action: &str, cfg: &Config) -> Option<ToolResult> {
        if let Some(r) = self.check_enabled(cfg) {
            return Some(r);
        }
        if !crate::tools::effective_autonomy::call_policy(&self.security).can_act() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Security policy: read-only mode, cannot perform '{action}'")),
            });
        }
        None
    }

    /// A job's model set by the model (`add`, or `update`/`patch` with
    /// `patch.model`) is a model-made durable selection (D-034/D-035): it
    /// must be verified on the default provider the job runs on.
    fn refuse_unverified_model(cfg: &Config, model: &str) -> Option<ToolResult> {
        crate::providers::selection::check(
            cfg,
            crate::providers::selection::default_provider(cfg),
            model,
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::DurableSelection,
        )
        .err()
        .map(|refusal| ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("The job's model is refused: {refusal}")),
        })
    }

    /// Refuse `job_id` to a caller that may not change or run it (CRON-ORIGIN).
    ///
    /// A job's origin decides whose authority its runs have, so the job must
    /// not be rewritable by anyone else: a webhook caller that could `update`
    /// the prompt of an operator-created agent job would be running its own
    /// text with the operator's authority. The local-process operator (the
    /// terminal UI and the local-operator CLI tuple, [`scope_is_local_process_operator`]) may
    /// manage every job and never adopts one by doing so — the origin is not
    /// changed by any update. Any other trusted caller may manage only jobs
    /// whose stored owner is its own owner (the same owner
    /// [`cron::lineage_from_trusted_scope`] records at creation); a job with no
    /// stored owner belongs to no such caller, except that the authenticated
    /// Console Main also manages the ownerless jobs the local operator created
    /// through the CLI. A call without a
    /// runtime-authored scope bypassed the execution service entirely (an
    /// in-process call) and keeps the unrestricted behavior it always had,
    /// as `RunOwnerScope::from_args` does.
    ///
    /// A job that does not exist is left to the action itself to report.
    fn refuse_unless_caller_may_manage(cfg: &Config, args: &serde_json::Value, job_id: &str) -> Option<ToolResult> {
        let caller = CallerScope::from_args(cfg, args);
        let job = cron::get_job(cfg, job_id).ok()?;
        if caller.admits(cfg, &job) {
            return None;
        }
        Some(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!(
                "cron job '{job_id}' was created by another principal; only its creator or the local operator can change or run it"
            )),
        })
    }

    /// Hide `job_id` from a caller that may not read it (R-CRON F-3): the
    /// answer is the one a missing job gets, so a read cannot confirm that
    /// another principal's job exists. (The manage actions answer differently:
    /// [`Self::refuse_unless_caller_may_manage`] says the job belongs to
    /// another principal.) Same admission rule as that method.
    ///
    /// A job that cannot be read is hidden from an owner-scoped caller too
    /// (R-CRONf N-1). `runs`/`history`/`events` would otherwise go on to the
    /// run and event tables, which answer `[]` for an id that never existed
    /// and keep a removed job's events, so the two answers would tell a
    /// missing id apart from another principal's job. The local operator (and
    /// an in-process call) still reads a removed job's events.
    fn hide_unless_caller_may_read(cfg: &Config, args: &serde_json::Value, job_id: &str) -> Option<ToolResult> {
        let caller = CallerScope::from_args(cfg, args);
        let admitted = match cron::get_job(cfg, job_id) {
            Ok(job) => caller.admits(cfg, &job),
            Err(error) => {
                if matches!(caller, CallerScope::Unrestricted) {
                    return None;
                }
                // The caller sees only "not found"; keep the cause for the
                // operator. A job that does not exist is the ordinary answer;
                // anything else (a row that cannot be decoded, a store
                // failure) is worth a warning.
                if error.to_string() == format!("Cron job '{job_id}' not found") {
                    tracing::debug!(job_id, "cron job not found for an owner-scoped read");
                } else {
                    tracing::warn!(job_id, %error, "cron job unreadable for an owner-scoped read");
                }
                false
            }
        };
        (!admitted).then(|| ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("Job '{job_id}' not found")),
        })
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
                let persisted_grant = ApprovalGrant::persisted_runner_grant(
                    "cron_scheduler",
                    command,
                    approval_grant.as_ref(),
                    PERSISTED_APPROVAL_GRANT_TTL_SECS,
                )
                .map(|grant| serde_json::to_string(&grant))
                .transpose()?;
                cron::add_shell_job_with_lineage_approval_and_delete(
                    cfg,
                    name,
                    schedule,
                    command,
                    persisted_grant,
                    delete_after_run,
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
                // Stored as the check resolves it (R-PMMCh C-2).
                let model = args
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(|model| model.trim().to_string());
                // A job the model creates is a model-made durable selection
                // (D-034/D-035): its model must be verified, not merely known
                // by id, or no job is written.
                if let Some(refused) = model
                    .as_deref()
                    .and_then(|model| Self::refuse_unverified_model(cfg, model))
                {
                    return Ok(refused);
                }
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
    attempt_id: Option<String>,
    worker_id: Option<String>,
}

/// Which jobs a caller may see, change or run (CRON-ORIGIN, R-CRON F-3).
///
/// The local-process operator (the terminal UI and the local-operator CLI tuple,
/// [`scope_is_local_process_operator`]) reaches every job. Any other trusted
/// caller reaches only jobs whose stored owner is its own owner (the owner
/// [`cron::lineage_from_trusted_scope`] records at creation); a caller with no
/// owner, or a job with none, matches nothing. A call without a
/// runtime-authored scope bypassed the execution service (an in-process call)
/// and stays unrestricted, as `RunOwnerScope::from_args` does.
enum CallerScope {
    Unrestricted,
    Owner(Option<String>),
    /// The authenticated, non-isolated Console Main: its owner's jobs, and
    /// the jobs the local operator created through the CLI, which carry no
    /// owner (the Console holder is the operator of this instance).
    Console(Option<String>),
    Named { owner: Option<String>, creator: String },
}

impl CallerScope {
    fn from_args(cfg: &Config, args: &serde_json::Value) -> Self {
        let trusted = args
            .get("_zc_scope_trusted")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !trusted || scope_is_local_process_operator(args) {
            return Self::Unrestricted;
        }
        let owner = cron::lineage_from_trusted_scope(cfg, args).owner_id;
        match crate::tools::execution::trusted_scope_read_partition(args) {
            Ok(Some(crate::memory::AgentPartition {
                key: crate::memory::PartitionKey::Named { name, incarnation },
                ..
            })) => Self::Named {
                owner,
                creator: crate::security::agent_level::creator_agent_named(name.as_str(), &incarnation),
            },
            Ok(_) => {
                let console = args
                    .get("_zc_scope")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|scope| scope.get("runtime_source"))
                    .and_then(serde_json::Value::as_str)
                    == Some(crate::runtime::envelope::RuntimeSource::Console.as_str());
                if console && owner.is_some() && !crate::tools::execution::scope_requires_owner_match(args) {
                    Self::Console(owner)
                } else {
                    Self::Owner(owner)
                }
            }
            Err(_) => Self::Owner(None),
        }
    }

    fn admits(&self, cfg: &Config, job: &cron::CronJob) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Owner(Some(owner)) => job.owner_id.as_deref() == Some(owner.as_str()),
            Self::Owner(None) | Self::Console(None) => false,
            Self::Console(Some(owner)) => {
                job.owner_id.as_deref() == Some(owner.as_str())
                    || (job.owner_id.is_none() && job.origin.is_local_operator())
            }
            Self::Named { owner, creator } => {
                owner
                    .as_deref()
                    .is_some_and(|owner| job.owner_id.as_deref() == Some(owner))
                    && cron::job_creator(cfg, &job.id).is_ok_and(|(_, stamp)| stamp.agent.as_ref() == Some(creator))
            }
        }
    }
}

fn truncate_str(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out: String = input.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

impl CronTool {
    fn execution_schema(&self) -> &'static std::sync::Arc<serde_json::Value> {
        static SCHEMA: std::sync::LazyLock<std::sync::Arc<serde_json::Value>> = std::sync::LazyLock::new(|| {
            std::sync::Arc::new({
                crate::tools::schema::with_action_exclusive_alternatives(
                    crate::tools::schema::with_action_alternatives(
                        crate::tools::schema::with_action_alternatives(
                            crate::tools::schema::with_action_requirements(
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
                                        "description": "Create a job with 'add'/'schedule' (recurring) or 'once' \
                                                        (one-shot); inspect with 'list'/'get'/'runs'/'history'/'events'/'status'; \
                                                        change one with 'update'/'patch'; trigger it now with 'run'; \
                                                        halt or resume it with 'pause'/'resume'; delete it with 'remove'/'cancel'."
                                    },
                                    "job_id": {
                                        "type": "string",
                                        "description": "Job ID for 'get'/'runs'/'history'/'events'/'update'/'patch'/'run'/'pause'/'resume'/'remove'/'cancel'."
                                    },
                                    "expression": {
                                        "type": "string",
                                        "description": "Cron expression (e.g. `*/5 * * * *`) for 'add'/'schedule'; alternative to 'schedule'."
                                    },
                                    "command": {
                                        "type": "string",
                                        "description": "Shell command to run; required by 'once' and one way to give 'add'/'schedule' its work."
                                    },
                                    "delay": {
                                        "type": "string",
                                        "description": "One-shot delay for 'once' (e.g. `30m`, `2h`); alternative to 'run_at'."
                                    },
                                    "run_at": {
                                        "type": "string",
                                        "description": "One-shot absolute RFC3339 time for 'once'; alternative to 'delay'."
                                    },
                                    "schedule": {
                                        "type": "object",
                                        "description": "'add'/'schedule': {kind:cron,expr,tz?} | {kind:at,at} | {kind:every,every_ms}. Alternative to 'expression'.",
                                        "properties": {
                                            "kind": { "type": "string", "enum": ["cron", "at", "every"] },
                                            "expr": { "type": "string", "minLength": 1 },
                                            "tz": { "type": "string", "minLength": 1 },
                                            "at": { "type": "string", "minLength": 1 },
                                            "every_ms": { "type": "integer", "minimum": 1 }
                                        },
                                        "required": ["kind"],
                                        "allOf": [
                                            {
                                                "if": { "properties": { "kind": { "const": "cron" } }, "required": ["kind"] },
                                                "then": { "required": ["expr"] }
                                            },
                                            {
                                                "if": { "properties": { "kind": { "const": "at" } }, "required": ["kind"] },
                                                "then": { "required": ["at"] }
                                            },
                                            {
                                                "if": { "properties": { "kind": { "const": "every" } }, "required": ["kind"] },
                                                "then": { "required": ["every_ms"] }
                                            }
                                        ]
                                    },
                                    "name": {
                                        "type": "string",
                                        "description": "Human-readable job name."
                                    },
                                    "payload": {
                                        "type": "object",
                                        "description": "'add'/'schedule' payload: {kind:agentTurn,message} runs an isolated LLM turn; {kind:systemEvent,text} injects text into the main session.",
                                        "properties": {
                                            "kind": { "type": "string", "enum": ["agentTurn", "systemEvent"] },
                                            "message": { "type": "string", "minLength": 1, "description": "agentTurn prompt." },
                                            "text": { "type": "string", "minLength": 1, "description": "systemEvent text." }
                                        },
                                        "required": ["kind"],
                                        "anyOf": [
                                            { "required": ["message"] },
                                            { "required": ["text"] }
                                        ]
                                    },
                                    "job_type": {
                                        "type": "string",
                                        "enum": ["shell", "agent"],
                                        "description": "Legacy alternative to 'payload'. 'agent' runs an LLM turn."
                                    },
                                    "prompt": {
                                        "type": "string",
                                        "description": "LLM prompt for agent jobs on 'add'/'schedule'; overridden by payload."
                                    },
                                    "session_target": {
                                        "type": "string",
                                        "enum": ["isolated", "main"],
                                        "description": "Agent-job target: isolated=new context (default), main=inject into main session."
                                    },
                                    "model": {
                                        "type": "string",
                                        "description": "Model override for agent jobs."
                                    },
                                    "delivery": {
                                        "type": "object",
                                        "description": "Agent-job result delivery: {mode:'announce',channel,to} or {mode:'none'}.",
                                        "properties": {
                                            "mode": { "type": "string", "enum": ["none", "announce"] },
                                            "channel": { "type": "string", "enum": ["signal", "telegram", "discord", "slack", "mattermost"] },
                                            "to": { "type": "string", "description": "E.164 phone, UUID, or group:<groupId>" },
                                            "best_effort": { "type": "boolean", "default": true }
                                        }
                                    },
                                    "delete_after_run": {
                                        "type": "boolean",
                                        "description": "Auto-delete one-shot jobs after success (default true for 'at')."
                                    },
                                    "patch": {
                                        "type": "object",
                                        "description": "Fields to update for 'update'/'patch'."
                                    },
                                    "limit": {
                                        "type": "integer",
                                        "description": "Max entries for 'runs' (default 10)."
                                    },
                                },
                                        "required": ["action"]
                                    }),
                                "action",
                                &[
                                    crate::tools::schema::ActionRequirement {
                                        action: "get",
                                        required: &["job_id"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "runs",
                                        required: &["job_id"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "history",
                                        required: &["job_id"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "events",
                                        required: &["job_id"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "once",
                                        required: &["command"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "remove",
                                        required: &["job_id"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "cancel",
                                        required: &["job_id"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "update",
                                        required: &["job_id", "patch"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "patch",
                                        required: &["job_id", "patch"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "run",
                                        required: &["job_id"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "pause",
                                        required: &["job_id"],
                                    },
                                    crate::tools::schema::ActionRequirement {
                                        action: "resume",
                                        required: &["job_id"],
                                    },
                                ],
                            ),
                            "action",
                            "add",
                            &[
                                &["expression", "command"],
                                &["schedule", "command"],
                                &["expression", "prompt"],
                                &["schedule", "prompt"],
                                &["expression", "payload"],
                                &["schedule", "payload"],
                            ],
                        ),
                        "action",
                        "schedule",
                        &[
                            &["expression", "command"],
                            &["schedule", "command"],
                            &["expression", "prompt"],
                            &["schedule", "prompt"],
                            &["expression", "payload"],
                            &["schedule", "payload"],
                        ],
                    ),
                    "action",
                    "once",
                    &[&["delay"], &["run_at"]],
                )
            })
        });
        &SCHEMA
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Unified cron/scheduler management — the single entry point for ALL scheduled-task \
         operations. Actions: 'add'/'schedule' (recurring job: `expression`+`command`, or a \
         `schedule` object with `payload`/`job_type`); 'once' (one-shot shell job via `delay` \
         or `run_at`); 'list'; 'get'; 'remove'/'cancel'; 'update'/'patch'; 'run'; \
         'runs'/'history'; 'events'; 'pause'; 'resume'; 'status'."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::publish_flat_schema(self.execution_schema())
    }

    fn execution_parameters_schema(&self) -> Option<std::sync::Arc<serde_json::Value>> {
        Some(std::sync::Arc::clone(self.execution_schema()))
    }

    async fn execute(&self, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        crate::tools::schema::validate_execution_contract(self.name(), self.execution_schema(), &mut args)?;
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
                let caller = CallerScope::from_args(&cfg, &args);
                match cron::list_jobs(&cfg).map(|jobs| {
                    jobs.into_iter()
                        .filter(|job| caller.admits(&cfg, job))
                        .collect::<Vec<_>>()
                }) {
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
                            let mut flag_values = Vec::new();
                            if paused {
                                flag_values.push("disabled".to_string());
                            }
                            if one_shot {
                                flag_values.push("one-shot".to_string());
                            }
                            if let Some(terminal) = job.terminal_state {
                                flag_values.push(format!("terminal={}", terminal.as_str()));
                            }
                            let flags = if flag_values.is_empty() {
                                String::new()
                            } else {
                                format!(" [{}]", flag_values.join(", "))
                            };
                            let last_run = job.last_run.map_or_else(|| "never".to_string(), |v| v.to_rfc3339());
                            let last_status = job.last_status.as_deref().unwrap_or("n/a");
                            let claim = job.claim.as_ref().map_or_else(
                                || "unclaimed".to_string(),
                                |claim| {
                                    format!(
                                        "claimed_by={} attempt={} expires={}",
                                        claim.worker_id,
                                        claim.attempt_id,
                                        claim.expires_at.to_rfc3339()
                                    )
                                },
                            );
                            lines.push(format!(
                                "- {} | {} | next={} | last={} ({}){} | {} | cmd: {}",
                                job.id,
                                job.expression,
                                job.next_run.to_rfc3339(),
                                last_run,
                                last_status,
                                flags,
                                claim,
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
                if let Some(hidden) = Self::hide_unless_caller_may_read(&cfg, &args, job_id) {
                    return Ok(hidden);
                }
                match cron::get_job(&cfg, job_id) {
                    Ok(job) => {
                        let detail = json!({
                            "id": job.id,
                            "expression": job.expression,
                            "command": job.command,
                            "next_run": job.next_run.to_rfc3339(),
                            "last_run": job.last_run.map(|v| v.to_rfc3339()),
                            "last_status": job.last_status,
                            "terminal_state": job.terminal_state,
                            "claim": job.claim,
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
                let caller = CallerScope::from_args(&cfg, &args);
                let jobs = cron::list_jobs(&cfg)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|job| caller.admits(&cfg, job))
                    .collect::<Vec<_>>();
                let terminal_count = jobs.iter().filter(|job| job.terminal_state.is_some()).count();
                let enabled_count = jobs
                    .iter()
                    .filter(|job| job.enabled && job.terminal_state.is_none())
                    .count();
                let disabled_count = jobs.len() - enabled_count - terminal_count;
                let claimed_count = jobs.iter().filter(|job| job.claim.is_some()).count();
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "⏰ Cron Status\n\
                         ─────────────\n\
                         Enabled:  true\n\
                         Jobs:     {} total ({} active, {} paused, {} terminal, {} claimed)",
                        jobs.len(),
                        enabled_count,
                        disabled_count,
                        terminal_count,
                        claimed_count,
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
                if let Some(hidden) = Self::hide_unless_caller_may_read(&cfg, &args, job_id) {
                    return Ok(hidden);
                }
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
                                attempt_id: run.attempt_id,
                                worker_id: run.worker_id,
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
                if let Some(hidden) = Self::hide_unless_caller_may_read(&cfg, &args, job_id) {
                    return Ok(hidden);
                }
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
                if let Some(r) = self.enforce_mutation(action, &cfg) {
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
                if let Some(refused) = Self::refuse_unless_caller_may_manage(&cfg, &args, job_id) {
                    return Ok(refused);
                }
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
                if let Some(refused) = Self::refuse_unless_caller_may_manage(&cfg, &args, job_id) {
                    return Ok(refused);
                }
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
                // Stored as the check resolves it (R-PMMCh C-2).
                if let Some(model) = patch.model.as_mut() {
                    *model = model.trim().to_string();
                }
                // A model the patch sets gets the same check as on `add`, so a
                // job cannot be switched to an unverified model after creation.
                if let Some(refused) = patch
                    .model
                    .as_deref()
                    .and_then(|model| Self::refuse_unverified_model(&cfg, model))
                {
                    return Ok(refused);
                }
                // Re-anchor the delivery principal whenever the patch rewrites
                // `delivery`. The stored identity is what the scheduler
                // authorizes the announcement as, so leaving the original
                // creator's identity on a destination somebody else chose would
                // let one caller aim another caller's outbound reach. Taken
                // from the runtime-injected trusted scope only; a caller
                // without one clears the identity rather than inheriting it,
                // which can only narrow what the job may reach.
                if patch.delivery.is_some() {
                    patch.delivery_principal = Some(cron::delivery_principal_from_trusted_scope(&args));
                }
                // A command or prompt rewrite re-records this call's level and agent.
                patch.creator = Some(crate::tools::execution::creator_stamp_from_trusted_args(&args));
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                if let Some(command) = &patch.command {
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
                if let Some(refused) = Self::refuse_unless_caller_may_manage(&cfg, &args, job_id) {
                    return Ok(refused);
                }
                if !crate::tools::effective_autonomy::call_policy(&self.security).can_act() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Security policy: read-only mode, cannot perform 'cron run'".into()),
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
                let terminal_rerun = job.terminal_state.is_some();
                let worker_id = format!("cron-manual-{}", uuid::Uuid::new_v4());
                let manual_claim = if terminal_rerun {
                    cron::claim_terminal_job_for_manual_rerun(
                        &cfg,
                        &job,
                        &worker_id,
                        Utc::now(),
                        ChronoDuration::seconds(cfg.scheduler.claim_lease_secs as i64),
                    )?
                } else {
                    cron::claim_job_if_current_for_manual_run(
                        &cfg,
                        &job,
                        &worker_id,
                        Utc::now(),
                        ChronoDuration::seconds(cfg.scheduler.claim_lease_secs as i64),
                    )?
                };
                let Some(manual_claim) = manual_claim else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("cron job '{}' changed or is already running", job.id)),
                    });
                };
                let started_at = Utc::now();
                let (success, output) =
                    cron::scheduler::execute_claimed_job_preauthorized_for_tool(&cfg, &job, manual_claim, self.name())
                        .await;
                let finished_at = Utc::now();
                let duration_ms = (finished_at - started_at).num_milliseconds();
                let status = if success { "ok" } else { "error" };
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
                if let Some(refused) = Self::refuse_unless_caller_may_manage(&cfg, &args, job_id) {
                    return Ok(refused);
                }
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
                if let Some(refused) = Self::refuse_unless_caller_may_manage(&cfg, &args, job_id) {
                    return Ok(refused);
                }
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
mod tests;
