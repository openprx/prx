use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::manage::{
    ApplyOptions, ConfigActor, ConfigChange, ConfigChangeRisk, ConfigChangeSummary, ConfigTransactions,
};
use crate::config::{Config, ProxyConfig, ProxyScope, SharedConfig, runtime_proxy_config};
use crate::security::SecurityPolicy;
use crate::security::SideEffectGate;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::util::MaybeSet;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::fs;
use std::sync::Arc;

/// A `set` built from one desired-configuration snapshot: the snapshot, the
/// configuration the change produces, and the snapshot's revision.
struct PreparedSet {
    current: Config,
    updated: Config,
    revision: Option<String>,
}

pub struct ProxyConfigTool {
    config: SharedConfig,
    security: Arc<SecurityPolicy>,
}

impl ProxyConfigTool {
    pub const fn new(config: SharedConfig, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn load_config_without_env(&self) -> anyhow::Result<Config> {
        let current = self.config.load_full();
        let contents = fs::read_to_string(&current.config_path).map_err(|error| {
            anyhow::anyhow!("Failed to read config file {}: {error}", current.config_path.display())
        })?;

        let mut parsed: Config = toml::from_str(&contents).map_err(|error| {
            anyhow::anyhow!("Failed to parse config file {}: {error}", current.config_path.display())
        })?;
        parsed.config_path = current.config_path.clone();
        parsed.workspace_dir = current.workspace_dir.clone();
        Ok(parsed)
    }

    fn require_write_access(&self) -> Option<ToolResult> {
        if !self.security.can_act() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        None
    }

    fn authorize_resource_operation(
        &self,
        operation_name: &str,
        risk: ResourceRiskLevel,
        approval_grant: Option<&ApprovalGrant>,
    ) -> Option<ToolResult> {
        SideEffectGate::new(self.security.as_ref())
            .authorize_resource_operation(self.name(), operation_name, risk, approval_grant)
            .err()
            .map(|error| ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            })
    }

