//! The workspace `mcp.json` overlay: parsing the file and narrowing or
//! extending the configured servers with it.
//!
//! The methods are an `impl McpTool` block here; a child module sees the
//! private fields of its parent, so the bodies need no path changes.

use super::McpTool;
use crate::config::{McpConfig, McpServerConfig, McpTransport, WorkspaceMcpServers};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Deserialize)]
struct McpJsonRoot {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpJsonServer>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct McpJsonServer {
    #[serde(default)]
    pub(super) transport: Option<String>,
    #[serde(default)]
    pub(super) command: Option<String>,
    #[serde(default)]
    pub(super) args: Vec<String>,
    #[serde(default)]
    pub(super) url: Option<String>,
    #[serde(default)]
    pub(super) env: HashMap<String, String>,
    /// HTTP request headers; plaintext in this file, as the workspace wrote them.
    #[serde(default)]
    pub(super) headers: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) startup_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(super) request_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(super) tool_name_prefix: Option<String>,
    #[serde(default)]
    pub(super) allow_tools: Vec<String>,
    #[serde(default)]
    pub(super) deny_tools: Vec<String>,
    /// Disable a server `config.toml` defines (the explicit way for a
    /// workspace to leave one out).
    #[serde(default)]
    pub(super) disabled: bool,
}

impl McpTool {
    /// The effective configuration with the workspace `mcp.json` applied,
    /// and a note for every entry that did not start as written.
    ///
    /// The servers `config.toml` defines stay unless an entry of the same
    /// name explicitly narrows them (`"disabled": true`, or an allow-list
    /// that leaves no tool): a non-empty `mcp.json` never silently empties
    /// the configured set (D-088 C-7).
    #[allow(clippy::too_many_lines)]
    pub(super) fn load_workspace_overlay(
        &self,
        below_full: &mut HashMap<String, McpServerConfig>,
    ) -> anyhow::Result<Option<(McpConfig, Vec<String>)>> {
        if !self.mcp_json_path.exists() {
            return Ok(None);
        }

        let raw = std::fs::read_to_string(&self.mcp_json_path)?;
        // A serde message can echo the offending value (`invalid type: string
        // "Bearer ..."`), so only its position and category are reported.
        let parsed: McpJsonRoot = serde_json::from_str(&raw).map_err(|error| {
            anyhow::anyhow!(
                "workspace mcp.json is not valid MCP configuration (line {}, column {}, {:?})",
                error.line(),
                error.column(),
                error.classify()
            )
        })?;

        let mut cfg = self.base_config.clone();
        let mut notes = Vec::new();
        let mut names = parsed.mcp_servers.keys().cloned().collect::<Vec<_>>();
        names.sort();
        let mut entries = parsed.mcp_servers;
        for name in names {
            let Some(server) = entries.remove(&name) else {
                continue;
            };
            let shown = name.escape_default().to_string();
            // ── A server config.toml defines: narrowing only ──
            // The whole definition (transport, command, args, env, url,
            // timeouts, tool prefix) is the operator's. The workspace may
            // disable the server or expose fewer of its tools, nothing else
            // (D-085 F-1, F-2).
            if let Some(base_server) = self.base_config.servers.get(&name) {
                let ignored = Self::ignored_workspace_fields(&server, base_server);
                if !ignored.is_empty() {
                    tracing::warn!(
                        server = %shown,
                        fields = ?ignored,
                        path = %self.mcp_json_path.display(),
                        "workspace mcp.json may only narrow a server registered in config.toml; \
                         using the config.toml definition for these fields"
                    );
                    notes.push(format!(
                        "server '{shown}' is defined in config.toml; the workspace mcp.json fields {} are ignored \
                         (a workspace may only disable it or narrow its tools)",
                        ignored.join(", ")
                    ));
                }
                if server.disabled {
                    cfg.servers.remove(&name);
                    notes.push(format!(
                        "server '{shown}' from config.toml is disabled by the workspace mcp.json \
                         (`\"disabled\": true`); remove that entry to start it"
                    ));
                    continue;
                }
                // At Full the workspace allow/deny lists are not applied.
                if self.full() {
                    continue;
                }
                match Self::narrowed(base_server, &server.allow_tools, &server.deny_tools) {
                    // With a Full caller the definition stays whole and the
                    // narrowed one is the view of callers below Full.
                    Some(narrowed) if self.keeps_full_only() => {
                        below_full.insert(name, narrowed);
                    }
                    Some(narrowed) => {
                        cfg.servers.insert(name, narrowed);
                    }
                    None => {
                        tracing::warn!(
                            server = %shown,
                            path = %self.mcp_json_path.display(),
                            "workspace mcp.json allows none of the tools config.toml exposes for this \
                             server; the server is disabled"
                        );
                        cfg.servers.remove(&name);
                        notes.push(format!(
                            "server '{shown}' from config.toml is disabled: the workspace mcp.json allows none of \
                             the tools config.toml exposes for it; fix its `allow_tools` or remove the entry"
                        ));
                    }
                }
                continue;
            }
            if server.disabled {
                continue;
            }

            let mut converted = Self::convert_json_server(server);
            let transport = match converted.transport {
                McpTransport::Stdio => "stdio",
                McpTransport::Http => "http",
            };

            // ── A new server: only with the operator's opt-in ──
            // At Full a workspace server loads without the opt-in.
            let full = self.full();
            let allowed = full
                || match converted.transport {
                    McpTransport::Stdio => self.base_config.workspace_servers == WorkspaceMcpServers::Stdio,
                    McpTransport::Http => self.base_config.workspace_servers != WorkspaceMcpServers::Off,
                };
            if !allowed {
                tracing::warn!(
                    server = %shown,
                    transport,
                    path = %self.mcp_json_path.display(),
                    "Blocked MCP server from workspace mcp.json: config.toml does not define it and \
                     `mcp.workspace_servers` does not allow a workspace to add one of this transport"
                );
                let (needed, grants) = match converted.transport {
                    McpTransport::Stdio => (
                        "stdio",
                        "a high-risk change: the workspace can then start programs that run as PRX's user",
                    ),
                    McpTransport::Http => ("http_only", "a high-risk change"),
                };
                notes.push(format!(
                    "workspace server '{shown}' ({transport}) is not started: config.toml does not define it and \
                     `mcp.workspace_servers` does not allow it. Define it in config.toml, or set \
                     `[mcp] workspace_servers = \"{needed}\"` in config.toml ({grants})"
                ));
                continue;
            }

            // ── Security gate: validate stdio commands ──
            if converted.transport == McpTransport::Stdio && !full {
                if let Some(ref cmd) = converted.command {
                    if !self.is_command_allowed(&name, cmd) {
                        tracing::warn!(
                            server = %shown,
                            command = %cmd,
                            path = %self.mcp_json_path.display(),
                            "Blocked MCP server from workspace mcp.json: \
                             command is not in the allowed list and server \
                             is not pre-registered in config.toml"
                        );
                        notes.push(format!(
                            "workspace server '{shown}' (stdio) is not started: its command is not a bare launcher \
                             from the fixed list (npx, node, python, uvx, ...); define it in config.toml instead"
                        ));
                        continue;
                    }
                }
            }

            // ── Security gate: block HTTP URLs targeting private/local hosts ──
            if converted.transport == McpTransport::Http {
                if let Some(ref url) = converted.url {
                    if let Err(e) = Self::validate_http_target(url, full) {
                        tracing::warn!(
                            server = %shown,
                            url = %crate::providers::redact_endpoint_userinfo(url),
                            error = %crate::security::redaction::redact_provider_text(&e.to_string()),
                            path = %self.mcp_json_path.display(),
                            "Blocked MCP HTTP server from workspace mcp.json: \
                             URL targets a private or local address"
                        );
                        notes.push(format!(
                            "workspace server '{shown}' (http) is not started: its URL targets a private or local \
                             address; define it in config.toml instead"
                        ));
                        continue;
                    }
                }
            }

            // ── Security gate: strip variables a workspace may not set ──
            // At an effective Full level of the install the variables are kept:
            // they are not secrets, and PRX's own secrets never reach the child.
            let removed = if full || self.keeps_full_only() {
                Vec::new()
            } else {
                Self::sanitize_env_vars(&mut converted.env)
            };
            if !removed.is_empty() {
                tracing::warn!(
                    server = %shown,
                    removed_vars = ?removed,
                    "Stripped environment variables a workspace may not set from \
                     workspace mcp.json server config"
                );
                notes.push(format!(
                    "workspace server '{shown}': environment variables a workspace may not set were dropped: {}",
                    removed.join(", ")
                ));
            }

            cfg.servers.insert(name, converted);
        }

        Ok(Some((cfg, notes)))
    }

