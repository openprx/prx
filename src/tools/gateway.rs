//! Gateway management tool — inspect and control the OpenPRX daemon.
//!
//! Provides a minimal interface for the agent to manage its own gateway:
//!  - `config.get` — return the current gateway configuration
//!  - `config.patch` — apply a JSON merge patch to config.toml
//!  - `status`     — show uptime, model, provider, and channel info
//!  - `restart`    — trigger a graceful daemon restart via SIGHUP
//!  - `version`    — return OpenPRX version info
//!  - `components` — list active runtime components
//!
//! Designed to align with OpenClaw's `gateway` tool interface.

use super::traits::{Tool, ToolResult};
use crate::config::{Config, SharedConfig};
use anyhow::Context;
use async_trait::async_trait;
use serde_json::json;
use std::time::Instant;

pub struct GatewayTool {
    config: SharedConfig,
    /// Provider name (e.g. "anthropic", "openrouter")
    provider_name: String,
    /// Current model (e.g. "claude-sonnet-4-6")
    model: String,
    /// Active channel names (e.g. ["signal", "telegram"])
    channels: Vec<String>,
    /// Number of registered runtime tools, if known
    tools_count: usize,
    /// Process start time for uptime calculation
    started_at: Instant,
}

impl GatewayTool {
    pub fn new(
        config: SharedConfig,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        channels: Vec<String>,
    ) -> Self {
        Self {
            config,
            provider_name: provider_name.into(),
            model: model.into(),
            channels,
            tools_count: 0,
            started_at: Instant::now(),
        }
    }

    pub fn with_tools_count(mut self, tools_count: usize) -> Self {
        self.tools_count = tools_count;
        self
    }

    fn format_uptime(elapsed_secs: u64) -> String {
        if elapsed_secs < 60 {
            format!("{elapsed_secs}s")
        } else if elapsed_secs < 3600 {
            format!("{}m {}s", elapsed_secs / 60, elapsed_secs % 60)
        } else {
            format!("{}h {}m", elapsed_secs / 3600, (elapsed_secs % 3600) / 60)
        }
    }

    fn merge_json_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
        match patch {
            serde_json::Value::Object(patch_map) => {
                if !target.is_object() {
                    *target = json!({});
                }

                let Some(target_map) = target.as_object_mut() else {
                    return;
                };

                for (key, patch_value) in patch_map {
                    if patch_value.is_null() {
                        target_map.remove(key);
                        continue;
                    }

                    let target_entry = target_map.entry(key.clone()).or_insert_with(|| json!(null));
                    Self::merge_json_patch(target_entry, patch_value);
                }
            }
            _ => {
                *target = patch.clone();
            }
        }
    }

    fn active_providers(&self, cfg: &Config) -> Vec<String> {
        let mut providers = Vec::new();
        providers.push(self.provider_name.clone());

        if let Some(default_provider) = cfg
            .default_provider
            .as_ref()
            .map(|provider| provider.trim())
            .filter(|provider| !provider.is_empty())
            .map(ToOwned::to_owned)
        {
            let already_present = providers
                .iter()
                .any(|provider| provider.eq_ignore_ascii_case(&default_provider));
            if !already_present {
                providers.push(default_provider);
            }
        }

        providers
    }

    async fn apply_config_patch(&self, patch: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let current = self.config.load_full();
        let config_path = current.config_path.clone();
        let raw = tokio::fs::read_to_string(&config_path)
            .await
            .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

        let raw_toml: toml::Value = toml::from_str(&raw).with_context(|| {
            format!(
                "Failed to parse config.toml before patch: {}",
                config_path.display()
            )
        })?;
        let mut raw_json = serde_json::to_value(raw_toml)
            .context("Failed to convert config TOML into JSON value")?;

        Self::merge_json_patch(&mut raw_json, patch);

        let mut patched_config: Config =
            serde_json::from_value(raw_json).context("Patched config is invalid for OpenPRX")?;
        patched_config.workspace_dir = current.workspace_dir.clone();
        patched_config.config_path = current.config_path.clone();
        patched_config.save().await?;

        let output = json!({
            "status": "patched",
            "config_path": config_path.display().to_string(),
            "reload": "watcher_will_reload_on_file_change"
        });

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&output)?,
            error: None,
        })
    }
}

#[async_trait]
impl Tool for GatewayTool {
    fn name(&self) -> &str {
        "gateway"
    }

