//! Config reload tool.
//!
//! The tool does not decide or publish fields itself. It delegates to the
//! process-level configuration generation owner and reports the exact applied,
//! rebuilt, restarted and restart-required sets.

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::{ConfigReloadTrigger, SharedConfig};
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Tool that hot-reloads the merged configuration at runtime.
///
/// Accepts the process-level [`SharedConfig`] generation owner.
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
        "Reload merged configuration through the process ConfigGeneration owner. \
         The result explicitly separates fields applied live, runtime objects rebuilt, \
         components restarted, and fields that still require a process restart."
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

        let config_path = self.config.load_full().config_path.clone();
        let manager = Arc::clone(&self.config);
        let report =
            match tokio::task::spawn_blocking(move || manager.reload_from_disk(ConfigReloadTrigger::Tool)).await {
                Ok(Ok(report)) => report,
                Ok(Err(error)) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Failed to load merged config from {} (including config.d): {error}",
                            config_path.display()
                        )),
                    });
                }
                Err(error) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Config reload worker failed for {}: {error}",
                            config_path.display()
                        )),
                    });
                }
            };
        let output = serde_json::to_string_pretty(&serde_json::json!({
            "status": report.status(),
            "active_generation": report.active_generation.0,
            "active_source_revision": report.active_source_revision.fingerprint_sha256,
            "desired_source_revision": report.desired_source_revision.fingerprint_sha256,
            "changed": report.changed,
            "applied": report.applied,
            "rebuilt": report.rebuilt,
            "restarted": report.restarted,
            "restart_required": report.restart_required,
            "participant_acks": report.participant_acks,
        }))?;

        tracing::info!(
            path = %config_path.display(),
            active_generation = report.active_generation.0,
            status = report.status(),
            "Config generation reload completed"
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

        let workspace = tempfile::tempdir().unwrap();
        let cfg = Config::load_from_path(tmp.path(), workspace.path().to_path_buf()).unwrap();

        let tool = make_tool_with_config(cfg);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["status"], "unchanged", "Unexpected reload result: {output}");
        assert_eq!(output["changed"], serde_json::json!([]));
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
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            output["changed"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field == "agent")),
            "Expected fragment-driven agent diff in output: {output}"
        );

        let updated = shared.load_full();
        assert_eq!(updated.agent.max_history_messages, 123);
    }
}
