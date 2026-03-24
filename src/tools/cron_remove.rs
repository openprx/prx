use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::SharedConfig;
use crate::cron;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct CronRemoveTool {
    config: SharedConfig,
    security: Arc<SecurityPolicy>,
}

impl CronRemoveTool {
    pub const fn new(config: SharedConfig, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn enforce_mutation_allowed(&self, action: &str) -> Option<ToolResult> {
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

#[async_trait]
impl Tool for CronRemoveTool {
    fn name(&self) -> &str {
        "cron_remove"
    }

    fn description(&self) -> &str {
        "Remove a cron job by id"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string" }
            },
            "required": ["job_id"]
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

        let job_id = match args.get("job_id").and_then(serde_json::Value::as_str) {
            Some(v) if !v.trim().is_empty() => v,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'job_id' parameter".to_string()),
                });
            }
        };

        if let Some(blocked) = self.enforce_mutation_allowed("cron_remove") {
            return Ok(blocked);
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
    use crate::config::{Config, new_shared};
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
    async fn removes_existing_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo ok").unwrap();
        let tool = CronRemoveTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool.execute(json!({"job_id": job.id})).await.unwrap();
        assert!(result.success);
        assert!(cron::list_jobs(&cfg_snap).unwrap().is_empty());
    }

    #[tokio::test]
    async fn errors_when_job_id_missing() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let cfg_snap = cfg.load_full();
        let tool = CronRemoveTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Missing 'job_id'"));
    }

    #[tokio::test]
    async fn blocks_remove_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg_snap = Arc::new(config.clone());
        let job = cron::add_job(&cfg_snap, "*/5 * * * *", "echo ok").unwrap();
        let cfg = new_shared(config);
        let tool = CronRemoveTool::new(Arc::clone(&cfg), test_security(&cfg_snap));

        let result = tool.execute(json!({"job_id": job.id})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only"));
    }
}
