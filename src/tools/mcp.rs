use super::traits::{Tool, ToolResult, ToolSpec};
use crate::config::{McpConfig, McpServerConfig, McpTransport};
use crate::security::SecurityPolicy;
use anyhow::bail;
use async_trait::async_trait;
use parking_lot::RwLock;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{model::CallToolRequestParams, ServiceExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::process::Command;

const MCP_JSON_FILE: &str = "mcp.json";
const MCP_ROOT_NAME: &str = "mcp_call";

#[derive(Debug, Clone, Default)]
struct DiscoveredToolMeta {
    description: Option<String>,
    input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeState {
    effective_config: McpConfig,
    mcp_json_mtime: Option<SystemTime>,
    /// Whether initial tool discovery has been performed at least once.
    initialized: bool,
    discovered_tools: HashMap<String, HashMap<String, DiscoveredToolMeta>>, // server -> tool -> meta
}

#[derive(Debug, Clone, Deserialize)]
struct McpJsonRoot {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpJsonServer>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpJsonServer {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    startup_timeout_ms: Option<u64>,
    #[serde(default)]
    request_timeout_ms: Option<u64>,
    #[serde(default)]
    tool_name_prefix: Option<String>,
    #[serde(default)]
    allow_tools: Vec<String>,
    #[serde(default)]
    deny_tools: Vec<String>,
}

/// Generic MCP proxy tool.
///
/// Behavior:
/// - Loads base MCP config from `config.toml`.
/// - Applies live overrides from `<workspace>/mcp.json` when present.
/// - Auto-discovers remote tools via MCP `list_tools`.
/// - Exposes dynamic per-tool aliases: `<prefix>__<server>__<tool>`.
pub struct McpTool {
    security: Arc<SecurityPolicy>,
    base_config: McpConfig,
    mcp_json_path: PathBuf,
    state: RwLock<RuntimeState>,
}

impl McpTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        base_config: McpConfig,
        workspace_dir: PathBuf,
    ) -> Self {
        let state = RuntimeState {
            effective_config: base_config.clone(),
            mcp_json_mtime: None,
            initialized: false,
            discovered_tools: HashMap::new(),
        };

        Self {
            security,
            base_config,
            mcp_json_path: workspace_dir.join(MCP_JSON_FILE),
            state: RwLock::new(state),
        }
    }

    /// Return runtime-discovered tools grouped by server name.
    /// Each entry is `(tool_name, description)`.
    pub fn list_discovered_tools(&self) -> HashMap<String, Vec<(String, Option<String>)>> {
        let state = self.state.read();
        let mut result = HashMap::new();
        for (server_name, tools) in &state.discovered_tools {
            let entries: Vec<(String, Option<String>)> = tools
                .iter()
                .map(|(name, meta)| (name.clone(), meta.description.clone()))
                .collect();
            result.insert(server_name.clone(), entries);
        }
        result
    }

    fn alias_name(prefix: &str, server: &str, tool: &str) -> String {
        format!("{prefix}__{server}__{tool}")
    }

    fn parse_alias_name(cfg: &McpConfig, name: &str) -> Option<(String, String)> {
        for (server_name, server_cfg) in &cfg.servers {
            let prefix = format!("{}__{}__", server_cfg.tool_name_prefix, server_name);
            if let Some(tool) = name.strip_prefix(&prefix) {
                if !tool.is_empty() {
                    return Some((server_name.clone(), tool.to_string()));
                }
            }
        }
        None
    }

    fn load_effective_config_from_json(&self) -> anyhow::Result<Option<McpConfig>> {
        if !self.mcp_json_path.exists() {
            return Ok(None);
        }

        let raw = std::fs::read_to_string(&self.mcp_json_path)?;
        let parsed: McpJsonRoot = serde_json::from_str(&raw)?;

        let mut cfg = self.base_config.clone();
        if let Some(enabled) = parsed.enabled {
            cfg.enabled = enabled;
        }

        if !parsed.mcp_servers.is_empty() {
            let mut servers = HashMap::new();
            for (name, server) in parsed.mcp_servers {
                servers.insert(name, Self::convert_json_server(server));
            }
            cfg.servers = servers;
        }

        Ok(Some(cfg))
    }

    fn convert_json_server(server: McpJsonServer) -> McpServerConfig {
        let mut out = McpServerConfig::default();

        if let Some(enabled) = server.enabled {
            out.enabled = enabled;
        }
        if let Some(transport) = server.transport {
            out.transport = match transport.to_lowercase().as_str() {
                "http" => McpTransport::Http,
                _ => McpTransport::Stdio,
            };
        } else if server.url.is_some() {
            out.transport = McpTransport::Http;
        }

        out.command = server.command;
        out.args = server.args;
        out.url = server.url;
        out.env = server.env;

        if let Some(v) = server.startup_timeout_ms {
            out.startup_timeout_ms = v;
        }
        if let Some(v) = server.request_timeout_ms {
            out.request_timeout_ms = v;
        }
        if let Some(v) = server.tool_name_prefix {
            out.tool_name_prefix = v;
        }

        out.allow_tools = server.allow_tools;
        out.deny_tools = server.deny_tools;

        out
    }

    fn file_mtime(path: &Path) -> anyhow::Result<Option<SystemTime>> {
        if !path.exists() {
            return Ok(None);
        }
        let metadata = std::fs::metadata(path)?;
        Ok(metadata.modified().ok())
    }

    fn resolve_server<'a>(cfg: &'a McpConfig, name: &str) -> anyhow::Result<&'a McpServerConfig> {
        if !cfg.enabled {
            bail!("MCP integration is disabled in effective config");
        }

        let Some(server) = cfg.servers.get(name) else {
            bail!("Unknown MCP server '{name}'");
        };

        if !server.enabled {
            bail!("MCP server '{name}' is disabled");
        }

        Ok(server)
    }

    fn tool_allowed(server: &McpServerConfig, tool_name: &str) -> bool {
        if server.deny_tools.iter().any(|t| t == tool_name) {
            return false;
        }

        if server.allow_tools.is_empty() {
            return true;
        }

        server.allow_tools.iter().any(|t| t == tool_name)
    }

    fn extract_call_success_and_output(value: &serde_json::Value) -> (bool, String) {
        let is_error = value
            .get("isError")
            .or_else(|| value.get("is_error"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let content = value
            .get("content")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let text = item.get("text").and_then(serde_json::Value::as_str);
                        if text.is_some() {
                            return text.map(ToString::to_string);
                        }

                        if item
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|t| t.eq_ignore_ascii_case("text"))
                        {
                            return Some(item.to_string());
                        }

                        None
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        let output = if content.is_empty() {
            value.to_string()
        } else {
            content
        };

        (!is_error, output)
    }

    async fn discover_server_tools_stdio(
        server_name: &str,
        server: &McpServerConfig,
    ) -> anyhow::Result<HashMap<String, DiscoveredToolMeta>> {
        let command = server
            .command
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("MCP server '{server_name}' uses stdio but command is missing")
            })?;

        let mut cmd = Command::new(command);
        cmd.args(&server.args);
        if !server.env.is_empty() {
            cmd.envs(server.env.clone());
        }

        let startup_timeout = Duration::from_millis(server.startup_timeout_ms);
        let transport = TokioChildProcess::new(cmd)?;
        let client = tokio::time::timeout(startup_timeout, ().serve(transport))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "MCP startup timed out after {} ms",
                    server.startup_timeout_ms
                )
            })??;

        let list = client.peer().list_all_tools().await?;
        let _ = client.cancel().await;

        let tools = list
            .into_iter()
            .map(|tool| {
                let meta = DiscoveredToolMeta {
                    description: tool.description.map(|v| v.to_string()),
                    input_schema: serde_json::to_value(tool.input_schema).ok(),
                };
                (tool.name.to_string(), meta)
            })
            .collect::<HashMap<_, _>>();

        Ok(tools)
    }

    async fn discover_server_tools_http(
        server_name: &str,
        server: &McpServerConfig,
    ) -> anyhow::Result<HashMap<String, DiscoveredToolMeta>> {
        let url = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("MCP server '{server_name}' uses http but url is missing")
            })?;

        let startup_timeout = Duration::from_millis(server.startup_timeout_ms);
        let transport = StreamableHttpClientTransport::from_uri(url);
        let client = tokio::time::timeout(startup_timeout, ().serve(transport))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "MCP startup timed out after {} ms",
                    server.startup_timeout_ms
                )
            })??;

        let list = client.peer().list_all_tools().await?;
        let _ = client.cancel().await;

        let tools = list
            .into_iter()
            .map(|tool| {
                let meta = DiscoveredToolMeta {
                    description: tool.description.map(|v| v.to_string()),
                    input_schema: serde_json::to_value(tool.input_schema).ok(),
                };
                (tool.name.to_string(), meta)
            })
            .collect::<HashMap<_, _>>();

        Ok(tools)
    }

    async fn discover_server_tools(
        server_name: &str,
        server: &McpServerConfig,
    ) -> anyhow::Result<HashMap<String, DiscoveredToolMeta>> {
        match server.transport {
            McpTransport::Stdio => Self::discover_server_tools_stdio(server_name, server).await,
            McpTransport::Http => Self::discover_server_tools_http(server_name, server).await,
        }
    }

    async fn call_stdio(
        server_name: &str,
        server: &McpServerConfig,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> anyhow::Result<(bool, String)> {
        let command = server
            .command
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("MCP server '{server_name}' uses stdio but command is missing")
            })?;

        tracing::debug!(
            server = server_name,
            tool = tool_name,
            args = ?arguments,
            "MCP call_stdio: invoking tool"
        );

        let mut cmd = Command::new(command);
        cmd.args(&server.args);
        if !server.env.is_empty() {
            cmd.envs(server.env.clone());
        }

        let startup_timeout = Duration::from_millis(server.startup_timeout_ms);
        let request_timeout = Duration::from_millis(server.request_timeout_ms);
        let transport = TokioChildProcess::new(cmd)?;
        let client = tokio::time::timeout(startup_timeout, ().serve(transport))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "MCP startup timed out after {} ms",
                    server.startup_timeout_ms
                )
            })??;

        let result = tokio::time::timeout(
            request_timeout,
            client.call_tool(CallToolRequestParams {
                meta: None,
                name: tool_name.to_string().into(),
                arguments,
                task: None,
            }),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("MCP call timed out after {} ms", server.request_timeout_ms)
        })?;

        let _ = client.cancel().await;

        match result {
            Ok(r) => {
                let value = serde_json::to_value(r)?;
                tracing::debug!(server = server_name, tool = tool_name, result = %value, "MCP call_stdio: result");
                Ok(Self::extract_call_success_and_output(&value))
            }
            Err(e) => {
                tracing::warn!(server = server_name, tool = tool_name, error = %e, "MCP call_stdio: error");
                Err(e.into())
            }
        }
    }

    async fn call_http(
        server_name: &str,
        server: &McpServerConfig,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> anyhow::Result<(bool, String)> {
        let url = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("MCP server '{server_name}' uses http but url is missing")
            })?;

        let startup_timeout = Duration::from_millis(server.startup_timeout_ms);
        let request_timeout = Duration::from_millis(server.request_timeout_ms);
        let transport = StreamableHttpClientTransport::from_uri(url);
        let client = tokio::time::timeout(startup_timeout, ().serve(transport))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "MCP startup timed out after {} ms",
                    server.startup_timeout_ms
                )
            })??;

        let result = tokio::time::timeout(
            request_timeout,
            client.call_tool(CallToolRequestParams {
                meta: None,
                name: tool_name.to_string().into(),
                arguments,
                task: None,
            }),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("MCP call timed out after {} ms", server.request_timeout_ms)
        })??;

        let _ = client.cancel().await;
        let value = serde_json::to_value(result)?;
        Ok(Self::extract_call_success_and_output(&value))
    }

    async fn refresh_runtime_state(&self) {
        let file_mtime = Self::file_mtime(&self.mcp_json_path).ok().flatten();
        let (current_mtime, initialized) = {
            let state = self.state.read();
            (state.mcp_json_mtime, state.initialized)
        };

        // Skip if already initialized and config file hasn't changed.
        // Note: `None == None` when there is no mcp.json — we must NOT skip on the
        // very first call (before `initialized` is set to true), otherwise tools are
        // never discovered when the user relies purely on config.toml.
        if initialized && file_mtime == current_mtime {
            return;
        }

        let mut new_config = self.base_config.clone();
        if let Ok(Some(from_file)) = self.load_effective_config_from_json() {
            new_config = from_file;
        }

        let mut discovered = HashMap::new();
        for (server_name, server) in &new_config.servers {
            if !server.enabled {
                continue;
            }
            if let Ok(tools) = Self::discover_server_tools(server_name, server).await {
                discovered.insert(server_name.clone(), tools);
            }
        }

        let mut state = self.state.write();
        state.effective_config = new_config;
        state.discovered_tools = discovered;
        state.mcp_json_mtime = file_mtime;
        state.initialized = true;
    }

    fn refresh_state_from_file_only(&self) {
        let file_mtime = Self::file_mtime(&self.mcp_json_path).ok().flatten();
        let current_mtime = self.state.read().mcp_json_mtime;
        if file_mtime == current_mtime {
            return;
        }

        let mut new_config = self.base_config.clone();
        if let Ok(Some(from_file)) = self.load_effective_config_from_json() {
            new_config = from_file;
        }

        let mut state = self.state.write();
        state.effective_config = new_config;
        state.discovered_tools.clear();
        state.mcp_json_mtime = file_mtime;
    }

    async fn validate_and_call(
        &self,
        server_name: String,
        tool_name: String,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> anyhow::Result<(bool, String)> {
        let (effective_config, discovered_for_server) = {
            let state = self.state.read();
            (
                state.effective_config.clone(),
                state
                    .discovered_tools
                    .get(&server_name)
                    .cloned()
                    .unwrap_or_default(),
            )
        };

        let server = Self::resolve_server(&effective_config, &server_name)?;

        if !Self::tool_allowed(server, &tool_name) {
            bail!(
                "Tool '{tool_name}' is blocked by allow/deny rules for MCP server '{server_name}'"
            );
        }

        if !discovered_for_server.is_empty() && !discovered_for_server.contains_key(&tool_name) {
            let available = discovered_for_server
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                    "Tool '{tool_name}' not found on MCP server '{server_name}'. Available: [{available}]"
                );
        }

        match server.transport {
            McpTransport::Stdio => {
                Self::call_stdio(&server_name, server, &tool_name, arguments).await
            }
            McpTransport::Http => {
                Self::call_http(&server_name, server, &tool_name, arguments).await
            }
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        MCP_ROOT_NAME
    }

    fn description(&self) -> &str {
        "Call tools exposed by configured MCP servers. \
         Supports hot config reload from workspace/mcp.json and dynamic aliases."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.refresh_state_from_file_only();

        let state = self.state.read();
        let server_names = state
            .effective_config
            .servers
            .iter()
            .filter_map(|(name, cfg)| cfg.enabled.then_some(name.clone()))
            .collect::<Vec<_>>();

        let mut tool_set = HashSet::new();
        for per_server in state.discovered_tools.values() {
            for tool_name in per_server.keys() {
                tool_set.insert(tool_name.clone());
            }
        }
        let mut tool_names = tool_set.into_iter().collect::<Vec<_>>();
        tool_names.sort();

        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Configured MCP server name",
                    "enum": server_names
                },
                "tool": {
                    "type": "string",
                    "description": "Remote MCP tool name to invoke",
                    "enum": tool_names
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments object forwarded to MCP call_tool",
                    "default": {}
                }
            },
            "required": ["server", "tool"]
        })
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.refresh_state_from_file_only();

        let state = self.state.read();
        let mut specs = vec![self.spec()];

        for (server_name, tools) in &state.discovered_tools {
            let Some(server_cfg) = state.effective_config.servers.get(server_name) else {
                continue;
            };
            if !server_cfg.enabled {
                continue;
            }

            for (tool_name, meta) in tools {
                if !Self::tool_allowed(server_cfg, tool_name) {
                    continue;
                }

                let alias = Self::alias_name(&server_cfg.tool_name_prefix, server_name, tool_name);
                specs.push(ToolSpec {
                    name: alias,
                    description: meta.description.clone().unwrap_or_else(|| {
                        format!("MCP tool '{}' from server '{}'", tool_name, server_name)
                    }),
                    parameters: meta
                        .input_schema
                        .clone()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                });
            }
        }

        specs
    }

    fn supports_name(&self, name: &str) -> bool {
        if name == MCP_ROOT_NAME {
            return true;
        }

        let state = self.state.read();
        Self::parse_alias_name(&state.effective_config, name).is_some()
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        self.refresh_runtime_state().await;
        Ok(())
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_named(MCP_ROOT_NAME, args).await
    }

    async fn execute_named(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: rate limit exceeded".into()),
            });
        }

        self.refresh_runtime_state().await;

        let (server_name, tool_name, arguments) = if name == MCP_ROOT_NAME {
            let server_name = args
                .get("server")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Missing 'server' parameter"))?
                .to_string();
            let tool_name = args
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Missing 'tool' parameter"))?
                .to_string();
            let arguments = args
                .get("arguments")
                .and_then(serde_json::Value::as_object)
                .cloned();
            (server_name, tool_name, arguments)
        } else {
            let state = self.state.read();
            let (server_name, tool_name) = Self::parse_alias_name(&state.effective_config, name)
                .ok_or_else(|| anyhow::anyhow!("Unknown MCP alias tool '{name}'"))?;
            drop(state);

            let arguments = args.as_object().cloned().ok_or_else(|| {
                anyhow::anyhow!("MCP alias tool '{name}' expects object arguments")
            })?;
            (server_name, tool_name, Some(arguments))
        };

        let call_result = self
            .validate_and_call(server_name, tool_name, arguments)
            .await;

        match call_result {
            Ok((success, output)) => Ok(ToolResult {
                success,
                output,
                error: None,
            }),
            Err(err) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(err.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;

    // ── tool_allowed rules ────────────────────────────────────

    #[test]
    fn tool_allow_deny_rules() {
        let mut cfg = McpServerConfig {
            allow_tools: vec!["allowed".into()],
            ..McpServerConfig::default()
        };

        assert!(McpTool::tool_allowed(&cfg, "allowed"));
        assert!(!McpTool::tool_allowed(&cfg, "other"));

        cfg.deny_tools = vec!["blocked".into()];
        assert!(!McpTool::tool_allowed(&cfg, "blocked"));
    }

    #[test]
    fn tool_allowed_empty_lists_allows_all() {
        let cfg = McpServerConfig::default();
        assert!(McpTool::tool_allowed(&cfg, "anything"));
    }

    #[test]
    fn tool_allowed_deny_takes_priority() {
        let cfg = McpServerConfig {
            allow_tools: vec!["tool1".into()],
            deny_tools: vec!["tool1".into()],
            ..McpServerConfig::default()
        };
        assert!(
            !McpTool::tool_allowed(&cfg, "tool1"),
            "deny should override allow"
        );
    }

    // ── convert_json_server ─────────────────────────────────────

    #[test]
    fn convert_json_server_defaults_transport_from_url() {
        let server = McpJsonServer {
            enabled: Some(true),
            transport: None,
            command: None,
            args: Vec::new(),
            url: Some("http://127.0.0.1:8181/mcp".into()),
            env: HashMap::new(),
            startup_timeout_ms: None,
            request_timeout_ms: None,
            tool_name_prefix: None,
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
        };

        let out = McpTool::convert_json_server(server);
        assert_eq!(out.transport, McpTransport::Http);
        assert_eq!(out.url.as_deref(), Some("http://127.0.0.1:8181/mcp"));
    }

    #[test]
    fn convert_json_server_defaults_to_stdio_when_command_present() {
        let server = McpJsonServer {
            enabled: Some(true),
            transport: None,
            command: Some("my-tool".into()),
            args: vec!["--serve".into()],
            url: None,
            env: HashMap::new(),
            startup_timeout_ms: None,
            request_timeout_ms: None,
            tool_name_prefix: None,
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
        };
        let out = McpTool::convert_json_server(server);
        assert_eq!(out.transport, McpTransport::Stdio);
        assert_eq!(out.command.as_deref(), Some("my-tool"));
        assert_eq!(out.args, vec!["--serve"]);
    }

    #[test]
    fn convert_json_server_disabled_flag() {
        let server = McpJsonServer {
            enabled: Some(false),
            transport: None,
            command: Some("x".into()),
            args: Vec::new(),
            url: None,
            env: HashMap::new(),
            startup_timeout_ms: None,
            request_timeout_ms: None,
            tool_name_prefix: None,
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
        };
        let out = McpTool::convert_json_server(server);
        assert!(!out.enabled);
    }

    // ── parse_alias_name ────────────────────────────────────────

    #[test]
    fn parse_alias_name_resolves_server_and_tool() {
        let mut cfg = McpConfig::default();
        cfg.enabled = true;
        cfg.servers.insert(
            "qmd".into(),
            McpServerConfig {
                tool_name_prefix: "mcp".into(),
                ..McpServerConfig::default()
            },
        );

        let parsed = McpTool::parse_alias_name(&cfg, "mcp__qmd__search");
        assert_eq!(parsed, Some(("qmd".into(), "search".into())));
    }

    #[test]
    fn parse_alias_name_no_match() {
        let cfg = McpConfig::default();
        assert!(McpTool::parse_alias_name(&cfg, "unrelated_name").is_none());
    }

    #[test]
    fn parse_alias_name_default_prefix() {
        let mut cfg = McpConfig::default();
        cfg.enabled = true;
        cfg.servers.insert(
            "myserver".into(),
            McpServerConfig {
                tool_name_prefix: "mcp".into(),
                ..McpServerConfig::default()
            },
        );
        let parsed = McpTool::parse_alias_name(&cfg, "mcp__myserver__run_query");
        assert_eq!(parsed, Some(("myserver".into(), "run_query".into())));
    }

    // ── alias_name ──────────────────────────────────────────────

    #[test]
    fn alias_name_format() {
        assert_eq!(
            McpTool::alias_name("mcp", "server1", "search"),
            "mcp__server1__search"
        );
    }

    // ── extract_call_success_and_output ──────────────────────────

    #[test]
    fn extract_call_success_text_content() {
        let value = json!({
            "content": [{"type": "text", "text": "hello world"}]
        });
        let (success, output) = McpTool::extract_call_success_and_output(&value);
        assert!(success);
        assert_eq!(output, "hello world");
    }

    #[test]
    fn extract_call_error_flag() {
        let value = json!({
            "isError": true,
            "content": [{"type": "text", "text": "error msg"}]
        });
        let (success, output) = McpTool::extract_call_success_and_output(&value);
        assert!(!success);
        assert_eq!(output, "error msg");
    }

    #[test]
    fn extract_call_is_error_snake_case() {
        let value = json!({
            "is_error": true,
            "content": []
        });
        let (success, _) = McpTool::extract_call_success_and_output(&value);
        assert!(!success);
    }

    #[test]
    fn extract_call_empty_content_falls_back_to_json() {
        let value = json!({"data": 42});
        let (success, output) = McpTool::extract_call_success_and_output(&value);
        assert!(success);
        assert!(output.contains("42"));
    }

    #[test]
    fn extract_call_multiple_text_items_joined() {
        let value = json!({
            "content": [
                {"type": "text", "text": "line1"},
                {"type": "text", "text": "line2"}
            ]
        });
        let (_, output) = McpTool::extract_call_success_and_output(&value);
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
    }

    // ── McpTool metadata ────────────────────────────────────────

    #[test]
    fn mcp_tool_name() {
        let tool = McpTool::new(
            Arc::new(SecurityPolicy::default()),
            McpConfig::default(),
            std::env::temp_dir(),
        );
        assert_eq!(tool.name(), MCP_ROOT_NAME);
    }

    #[test]
    fn mcp_tool_description_non_empty() {
        let tool = McpTool::new(
            Arc::new(SecurityPolicy::default()),
            McpConfig::default(),
            std::env::temp_dir(),
        );
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn mcp_tool_schema_requires_server_and_tool() {
        let tool = McpTool::new(
            Arc::new(SecurityPolicy::default()),
            McpConfig::default(),
            std::env::temp_dir(),
        );
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().expect("test: required");
        assert!(required.iter().any(|v| v == "server"));
        assert!(required.iter().any(|v| v == "tool"));
    }

    // ── list_discovered_tools ───────────────────────────────────

    #[test]
    fn list_discovered_tools_empty_initially() {
        let tool = McpTool::new(
            Arc::new(SecurityPolicy::default()),
            McpConfig::default(),
            std::env::temp_dir(),
        );
        let tools = tool.list_discovered_tools();
        assert!(tools.is_empty());
    }

    // ── Security: read-only ─────────────────────────────────────

    #[tokio::test]
    async fn readonly_blocks_execute() {
        let security = Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::ReadOnly,
            max_actions_per_hour: 1000,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = McpTool::new(security, McpConfig::default(), std::env::temp_dir());
        let result = tool
            .execute(json!({"server": "s", "tool": "t"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only"));
    }

    // ── MCP disabled ────────────────────────────────────────────

    #[tokio::test]
    async fn mcp_disabled_returns_error() {
        let mut cfg = McpConfig::default();
        cfg.enabled = false;
        let tool = McpTool::new(
            Arc::new(SecurityPolicy::default()),
            cfg,
            std::env::temp_dir(),
        );
        let result = tool
            .execute(json!({"server": "s", "tool": "t"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("disabled")
                || result
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("not found")
        );
    }
}
