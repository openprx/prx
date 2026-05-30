//! Config hot-reload tool — re-reads config.toml + enabled config.d fragments
//! and updates runtime-mutable settings.
//!
//! Hot-reloadable fields (take effect immediately):
//!   - `default_temperature`
//!   - `agent.*` (max_tool_iterations, max_history_messages, parallel_tools,
//!     compact_context, read_only_tool_concurrency_window,
//!     read_only_tool_timeout_secs, priority_scheduling_enabled, low_priority_tools,
//!     rollout/kill-switch/rollback thresholds)
//!   - `heartbeat.enabled`, `heartbeat.interval_minutes`
//!   - `cron.enabled`, `cron.max_run_history`
//!   - `web_search.enabled`, `web_search.max_results`
//!
//! Fields that require a full restart to take effect:
//!   - `api_key`, `api_url`, `default_provider`, `default_model`
//!   - `channels_config` (Signal, WhatsApp, Telegram, etc.)
//!   - `memory`, `storage` backends
//!   - `autonomy` / security policy

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::{Config, SharedConfig};
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Tool that hot-reloads the merged configuration at runtime.
///
/// Accepts a [`SharedConfig`] (ArcSwap-backed) and atomically stores the new
/// config after validation — no Mutex required.
pub struct ConfigReloadTool {
    config: SharedConfig,
    security: Arc<SecurityPolicy>,
}

impl ConfigReloadTool {
    /// Create a new `ConfigReloadTool` backed by the shared config state.
    pub fn new(config: SharedConfig) -> Self {
        Self::with_security(config, Arc::new(SecurityPolicy::default()))
    }

    pub const fn with_security(config: SharedConfig, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }
}

#[async_trait]
impl Tool for ConfigReloadTool {
    fn name(&self) -> &str {
        "config_reload"
    }

    fn description(&self) -> &str {
        "Reload merged configuration (config.toml + enabled config.d files) without restarting the daemon. \
         Hot-reloads: temperature, agent settings (max iterations/history, concurrency, priority), \
         heartbeat, cron, and web_search settings. \
         Provider, model, channels, memory, and security require a full restart."
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
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
        if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
            self.name(),
            "config_reload:reload",
            ResourceRiskLevel::Low,
            approval_grant.as_ref(),
        ) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // 1. Read runtime config locations from current config (lock-free)
        let current = self.config.load_full();
        let config_path = current.config_path.clone();
        let workspace_dir = current.workspace_dir.clone();