    fn parse_scope(raw: &str) -> Option<ProxyScope> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "environment" | "env" => Some(ProxyScope::Environment),
            "prx" | "internal" | "core" => Some(ProxyScope::Zeroclaw),
            "services" | "service" => Some(ProxyScope::Services),
            _ => None,
        }
    }

    fn parse_string_list(raw: &Value, field: &str) -> anyhow::Result<Vec<String>> {
        if let Some(raw_string) = raw.as_str() {
            return Ok(raw_string
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(ToOwned::to_owned)
                .collect());
        }

        if let Some(array) = raw.as_array() {
            let mut out = Vec::new();
            for item in array {
                let value = item
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("'{field}' array must only contain strings"))?;
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
            return Ok(out);
        }

        anyhow::bail!("'{field}' must be a string or string[]")
    }

    fn parse_optional_string_update(args: &Value, field: &str) -> anyhow::Result<MaybeSet<String>> {
        let Some(raw) = args.get(field) else {
            return Ok(MaybeSet::Unset);
        };

        if raw.is_null() {
            return Ok(MaybeSet::Null);
        }

        let value = raw
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'{field}' must be a string or null"))?
            .trim()
            .to_string();

        let output = if value.is_empty() {
            MaybeSet::Null
        } else {
            MaybeSet::Set(value)
        };
        Ok(output)
    }

    /// A proxy URL as the model may see it: its `user:password@` replaced,
    /// the host kept (`secret_paths::ENDPOINT_PATHS` lists the proxy URLs).
    fn shown_url(url: Option<&str>) -> Option<String> {
        url.map(|url| crate::providers::redact_endpoint_userinfo(url).into_owned())
    }

    fn env_snapshot() -> Value {
        let var = |name: &str| Self::shown_url(std::env::var(name).ok().as_deref());
        json!({
            "HTTP_PROXY": var("HTTP_PROXY"),
            "HTTPS_PROXY": var("HTTPS_PROXY"),
            "ALL_PROXY": var("ALL_PROXY"),
            "NO_PROXY": std::env::var("NO_PROXY").ok(),
        })
    }

    fn proxy_json(proxy: &ProxyConfig) -> Value {
        json!({
            "configured": proxy.has_any_proxy_url(),
            "scope": proxy.scope,
            "http_proxy": Self::shown_url(proxy.http_proxy.as_deref()),
            "https_proxy": Self::shown_url(proxy.https_proxy.as_deref()),
            "all_proxy": Self::shown_url(proxy.all_proxy.as_deref()),
            "no_proxy": proxy.normalized_no_proxy(),
            "services": proxy.normalized_services(),
        })
    }

    fn handle_get(&self) -> anyhow::Result<ToolResult> {
        let file_proxy = self.load_config_without_env()?.proxy;
        let runtime_proxy = runtime_proxy_config();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "proxy": Self::proxy_json(&file_proxy),
                "runtime_proxy": Self::proxy_json(&runtime_proxy),
                "environment": Self::env_snapshot(),
            }))?,
            error: None,
        })
    }

    fn handle_list_services(&self) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "supported_service_keys": ProxyConfig::supported_service_keys(),
                "supported_selectors": ProxyConfig::supported_service_selectors(),
                "usage_example": {
                    "action": "set",
                    "scope": "services",
                    "services": ["provider.openai", "tool.http_request", "channel.telegram"]
                }
            }))?,
            error: None,
        })
    }

    /// The configuration `set` changes (the complete desired configuration,
    /// every `config.d` fragment and decrypted secret included) and the one
    /// it would store. Nothing is written: the change is classified first
    /// (D-049 C) and then saved as a configuration transaction (D-050).
    fn prepare_set(&self, args: &Value) -> anyhow::Result<PreparedSet> {
        // The snapshot and its revision are read together (D-051): the save
        // is bound to exactly the configuration the change was built on, so
        // a write that lands in between is a conflict, never silently undone.
        let desired = self.config.desired();
        let current = desired.config.as_ref().clone();
        let revision = ConfigTransactions::base_revision(&desired);
        let mut cfg = current.clone();
        let mut proxy = cfg.proxy.clone();

        if let Some(scope_raw) = args.get("scope") {
            let scope = scope_raw
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("'scope' must be a string"))?;
            proxy.scope = Self::parse_scope(scope)
                .ok_or_else(|| anyhow::anyhow!("Invalid scope '{scope}'. Use environment|prx|services"))?;
        }

        match Self::parse_optional_string_update(args, "http_proxy")? {
            MaybeSet::Set(update) => {
                proxy.http_proxy = Some(update);
            }
            MaybeSet::Null => {
                proxy.http_proxy = None;
            }
            MaybeSet::Unset => {}
        }

        match Self::parse_optional_string_update(args, "https_proxy")? {
            MaybeSet::Set(update) => {
                proxy.https_proxy = Some(update);
            }
            MaybeSet::Null => {
                proxy.https_proxy = None;
            }
            MaybeSet::Unset => {}
        }

        match Self::parse_optional_string_update(args, "all_proxy")? {
            MaybeSet::Set(update) => {
                proxy.all_proxy = Some(update);
            }
            MaybeSet::Null => {
                proxy.all_proxy = None;
            }
            MaybeSet::Unset => {}
        }

        if let Some(no_proxy_raw) = args.get("no_proxy") {
            proxy.no_proxy = Self::parse_string_list(no_proxy_raw, "no_proxy")?;
        }

        if let Some(services_raw) = args.get("services") {
            proxy.services = Self::parse_string_list(services_raw, "services")?;
        }

        proxy.no_proxy = proxy.normalized_no_proxy();
        proxy.services = proxy.normalized_services();
        proxy.validate()?;

        cfg.proxy = proxy;
        Ok(PreparedSet {
            current,
            updated: cfg,
            revision,
        })
    }

    /// The approval tier of a `set`: the configuration plan's risk decides
    /// it, through the same classifier every configuration write uses
    /// (`config::manage::plan`). A proxy URL is an endpoint every
    /// credentialed request goes through, so re-pointing one (or adding
    /// userinfo) is high risk; a scope, service or `no_proxy` change is not.
    fn set_risk(current: &Config, updated: &Config) -> anyhow::Result<(ResourceRiskLevel, ConfigChangeSummary)> {
        let summary = ConfigChangeSummary::between(Some(current), updated)?;
        let risk = if summary.risk == ConfigChangeRisk::High {
            ResourceRiskLevel::High
        } else {
            ResourceRiskLevel::Medium
        };
        Ok((risk, summary))
    }

    async fn handle_set(&self, prepared: &PreparedSet, summary: &ConfigChangeSummary) -> anyhow::Result<ToolResult> {
        let (current, cfg) = (&prepared.current, &prepared.updated);
        let previous_scope = current.proxy.scope;
        let proxy = cfg.proxy.clone();
        // Saved through the transaction layer: history record, revision check
        // against the configuration the change was built on, compensating
        // restore if the runtime refuses it (D-050). The approval gate has
        // already approved the plan's risk; it confirms exactly the reasons
        // it classified, bound to that revision. A plan with more (the files
        // drifted from the desired configuration) is refused.
        let transactions = ConfigTransactions::new(Arc::clone(&self.config));
        let base_revision = prepared.revision.clone();
        let plan = transactions
            .plan(ConfigChange::Structured(Box::new(cfg.clone())))
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let confirmed = plan
            .summary()
            .risk_reasons
            .iter()
            .all(|reason| summary.risk_reasons.contains(reason));
        let outcome = transactions
            .apply(
                plan,
                ConfigActor::AgentProxyConfig,
                ApplyOptions {
                    expected_revision: base_revision.as_deref(),
                    confirm_high_risk: confirmed,
                    confirmed_revision: base_revision.as_deref(),
                },
            )
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let report = &outcome.report;
        let active_proxy = self.config.load_full().proxy.clone();

        if report.rebuilt.iter().any(|field| field == "proxy")
            && proxy.has_any_proxy_url()
            && proxy.scope == ProxyScope::Environment
        {
            proxy.apply_to_process_env();
        } else if report.rebuilt.iter().any(|field| field == "proxy") && previous_scope == ProxyScope::Environment {
            ProxyConfig::clear_process_env();
        }

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "message": if report.restart_required.iter().any(|field| field == "proxy") {
                    "Proxy configuration saved; process restart required before runtime activation"
                } else {
                    "Proxy configuration updated and activated"
                },
                "status": report.status(),
                "active_generation": report.active_generation.0,
                "restart_required": report.restart_required,
                "desired_proxy": Self::proxy_json(&proxy),
                "active_runtime_proxy": Self::proxy_json(&active_proxy),
                "environment": Self::env_snapshot(),
                // The plan's risk, which set the approval tier; reasons carry
                // paths only.
                "risk": summary.risk,
                "risk_reasons": summary.risk_reasons,
                "record_id": outcome.record_id,
            }))?,
            error: None,
        })
    }

    fn handle_apply_env(&self) -> anyhow::Result<ToolResult> {
        let proxy = self.config.load_full().proxy.clone();
        proxy.validate()?;

        if !proxy.has_any_proxy_url() {
            anyhow::bail!("Proxy is not configured. Use action 'set' with a proxy URL first");
        }

        if proxy.scope != ProxyScope::Environment {
            anyhow::bail!(
                "apply_env only works when proxy.scope is 'environment' (current: {:?})",
                proxy.scope
            );
        }

        proxy.apply_to_process_env();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "message": "Proxy environment variables applied",
                "proxy": Self::proxy_json(&proxy),
                "environment": Self::env_snapshot(),
            }))?,
            error: None,
        })
    }

    fn handle_clear_env(&self) -> anyhow::Result<ToolResult> {
        ProxyConfig::clear_process_env();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "message": "Proxy environment variables cleared",
                "environment": Self::env_snapshot(),
            }))?,
            error: None,
        })
    }
}

