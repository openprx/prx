//! Gateway management tool — inspect and control the OpenPRX daemon.
//!
//! Provides a minimal interface for the agent to manage its own gateway:
//!  - `config.get` — return the current gateway configuration
//!  - `config.patch` — apply a JSON merge patch to the configuration as one
//!    validated, activated and recorded configuration transaction
//!  - `status`     — show uptime, model, provider, and channel info
//!  - `restart`    — trigger a graceful daemon restart via SIGHUP
//!  - `version`    — return OpenPRX version info
//!  - `components` — list active runtime components
//!
//! Designed to align with OpenClaw's `gateway` tool interface.

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::manage::{ApplyOptions, ConfigActor, ConfigChange, ConfigTransactions};
use crate::config::secret_paths;
use crate::config::{Config, SharedConfig};
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use anyhow::Context;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
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
    /// Security policy for mutating gateway operations.
    security: Arc<SecurityPolicy>,
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
        // Only ever shown to the model: a `custom:` URL may carry
        // `user:password@`, which it must not see.
        let provider_name = provider_name.into();
        let provider_name = crate::providers::redact_endpoint_userinfo(&provider_name).into_owned();
        Self {
            config,
            provider_name,
            model: model.into(),
            channels,
            tools_count: 0,
            security: Arc::new(SecurityPolicy::default()),
            started_at: Instant::now(),
        }
    }

    pub const fn with_tools_count(mut self, tools_count: usize) -> Self {
        self.tools_count = tools_count;
        self
    }

    pub fn with_security(mut self, security: Arc<SecurityPolicy>) -> Self {
        self.security = security;
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
            .map(|provider| crate::providers::redact_endpoint_userinfo(provider).into_owned())
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

    /// Apply a JSON merge patch to the complete desired configuration (every
    /// `config.d` fragment included) as one configuration transaction: it is
    /// saved through the encrypting save path, activated immediately, and
    /// restored if the runtime refuses it.
    async fn apply_config_patch(
        &self,
        patch: &serde_json::Value,
        confirm_high_risk: bool,
        confirmed_revision: Option<&str>,
    ) -> anyhow::Result<ToolResult> {
        let refused = |error: String| ToolResult {
            success: false,
            output: String::new(),
            error: Some(error),
        };
        let desired = self.config.desired();
        let mut merged =
            serde_json::to_value(desired.config.as_ref()).context("Failed to render the current configuration")?;
        let stored_masks = secret_paths::scan(&merged).masked;
        Self::merge_json_patch(&mut merged, patch);
        let introduced = secret_paths::scan(&merged)
            .masked
            .into_iter()
            .filter(|path| !stored_masks.contains(path))
            .collect::<Vec<_>>();
        if !introduced.is_empty() {
            return Ok(refused(format!(
                "The patch sets the redaction mask `{}` as a credential at: {}; nothing was written.",
                secret_paths::REDACTION_MASK,
                introduced.join(", ")
            )));
        }
        // Deserialization and validation messages can quote a value.
        let patch_secrets = secret_paths::scan(patch);
        let mut patched: Config = match serde_json::from_value(merged) {
            Ok(patched) => patched,
            Err(error) => {
                return Ok(refused(
                    patch_secrets.scrub(&format!("Patched config is invalid for OpenPRX: {error}")),
                ));
            }
        };
        patched.workspace_dir = desired.config.workspace_dir.clone();
        patched.config_path = desired.config.config_path.clone();
        // The model capability tables decide what a model is allowed to do (and
        // what schema a tool is enforced against); an agent tool may not
        // grant capabilities to itself. The resulting tables are compared, so
        // an addition, a change, and a deletion through `null` on the table
        // or on any parent key are all refused.
        if patched.providers.models != desired.config.providers.models
            || patched.providers.tool_dialects != desired.config.providers.tool_dialects
        {
            return Ok(refused(
                "config.patch does not edit `[[providers.models]]` or `[[providers.tool_dialects]]`: they declare \
                 what a model can do. Edit them in the configuration file; nothing was written."
                    .to_string(),
            ));
        }
        // A model never changes the decision plane: its backend, remote
        // processing, policy, thresholds or exploration (CDP 15, CDP-04).
        // The resulting section is compared, so a change to any key, and a
        // deletion through `null` on it or a parent, are all refused.
        if patched.causal_tree != desired.config.causal_tree {
            return Ok(refused(
                "config.patch does not change `[causal_tree]`, the decision plane: its backend, remote processing, \
                 policy and thresholds are set by the operator with `prx causal config`. Nothing was written."
                    .to_string(),
            ));
        }
        // Every selection the patch affects is resolved by the transaction as
        // a durable selection made by a model (D-018 Q1, D-034): a model the
        // evidence rules out, or one it does not verify, is refused before
        // anything is written.
        crate::providers::selection::trim_changed_model_names(desired.config.as_ref(), &mut patched);

        let config_path = patched.config_path.display().to_string();
        let transactions = ConfigTransactions::new(Arc::clone(&self.config));
        let base_revision = ConfigTransactions::base_revision(&desired);
        let result = match transactions.plan(ConfigChange::Structured(Box::new(patched))).await {
            Ok(plan) => {
                transactions
                    .apply(
                        plan,
                        ConfigActor::AgentConfigPatch,
                        ApplyOptions {
                            expected_revision: base_revision.as_deref(),
                            confirm_high_risk,
                            confirmed_revision,
                        },
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                // A high-risk patch is confirmed only together with the
                // revision the refusal names (D-049 B): say which one.
                let hint = match &error {
                    crate::config::manage::ConfigTransactionError::ConfirmationRequired {
                        required: "confirm_high_risk" | "if_match",
                        revision: Some(revision),
                        ..
                    } => format!(
                        " Only if the operator asked for exactly this change, repeat the call with \
                         confirm_high_risk=true and revision=\"{revision}\"."
                    ),
                    crate::config::manage::ConfigTransactionError::Invalid(_) => "; nothing was written.".to_string(),
                    _ => String::new(),
                };
                return Ok(refused(patch_secrets.scrub(&format!("{error}{hint}"))));
            }
        };

        let changed = outcome
            .summary
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>();
        let mut output = json!({
            "status": "patched",
            "config_path": config_path,
            "activation": outcome.report.status(),
            "active_generation": outcome.report.active_generation.0,
            "restart_required": outcome.report.restart_required,
            "changed": changed,
            "risk": outcome.summary.risk,
            "record_id": outcome.record_id,
        });
        // A removed key in the patch is dropped like a file load drops it;
        // the result says so instead of staying silent (B2f).
        let warnings = crate::config::deprecations::removed_keys_warning(patch)
            .into_iter()
            .chain(outcome.warnings)
            .collect::<Vec<_>>();
        if let (false, Some(object)) = (warnings.is_empty(), output.as_object_mut()) {
            object.insert("warning".to_string(), serde_json::Value::String(warnings.join(" ")));
        }

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
         'restart' (send SIGHUP for graceful restart). \
         'config.patch' and 'restart' are available only to the local operator in the terminal UI; \
         they are refused for remote channels, gateway/webhook, console and external MCP callers. \
         'config.patch' never changes `[causal_tree]` (the decision plane: its backend, remote processing, \
         policy and thresholds are the operator's)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::with_action_requirements(
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["config.get", "config.patch", "status", "version", "components", "restart"],
                        "description": "Action to perform: 'config.get' reads the current config, 'config.patch' merges a JSON merge patch into it, 'status' reports uptime/model/channels, 'version' reports the build version, 'components' lists active channels/providers/memory/tools, 'restart' sends SIGHUP for a graceful restart."
                    },
                    "patch": {
                        "type": "object",
                        "description": "JSON merge patch payload, required for action='config.patch'."
                    },
                    "confirm_high_risk": {
                        "type": "boolean",
                        "description": "For action='config.patch': confirms a high-risk patch: one that writes a credential, turns secrets.encrypt off, re-points an endpoint that receives a credential (a provider, proxy, MCP server, node, channel server, memory database), changes an MCP server's environment or launch command, changes who is admitted (paired tokens, webhook or channel verify secrets, pairing, JWT or SPIFFE trust, sender allow-lists, identity bindings, tunnel exposure), widens what a principal may do (privilege_widened: user policies, autonomy scopes or level, workspace or path limits, exposed MCP tools, node commands, the custom tunnel command, the sandbox runtime, its network, a writable root filesystem, a mounted workspace or wider workspace roots, fetchable domains, operator tools offered to channels, the audit log's size or location), or any change while the stored configuration does not load (baseline_unavailable). Such a patch is refused without it; set it only when the operator asked for exactly that change, together with 'revision'."
                    },
                    "revision": {
                        "type": "string",
                        "description": "For action='config.patch' with confirm_high_risk: the revision the refusal of this exact patch reported. The confirmation is bound to it; without it, or after the configuration changed, the patch is refused again."
                    }
                },
                "required": ["action"]
            }),
            "action",
            &[crate::tools::schema::ActionRequirement {
                action: "config.patch",
                required: &["patch"],
            }],
        )
    }

    #[allow(unsafe_code)]
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

        // Destructive actions are gated twice, and the two gates answer
        // different questions.
        //
        // Gate 1 (here) — *who asked*. `_prx_scope_trusted` is injected by
        // `tools::execution::normalize_arguments` from
        // `ToolExecutionContext::is_local_operator`, and is true only for a turn
        // driven by the human at this machine's terminal UI. A remote IM user, a
        // gateway/webhook client, the console UI, cron and every spawned worker
        // get `false`; the MCP HTTP surface never gets the key at all, because
        // `gateway::compat::inject_trusted_scope` strips the `_prx_` prefix and
        // re-injects only `_zc_` scope. The value can never come from the
        // caller: `strip_runtime_only_args` drops any caller-supplied copy
        // before the runtime writes the authoritative one.
        //
        // Gate 2 (below) — *is it approved*. `SideEffectGate` demands a runtime
        // approval grant, but only at `AutonomyLevel::Supervised`; at the
        // default `Full` it passes medium-risk operations unconditionally
        // (`SecurityPolicy::decide` returns `Allow`, so no approval is ever even
        // requested). Gate 1 is therefore the *only* thing separating a remote
        // WhatsApp message from a daemon restart on a default install — do not
        // remove it on the assumption that gate 2 covers it.
        if action == "config.patch" || action == "restart" {
            let trusted = args
                .get("_prx_scope_trusted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !trusted {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Destructive gateway action '{action}' is restricted to the local operator \
                         (the terminal UI session); it is not available to remote channels, \
                         the gateway/webhook surface, the console, or external MCP clients."
                    )),
                });
            }

            let operation_name = format!("gateway:{action}");
            let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
            if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
                self.name(),
                &operation_name,
                ResourceRiskLevel::Medium,
                approval_grant.as_ref(),
            ) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(error),
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
                            error: Some("Invalid 'patch': expected an object for action 'config.patch'".to_string()),
                        });
                    }
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'patch' parameter for action 'config.patch'".to_string()),
                        });
                    }
                };
                let confirm_high_risk = args
                    .get("confirm_high_risk")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let revision = args
                    .get("revision")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|revision| !revision.is_empty());
                self.apply_config_patch(patch, confirm_high_risk, revision).await
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
                    if gw.require_pairing { "required" } else { "disabled" },
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
                    "Unknown action '{other}'. Use: config.get, config.patch, status, version, components, restart."
                )),
            }),
        }
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
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> GatewayTool {
        let config = new_shared(Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        });
        GatewayTool::new(config, "anthropic", "claude-sonnet-4-6", vec!["signal".to_string()]).with_tools_count(12)
    }

    /// The shared action contract every action-dispatched tool is held to:
    /// the tool-list description and the `action` parameter's own description
    /// must each name every enumerated action, and no parameter may ship
    /// undescribed.
    #[test]
    fn gateway_action_tool_contract() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        crate::tools::schema::action_contract::assert_action_tool_contract(
            tool.name(),
            tool.description(),
            &tool.parameters_schema(),
        );
    }

    #[test]
    fn name_and_description() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        assert_eq!(tool.name(), "gateway");
        assert!(!tool.description().is_empty(), "expected a non-empty value");
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
        assert!(result.error.unwrap_or_default().contains("Missing 'action'"));
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
        let tool = GatewayTool::new(shared, "anthropic", "claude-sonnet-4-6", vec![]).with_security(Arc::new(
            SecurityPolicy {
                autonomy: crate::security::AutonomyLevel::Supervised,
                ..SecurityPolicy::default()
            },
        ));

        let result = tool
            .execute(json!({
                "action": "config.patch",
                "_prx_scope_trusted": true,
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                    serde_json::to_value(ApprovalGrant::for_resource_operation(
                        "gateway",
                        "gateway:config.patch",
                        "test",
                        None
                    )).unwrap(),
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

    /// D-074 P1-1: `config.patch` rebuilds the configuration from JSON; the
    /// canonical causal settings survive the round trip, and the
    /// saved configuration keeps `schema_version`.
    #[tokio::test]
    async fn config_patch_keeps_the_causal_settings_and_the_version() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: config_path.clone(),
            ..Config::default()
        };
        cfg.causal_tree.policy.max_candidates = 5;
        cfg.causal_tree.scoring.weights.w_cost = Some(0.4);

        cfg.save().await.unwrap();
        let tool = GatewayTool::new(new_shared(cfg), "anthropic", "claude-sonnet-4-6", vec![]).with_security(Arc::new(
            SecurityPolicy {
                autonomy: crate::security::AutonomyLevel::Supervised,
                ..SecurityPolicy::default()
            },
        ));

        let result = run_patch(&tool, json!({"gateway": {"port": 3301}})).await;
        assert!(result.success, "{:?}", result.error);

        let causal = tool.config.desired().config.causal_tree.clone();
        assert_eq!(causal.scoring.weights.w_cost, Some(0.4));
        let reloaded = Config::load_from_path(&config_path, tmp.path().join("workspace")).unwrap();
        assert_eq!(reloaded.causal_tree, causal);
        assert_eq!(reloaded.causal_tree.schema_version, 1);
        let mut stored = tokio::fs::read_to_string(&config_path).await.unwrap();
        if let Ok(entries) = std::fs::read_dir(tmp.path().join("config.d")) {
            for entry in entries.flatten() {
                stored.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        assert!(stored.contains("schema_version = 1"), "{stored}");
    }

    /// CDP-04 (V6.07 `config.patch` half, SP-CDP-A 5.4): a model never
    /// changes the decision plane. Each change to the resulting
    /// `[causal_tree]`, including enabling remote processing, a threshold,
    /// exploration and a deletion through `null`, is refused with nothing
    /// written, even when confirmed.
    #[tokio::test]
    async fn config_patch_refuses_every_causal_tree_change() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: config_path.clone(),
            ..Config::default()
        };
        cfg.causal_tree.enabled = false;
        cfg.causal_tree.policy.max_candidates = 5;

        cfg.save().await.unwrap();
        let tool = GatewayTool::new(new_shared(cfg), "anthropic", "claude-sonnet-4-6", vec![]).with_security(Arc::new(
            SecurityPolicy {
                autonomy: crate::security::AutonomyLevel::Supervised,
                ..SecurityPolicy::default()
            },
        ));
        let before = tokio::fs::read_to_string(&config_path).await.unwrap();
        for patch in [
            json!({"causal_tree": {"backend": {"remote_enabled": true}}}),
            json!({"causal_tree": {"enabled": true}}),
            json!({"causal_tree": {"policy": {"commit_threshold": 0.1}}}),
            json!({"causal_tree": {"evaluation": {"exploration": "read_only", "exploration_rate": 0.05}}}),
            json!({"causal_tree": {"policy": null}}),
            json!({"causal_tree": null}),
        ] {
            let result = run_patch(&tool, patch.clone()).await;
            assert!(!result.success, "{patch} was written");
            let error = result.error.unwrap_or_default();
            assert!(
                error.contains("[causal_tree]") && error.contains("prx causal config"),
                "{error}"
            );
            assert_eq!(
                tokio::fs::read_to_string(&config_path).await.unwrap(),
                before,
                "{patch}"
            );
        }
        let result = run_patch(&tool, json!({"gateway": {"port": 3302}})).await;
        assert!(result.success, "other settings still patch: {:?}", result.error);
    }

    // ── PMM-C-10: config.patch goes through the resolver ─────────────

    async fn patch_fixture(tmp: &TempDir) -> (GatewayTool, std::path::PathBuf) {
        let config_path = tmp.path().join("config.toml");
        let mut cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: config_path.clone(),
            default_provider: Some("openrouter".to_string()),
            default_model: Some("vendor/chat-model".to_string()),
            api_key: Some("PMMC-PATCH-ROOT".to_string()),
            ..Config::default()
        };
        for (model, operations) in [
            ("jev-decision-test", "decision_endpoint"),
            ("vendor/chat-model", "chat"),
            ("vendor/declared", "chat"),
        ] {
            cfg.providers.models.push(
                toml::from_str(&format!(
                    "provider = \"openrouter\"\nmodel = \"{model}\"\noperations = [\"{operations}\"]\n"
                ))
                .unwrap(),
            );
        }
        cfg.save().await.unwrap();
        let tool = GatewayTool::new(new_shared(cfg), "openrouter", "vendor/chat-model", vec![]).with_security(
            Arc::new(SecurityPolicy {
                autonomy: crate::security::AutonomyLevel::Supervised,
                ..SecurityPolicy::default()
            }),
        );
        (tool, config_path)
    }

    async fn run_patch(tool: &GatewayTool, patch: serde_json::Value) -> ToolResult {
        // A confirmation is bound to the current revision (D-049 B).
        let revision = ConfigTransactions::base_revision(&tool.config.desired());
        tool.execute(json!({
            "action": "config.patch",
            "_prx_scope_trusted": true,
            "confirm_high_risk": true,
            "revision": revision,
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                serde_json::to_value(ApprovalGrant::for_resource_operation(
                    "gateway",
                    "gateway:config.patch",
                    "test",
                    None
                )).unwrap(),
            "patch": patch
        }))
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn config_patch_refuses_a_default_model_that_cannot_chat_or_is_unverified() {
        let tmp = TempDir::new().unwrap();
        let (tool, config_path) = patch_fixture(&tmp).await;
        let before = tokio::fs::read_to_string(&config_path).await.unwrap();

        let result = run_patch(&tool, json!({"default_model": "jev-decision-test"})).await;
        assert!(!result.success, "a decision-only default was written");
        assert!(
            result
                .error
                .clone()
                .unwrap_or_default()
                .contains("cannot be used for chat"),
            "{result:?}"
        );

        let result = run_patch(&tool, json!({"default_model": "vendor/never-seen"})).await;
        assert!(!result.success, "an unverified default was written");
        assert!(
            result.error.clone().unwrap_or_default().contains("not verified"),
            "{result:?}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&config_path).await.unwrap(),
            before,
            "nothing was written"
        );

        let result = run_patch(&tool, json!({"default_model": "vendor/declared"})).await;
        assert!(result.success, "a declared chat model is accepted: {result:?}");
    }

    /// R-PMMCh C-2: a model name config.patch accepts is stored trimmed.
    #[tokio::test]
    async fn config_patch_stores_model_names_trimmed() {
        let tmp = TempDir::new().unwrap();
        let (tool, config_path) = patch_fixture(&tmp).await;
        let result = run_patch(
            &tool,
            json!({
                "default_model": "  vendor/declared ",
                "model_routes": [{ "hint": "fast", "provider": "openrouter", "model": " vendor/declared\t" }]
            }),
        )
        .await;
        assert!(result.success, "{result:?}");
        let parsed = Config::load_from_path(&config_path, tmp.path().join("workspace")).expect("test: reload");
        assert_eq!(parsed.default_model.as_deref(), Some("vendor/declared"));
        assert_eq!(
            parsed
                .model_routes
                .iter()
                .map(|route| route.model.as_str())
                .collect::<Vec<_>>(),
            vec!["vendor/declared"]
        );
    }

    #[tokio::test]
    async fn config_patch_refuses_edits_to_the_capability_tables() {
        let tmp = TempDir::new().unwrap();
        let (tool, config_path) = patch_fixture(&tmp).await;
        let before = tokio::fs::read_to_string(&config_path).await.unwrap();
        for patch in [
            json!({"providers": {"models": [{"provider": "openrouter", "model": "x", "operations": ["chat"]}]}}),
            json!({"providers": {"tool_dialects": [{"provider": "vllm", "tool_schema": "no_root_combinators"}]}}),
            // R-PMMC F-2: deleting the tables through `null` on the table or
            // on its parent is an edit too.
            json!({"providers": null}),
            json!({"providers": {"models": null}}),
        ] {
            let result = run_patch(&tool, patch).await;
            assert!(!result.success, "a capability table was edited through config.patch");
            let error = result.error.unwrap_or_default();
            assert!(
                error.contains("providers.models") && error.contains("providers.tool_dialects"),
                "{error}"
            );
        }
        assert_eq!(tokio::fs::read_to_string(&config_path).await.unwrap(), before);
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
        // `make_tool` leaves autonomy at the default `Full`, where
        // `SideEffectGate` waves medium-risk operations through. So this denial
        // is produced by gate 1 alone — which is exactly the property that makes
        // the local-operator marker load-bearing rather than decorative.
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("restricted to the local operator"),
            "gate 1 must deny a non-operator config.patch at the default autonomy level"
        );
    }

    /// Pins the fact that motivates gate 1 (audit G12): at the default
    /// `AutonomyLevel::Full` the second gate authorizes medium-risk resource
    /// operations unconditionally, with no approval grant present. Deleting gate
    /// 1 and "relying on SideEffectGate" would therefore hand `config.patch` /
    /// `restart` to every remote channel. If this test ever starts failing
    /// because `Full` grew a real check, gate 1 can be revisited — not before.
    #[tokio::test]
    async fn second_gate_does_not_block_at_default_autonomy() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: config_path.clone(),
            ..Config::default()
        };
        cfg.save().await.unwrap();
        let tool = GatewayTool::new(new_shared(cfg), "anthropic", "claude-sonnet-4-6", vec![])
            .with_security(Arc::new(SecurityPolicy::default()));
        assert_eq!(
            SecurityPolicy::default().autonomy,
            crate::security::AutonomyLevel::Full,
            "this test is only meaningful while Full is the default"
        );

        // No `_zc_approval_grant` anywhere in the arguments.
        let result = tool
            .execute(json!({
                "action": "config.patch",
                "_prx_scope_trusted": true,
                "patch": { "gateway": { "port": 3301 } }
            }))
            .await
            .unwrap();

        assert!(
            result.success,
            "SideEffectGate is a no-op for medium risk at Full: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn config_patch_requires_resource_grant() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: config_path.clone(),
            ..Config::default()
        };
        cfg.save().await.unwrap();
        let shared = new_shared(cfg);
        let tool = GatewayTool::new(shared, "anthropic", "claude-sonnet-4-6", vec![]).with_security(Arc::new(
            SecurityPolicy {
                autonomy: crate::security::AutonomyLevel::Supervised,
                ..SecurityPolicy::default()
            },
        ));

        let result = tool
            .execute(json!({
                "action": "config.patch",
                "_prx_scope_trusted": true,
                "patch": { "gateway": { "port": 3300 } }
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("runtime approval grant"));
        let contents = tokio::fs::read_to_string(config_path).await.unwrap();
        let parsed: Config = toml::from_str(&contents).unwrap();
        assert_ne!(parsed.gateway.port, 3300);
    }

    #[tokio::test]
    async fn restart_rejects_untrusted() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.execute(json!({ "action": "restart" })).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("restricted to the local operator"),
            "a restart from a non-operator scope must be refused"
        );
    }

    fn trusted_patch(patch: serde_json::Value) -> serde_json::Value {
        json!({
            "action": "config.patch",
            "_prx_scope_trusted": true,
            "patch": patch,
        })
    }

    /// A config in the default split layout with a model route (stored in
    /// `config.d/routing.toml`) and a memory setting (`config.d/memory.toml`).
    async fn split_config(tmp: &TempDir) -> Config {
        let mut cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        cfg.model_routes = vec![crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: "openrouter".into(),
            model: "route-model".into(),
            api_key: Some("route-sentinel-credential".into()),
            credential_ref: None,
        }];
        cfg.memory.auto_save = !Config::default().memory.auto_save;
        crate::config::files::write_split_config(&cfg, false).await.unwrap();
        assert!(
            std::fs::read_to_string(tmp.path().join("config.d/routing.toml"))
                .unwrap()
                .contains("route-model"),
            "test precondition: the route lives in a fragment"
        );
        Config::load_from_path(&cfg.config_path, cfg.workspace_dir.clone()).unwrap()
    }

    /// RED before PMM-D (row CONFIG-PATCH): `config.patch` rebuilt the whole
    /// configuration from `config.toml` alone, so saving it reset every
    /// `config.d` fragment (routes, memory, channels) to defaults. After:
    /// the patch applies to the complete configuration and every fragment
    /// setting survives.
    #[tokio::test]
    async fn config_patch_keeps_every_config_d_fragment() {
        let tmp = TempDir::new().unwrap();
        let cfg = split_config(&tmp).await;
        let tool = GatewayTool::new(new_shared(cfg.clone()), "anthropic", "claude-sonnet-4-6", vec![]);

        let result = tool
            .execute(trusted_patch(json!({ "default_temperature": 0.4 })))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);

        let loaded = Config::load_from_path(&cfg.config_path, cfg.workspace_dir.clone()).unwrap();
        assert!((loaded.default_temperature - 0.4).abs() < f64::EPSILON);
        assert_eq!(
            serde_json::to_value(&loaded.model_routes).unwrap(),
            serde_json::to_value(&cfg.model_routes).unwrap(),
            "the routing fragment was lost"
        );
        assert_eq!(
            loaded.memory.auto_save, cfg.memory.auto_save,
            "the memory fragment was lost"
        );
        assert!(!result.output.contains("route-sentinel-credential"));
    }

    /// RED before PMM-D: `config.patch` only wrote the file and left
    /// activation to the file watcher, so the running configuration did not
    /// change when the tool reported success (and never, without a watcher).
    /// After: the patch is active when the tool returns.
    #[tokio::test]
    async fn config_patch_is_active_when_it_returns() {
        let tmp = TempDir::new().unwrap();
        let cfg = split_config(&tmp).await;
        let shared = new_shared(cfg);
        let tool = GatewayTool::new(Arc::clone(&shared), "anthropic", "claude-sonnet-4-6", vec![]);

        let result = tool
            .execute(trusted_patch(json!({ "default_temperature": 0.3 })))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!((shared.load_full().default_temperature - 0.3).abs() < f64::EPSILON);
    }

    /// PMMD-g (N-3): the `changed` list the model reads never carries a
    /// credentialed price key.
    #[tokio::test]
    async fn config_patch_output_never_carries_url_userinfo() {
        let tmp = TempDir::new().unwrap();
        let cfg = split_config(&tmp).await;
        let tool = GatewayTool::new(new_shared(cfg), "anthropic", "claude-sonnet-4-6", vec![]);
        let key = "custom:http://alice:userinfo-sentinel-pw@127.0.0.1:1/v1/model-a";

        let mut args = trusted_patch(json!({ "cost": { "prices": { key: { "input": 1.0, "output": 2.0 } } } }));
        let refused = tool.execute(args.clone()).await.unwrap();
        assert!(!refused.success, "a credentialed price key needs confirmation");
        assert!(!refused.error.unwrap_or_default().contains("userinfo-sentinel-pw"));

        args["confirm_high_risk"] = json!(true);
        args["revision"] = json!(ConfigTransactions::base_revision(&tool.config.desired()));
        let result = tool.execute(args).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(!result.output.contains("userinfo-sentinel-pw"), "{}", result.output);
        assert!(result.output.contains("[redacted]@"), "{}", result.output);
    }

    /// INT-2B a55 (R-C2BUh, D-027): `config.patch` refuses a price key that
    /// collides with a stored credentialed key once redacted, even confirmed
    /// against the current revision. Nothing is written and the error names
    /// only the redacted key. Coverage for working code (no red-before).
    #[tokio::test]
    async fn config_patch_refuses_a_colliding_credentialed_price_key() {
        let tmp = TempDir::new().unwrap();
        let cfg = split_config(&tmp).await;
        let shared = new_shared(cfg);
        let tool = GatewayTool::new(Arc::clone(&shared), "anthropic", "claude-sonnet-4-6", vec![]);
        let stored = "custom:http://alice:userinfo-sentinel-a55@127.0.0.1:1/v1/model-a";
        let mut args = trusted_patch(json!({ "cost": { "prices": { stored: { "input": 1.0, "output": 2.0 } } } }));
        args["confirm_high_risk"] = json!(true);
        args["revision"] = json!(ConfigTransactions::base_revision(&tool.config.desired()));
        let stored_result = tool.execute(args).await.unwrap();
        assert!(stored_result.success, "{:?}", stored_result.error);
        let keys = |shared: &crate::config::SharedConfig| {
            shared
                .desired()
                .config
                .cost
                .prices
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
        };
        let before = keys(&shared);

        let colliding = "custom:http://bob:other-sentinel-a55@127.0.0.1:1/v1/model-a";
        let mut args = trusted_patch(json!({ "cost": { "prices": { colliding: { "input": 9.0, "output": 9.0 } } } }));
        args["confirm_high_risk"] = json!(true);
        args["revision"] = json!(ConfigTransactions::base_revision(&tool.config.desired()));
        let refused = tool.execute(args).await.unwrap();
        assert!(!refused.success, "a colliding price key must be refused");
        let error = refused.error.unwrap_or_default();
        assert!(
            error.contains("custom:http://[redacted]@127.0.0.1:1/v1/model-a"),
            "{error}"
        );
        assert!(
            !error.contains("userinfo-sentinel-a55") && !error.contains("other-sentinel-a55"),
            "{error}"
        );
        assert_eq!(keys(&shared), before, "nothing may be written");
    }

    /// B2f (R-INT2BB2 F-10), RED before: `security.audit.sign_events` in a
    /// patch was dropped silently. After: the result carries the same
    /// removed-key warning a file load logs, and nothing is stored for it.
    #[tokio::test]
    async fn config_patch_warns_about_a_removed_key() {
        let tmp = TempDir::new().unwrap();
        let cfg = split_config(&tmp).await;
        let tool = GatewayTool::new(new_shared(cfg), "anthropic", "claude-sonnet-4-6", vec![]);
        let result = tool
            .execute(trusted_patch(
                json!({ "security": { "audit": { "sign_events": true } }, "default_temperature": 0.4 }),
            ))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(
            result.output.contains("security.audit.sign_events"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("Ignoring removed configuration key"),
            "{}",
            result.output
        );
        let stored = std::fs::read_dir(tmp.path())
            .unwrap()
            .chain(std::fs::read_dir(tmp.path().join("config.d")).into_iter().flatten())
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "toml"))
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect::<String>();
        assert!(!stored.contains("sign_events"), "{stored}");
    }

    /// D-049 B, RED before SECRETS-h: `confirm_high_risk` alone applied a
    /// high-risk patch. After: the confirmation is bound to the revision the
    /// refusal names; without it the patch is refused again, naming the
    /// revision to send, and nothing is written. The schema has `revision`.
    #[tokio::test]
    async fn config_patch_confirmation_is_bound_to_a_revision() {
        let tmp = TempDir::new().unwrap();
        let cfg = split_config(&tmp).await;
        let shared = new_shared(cfg);
        let tool = GatewayTool::new(Arc::clone(&shared), "anthropic", "claude-sonnet-4-6", vec![]);

        let mut args = trusted_patch(json!({ "api_key": "sentinel-patched-root" }));
        args["confirm_high_risk"] = json!(true);
        let refused = tool.execute(args.clone()).await.unwrap();
        assert!(!refused.success, "an unbound confirmation must be refused");
        let error = refused.error.unwrap_or_default();
        assert!(error.contains("if_match"), "{error}");
        assert!(
            error.contains("revision=\""),
            "the refusal names the revision to send: {error}"
        );
        assert!(!error.contains("sentinel-patched-root"), "{error}");
        assert_ne!(
            shared.desired().config.api_key.as_deref(),
            Some("sentinel-patched-root")
        );

        args["revision"] = json!(ConfigTransactions::base_revision(&shared.desired()));
        let result = tool.execute(args).await.unwrap();
        assert!(result.success, "{:?}", result.error);

        let schema = tool.parameters_schema().to_string();
        assert!(schema.contains("\"revision\""), "{schema}");
    }

    /// RED before PMME-g (D-088 C-1): renaming a route's hint while moving it
    /// to another provider kept its `api_key` and handed it to the new
    /// vendor, and `config.patch` applied it without any confirmation.
    #[tokio::test]
    async fn config_patch_cannot_move_a_route_key_to_another_provider_by_renaming_the_route() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        cfg.model_routes = vec![crate::config::ModelRouteConfig {
            hint: "fast".into(),
            provider: "openrouter".into(),
            model: "vendor/route-model".into(),
            api_key: Some("sentinel-rename-route-key".into()),
            credential_ref: None,
        }];
        for (provider, model) in [("openrouter", "vendor/route-model"), ("anthropic", "claude-sonnet-4-5")] {
            cfg.providers.models.push(
                toml::from_str(&format!(
                    "provider = \"{provider}\"\nmodel = \"{model}\"\noperations = [\"chat\"]\n"
                ))
                .unwrap(),
            );
        }
        cfg.save().await.unwrap();
        let loaded = Config::load_from_path(&cfg.config_path, cfg.workspace_dir.clone()).unwrap();
        let shared = new_shared(loaded);
        let tool = GatewayTool::new(Arc::clone(&shared), "openrouter", "vendor/route-model", vec![]);
        let before = tokio::fs::read_to_string(&cfg.config_path).await.unwrap();

        let result = tool
            .execute(trusted_patch(json!({
                "model_routes": [{
                    "hint": "fast2",
                    "provider": "anthropic",
                    "model": "claude-sonnet-4-5",
                    "api_key": "sentinel-rename-route-key"
                }]
            })))
            .await
            .unwrap();
        assert!(
            !result.success,
            "the route key changed providers unconfirmed: {result:?}"
        );
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("sends a stored credential to a different provider"),
            "{error}"
        );
        assert!(!error.contains("sentinel-rename-route-key"), "{error}");
        assert_eq!(tokio::fs::read_to_string(&cfg.config_path).await.unwrap(), before);
    }

    /// D-087 (5): `config.patch` gets the PMME-f classification too:
    /// switching the default provider away from the root key is high risk
    /// (the refusal names `sends a stored credential to a different
    /// provider`), nothing is written before the confirmation bound to the
    /// revision, and the confirmed patch applies. The new default is
    /// declared, so the strict (model-driven) selection check passes and
    /// the refusal is the risk alone.
    #[tokio::test]
    async fn config_patch_switching_the_default_away_from_the_root_key_is_high_risk() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            api_key: Some("sentinel-root-owner-key".into()),
            ..Config::default()
        };
        cfg.providers.models = vec![crate::config::schema::ProviderModelConfig {
            provider: "anthropic".into(),
            model: Some("claude-sonnet-4-5".into()),
            operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
            native_tools: None,
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: None,
        }];
        cfg.save().await.unwrap();
        let cfg = Config::load_from_path(&cfg.config_path, cfg.workspace_dir.clone()).unwrap();
        let shared = new_shared(cfg);
        let tool = GatewayTool::new(Arc::clone(&shared), "anthropic", "claude-sonnet-4-6", vec![]);
        let before = std::fs::read_to_string(tmp.path().join("config.toml")).unwrap();

        let mut args = trusted_patch(json!({ "default_provider": "anthropic", "default_model": "claude-sonnet-4-5" }));
        let refused = tool.execute(args.clone()).await.unwrap();
        assert!(!refused.success, "the root key changed hands unconfirmed");
        let error = refused.error.unwrap_or_default();
        assert!(
            error.contains("sends a stored credential to a different provider"),
            "{error}"
        );
        assert!(!error.contains("sentinel-root-owner-key"), "{error}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("config.toml")).unwrap(),
            before,
            "nothing was written"
        );

        args["confirm_high_risk"] = json!(true);
        args["revision"] = json!(ConfigTransactions::base_revision(&shared.desired()));
        let result = tool.execute(args).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(shared.desired().config.default_provider.as_deref(), Some("anthropic"));
    }

    /// D-049 F, RED before SECRETS-h: the `confirm_high_risk` description
    /// named only credentials, encryption and provider endpoints. After it
    /// names every high-risk reason the plan gives.
    #[test]
    fn config_patch_description_names_every_high_risk_reason() {
        let tool = GatewayTool::new(new_shared(Config::default()), "anthropic", "claude-sonnet-4-6", vec![]);
        let schema = tool.parameters_schema().to_string();
        for reason in [
            "who is admitted",
            "environment",
            "memory database",
            "proxy",
            "privilege_widened",
            "baseline_unavailable",
            "sandbox runtime",
            "writable root filesystem",
            "mounted workspace",
            "fetchable domains",
            "operator tools offered to channels",
            "audit log",
        ] {
            assert!(schema.contains(reason), "the description must name {reason}: {schema}");
        }
    }
}