        if config_path.as_os_str().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Config path is not set; cannot reload.".into()),
            });
        }

        // 2. Load merged config (config.toml + enabled config.d/*.toml) and validate.
        let fresh: Config = match Config::load_from_path(&config_path, workspace_dir) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Failed to load merged config from {} (including config.d): {e}",
                        config_path.display()
                    )),
                });
            }
        };

        // 3. Diff hot-reloadable fields and atomically store new config
        let mut changes: Vec<String> = Vec::new();
        {
            let old = self.config.load_full();

            // Temperature
            if (old.default_temperature - fresh.default_temperature).abs() > 1e-9 {
                changes.push(format!(
                    "temperature: {:.2} → {:.2}",
                    old.default_temperature, fresh.default_temperature
                ));
            }

            // Agent: max_tool_iterations
            if old.agent.max_tool_iterations != fresh.agent.max_tool_iterations {
                changes.push(format!(
                    "agent.max_tool_iterations: {} → {}",
                    old.agent.max_tool_iterations, fresh.agent.max_tool_iterations
                ));
            }

            // Agent: max_history_messages
            if old.agent.max_history_messages != fresh.agent.max_history_messages {
                changes.push(format!(
                    "agent.max_history_messages: {} → {}",
                    old.agent.max_history_messages, fresh.agent.max_history_messages
                ));
            }

            // Agent: parallel_tools
            if old.agent.parallel_tools != fresh.agent.parallel_tools {
                changes.push(format!(
                    "agent.parallel_tools: {} → {}",
                    old.agent.parallel_tools, fresh.agent.parallel_tools
                ));
            }

            // Agent: compact_context
            if old.agent.compact_context != fresh.agent.compact_context {
                changes.push(format!(
                    "agent.compact_context: {} → {}",
                    old.agent.compact_context, fresh.agent.compact_context
                ));
            }
            if old.agent.read_only_tool_concurrency_window != fresh.agent.read_only_tool_concurrency_window {
                changes.push(format!(
                    "agent.read_only_tool_concurrency_window: {} → {}",
                    old.agent.read_only_tool_concurrency_window, fresh.agent.read_only_tool_concurrency_window
                ));
            }
            if old.agent.read_only_tool_timeout_secs != fresh.agent.read_only_tool_timeout_secs {
                changes.push(format!(
                    "agent.read_only_tool_timeout_secs: {} → {}",
                    old.agent.read_only_tool_timeout_secs, fresh.agent.read_only_tool_timeout_secs
                ));
            }
            if old.agent.priority_scheduling_enabled != fresh.agent.priority_scheduling_enabled {
                changes.push(format!(
                    "agent.priority_scheduling_enabled: {} → {}",
                    old.agent.priority_scheduling_enabled, fresh.agent.priority_scheduling_enabled
                ));
            }
            if old.agent.low_priority_tools != fresh.agent.low_priority_tools {
                changes.push(format!(
                    "agent.low_priority_tools: {:?} → {:?}",
                    old.agent.low_priority_tools, fresh.agent.low_priority_tools
                ));
            }
            if old.agent.concurrency_kill_switch_force_serial != fresh.agent.concurrency_kill_switch_force_serial {
                changes.push(format!(
                    "agent.concurrency_kill_switch_force_serial: {} → {}",
                    old.agent.concurrency_kill_switch_force_serial, fresh.agent.concurrency_kill_switch_force_serial
                ));
            }
            if old.agent.concurrency_rollout_stage != fresh.agent.concurrency_rollout_stage {
                changes.push(format!(
                    "agent.concurrency_rollout_stage: {} → {}",
                    old.agent.concurrency_rollout_stage, fresh.agent.concurrency_rollout_stage
                ));
            }
            if old.agent.concurrency_rollout_sample_percent != fresh.agent.concurrency_rollout_sample_percent {
                changes.push(format!(
                    "agent.concurrency_rollout_sample_percent: {} → {}",
                    old.agent.concurrency_rollout_sample_percent, fresh.agent.concurrency_rollout_sample_percent
                ));
            }
            if old.agent.concurrency_rollout_channels != fresh.agent.concurrency_rollout_channels {
                changes.push(format!(
                    "agent.concurrency_rollout_channels: {:?} → {:?}",
                    old.agent.concurrency_rollout_channels, fresh.agent.concurrency_rollout_channels
                ));
            }
            if old.agent.concurrency_auto_rollback_enabled != fresh.agent.concurrency_auto_rollback_enabled {
                changes.push(format!(
                    "agent.concurrency_auto_rollback_enabled: {} → {}",
                    old.agent.concurrency_auto_rollback_enabled, fresh.agent.concurrency_auto_rollback_enabled
                ));
            }
            if (old.agent.concurrency_rollback_timeout_rate_threshold
                - fresh.agent.concurrency_rollback_timeout_rate_threshold)
                .abs()
                > f64::EPSILON
            {
                changes.push(format!(
                    "agent.concurrency_rollback_timeout_rate_threshold: {:.3} → {:.3}",
                    old.agent.concurrency_rollback_timeout_rate_threshold,
                    fresh.agent.concurrency_rollback_timeout_rate_threshold
                ));
            }
            if (old.agent.concurrency_rollback_cancel_rate_threshold
                - fresh.agent.concurrency_rollback_cancel_rate_threshold)
                .abs()
                > f64::EPSILON
            {
                changes.push(format!(
                    "agent.concurrency_rollback_cancel_rate_threshold: {:.3} → {:.3}",
                    old.agent.concurrency_rollback_cancel_rate_threshold,
                    fresh.agent.concurrency_rollback_cancel_rate_threshold
                ));
            }
            if (old.agent.concurrency_rollback_error_rate_threshold
                - fresh.agent.concurrency_rollback_error_rate_threshold)
                .abs()
                > f64::EPSILON
            {
                changes.push(format!(
                    "agent.concurrency_rollback_error_rate_threshold: {:.3} → {:.3}",
                    old.agent.concurrency_rollback_error_rate_threshold,
                    fresh.agent.concurrency_rollback_error_rate_threshold
                ));
            }

            // Heartbeat
            if old.heartbeat.enabled != fresh.heartbeat.enabled {
                changes.push(format!(
                    "heartbeat.enabled: {} → {}",
                    old.heartbeat.enabled, fresh.heartbeat.enabled
                ));
            }
            if old.heartbeat.interval_minutes != fresh.heartbeat.interval_minutes {
                changes.push(format!(
                    "heartbeat.interval_minutes: {} → {}",
                    old.heartbeat.interval_minutes, fresh.heartbeat.interval_minutes
                ));
            }

            // Cron
            if old.cron.enabled != fresh.cron.enabled {
                changes.push(format!("cron.enabled: {} → {}", old.cron.enabled, fresh.cron.enabled));
            }
            if old.cron.max_run_history != fresh.cron.max_run_history {
                changes.push(format!(
                    "cron.max_run_history: {} → {}",
                    old.cron.max_run_history, fresh.cron.max_run_history
                ));
            }

            // Web search
            if old.web_search.enabled != fresh.web_search.enabled {
                changes.push(format!(
                    "web_search.enabled: {} → {}",
                    old.web_search.enabled, fresh.web_search.enabled
                ));
            }
            if old.web_search.max_results != fresh.web_search.max_results {
                changes.push(format!(
                    "web_search.max_results: {} → {}",
                    old.web_search.max_results, fresh.web_search.max_results
                ));
            }

            // Atomically publish — preserve runtime paths
            let mut updated = fresh;
            updated.config_path = old.config_path.clone();
            updated.workspace_dir = old.workspace_dir.clone();
            if updated.memory.acl_enabled != old.memory.acl_enabled {
                changes.push(format!(
                    "memory.acl_enabled: {} → {} (deferred; restart required)",
                    old.memory.acl_enabled, updated.memory.acl_enabled
                ));
                updated.memory.acl_enabled = old.memory.acl_enabled;
            }
            self.config.store(Arc::new(updated));
        }

        let output = if changes.is_empty() {
            format!(
                "✅ Config reloaded from `{}` — no hot-reloadable changes detected.",
                config_path.display()
            )
        } else {
            format!(
                "✅ Config reloaded from `{}`.\n\nChanges applied:\n{}",
                config_path.display(),
                changes
                    .iter()
                    .map(|c| format!("  • {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };

        tracing::info!(
            path = %config_path.display(),
            changes = %changes.len(),
            "Config hot-reloaded"
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
        &[ToolCategory::System]
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, new_shared};

    fn make_tool_with_config(cfg: Config) -> ConfigReloadTool {
        ConfigReloadTool::new(new_shared(cfg))
    }

    fn make_readonly_tool_with_config(cfg: Config) -> ConfigReloadTool {
        ConfigReloadTool::with_security(
            new_shared(cfg),
            Arc::new(SecurityPolicy {
                autonomy: crate::security::policy::AutonomyLevel::ReadOnly,
                ..SecurityPolicy::default()
            }),
        )
    }

    #[test]
    fn name_and_description() {
        let tool = make_tool_with_config(Config::default());
        assert_eq!(tool.name(), "config_reload");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn schema_no_required_params() {
        let tool = make_tool_with_config(Config::default());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.is_empty());
    }

    #[tokio::test]
    async fn missing_config_path_returns_error() {
        let cfg = Config {
            config_path: std::path::PathBuf::new(),
            ..Config::default()
        };
        let tool = make_tool_with_config(cfg);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Config path is not set"));
    }

    #[tokio::test]
    async fn invalid_config_path_returns_error() {
        let mut cfg = Config::default();
        cfg.config_path = std::path::PathBuf::from("/nonexistent/path/config.toml");
        let tool = make_tool_with_config(cfg);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Failed to load merged config"));
    }

    #[tokio::test]
    async fn reload_obeys_readonly_resource_gate() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(tmp.path(), "default_temperature = 0.3\n")
            .await
            .unwrap();

        let mut cfg = Config::default();
        cfg.config_path = tmp.path().to_path_buf();
        let tool = make_readonly_tool_with_config(cfg);

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only mode"));
    }

    #[tokio::test]
    async fn reloads_temperature_change() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = "default_temperature = 0.3\n";
        tokio::fs::write(tmp.path(), content).await.unwrap();

        let mut cfg = Config::default();
        cfg.config_path = tmp.path().to_path_buf();
        cfg.default_temperature = 0.7;

        let shared = new_shared(cfg);
        let tool = ConfigReloadTool::new(Arc::clone(&shared));

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success, "Expected success: {:?}", result.error);
        assert!(
            result.output.contains("temperature"),
            "Expected temperature change in output: {}",
            result.output
        );

        let updated = shared.load_full();
        assert!(
            (updated.default_temperature - 0.3).abs() < 1e-9,
            "Temperature should be 0.3, got {}",
            updated.default_temperature
        );
    }

    #[tokio::test]
    async fn no_changes_when_config_unchanged() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let default_temp = Config::default().default_temperature;
        let content = format!("default_temperature = {default_temp}\n");
        tokio::fs::write(tmp.path(), &content).await.unwrap();

        let mut cfg = Config::default();
        cfg.config_path = tmp.path().to_path_buf();

        let tool = make_tool_with_config(cfg);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert!(
            result.output.contains("no hot-reloadable changes"),
            "Expected no-change message: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn reload_reads_enabled_config_d_fragments() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config_path = tmpdir.path().join("config.toml");
        let config_d = tmpdir.path().join("config.d");
        tokio::fs::create_dir_all(&config_d).await.unwrap();

        tokio::fs::write(
            &config_path,
            r#"
default_temperature = 0.7

[modules]
memory = false
channels = false
network = false
security = false
scheduler = false
agent = true
identity = false
routing = false
tools = false
integrations = false
nodes = false
cost = false
observability = false
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            config_d.join("agent.toml"),
            r#"
[agent]
max_history_messages = 123
"#,
        )
        .await
        .unwrap();

        let mut cfg = Config::default();
        cfg.config_path = config_path;
        cfg.workspace_dir = tmpdir.path().join("workspace");
        cfg.agent.max_history_messages = 50;
        tokio::fs::create_dir_all(&cfg.workspace_dir).await.unwrap();

        let shared = new_shared(cfg);
        let tool = ConfigReloadTool::new(Arc::clone(&shared));

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success, "Expected success: {:?}", result.error);
        assert!(
            result.output.contains("agent.max_history_messages: 50 → 123"),
            "Expected fragment-driven diff in output: {}",
            result.output
        );

        let updated = shared.load_full();
        assert_eq!(updated.agent.max_history_messages, 123);
    }
}
