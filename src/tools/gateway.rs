//! Gateway management tool — inspect and control the ZeroClaw daemon.
//!
//! Provides a minimal interface for the agent to manage its own gateway:
//!  - `config.get` — return the current gateway configuration
//!  - `status`     — show uptime, model, provider, and channel info
//!  - `restart`    — trigger a graceful daemon restart via SIGHUP
//!
//! Designed to align with OpenClaw's `gateway` tool interface.

use super::traits::{Tool, ToolResult};
use crate::config::Config;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

pub struct GatewayTool {
    config: Arc<Config>,
    /// Provider name (e.g. "anthropic", "openrouter")
    provider_name: String,
    /// Current model (e.g. "claude-sonnet-4-6")
    model: String,
    /// Active channel names (e.g. ["signal", "telegram"])
    channels: Vec<String>,
    /// Process start time for uptime calculation
    started_at: Instant,
}

impl GatewayTool {
    pub fn new(
        config: Arc<Config>,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        channels: Vec<String>,
    ) -> Self {
        Self {
            config,
            provider_name: provider_name.into(),
            model: model.into(),
            channels,
            started_at: Instant::now(),
        }
    }

    fn format_uptime(elapsed_secs: u64) -> String {
        if elapsed_secs < 60 {
            format!("{elapsed_secs}s")
        } else if elapsed_secs < 3600 {
            format!("{}m {}s", elapsed_secs / 60, elapsed_secs % 60)
        } else {
            format!(
                "{}h {}m",
                elapsed_secs / 3600,
                (elapsed_secs % 3600) / 60
            )
        }
    }
}

#[async_trait]
impl Tool for GatewayTool {
    fn name(&self) -> &str {
        "gateway"
    }

    fn description(&self) -> &str {
        "Manage the ZeroClaw gateway daemon. \
         Actions: 'config.get' (read current config), 'status' (uptime/model/channels), \
         'restart' (send SIGHUP for graceful restart)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["config.get", "status", "restart"],
                    "description": "Action to perform."
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
            "config.get" => {
                // Read the gateway section from config and return it
                let gw = &self.config.gateway;
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
                    "config_path": self.config.config_path.display().to_string(),
                });
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&output)?,
                    error: None,
                })
            }

            "status" => {
                let uptime_secs = self.started_at.elapsed().as_secs();
                let uptime_str = Self::format_uptime(uptime_secs);
                let channels_str = if self.channels.is_empty() {
                    "none".to_string()
                } else {
                    self.channels.join(", ")
                };
                let gw = &self.config.gateway;
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

            "restart" => {
                // Send SIGHUP to self for graceful restart
                #[cfg(unix)]
                {
                    let pid = std::process::id();
                    // SAFETY: kill(2) with SIGHUP is safe to call with our own PID
                    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) };
                    if ret == 0 {
                        Ok(ToolResult {
                            success: true,
                            output: format!("SIGHUP sent to PID {pid} — daemon will restart gracefully."),
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
                        error: Some("Graceful restart via SIGHUP is only supported on Unix systems.".to_string()),
                    })
                }
            }

            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{other}'. Use: config.get, status, restart."
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> GatewayTool {
        let config = Arc::new(Config {
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
        let result = tool
            .execute(json!({"action": "config.get"}))
            .await
            .unwrap();
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
        assert!(result.output.contains("3000"));
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .execute(json!({"action": "explode"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Unknown action"));
    }

    #[tokio::test]
    async fn missing_action_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Missing 'action'"));
    }
}
