use super::traits::{Tool, ToolResult};
use crate::config::SharedConfig;
use crate::cron::{self, DeliveryConfig, JobType, Schedule, SessionTarget};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct CronAddTool {
    config: SharedConfig,
    security: Arc<SecurityPolicy>,
}

impl CronAddTool {
    pub fn new(config: SharedConfig, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn enforce_mutation_allowed(&self, action: &str) -> Option<ToolResult> {
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

#[async_trait]
impl Tool for CronAddTool {
    fn name(&self) -> &str {
        "cron_add"
    }

    fn description(&self) -> &str {
        "Create a scheduled cron job (shell or agent) with cron/at/every schedules. \
         Supports payload.kind='agentTurn' (isolated LLM run with tools) or 'systemEvent' \
         (text injection to main session). Use delivery.mode='announce' with delivery.channel='signal' \
         and delivery.to=<recipient> to announce results to a channel."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Human-readable job name"
                },
                "schedule": {
                    "type": "object",
                    "description": "Schedule: {kind:'at',at:'ISO-8601'} | {kind:'every',every_ms:30000} | {kind:'cron',expr:'0 9 * * *',tz?:'America/New_York'}"
                },
                "payload": {
                    "type": "object",
                    "description": "Payload config: {kind:'agentTurn',message:'task prompt'} runs isolated LLM; {kind:'systemEvent',text:'message text'} sends a plain message. Shorthand for job_type+prompt.",
                    "properties": {
                        "kind": { "type": "string", "enum": ["agentTurn", "systemEvent"] },
                        "message": { "type": "string", "description": "Task for agentTurn" },
                        "text": { "type": "string", "description": "Text for systemEvent" }
                    }
                },
                "job_type": {
                    "type": "string",
                    "enum": ["shell", "agent"],
                    "description": "Legacy: use 'payload' instead. 'agent' runs an LLM turn."
                },
                "command": {
                    "type": "string",
                    "description": "Shell command for job_type='shell'"
                },
                "prompt": {
                    "type": "string",
                    "description": "LLM prompt for job_type='agent'. Overridden by payload.message."
                },
                "session_target": {
                    "type": "string",
                    "enum": ["isolated", "main"],
                    "description": "isolated=new context (default), main=inject into main session"
                },
                "model": { "type": "string", "description": "Override model for agent jobs" },
                "delivery": {
                    "type": "object",
                    "description": "Delivery config: {mode:'announce',channel:'signal',to:'<phone|uuid|group:ID>'} or {mode:'none'}",
                    "properties": {
                        "mode": { "type": "string", "enum": ["none", "announce"] },
                        "channel": { "type": "string", "enum": ["signal", "telegram", "discord", "slack", "mattermost"] },
                        "to": { "type": "string", "description": "Recipient: E.164 phone, UUID, or group:<groupId>" },
                        "best_effort": { "type": "boolean", "default": true }
                    }
                },
                "delete_after_run": {
                    "type": "boolean",
                    "description": "Auto-delete one-shot jobs after success (default true for 'at' schedule)"
                },
                "approved": {
                    "type": "boolean",
                    "description": "Set true to explicitly approve medium/high-risk shell commands in supervised mode",
                    "default": false
                }
            },
            "required": ["schedule"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let cfg = self.config.load_full();
        if !cfg.cron.enabled {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cron is disabled by config (cron.enabled=false)".to_string()),
            });
        }

        let schedule = match args.get("schedule") {
            Some(v) => match serde_json::from_value::<Schedule>(v.clone()) {
                Ok(schedule) => schedule,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Invalid schedule: {e}")),
                    });
                }
            },
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'schedule' parameter".to_string()),
                });
            }
        };

        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        // Support OpenClaw-compatible payload.kind API as an alias for job_type + prompt/command.
        // payload.kind='agentTurn' → agent job (isolated)
        // payload.kind='systemEvent' → agent job (main session, sends text directly)
        let payload = args.get("payload");
        let payload_kind = payload
            .and_then(|p| p.get("kind"))
            .and_then(serde_json::Value::as_str);

        // If payload.kind is set, it overrides job_type
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
        let approved = args
            .get("approved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

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

                if let Err(reason) = self.security.validate_command_execution(command, approved) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(reason),
                    });
                }

                if let Some(blocked) = self.enforce_mutation_allowed("cron_add") {
                    return Ok(blocked);
                }

                cron::add_shell_job(&cfg, name, schedule, command)
            }
            JobType::Agent => {
                // Resolve prompt: payload.message > payload.text > prompt field
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

                // payload.kind='systemEvent' → main session; 'agentTurn' or no payload → isolated
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
                        Ok(cfg) => Some(cfg),
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

                if let Some(blocked) = self.enforce_mutation_allowed("cron_add") {
                    return Ok(blocked);
                }

                cron::add_agent_job(
                    &cfg,
                    name,
                    schedule,
                    prompt,
                    session_target,
                    model,
                    delivery,
                    delete_after_run,
                )
            }
        };

        match result {
            Ok(job) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "id": job.id,
                    "name": job.name,
                    "job_type": job.job_type,
                    "schedule": job.schedule,
                    "next_run": job.next_run,
                    "enabled": job.enabled
                }))?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{new_shared, Config};
    use crate::security::AutonomyLevel;
    use tempfile::TempDir;

    async fn test_config(tmp: &TempDir) -> SharedConfig {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        new_shared(config)
    }

    fn test_security(cfg: &Config) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::from_config(
            &cfg.autonomy,
            &cfg.workspace_dir,
        ))
    }

    #[tokio::test]
    async fn adds_shell_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronAddTool::new(Arc::clone(&cfg), test_security(&cfg_snap));
        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "echo ok"
            }))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("next_run"));
    }

    #[tokio::test]
    async fn blocks_disallowed_shell_command() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.allowed_commands = vec!["echo".into()];
        config.autonomy.level = AutonomyLevel::Supervised;
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        let cfg = new_shared(config);
        let cfg_snap = cfg.load_full();
        let tool = CronAddTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "curl https://example.com"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("not allowed"));
    }

    #[tokio::test]
    async fn blocks_mutation_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = new_shared(config);
        let cfg_snap = cfg.load_full();
        let tool = CronAddTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "echo ok"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("read-only") || error.contains("not allowed"));
    }

    #[tokio::test]
    async fn medium_risk_shell_command_requires_approval() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.allowed_commands = vec!["touch".into()];
        config.autonomy.level = AutonomyLevel::Supervised;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = new_shared(config);
        let cfg_snap = cfg.load_full();
        let tool = CronAddTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let denied = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "touch cron-approval-test"
            }))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied
            .error
            .unwrap_or_default()
            .contains("explicit approval"));

        let approved = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "touch cron-approval-test",
                "approved": true
            }))
            .await
            .unwrap();
        assert!(approved.success, "{:?}", approved.error);
    }

    #[tokio::test]
    async fn rejects_invalid_schedule() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronAddTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "every", "every_ms": 0 },
                "job_type": "shell",
                "command": "echo nope"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("every_ms must be > 0"));
    }

    #[tokio::test]
    async fn agent_job_requires_prompt() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronAddTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "agent"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Missing prompt"));
    }
}