    /// The fields a workspace entry sets for a registered server that are
    /// ignored because they are not a narrowing (names only, never values).
    fn ignored_workspace_fields(server: &McpJsonServer, base: &McpServerConfig) -> Vec<&'static str> {
        let transport = server.transport.as_deref().map(|transport| {
            if transport.eq_ignore_ascii_case("http") {
                McpTransport::Http
            } else {
                McpTransport::Stdio
            }
        });
        [
            (
                "transport",
                transport.is_some_and(|transport| transport != base.transport),
            ),
            ("command", server.command.is_some() && server.command != base.command),
            ("args", !server.args.is_empty() && server.args != base.args),
            ("url", server.url.is_some() && server.url != base.url),
            ("env", !server.env.is_empty() && server.env != base.env),
            ("headers", !server.headers.is_empty() && server.headers != base.headers),
            (
                "startup_timeout_ms",
                server
                    .startup_timeout_ms
                    .is_some_and(|value| value != base.startup_timeout_ms),
            ),
            (
                "request_timeout_ms",
                server
                    .request_timeout_ms
                    .is_some_and(|value| value != base.request_timeout_ms),
            ),
            (
                "tool_name_prefix",
                server
                    .tool_name_prefix
                    .as_ref()
                    .is_some_and(|value| *value != base.tool_name_prefix),
            ),
        ]
        .into_iter()
        .filter_map(|(field, ignored)| ignored.then_some(field))
        .collect()
    }

    /// `base` with at most the tools `allow` names (when it names any) and
    /// without the tools `deny` names: never more than `base` exposes.
    /// `None` when nothing would be left (an empty allow-list means "all",
    /// so an empty intersection disables the server instead).
    fn narrowed(base: &McpServerConfig, allow: &[String], deny: &[String]) -> Option<McpServerConfig> {
        let mut narrowed = base.clone();
        if !allow.is_empty() {
            narrowed.allow_tools = if base.allow_tools.is_empty() {
                allow.to_vec()
            } else {
                base.allow_tools
                    .iter()
                    .filter(|tool| allow.contains(tool))
                    .cloned()
                    .collect()
            };
            if narrowed.allow_tools.is_empty() {
                return None;
            }
        }
        for tool in deny {
            if !narrowed.deny_tools.contains(tool) {
                narrowed.deny_tools.push(tool.clone());
            }
        }
        Some(narrowed)
    }

    pub(super) fn convert_json_server(server: McpJsonServer) -> McpServerConfig {
        let mut out = McpServerConfig::default();

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
        out.headers = server.headers;

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
}