    fn description(&self) -> &str {
        "Manage the OpenPRX gateway daemon. \
         Actions: 'config.get' (read current config), 'config.patch' (merge patch config), \
         'status' (uptime/model/channels), 'version' (build version), \
         'components' (active channels/providers/memory/tools), \
         'restart' (send SIGHUP for graceful restart)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["config.get", "config.patch", "status", "version", "components", "restart"],
                    "description": "Action to perform."
                },
                "patch": {
                    "type": "object",
                    "description": "JSON merge patch payload, required for action='config.patch'."
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

        // Destructive actions require a trusted scope marker to prevent
        // unauthorized callers from mutating config or restarting the daemon.
        if action == "config.patch" || action == "restart" {
            let trusted = args
                .get("_prx_scope_trusted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !trusted {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "Destructive gateway action requires trusted scope (_prx_scope_trusted=true)"
                            .to_string(),
                    ),
                });
            }
        }

        let cfg = self.config.load_full();
        match action {
            "config.get" => {
                // Read the gateway section from config and return it
                let gw = &cfg.gateway;
                let output = json!({
                    "host": gw.host,
                    "port": gw.port,
                    "require_pairing": gw.require_pairing,
                    "allow_public_bind": gw.allow_public_bind,
                    "pair_rate_limit_per_minute": gw.pair_rate_limit_per_minute,
                    "webhook_rate_limit_per_minute": gw.webhook_rate_limit_per_minute,
                    "trust_forwarded_headers": gw.trust_forwarded_headers,
                    "rate_limit_max_keys": gw.rate_limit_max_keys,
                    "idempotency_ttl_secs": gw.idempotency_ttl_secs,
                    "idempotency_max_keys": gw.idempotency_max_keys,
                    "paired_tokens_count": gw.paired_tokens.len(),
                    "config_path": cfg.config_path.display().to_string(),
                });
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&output)?,
                    error: None,
                })
            }

            "config.patch" => {
                let patch = match args.get("patch") {
                    Some(value) if value.is_object() => value,
                    Some(_) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Invalid 'patch': expected an object for action 'config.patch'"
                                    .to_string(),
                            ),
                        });
                    }
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Missing 'patch' parameter for action 'config.patch'".to_string(),
                            ),
                        });
                    }
                };
                self.apply_config_patch(patch).await
            }

            "status" => {
                let uptime_secs = self.started_at.elapsed().as_secs();
                let uptime_str = Self::format_uptime(uptime_secs);
                let channels_str = if self.channels.is_empty() {
                    "none".to_string()
                } else {
                    self.channels.join(", ")
                };
                let gw = &cfg.gateway;
                let output = format!(
                    "🌐 Gateway Status\n\
                     ─────────────────\n\
                     Model:      {}\n\
                     Provider:   {}\n\
                     Channels:   {}\n\
                     Uptime:     {}\n\
                     Listen:     {}:{}\n\
                     Pairing:    {}",
                    self.model,
                    self.provider_name,
                    channels_str,
                    uptime_str,
                    gw.host,
                    gw.port,
                    if gw.require_pairing {
                        "required"
                    } else {
                        "disabled"
                    },
                );
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }

            "version" => {
                let output = json!({
                    "name": env!("CARGO_PKG_NAME"),
                    "version": env!("CARGO_PKG_VERSION"),
                });
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&output)?,
                    error: None,
                })
            }

            "components" => {
                let channels = self.channels.clone();
                let providers = self.active_providers(&cfg);
                let memory_backend = crate::memory::effective_memory_backend_name(
                    &cfg.memory.backend,
                    Some(&cfg.storage.provider.config),
                );
                let output = json!({
                    "channels": channels,
                    "providers": providers,
                    "memory_backend": memory_backend,
                    "tools_count": self.tools_count,
                });
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&output)?,
                    error: None,
                })
            }

            "restart" => {
                // Send SIGHUP to self for graceful restart
                #[cfg(unix)]
                {
                    let pid = std::process::id();

                    // Verify the PID still belongs to us by checking /proc/self/cmdline.
                    // This guards against PID reuse in the unlikely event the process
                    // table wraps between id() and kill().
                    let cmdline_path = format!("/proc/{pid}/cmdline");
                    match std::fs::read(&cmdline_path) {
                        Ok(data) => {
                            let cmdline = String::from_utf8_lossy(&data);
                            if !cmdline.contains("prx") {
                                return Ok(ToolResult {
                                    success: false,
                                    output: String::new(),
                                    error: Some(format!(
                                        "PID {pid} cmdline does not match expected process: {cmdline}"
                                    )),
                                });
                            }
                        }
                        Err(e) => {
                            // /proc may not exist (non-Linux), fall through since
                            // std::process::id() is inherently our own PID.
                            tracing::warn!("Could not read {cmdline_path} for PID validation: {e}");
                        }
                    }

                    // SAFETY: We send SIGHUP to our own PID (verified above via
                    // /proc/{pid}/cmdline).  kill(2) is safe for signal delivery
                    // to the calling process.
                    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) };
                    if ret == 0 {
                        Ok(ToolResult {
                            success: true,
                            output: format!(
                                "SIGHUP sent to PID {pid} — daemon will restart gracefully."
                            ),
                            error: None,
                        })
                    } else {
                        let err = std::io::Error::last_os_error();
                        Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to send SIGHUP: {err}")),
                        })
                    }
                }
                #[cfg(not(unix))]
                {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "Graceful restart via SIGHUP is only supported on Unix systems."
                                .to_string(),
                        ),
                    })
                }
            }

            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{other}'. Use: config.get, config.patch, status, version, components, restart."
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, new_shared};
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> GatewayTool {
        let config = new_shared(Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        });
        GatewayTool::new(
            config,
            "anthropic",
            "claude-sonnet-4-6",
            vec!["signal".to_string()],
        )
        .with_tools_count(12)
    }

    #[test]
    fn name_and_description() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        assert_eq!(tool.name(), "gateway");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn format_uptime_seconds() {
        assert_eq!(GatewayTool::format_uptime(45), "45s");
    }

    #[test]
    fn format_uptime_minutes() {
        assert_eq!(GatewayTool::format_uptime(125), "2m 5s");
    }

    #[test]
    fn format_uptime_hours() {
        assert_eq!(GatewayTool::format_uptime(7200), "2h 0m");
    }

    #[tokio::test]
    async fn config_get_returns_gateway_fields() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({"action": "config.get"})).await.unwrap();
        assert!(result.success);
        let val: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert!(val["port"].as_u64().is_some());
        assert!(val["host"].as_str().is_some());
    }

    #[tokio::test]
    async fn status_contains_expected_fields() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({"action": "status"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("claude-sonnet-4-6"));
        assert!(result.output.contains("anthropic"));
        assert!(result.output.contains("signal"));
        assert!(result.output.contains("16830"));
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({"action": "explode"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Unknown action"));
    }

    #[tokio::test]
    async fn missing_action_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("Missing 'action'")
        );
    }

    #[tokio::test]
    async fn version_returns_package_version() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({"action": "version"})).await.unwrap();
        assert!(result.success);
        let val: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(val["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn components_returns_expected_fields() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({"action": "components"})).await.unwrap();
        assert!(result.success);
        let val: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert!(val["channels"].is_array());
        assert!(val["providers"].is_array());
        assert!(val["memory_backend"].is_string());
        assert_eq!(val["tools_count"].as_u64(), Some(12));
    }

    #[tokio::test]
    async fn config_patch_updates_config_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: config_path.clone(),
            ..Config::default()
        };
        cfg.save().await.unwrap();
        let shared = new_shared(cfg.clone());
        let tool = GatewayTool::new(shared, "anthropic", "claude-sonnet-4-6", vec![]);

        let result = tool
            .execute(json!({
                "action": "config.patch",
                "_prx_scope_trusted": true,
                "patch": {
                    "gateway": {
                        "port": 3300
                    }
                }
            }))
            .await
            .unwrap();

        assert!(result.success);

        let contents = tokio::fs::read_to_string(config_path).await.unwrap();
        let parsed: Config = toml::from_str(&contents).unwrap();
        assert_eq!(parsed.gateway.port, 3300);
    }

    #[tokio::test]
    async fn config_patch_rejects_untrusted() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .execute(json!({
                "action": "config.patch",
                "patch": { "gateway": { "port": 9999 } }
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("trusted scope"));
    }

    #[tokio::test]
    async fn restart_rejects_untrusted() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({ "action": "restart" })).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("trusted scope"));
    }
}