#[async_trait]
impl Tool for ProxyConfigTool {
    fn name(&self) -> &str {
        "proxy_config"
    }

    fn description(&self) -> &str {
        "Manage ZeroClaw proxy settings (scope: environment | prx | services): 'get' reads the effective settings, 'set' writes them, 'list_services' lists the routable service selectors, 'apply_env' applies the proxy to the process environment, and 'clear_env' removes it"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["get", "set", "list_services", "apply_env", "clear_env"],
                    "default": "get",
                    "description": "Action to perform: 'get' reads the effective proxy settings, 'set' writes them, 'list_services' lists the routable service selectors, 'apply_env' applies the proxy to the process environment, 'clear_env' removes it. Defaults to 'get'."
                },
                "scope": {
                    "type": "string",
                    "enum": ["environment", "env", "prx", "internal", "core", "services", "service"],
                    "description": "Proxy scope. Canonical values: environment, prx, services."
                },
                "http_proxy": {
                    "type": "string",
                    "description": "HTTP proxy URL"
                },
                "https_proxy": {
                    "type": "string",
                    "description": "HTTPS proxy URL"
                },
                "all_proxy": {
                    "type": "string",
                    "description": "Fallback proxy URL for all protocols"
                },
                "no_proxy": {
                    "description": "Comma-separated string or array of NO_PROXY entries",
                    "oneOf": [
                        {"type": "string"},
                        {"type": "array", "items": {"type": "string"}}
                    ]
                },
                "services": {
                    "description": "Comma-separated string or array of service selectors; required when 'set' targets scope=services",
                    "oneOf": [
                        {"type": "string", "minLength": 1},
                        {"type": "array", "items": {"type": "string"}, "minItems": 1}
                    ]
                }
            },
            "allOf": [{
                "if": {
                    "properties": {
                        "action": {"const": "set"},
                        "scope": {"enum": ["services", "service"]}
                    },
                    "required": ["action", "scope"]
                },
                "then": {"required": ["services"]}
            }]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("get")
            .to_ascii_lowercase();
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);

        let result = match action.as_str() {
            "get" => {
                if let Some(blocked) = self.authorize_resource_operation(
                    "proxy_config:show",
                    ResourceRiskLevel::Low,
                    approval_grant.as_ref(),
                ) {
                    return Ok(blocked);
                }
                self.handle_get()
            }
            "list_services" => self.handle_list_services(),
            "set" => {
                if let Some(blocked) = self.require_write_access() {
                    return Ok(blocked);
                }
                // Classify before approval: the plan's risk is the approval
                // tier (D-049 C). At `supervised` a high-risk set needs a
                // runtime grant; at `full` it is approved (D-037).
                match self.prepare_set(&args).and_then(|prepared| {
                    Self::set_risk(&prepared.current, &prepared.updated).map(|risk| (prepared, risk))
                }) {
                    Ok((prepared, (risk, summary))) => {
                        if let Some(blocked) =
                            self.authorize_resource_operation("proxy_config:set", risk, approval_grant.as_ref())
                        {
                            return Ok(blocked);
                        }
                        self.handle_set(&prepared, &summary).await
                    }
                    Err(error) => Err(error),
                }
            }
            "apply_env" | "clear_env" => {
                if let Some(blocked) = self.require_write_access() {
                    return Ok(blocked);
                }

                let operation_name = match action.as_str() {
                    "apply_env" => "proxy_config:set",
                    "clear_env" => "proxy_config:unset",
                    other => anyhow::bail!("Unknown proxy sub-action '{other}'"),
                };
                if let Some(blocked) = self.authorize_resource_operation(
                    operation_name,
                    ResourceRiskLevel::Medium,
                    approval_grant.as_ref(),
                ) {
                    return Ok(blocked);
                }

                match action.as_str() {
                    "apply_env" => self.handle_apply_env(),
                    "clear_env" => self.handle_clear_env(),
                    other => anyhow::bail!("Unknown proxy sub-action '{other}'"),
                }
            }
            _ => anyhow::bail!("Unknown action '{action}'. Valid: get, set, list_services, apply_env, clear_env"),
        };

        match result {
            Ok(outcome) => Ok(outcome),
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
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
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use tempfile::TempDir;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn supervised_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    async fn test_config(tmp: &TempDir) -> SharedConfig {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.save().await.unwrap();
        crate::config::new_shared(config)
    }

    /// The shared action contract every action-dispatched tool is held to:
    /// the tool-list description and the `action` parameter's own description
    /// must each name every enumerated action, and no parameter may ship
    /// undescribed.
    #[test]
    fn proxy_config_action_tool_contract() {
        let tool = ProxyConfigTool::new(crate::config::new_shared(Config::default()), test_security());
        crate::tools::schema::action_contract::assert_action_tool_contract(
            tool.name(),
            tool.description(),
            &tool.parameters_schema(),
        );
    }

    #[tokio::test]
    async fn list_services_action_returns_known_keys() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, test_security());

        let result = tool.execute(json!({"action": "list_services"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("provider.openai"));
        assert!(result.output.contains("tool.http_request"));
    }

    #[tokio::test]
    async fn set_scope_services_requires_services_entries() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, test_security());

        let result = tool
            .execute(json!({
                "action": "set",
                "enabled": true,
                "scope": "services",
                "http_proxy": "http://127.0.0.1:7890",
                "services": []
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("proxy.scope='services'"));
    }

    #[tokio::test]
    async fn schema_requires_nonempty_services_for_service_scope_aliases() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, test_security());
        let schema = tool.parameters_schema();

        for scope in ["services", "service"] {
            assert!(
                !crate::tools::schema::validate_tool_arguments(&schema, &json!({"action": "set", "scope": scope}))
                    .is_empty(),
                "expected a non-empty value"
            );
            assert!(
                crate::tools::schema::validate_tool_arguments(
                    &schema,
                    &json!({"action": "set", "scope": scope, "services": ["provider.openai"]})
                )
                .is_empty(),
                "expected an empty value"
            );
        }
    }

    #[tokio::test]
    async fn set_requires_runtime_grant_in_supervised_mode() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, supervised_security());

        let result = tool
            .execute(json!({
                "action": "set",
                "http_proxy": "http://127.0.0.1:7890"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("runtime approval grant"));
    }

    #[tokio::test]
    async fn set_accepts_matching_resource_grant_in_supervised_mode() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, supervised_security());
        let grant = ApprovalGrant::for_resource_operation("proxy_config", "proxy_config:set", "test", None);

        let result = tool
            .execute(json!({
                "action": "set",
                "http_proxy": "http://127.0.0.1:7890",
                (crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG): serde_json::to_value(grant).unwrap()
            }))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
    }

    #[tokio::test]
    async fn set_and_get_round_trip_proxy_scope() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, test_security());

        let set_result = tool
            .execute(json!({
                "action": "set",
                "scope": "services",
                "http_proxy": "http://127.0.0.1:7890",
                "services": ["provider.openai", "tool.http_request"]
            }))
            .await
            .unwrap();
        assert!(set_result.success, "{:?}", set_result.error);
        let set_output: Value = serde_json::from_str(&set_result.output).unwrap();
        assert_eq!(set_output["status"], "restart_required");
        assert_eq!(set_output["restart_required"], json!(["proxy"]));
        assert!(
            set_output["message"]
                .as_str()
                .unwrap_or_default()
                .contains("restart required")
        );
        assert_eq!(set_output["active_runtime_proxy"]["configured"], false);
        assert_eq!(set_output["desired_proxy"]["configured"], true);

        let get_result = tool.execute(json!({"action": "get"})).await.unwrap();
        assert!(get_result.success);
        assert!(get_result.output.contains("provider.openai"));
        assert!(get_result.output.contains("services"));
    }

    #[tokio::test]
    async fn set_null_proxy_url_clears_existing_value() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, test_security());

        let set_result = tool
            .execute(json!({
                "action": "set",
                "http_proxy": "http://127.0.0.1:7890"
            }))
            .await
            .unwrap();
        assert!(set_result.success, "{:?}", set_result.error);

        let clear_result = tool
            .execute(json!({
                "action": "set",
                "http_proxy": null
            }))
            .await
            .unwrap();
        assert!(clear_result.success, "{:?}", clear_result.error);

        let get_result = tool.execute(json!({"action": "get"})).await.unwrap();
        assert!(get_result.success);
        let parsed: Value = serde_json::from_str(&get_result.output).unwrap();
        assert!(parsed["proxy"]["http_proxy"].is_null());
        assert!(parsed["runtime_proxy"]["http_proxy"].is_null());
    }

    /// RED before SECRETS-OTHER: `set` and `get` returned a proxy URL's
    /// `user:password@` to the model verbatim. After: the URLs are shown with
    /// only their userinfo replaced; the stored value keeps the credential.
    #[tokio::test]
    async fn proxy_urls_are_shown_without_their_userinfo() {
        const PASSWORD: &str = "userinfo-sentinel-proxy-pw";
        let tmp = TempDir::new().unwrap();
        let shared = test_config(&tmp).await;
        let tool = ProxyConfigTool::new(Arc::clone(&shared), test_security());

        let set_result = tool
            .execute(json!({
                "action": "set",
                "http_proxy": format!("http://proxy-user:{PASSWORD}@127.0.0.1:7890"),
                "all_proxy": format!("socks5://proxy-user:{PASSWORD}@127.0.0.1:7891"),
            }))
            .await
            .unwrap();
        assert!(set_result.success, "{:?}", set_result.error);
        assert!(
            !set_result.output.contains(PASSWORD),
            "set leaked: {}",
            set_result.output
        );
        assert!(
            set_result.output.contains("http://[redacted]@127.0.0.1:7890"),
            "{}",
            set_result.output
        );

        let get_result = tool.execute(json!({"action": "get"})).await.unwrap();
        assert!(get_result.success);
        assert!(
            !get_result.output.contains(PASSWORD),
            "get leaked: {}",
            get_result.output
        );
        assert!(
            get_result.output.contains("socks5://[redacted]@127.0.0.1:7891"),
            "{}",
            get_result.output
        );

        assert_eq!(
            shared.desired().config.proxy.http_proxy.as_deref(),
            Some(format!("http://proxy-user:{PASSWORD}@127.0.0.1:7890").as_str()),
            "the stored proxy keeps its credential"
        );
    }

    /// D-049 C, RED before SECRETS-h (R-SECRETSfg P17): `set` re-pointed
    /// every credentialed request through a proxy at a fixed Medium tier,
    /// outside the configuration plan. After: the plan classifies the change;
    /// a new proxy URL is high risk (an endpoint every credentialed request
    /// goes through, here with userinfo), the approval tier is High, and the
    /// output reports the risk and its reasons by path only. At `full`
    /// autonomy it is approved (D-037); a scope-only change stays below High.
    #[tokio::test]
    async fn set_is_classified_by_the_configuration_plan() {
        const PASSWORD: &str = "userinfo-sentinel-proxy-plan";
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, test_security());

        let result = tool
            .execute(json!({
                "action": "set",
                "https_proxy": format!("http://proxy-user:{PASSWORD}@127.0.0.1:7890"),
            }))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(!result.output.contains(PASSWORD), "{}", result.output);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["risk"], json!("high"), "{}", result.output);
        let reasons = output["risk_reasons"].to_string();
        assert!(reasons.contains("endpoint_changed"), "{reasons}");
        assert!(reasons.contains("proxy.https_proxy"), "{reasons}");

        let result = tool
            .execute(json!({"action": "set", "no_proxy": ["localhost"]}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["risk"], json!("low"), "{}", result.output);
    }

    /// D-049 C: at `supervised` a high-risk `set` without a runtime grant is
    /// refused before anything is written.
    #[tokio::test]
    async fn a_high_risk_set_needs_approval_when_supervised() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, supervised_security());
        let before = std::fs::read_to_string(tmp.path().join("config.toml")).unwrap();
        let result = tool
            .execute(json!({"action": "set", "http_proxy": "http://127.0.0.1:7890"}))
            .await
            .unwrap();
        assert!(!result.success, "{}", result.output);
        assert_eq!(std::fs::read_to_string(tmp.path().join("config.toml")).unwrap(), before);
    }

    /// D-050 item 6, RED before SECRETS-i (R-SECRETSfg H-8): `set` saved
    /// straight to `config.toml`, outside the transaction layer (no history
    /// record, no revision check). After: it is a configuration transaction:
    /// the output names the history record, and a file changed since the
    /// configuration was read is not overwritten.
    #[tokio::test]
    async fn set_is_a_configuration_transaction() {
        let tmp = TempDir::new().unwrap();
        let tool = ProxyConfigTool::new(test_config(&tmp).await, test_security());

        let result = tool
            .execute(json!({"action": "set", "no_proxy": ["localhost"]}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert!(output["record_id"].is_string(), "{}", result.output);

        // Another writer changes the file; the tool has not seen it.
        let path = tmp.path().join("config.toml");
        let edited = format!("{}\n# edited by hand\n", std::fs::read_to_string(&path).unwrap());
        std::fs::write(&path, &edited).unwrap();
        let result = tool
            .execute(json!({"action": "set", "no_proxy": ["localhost", "example.invalid"]}))
            .await
            .unwrap();
        assert!(!result.success, "a stale set must not overwrite: {}", result.output);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    }

    /// D-051, RED before SECRETS-j (R-SECRETSfg I-1): `set` built its change
    /// from one snapshot but read the revision it saved against later, at
    /// save time. A low-risk write landing in between was then silently
    /// reverted (the candidate was the old snapshot plus the proxy change).
    /// After: the revision is captured with the snapshot, the stale save is
    /// a conflict, and the interleaved change survives.
    #[tokio::test]
    async fn an_interleaved_write_is_never_reverted() {
        let tmp = TempDir::new().unwrap();
        let shared = test_config(&tmp).await;
        let tool = ProxyConfigTool::new(Arc::clone(&shared), test_security());

        let prepared = tool.prepare_set(&json!({"no_proxy": ["localhost"]})).unwrap();
        let (_, summary) = ProxyConfigTool::set_risk(&prepared.current, &prepared.updated).unwrap();

        // Another writer commits a low-risk change after the snapshot.
        let desired = shared.desired();
        let mut other = desired.config.as_ref().clone();
        other.default_temperature = 0.33;
        let transactions = ConfigTransactions::new(Arc::clone(&shared));
        let base = ConfigTransactions::base_revision(&desired);
        let plan = transactions
            .plan(ConfigChange::Structured(Box::new(other)))
            .await
            .unwrap();
        transactions
            .apply(
                plan,
                ConfigActor::GatewayConfigApi,
                ApplyOptions {
                    expected_revision: base.as_deref(),
                    ..ApplyOptions::default()
                },
            )
            .await
            .unwrap();
        assert!((shared.desired().config.default_temperature - 0.33).abs() < 1e-9);

        let result = tool.handle_set(&prepared, &summary).await;
        assert!(
            result.is_err(),
            "a save built on the old snapshot must conflict: {result:?}"
        );
        assert!(
            (shared.desired().config.default_temperature - 0.33).abs() < 1e-9,
            "the interleaved change must survive"
        );
        let on_disk = std::fs::read_to_string(tmp.path().join("config.toml")).unwrap();
        assert!(on_disk.contains("default_temperature = 0.33"), "{on_disk}");
    }
}
