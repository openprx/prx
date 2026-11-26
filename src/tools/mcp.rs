use super::traits::{Tool, ToolCategory, ToolResult, ToolSpec, ToolTier};
use crate::config::{McpConfig, McpServerConfig, McpTransport, WorkspaceMcpServers};
use crate::security::SecurityPolicy;
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel, SideEffectGate};
use anyhow::bail;
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};
use tokio::process::Command;

const MCP_JSON_FILE: &str = "mcp.json";
const MCP_ROOT_NAME: &str = "mcp_call";
const MCP_DISCOVERY_RETRY_BACKOFF: Duration = Duration::from_secs(30);
const MCP_STDIO_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

type McpClient = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// The HTTP client of every MCP streamable-HTTP transport.
///
/// Built here rather than taken from rmcp's default so its properties are
/// PRX's decision, not a dependency's:
/// - TLS: rustls from the `reqwest013` dependency, so an `https://` server is
///   reachable in every feature set (the crypto provider is the process
///   default, `ring`, installed at startup);
/// - redirects are never followed: the SSRF check (`validate_http_url`)
///   judges the configured URL only, so a `30x` to a loopback or metadata
///   address must not be taken, and the auth header must not be replayed to
///   another host;
/// - no idle connection pooling, as in rmcp's default (avoids delayed-ACK
///   stalls on a reused connection whose body was not drained).
///
/// `full` is the registry's level: at Full the resolver refuses only
/// unspecified and multicast addresses.
fn mcp_http_client(full: bool) -> anyhow::Result<reqwest013::Client> {
    let builder = reqwest013::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest013::redirect::Policy::none());
    let builder = if full {
        builder.dns_resolver(FullLevelResolver)
    } else {
        builder.dns_resolver(GlobalOnlyResolver)
    };
    builder
        .build()
        .map_err(|error| anyhow::anyhow!("failed to build the MCP HTTP client: {error}"))
}
/// The resolver of the MCP HTTP client: a name that resolves to a private,
/// loopback or otherwise non-global address is refused at connect time, so
/// a name that passed the load-time check (`validate_http_url`) cannot be
/// re-pointed at an internal address afterwards (DNS rebinding, D-088 C-4).
/// An IP literal is not resolved; the load-time check refuses a non-global
/// one. Through an HTTP proxy the proxy resolves the target, which this
/// cannot see.
struct GlobalOnlyResolver;

impl reqwest013::dns::Resolve for GlobalOnlyResolver {
    fn resolve(&self, name: reqwest013::dns::Name) -> reqwest013::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = super::http_request::lookup_host(&host, 0).await?;
            refuse_non_global(&host, addresses)
        })
    }
}

/// The MCP HTTP resolver at Full: private, loopback and link-local
/// addresses are allowed; unspecified and multicast addresses are refused.
struct FullLevelResolver;

impl reqwest013::dns::Resolve for FullLevelResolver {
    fn resolve(&self, name: reqwest013::dns::Name) -> reqwest013::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = super::http_request::lookup_host(&host, 0).await?;
            if addresses.is_empty() {
                return Err(format!("MCP HTTP host {host} did not resolve").into());
            }
            if addresses
                .iter()
                .any(|address| super::http_request::is_unroutable_ip(address.ip()))
            {
                return Err(format!("MCP HTTP host {host} resolves to an unspecified or multicast address").into());
            }
            let addresses: reqwest013::dns::Addrs = Box::new(addresses.into_iter());
            Ok(addresses)
        })
    }
}

/// The addresses of `host` when every one is globally routable.
fn refuse_non_global(
    host: &str,
    addresses: Vec<std::net::SocketAddr>,
) -> Result<reqwest013::dns::Addrs, Box<dyn std::error::Error + Send + Sync>> {
    if addresses.is_empty() {
        return Err(format!("MCP HTTP host {host} did not resolve").into());
    }
    if addresses
        .iter()
        .any(|address| super::http_request::is_non_global_ip(address.ip()))
    {
        return Err(format!("SSRF blocked: MCP HTTP host {host} resolves to a private/local address").into());
    }
    Ok(Box::new(addresses.into_iter()))
}

type StdioSessionSlot = Arc<tokio::sync::Mutex<Option<StdioSession>>>;

// ── Security: command whitelist & env var blocklist ──────────────────

/// Commands allowed from workspace `mcp.json` without pre-registration in
/// the global `config.toml`. These are common MCP server launchers.
static ALLOWED_MCP_COMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "npx", "node", "python", "python3", "uvx", "uv", "deno", "bun", "docker", "cargo", "go", "ruby", "php",
        "dotnet", "java",
    ])
});

#[derive(Debug, Clone, Default)]
struct DiscoveredToolMeta {
    description: Option<String>,
    input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeState {
    /// Invalidates pinned aliases across refreshes, including an ABA change
    /// back to an identical schema or destination after a session restart.
    mcp_state_epoch: u64,
    effective_config: McpConfig,
    mcp_json_mtime: Option<SystemTime>,
    /// Whether initial tool discovery has been performed at least once.
    initialized: bool,
    discovered_tools: HashMap<String, HashMap<String, DiscoveredToolMeta>>, // server -> tool -> meta
    discovery_errors: HashMap<String, String>,
    config_error: Option<String>,
    last_refresh_attempt: Option<SystemTime>,
    catalog_snapshot: super::SharedToolCatalogSnapshot,
    capability_specs: Vec<McpCapabilitySpec>,
    /// The registry's held level is Full: per-server allow/deny lists are
    /// not applied to the published catalog.
    full_level: bool,
    /// A caller that can run at Full exists although the registry's level is
    /// below Full: tools its allow/deny lists remove stay in the catalog,
    /// tagged `full_only`.
    keep_full_only: bool,
    /// Servers the workspace `mcp.json` narrows, as a caller below Full sees
    /// them (filled only with `keep_full_only`; `effective_config` then holds
    /// the unnarrowed definition for Full callers).
    below_full: HashMap<String, McpServerConfig>,
}

struct StdioSession {
    // Actual launch inputs, never printed; session names alone do not bind a destination.
    launch: McpServerConfig,
    client: McpClient,
    registration: crate::runtime::registry::WorkGuard,
}

impl StdioSession {
    async fn close(mut self) {
        if matches!(
            self.client.close_with_timeout(MCP_STDIO_CLOSE_TIMEOUT).await,
            Ok(Some(_))
        ) {
            self.registration.mark_reaped();
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerRuntimeInfo {
    pub name: String,
    pub transport: String,
    pub status: String,
    pub tool_count: usize,
    pub tools: Vec<String>,
    pub last_error: Option<String>,
    pub last_refresh_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpJsonRoot {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpJsonServer>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpJsonServer {
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
    /// Disable a server `config.toml` defines (the explicit way for a
    /// workspace to leave one out).
    #[serde(default)]
    disabled: bool,
}

/// One sentence per workspace `mcp.json` entry under `workspace_dir` that
/// did not start as written, with its remedy, for `prx doctor` (D-088 C-7).
/// Reads the file only; starts nothing.
#[must_use]
/// `level` is the configured autonomy level the registry would run at.
pub fn workspace_server_notes(
    base: &McpConfig,
    workspace_dir: &Path,
    level: crate::security::AutonomyLevel,
) -> Vec<String> {
    McpTool::new(
        Arc::new(SecurityPolicy {
            autonomy: level,
            ..SecurityPolicy::default()
        }),
        base.clone(),
        workspace_dir.to_path_buf(),
    )
    .workspace_server_notes()
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
    /// Where each stdio server gets its dedicated working directory
    /// (`<launch_root>/<server>-<hash>`) unless config.toml sets
    /// `working_dir`. `None` (no configuration directory): a stdio server
    /// without `working_dir` is refused, never started in the workspace
    /// (D-095 N-4).
    launch_root: Option<PathBuf>,
    state: RwLock<RuntimeState>,
    stdio_sessions: Mutex<HashMap<String, StdioSessionSlot>>,
    /// Servers a later configuration generation no longer runs: their
    /// sessions were stopped and this tool never starts them again
    /// (D-095 M-2).
    retired: Mutex<HashSet<String>>,
    lifecycle: tokio::sync::RwLock<()>,
}

pub struct McpStatusTool {
    mcp: Arc<McpTool>,
}

/// A currently discovered, allowed MCP capability with its exact schema.
/// Reading this snapshot does not refresh a server or start a process.
#[derive(Debug, Clone)]
pub(crate) struct McpCapabilitySpec {
    pub server: String,
    pub tool: String,
    pub spec: ToolSpec,
    /// Digest of the effective launch/routing config. Raw env values never
    /// leave the MCP tool or appear in catalog diagnostics.
    pub state_digest: [u8; 32],
    /// Hidden from and refused to callers below Full (see `level_view`).
    pub full_only: bool,
}

impl McpStatusTool {
    pub const fn new(mcp: Arc<McpTool>) -> Self {
        Self { mcp }
    }
}

/// Whether two definitions start the same process (or reach the same HTTP
/// endpoint): everything but the tool filters and the request timeout.
fn same_launch(a: &McpServerConfig, b: &McpServerConfig) -> bool {
    a.transport == b.transport
        && a.command == b.command
        && a.args == b.args
        && a.url == b.url
        && a.env == b.env
        && a.startup_timeout_ms == b.startup_timeout_ms
        && a.working_dir == b.working_dir
}

/// Put an MCP stdio server's child into the runtime work registry.
///
/// The transport owns the child, so the ledger row is bound to the returned
/// guard's scope instead of to a `Child` we hold. The pid doubles as the group
/// id because the command is spawned with `process_group(0)`.
fn register_mcp_child(server_name: &str, transport: &TokioChildProcess) -> crate::runtime::registry::WorkGuard {
    let pid = transport.id();
    let pgid = pid.and_then(|pid| i32::try_from(pid).ok()).filter(|pid| *pid > 0);
    crate::runtime::registry::register_process(&format!("mcp:{server_name}"), pid, pgid)
}

impl McpTool {
    pub fn new(security: Arc<SecurityPolicy>, base_config: McpConfig, workspace_dir: PathBuf) -> Self {
        let state = RuntimeState {
            mcp_state_epoch: 0,
            effective_config: base_config.clone(),
            mcp_json_mtime: None,
            initialized: false,
            discovered_tools: HashMap::new(),
            discovery_errors: HashMap::new(),
            config_error: None,
            last_refresh_attempt: None,
            catalog_snapshot: super::SharedToolCatalogSnapshot::default(),
            capability_specs: Vec::new(),
            full_level: security.autonomy == crate::security::AutonomyLevel::Full,
            keep_full_only: false,
            below_full: HashMap::new(),
        };

        let tool = Self {
            security,
            base_config,
            mcp_json_path: workspace_dir.join(MCP_JSON_FILE),
            launch_root: None,
            state: RwLock::new(state),
            stdio_sessions: Mutex::new(HashMap::new()),
            retired: Mutex::new(HashSet::new()),
            lifecycle: tokio::sync::RwLock::new(()),
        };
        tool.rebuild_catalog_snapshot();
        tool
    }

    /// Start stdio servers in dedicated directories under `launch_root`
    /// (production: `mcp/` beside `config.toml`, which the agent's workspace
    /// tools do not reach), never in the directory PRX was started from
    /// (D-088 C-3). Without it, only a server with an absolute `working_dir`
    /// starts (D-095 N-4).
    #[must_use]
    pub fn with_launch_root(mut self, launch_root: PathBuf) -> Self {
        self.launch_root = Some(launch_root);
        self
    }

    /// The servers this tool's configuration runs now, with their
    /// definitions: `config.toml` with the workspace `mcp.json` applied
    /// (`config.toml` alone when `mcp.json` is unreadable). Reads files
    /// only; starts nothing.
    fn effective_servers(&self) -> HashMap<String, McpServerConfig> {
        match self.load_workspace_overlay(&mut HashMap::new()) {
            Ok(Some((config, _))) => config.servers,
            Ok(None) | Err(_) => self.base_config.servers.clone(),
        }
    }

    /// The servers `next` runs exactly as this tool launches them: same
    /// transport, command, arguments, URL, environment, startup timeout,
    /// working directory and launch root. A server whose launch definition
    /// changed is not among them, so the process running the old
    /// definition is stopped when `next` replaces this tool (D-097 M-3).
    pub fn servers_launched_alike(&self, next: &Self) -> HashSet<String> {
        if self.launch_root != next.launch_root {
            return HashSet::new();
        }
        let ours = self.effective_servers();
        next.effective_servers()
            .into_iter()
            .filter(|(name, theirs)| ours.get(name).is_some_and(|mine| same_launch(mine, theirs)))
            .map(|(name, _)| name)
            .collect()
    }

    /// Stop every server of this tool that `keep` does not name, and never
    /// start it again from this tool (D-095 M-2). A configuration change
    /// that narrows `mcp.workspace_servers` or removes a server replaces this
    /// tool; turns pinned to the old generation may still hold it, so its
    /// sessions do not end on their own. Waits for calls in flight (they
    /// hold the lifecycle lock), then closes each session, which ends the
    /// child process. Returns how many running sessions were stopped.
    pub async fn retire_servers_except(&self, keep: &HashSet<String>) -> usize {
        let _lifecycle = self.lifecycle.write().await;
        let mut names = self
            .state
            .read()
            .effective_config
            .servers
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        names.extend(self.base_config.servers.keys().cloned());
        names.extend(self.stdio_sessions.lock().keys().cloned());
        names.retain(|name| !keep.contains(name));
        names.sort();
        names.dedup();
        self.retired.lock().extend(names.iter().cloned());
        let slots = {
            let mut sessions = self.stdio_sessions.lock();
            names
                .iter()
                .filter_map(|name| sessions.remove(name))
                .collect::<Vec<_>>()
        };
        let mut stopped = 0;
        for slot in slots {
            let session = slot.lock().await.take();
            if let Some(session) = session {
                session.close().await;
                stopped += 1;
            }
        }
        stopped
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

    /// Publish fixture discovery through the real state/snapshot producer, with
    /// no transport configured and no remote discovery or process launch.
    #[cfg(test)]
    pub(crate) fn test_publish_inventory(&self, server: &str, count: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            [0, 1, 2, 16, 17, 64, 65, 256, 257, 512, 513].contains(&count),
            "unsupported fixture inventory"
        );
        let mut state = self.state.write();
        let config = state
            .effective_config
            .servers
            .get(server)
            .ok_or_else(|| anyhow::anyhow!("fixture server missing"))?;
        anyhow::ensure!(
            config.command.is_none() && config.url.is_none(),
            "fixture transport must be absent"
        );
        let tools = (0..count)
            .map(|index| {
                (
                    format!("inventory_{index:04}"),
                    DiscoveredToolMeta {
                        description: Some("Read neutral fixture metadata".into()),
                        input_schema: Some(
                            json!({"type":"object","properties":{"ordinal":{"type":"integer","const":index}}}),
                        ),
                    },
                )
            })
            .collect();
        state.discovered_tools.insert(server.to_owned(), tools);
        state.mcp_state_epoch = state
            .mcp_state_epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("fixture epoch exhausted"))?;
        state.initialized = true;
        Self::publish_catalog_snapshot(&mut state, self.spec());
        Ok(())
    }

    pub(crate) fn capability_snapshot(&self) -> Vec<McpCapabilitySpec> {
        self.state.read().capability_specs.clone()
    }

    fn rebuild_catalog_snapshot(&self) {
        Self::publish_catalog_snapshot(&mut self.state.write(), self.spec());
    }

    fn publish_catalog_snapshot(state: &mut RuntimeState, root: ToolSpec) {
        let capabilities = Self::capability_specs_from_state(state);
        let mut specs = Vec::with_capacity(capabilities.len() + 1);
        specs.push((root, None));
        specs.extend(
            capabilities
                .iter()
                .map(|item| (item.spec.clone(), Some(item.state_digest))),
        );
        state.catalog_snapshot = super::SharedToolCatalogSnapshot::new(specs);
        state.capability_specs = capabilities;
    }

    fn capability_specs_from_state(state: &RuntimeState) -> Vec<McpCapabilitySpec> {
        let mut snapshot = Vec::new();
        // Epoch exhaustion cannot re-authorize an old reference.
        if state.mcp_state_epoch == u64::MAX {
            return snapshot;
        }
        for (server, tools) in &state.discovered_tools {
            let Some(server_config) = state.effective_config.servers.get(server) else {
                continue;
            };
            // `env` is a HashMap; canonicalize through Value's sorted map so
            // equivalent effective launch configs have the same generation.
            let Ok(config_bytes) = serde_json::to_value(server_config).and_then(|value| serde_json::to_vec(&value))
            else {
                continue;
            };
            let mut state_hasher = sha2::Sha256::new();
            state_hasher.update(b"prx.mcp.runtime-binding.v1\0");
            state_hasher.update(state.mcp_state_epoch.to_be_bytes());
            state_hasher.update(config_bytes);
            let state_digest: [u8; 32] = state_hasher.finalize().into();
            for (tool, metadata) in tools {
                // At Full the per-server allow/deny lists are ignored. Below
                // Full a filtered tool is dropped, or kept for Full callers.
                let full_only = !state.full_level && state.tool_full_only(server, server_config, tool);
                if full_only && !state.keep_full_only {
                    continue;
                }
                snapshot.push(McpCapabilitySpec {
                    server: server.clone(),
                    tool: tool.clone(),
                    state_digest,
                    full_only,
                    spec: ToolSpec {
                        name: Self::alias_name(&server_config.tool_name_prefix, server, tool),
                        description: metadata
                            .description
                            .clone()
                            .unwrap_or_else(|| format!("MCP tool '{tool}' from server '{server}'")),
                        parameters: metadata
                            .input_schema
                            .clone()
                            .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                    },
                });
            }
        }
        snapshot.sort_by(|left, right| left.spec.name.cmp(&right.spec.name));
        snapshot
    }

    pub fn server_runtime_info(&self) -> Vec<McpServerRuntimeInfo> {
        let state = self.state.read();
        let last_refresh_at = state
            .last_refresh_attempt
            .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339());
        let mut servers = state
            .effective_config
            .servers
            .iter()
            .map(|(name, config)| {
                let mut tools = state
                    .discovered_tools
                    .get(name)
                    .map(|entries| entries.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                tools.sort();
                let last_error = state.discovery_errors.get(name).cloned();
                let status = if last_error.is_some() {
                    "error"
                } else if state.discovered_tools.contains_key(name) {
                    "healthy"
                } else if state.initialized {
                    "unavailable"
                } else {
                    "configured"
                };
                McpServerRuntimeInfo {
                    name: name.clone(),
                    transport: match config.transport {
                        McpTransport::Stdio => "stdio",
                        McpTransport::Http => "http",
                    }
                    .to_string(),
                    status: status.to_string(),
                    tool_count: tools.len(),
                    tools,
                    last_error,
                    last_refresh_at: last_refresh_at.clone(),
                }
            })
            .collect::<Vec<_>>();
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        servers
    }

    pub fn config_error(&self) -> Option<String> {
        self.state.read().config_error.clone()
    }

    fn alias_name(prefix: &str, server: &str, tool: &str) -> String {
        format!("{prefix}__{server}__{tool}")
    }

    /// Strip PRX's runtime-only arguments off an argument object about to be
    /// forwarded to a third-party MCP server.
    ///
    /// The alias form of this tool forwards its whole root argument object, and
    /// by the time a tool runs the runtime has injected `_zc_scope` (workspace,
    /// owner and principal ids), `_zc_scope_trusted` and the approval grant. An
    /// external MCP server is an untrusted process: handing it PRX's internal
    /// identity leaks it, and handing it a grant hands over a replayable
    /// authorization token. `strip_runtime_only_args` is the single source of
    /// truth for which keys those are, so a runtime key added later is covered
    /// here for free.
    fn scrub_forwarded_arguments(
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        arguments.map(|mut arguments| {
            crate::tools::execution::strip_runtime_only_args(&mut arguments);
            arguments
        })
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

    // ── Security helpers ────────────────────────────────────────────

    /// Validate that an HTTP/SSE URL does not target private or local addresses.
    ///
    /// Reuses the SSRF defense from [`super::http_request`]: extracts the host,
    /// then checks for loopback, RFC-1918 private ranges, link-local, multicast,
    /// and DNS-rebinding (resolved IPs).  Returns `Ok(())` when the host is
    /// globally routable, or an error describing the block reason.
    fn validate_http_url(url: &str) -> anyhow::Result<()> {
        let host = super::http_request::extract_host(url)?;
        if super::http_request::is_private_or_local_host(&host) {
            bail!("SSRF blocked: MCP HTTP URL resolves to a private/local address (host: {host})");
        }
        Ok(())
    }

    /// [`Self::validate_http_url`] below Full. At Full (`full`) private,
    /// loopback and link-local hosts are allowed; userinfo and unspecified or
    /// multicast hosts are refused at every level.
    fn validate_http_target(url: &str, full: bool) -> anyhow::Result<()> {
        if !full {
            return Self::validate_http_url(url);
        }
        let host = super::http_request::extract_host(url)?;
        if super::http_request::is_unroutable_host(&host) {
            bail!("MCP HTTP URL targets an unspecified or multicast address (host: {host})");
        }
        Ok(())
    }

    /// Whether this registry runs at Full (its held policy's level). Used
    /// for load-time decisions, which have no call.
    fn full(&self) -> bool {
        self.security.autonomy == crate::security::AutonomyLevel::Full
    }

    /// Whether the current call runs at Full: the per-call level the
    /// execution service set, else the held policy's level. Used for
    /// dispatch-time decisions.
    fn call_full(&self) -> bool {
        crate::tools::effective_autonomy::call_policy(&self.security).autonomy == crate::security::AutonomyLevel::Full
    }

    /// Validate that a stdio server's command is safe to execute.
    ///
    /// A command is allowed if:
    /// 1. The server is pre-registered in `base_config.servers` **AND** the
    ///    command matches the one in base_config (prevents same-name override), OR
    /// 2. The command is a bare name (no path separators) that appears in
    ///    `ALLOWED_MCP_COMMANDS` (common MCP launchers).
    ///
    /// Commands containing path separators (`/` or `\`) are **always rejected**
    /// for non-pre-registered servers, preventing basename whitelist bypass
    /// (e.g. `/tmp/node` would no longer match the "node" whitelist entry).
    ///
    /// Returns `true` if the command is allowed, `false` otherwise.
    fn is_command_allowed(&self, server_name: &str, command: &str) -> bool {
        // Check 1: server is pre-registered in the global config (config.toml).
        // Only allow if the command matches the base_config command exactly.
        if let Some(base_server) = self.base_config.servers.get(server_name) {
            return base_server.command.as_deref() == Some(command);
        }

        // Check 2: reject any command containing path separators.
        // This prevents basename whitelist bypass (e.g. "/tmp/node" matching "node").
        if command.contains('/') || command.contains('\\') {
            return false;
        }

        // Check 3: bare command name must be a well-known MCP launcher.
        ALLOWED_MCP_COMMANDS.contains(command)
    }

    /// Remove every variable a workspace may not set from a server config's
    /// env map: dynamic-loader variables (`LD_*`, `DYLD_*`, ...),
    /// interpreter start-up hooks and `PATH`, matched by name and by prefix
    /// in any case through the shared launch classifier
    /// (`config::mcp_launch::classify_variable`, D-049 E). Returns the
    /// removed names (for logging; never a value).
    fn sanitize_env_vars(env: &mut HashMap<String, String>) -> Vec<String> {
        let mut removed = Vec::new();
        env.retain(
            |key, _| match crate::config::mcp_launch::classify_variable(key).refusal() {
                Some(_) => {
                    removed.push(key.clone());
                    false
                }
                None => true,
            },
        );
        removed.sort();
        removed
    }

    // ── Config loading ──────────────────────────────────────────────

    #[cfg(test)]
    fn load_effective_config_from_json(&self) -> anyhow::Result<Option<McpConfig>> {
        Ok(self.load_effective_config_with_view()?.map(|(config, _)| config))
    }

    /// The effective config and, with `keep_full_only`, the narrowed view
    /// of the servers the workspace `mcp.json` narrows.
    fn load_effective_config_with_view(
        &self,
    ) -> anyhow::Result<Option<(McpConfig, HashMap<String, McpServerConfig>)>> {
        let mut below_full = HashMap::new();
        Ok(self
            .load_workspace_overlay(&mut below_full)?
            .map(|(config, _)| (config, below_full)))
    }

    /// One sentence per workspace `mcp.json` entry that did not start as
    /// written (blocked, disabled or trimmed), each with its remedy; empty
    /// without an `mcp.json`. Names and transports only, never a value
    /// (D-088 C-7). `mcp_status` and `prx doctor` report these.
    pub fn workspace_server_notes(&self) -> Vec<String> {
        match self.load_workspace_overlay(&mut HashMap::new()) {
            Ok(Some((_, notes))) => notes,
            Ok(None) => Vec::new(),
            Err(error) => vec![format!(
                "workspace mcp.json at {} could not be read ({error}); only the servers config.toml defines run",
                self.mcp_json_path.display()
            )],
        }
    }

    /// The effective configuration with the workspace `mcp.json` applied,
    /// and a note for every entry that did not start as written.
    ///
    /// The servers `config.toml` defines stay unless an entry of the same
    /// name explicitly narrows them (`"disabled": true`, or an allow-list
    /// that leaves no tool): a non-empty `mcp.json` never silently empties
    /// the configured set (D-088 C-7).
    #[allow(clippy::too_many_lines)]
    fn load_workspace_overlay(
        &self,
        below_full: &mut HashMap<String, McpServerConfig>,
    ) -> anyhow::Result<Option<(McpConfig, Vec<String>)>> {
        if !self.mcp_json_path.exists() {
            return Ok(None);
        }

        let raw = std::fs::read_to_string(&self.mcp_json_path)?;
        let parsed: McpJsonRoot = serde_json::from_str(&raw)?;

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

    fn convert_json_server(server: McpJsonServer) -> McpServerConfig {
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
        let Some(server) = cfg.servers.get(name) else {
            bail!("Unknown MCP server '{name}'");
        };

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

        let output = if content.is_empty() { value.to_string() } else { content };

        (!is_error, output)
    }

    fn call_tool_params(
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> CallToolRequestParams {
        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        params
    }

    /// The command of a stdio MCP server, for both of its launch paths
    /// (discovery and call sessions).
    ///
    /// The child starts from a cleared environment: it inherits only the
    /// launch allowlist of PRX's own environment (`PATH`, `HOME`, `LANG`,
    /// `LC_*`, `TMPDIR`) plus the server's configured `env`, never the rest
    /// (provider keys exported in PRX's shell, other tools' tokens) (D-045,
    /// `config::mcp_launch`). It runs in its own process group, matching
    /// every other child PRX spawns, so an MCP server that forks helpers of
    /// its own can be ended as a whole, and it is killed when its handle is
    /// dropped. It starts in `working_dir` (its dedicated or configured
    /// directory), never in PRX's own working directory (D-088 C-3).
    fn stdio_command(server: &McpServerConfig, command: &str, working_dir: &Path) -> Command {
        let mut cmd = Command::new(command);
        cmd.args(&server.args);
        cmd.current_dir(working_dir);
        cmd.env_clear();
        cmd.envs(crate::config::mcp_launch::launch_environment(
            std::env::vars_os(),
            &server.env,
        ));
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
    }

    /// The working directory of stdio server `server_name` (D-088 C-3,
    /// D-095 N-4): the operator's absolute `working_dir`, which must be an
    /// existing directory and not a symlink, or else its dedicated directory
    /// under `launch_root`, created owner-only. A symlink or a non-directory
    /// where the dedicated directory (or its root) should be is refused,
    /// never followed or `chmod`ed. Without a launch root, a server without
    /// `working_dir` is refused.
    async fn prepare_launch_directory(
        launch_root: Option<&Path>,
        server_name: &str,
        server: &McpServerConfig,
    ) -> anyhow::Result<PathBuf> {
        let refuse_link = |path: &Path, what: &str| -> anyhow::Result<bool> {
            crate::config::mcp_launch::existing_real_directory(path, what)
                .map_err(|error| anyhow::anyhow!("MCP server '{}': {error}", server_name.escape_default()))
        };
        if server.working_dir.is_some() {
            let dir =
                crate::config::mcp_launch::launch_directory(Path::new(""), server_name, server.working_dir.as_deref())
                    .map_err(|error| anyhow::anyhow!(error))?;
            if !refuse_link(&dir, "working_dir")? {
                bail!(
                    "MCP server '{}': its working_dir {} does not exist",
                    server_name.escape_default(),
                    dir.display()
                );
            }
            return Ok(dir);
        }
        let Some(root) = launch_root else {
            bail!(
                "MCP server '{}' has no launch directory: PRX starts a stdio server in `mcp/<server>` beside \
                 config.toml, and this process has no configuration directory; set an absolute `working_dir`",
                server_name.escape_default()
            );
        };
        let dir = crate::config::mcp_launch::launch_directory(root, server_name, None)
            .map_err(|error| anyhow::anyhow!(error))?;
        crate::config::mcp_launch::prepare_launch_directory(root, &dir)
            .await
            .map_err(|error| anyhow::anyhow!("MCP server '{}': {error}", server_name.escape_default()))?;
        Ok(dir)
    }

    async fn discover_server_tools_stdio(
        server_name: &str,
        server: &McpServerConfig,
        launch_root: Option<&Path>,
    ) -> anyhow::Result<HashMap<String, DiscoveredToolMeta>> {
        let command = server
            .command
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' uses stdio but command is missing"))?;

        let working_dir = Self::prepare_launch_directory(launch_root, server_name, server).await?;
        let cmd = Self::stdio_command(server, command, &working_dir);

        let startup_timeout = Duration::from_millis(server.startup_timeout_ms);
        // `TokioChildProcess::new` deliberately replaces the command's stderr
        // with `inherit`, even if callers configured it beforehand. Use the
        // builder override so package-manager notices cannot corrupt ratatui's
        // alternate screen. MCP/transport failures still surface structurally.
        let (transport, _stderr) = TokioChildProcess::builder(cmd).stderr(Stdio::null()).spawn()?;
        let _registration = register_mcp_child(server_name, &transport);
        let client = tokio::time::timeout(startup_timeout, ().serve(transport))
            .await
            .map_err(|_| anyhow::anyhow!("MCP startup timed out after {} ms", server.startup_timeout_ms))??;

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

    /// Open a streamable-HTTP MCP session to `url`, which the caller has
    /// already passed through [`Self::validate_http_url`].
    ///
    /// Both HTTP entry points (discovery and `call_http`) connect here, so
    /// the transport's HTTP client is chosen in one place.
    async fn connect_http(url: &str, startup_timeout_ms: u64, full: bool) -> anyhow::Result<McpClient> {
        let transport = StreamableHttpClientTransport::with_client(
            mcp_http_client(full)?,
            StreamableHttpClientTransportConfig::with_uri(url),
        );
        tokio::time::timeout(Duration::from_millis(startup_timeout_ms), ().serve(transport))
            .await
            .map_err(|_| anyhow::anyhow!("MCP startup timed out after {startup_timeout_ms} ms"))?
            .map_err(Into::into)
    }

    async fn discover_server_tools_http(
        server_name: &str,
        server: &McpServerConfig,
        full: bool,
    ) -> anyhow::Result<HashMap<String, DiscoveredToolMeta>> {
        let url = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' uses http but url is missing"))?;

        // ── SSRF protection: block private/local addresses below Full ──
        Self::validate_http_target(url, full)?;

        let client = Self::connect_http(url, server.startup_timeout_ms, full).await?;

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
        launch_root: Option<&Path>,
        full: bool,
    ) -> anyhow::Result<HashMap<String, DiscoveredToolMeta>> {
        match server.transport {
            McpTransport::Stdio => Self::discover_server_tools_stdio(server_name, server, launch_root).await,
            McpTransport::Http => Self::discover_server_tools_http(server_name, server, full).await,
        }
    }

    async fn call_stdio(
        &self,
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
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' uses stdio but command is missing"))?;

        {
            let redacted_args = arguments
                .as_ref()
                .map(|a| crate::agent::diagnostics::redact_sensitive_json_keys(&serde_json::Value::Object(a.clone())));
            tracing::debug!(
                server = server_name,
                tool = tool_name,
                args = ?redacted_args,
                "MCP call_stdio: invoking tool"
            );
        }

        let startup_timeout = Duration::from_millis(server.startup_timeout_ms);
        let request_timeout = Duration::from_millis(server.request_timeout_ms);

        let slot = {
            let mut sessions = self.stdio_sessions.lock();
            Arc::clone(
                sessions
                    .entry(server_name.to_string())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
            )
        };
        let mut session = slot.lock().await;
        if session
            .as_ref()
            .is_some_and(|active| !same_launch(&active.launch, server))
        {
            bail!("MCP stdio session launch differs from the captured configuration; refresh is required");
        }

        if session
            .as_ref()
            .is_some_and(|session| session.client.is_closed() || session.client.is_transport_closed())
        {
            if let Some(stale) = session.take() {
                stale.close().await;
            }
        }

        if session.is_none() {
            let working_dir = Self::prepare_launch_directory(self.launch_root.as_deref(), server_name, server).await?;
            let cmd = Self::stdio_command(server, command, &working_dir);

            // See `discover_stdio`: the builder override is required because
            // the convenience constructor always inherits stderr.
            let (transport, _stderr) = TokioChildProcess::builder(cmd).stderr(Stdio::null()).spawn()?;
            let registration = register_mcp_child(server_name, &transport);
            let client = tokio::time::timeout(startup_timeout, ().serve(transport))
                .await
                .map_err(|_| anyhow::anyhow!("MCP startup timed out after {} ms", server.startup_timeout_ms))??;
            *session = Some(StdioSession {
                launch: server.clone(),
                client,
                registration,
            });
        }

        let Some(active_session) = session.as_ref() else {
            bail!("MCP stdio session for '{server_name}' was not initialized");
        };
        let client = &active_session.client;

        let result = match tokio::time::timeout(
            request_timeout,
            client.call_tool(Self::call_tool_params(tool_name, arguments)),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                if let Some(failed) = session.take() {
                    failed.close().await;
                }
                return Err(anyhow::anyhow!(
                    "MCP call timed out after {} ms; stdio session was reset and the call was not replayed",
                    server.request_timeout_ms
                ));
            }
        };

        match result {
            Ok(r) => {
                let value = serde_json::to_value(r)?;
                let result_type = if value.is_object() {
                    "object"
                } else if value.is_array() {
                    "array"
                } else if value.is_string() {
                    "string"
                } else {
                    "other"
                };
                let result_bytes = value.to_string().len();
                tracing::debug!(
                    server = server_name,
                    tool = tool_name,
                    result_type,
                    result_bytes,
                    "MCP call_stdio: result received"
                );
                Ok(Self::extract_call_success_and_output(&value))
            }
            Err(e) => {
                tracing::warn!(server = server_name, tool = tool_name, error = %e, "MCP call_stdio: error");
                if matches!(
                    e,
                    rmcp::service::ServiceError::TransportSend(_)
                        | rmcp::service::ServiceError::TransportClosed
                        | rmcp::service::ServiceError::UnexpectedResponse
                        | rmcp::service::ServiceError::Cancelled { .. }
                        | rmcp::service::ServiceError::Timeout { .. }
                ) && let Some(failed) = session.take()
                {
                    failed.close().await;
                }
                Err(e.into())
            }
        }
    }

    async fn close_stdio_sessions(&self) {
        let sessions = {
            let mut sessions = self.stdio_sessions.lock();
            sessions.drain().map(|(_, slot)| slot).collect::<Vec<_>>()
        };

        for slot in sessions {
            let session = {
                let mut session = slot.lock().await;
                session.take()
            };
            if let Some(session) = session {
                session.close().await;
            }
        }
    }

    async fn call_http(
        server_name: &str,
        server: &McpServerConfig,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        full: bool,
    ) -> anyhow::Result<(bool, String)> {
        let url = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' uses http but url is missing"))?;

        // ── SSRF protection: block private/local addresses below Full ──
        Self::validate_http_target(url, full)?;

        let request_timeout = Duration::from_millis(server.request_timeout_ms);
        let client = Self::connect_http(url, server.startup_timeout_ms, full).await?;

        let result = tokio::time::timeout(
            request_timeout,
            client.call_tool(Self::call_tool_params(tool_name, arguments)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("MCP call timed out after {} ms", server.request_timeout_ms))??;

        let _ = client.cancel().await;
        let value = serde_json::to_value(result)?;
        Ok(Self::extract_call_success_and_output(&value))
    }

    async fn refresh_runtime_state(&self, force: bool) {
        let _lifecycle = self.lifecycle.write().await;
        let file_mtime = Self::file_mtime(&self.mcp_json_path).ok().flatten();
        let (current_mtime, initialized, has_errors, last_refresh_attempt) = {
            let state = self.state.read();
            (
                state.mcp_json_mtime,
                state.initialized,
                !state.discovery_errors.is_empty(),
                state.last_refresh_attempt,
            )
        };

        // Skip if already initialized and config file hasn't changed.
        // Note: `None == None` when there is no mcp.json — we must NOT skip on the
        // very first call (before `initialized` is set to true), otherwise tools are
        // never discovered when the user relies purely on config.toml.
        if !force && initialized && file_mtime == current_mtime {
            if !has_errors
                || last_refresh_attempt
                    .and_then(|attempt| attempt.elapsed().ok())
                    .is_some_and(|elapsed| elapsed < MCP_DISCOVERY_RETRY_BACKOFF)
            {
                return;
            }
        }

        // A forced refresh or config change is an explicit lifecycle boundary:
        // close stateful stdio servers before rediscovery so calls cannot retain
        // stale configuration or browser/session state from the old generation.
        self.close_stdio_sessions().await;

        let (new_config, below_full, config_error) = match self.load_effective_config_with_view() {
            Ok(Some((from_file, below_full))) => (from_file, below_full, None),
            Ok(None) => (self.base_config.clone(), HashMap::new(), None),
            Err(error) => {
                tracing::warn!(path = %self.mcp_json_path.display(), error = %error, "MCP config reload failed");
                (self.base_config.clone(), HashMap::new(), Some(error.to_string()))
            }
        };

        let mut discovered = HashMap::new();
        let mut discovery_errors = HashMap::new();
        let retired = self.retired.lock().clone();
        for (server_name, server) in new_config.servers.iter().filter(|(name, _)| !retired.contains(*name)) {
            match Self::discover_server_tools(server_name, server, self.launch_root.as_deref(), self.full()).await {
                Ok(tools) => {
                    discovered.insert(server_name.clone(), tools);
                }
                Err(error) => {
                    // A transport error quotes the server URL, userinfo
                    // included; neither the log nor the status API shows it.
                    let error = crate::security::redaction::redact_provider_text(&error.to_string());
                    tracing::warn!(server = server_name, error = %error, "MCP tool discovery failed");
                    discovery_errors.insert(server_name.clone(), error);
                }
            }
        }

        let mut state = self.state.write();
        state.mcp_state_epoch = state.mcp_state_epoch.saturating_add(1);
        state.effective_config = new_config;
        state.below_full = below_full;
        state.discovered_tools = discovered;
        state.discovery_errors = discovery_errors;
        state.config_error = config_error;
        state.mcp_json_mtime = file_mtime;
        state.initialized = true;
        state.last_refresh_attempt = Some(SystemTime::now());
        Self::publish_catalog_snapshot(&mut state, self.spec());
    }

    /// Synchronize local metadata before catalog publication, without remote
    /// discovery or starting an MCP process. Root specs remain pure.
    pub(crate) fn refresh_catalog_config(&self) {
        self.refresh_state_from_file_only();
    }

    fn refresh_state_from_file_only(&self) {
        let file_mtime = Self::file_mtime(&self.mcp_json_path).ok().flatten();
        let current_mtime = self.state.read().mcp_json_mtime;
        if file_mtime == current_mtime {
            return;
        }

        let (new_config, below_full, config_error) = match self.load_effective_config_with_view() {
            Ok(Some((from_file, below_full))) => (from_file, below_full, None),
            Ok(None) => (self.base_config.clone(), HashMap::new(), None),
            Err(error) => (self.base_config.clone(), HashMap::new(), Some(error.to_string())),
        };

        let mut state = self.state.write();
        state.mcp_state_epoch = state.mcp_state_epoch.saturating_add(1);
        state.effective_config = new_config;
        state.below_full = below_full;
        state.discovered_tools.clear();
        state.discovery_errors.clear();
        state.config_error = config_error;
        state.mcp_json_mtime = file_mtime;
        state.initialized = false;
        state.last_refresh_attempt = None;
        Self::publish_catalog_snapshot(&mut state, self.spec());
    }

    async fn validate_and_call(
        &self,
        public_name: &str,
        server_name: String,
        tool_name: String,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> anyhow::Result<(bool, String)> {
        let _lifecycle = self.lifecycle.read().await;
        if self.retired.lock().contains(&server_name) {
            bail!(
                "MCP server '{}' was removed or narrowed by a configuration change; it is no longer started",
                server_name.escape_default()
            );
        }
        // The backend may have refreshed after the executor's final parity check.
        // Verify the pinned selection against the same publication from which
        // this call captures its actual destination, while lifecycle read
        // ownership prevents asynchronous refresh/retirement through transport.
        // A synchronous file-only update cannot change this captured config;
        // stdio separately checks its actual launched-session inputs.
        let server = {
            let state = self.state.read();
            let published = state.catalog_snapshot.entry(public_name);
            crate::tools::capability::execution_binding::verify_current_dispatch(
                MCP_ROOT_NAME,
                public_name,
                published.map(|entry| crate::tools::capability::contract::SchemaFingerprint(entry.fingerprint())),
                published.and_then(|entry| entry.binding_digest()),
            )?;
            let server = Self::resolve_server(&state.effective_config, &server_name)?;
            if !self.call_full() && state.tool_full_only(&server_name, server, &tool_name) {
                bail!("Tool '{tool_name}' is blocked by allow/deny rules for MCP server '{server_name}'");
            }
            if let Some(discovered) = state.discovered_tools.get(&server_name)
                && !discovered.is_empty()
                && !discovered.contains_key(&tool_name)
            {
                let available = discovered.keys().cloned().collect::<Vec<_>>().join(", ");
                bail!("Tool '{tool_name}' not found on MCP server '{server_name}'. Available: [{available}]");
            }
            server.clone()
        };

        match server.transport {
            McpTransport::Stdio => self.call_stdio(&server_name, &server, &tool_name, arguments).await,
            McpTransport::Http => Self::call_http(&server_name, &server, &tool_name, arguments, self.call_full()).await,
        }
    }
}

impl McpTool {
    fn execution_schema(&self) -> &'static std::sync::Arc<serde_json::Value> {
        static SCHEMA: std::sync::LazyLock<std::sync::Arc<serde_json::Value>> = std::sync::LazyLock::new(|| {
            std::sync::Arc::new({
                // The root schema is independent of catalog size. Exact remote names
                // and schemas are supplied only by turn-scoped capability discovery.
                let server_schema = json!({
                    "type": "string",
                    "description": "Configured MCP server name; required for 'call'"
                });
                let tool_schema = json!({
                    "type": "string",
                    "description": "Remote MCP tool name to invoke; required for 'call'"
                });

                crate::tools::schema::with_default_action_requirements(
                    json!({
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["call", "list", "status", "refresh"],
                                "default": "call",
                                "description": "'call' invokes a remote tool; 'list' and 'status' inspect discovery state and errors; 'refresh' forces rediscovery"
                            },
                            "server": server_schema,
                            "tool": tool_schema,
                            "arguments": {
                                "type": "object",
                                "description": "Arguments object forwarded to MCP call_tool for 'call'",
                                "default": {}
                            }
                        },
                        "required": ["action"],
                        "additionalProperties": false
                    }),
                    "action",
                    "call",
                    &[crate::tools::schema::ActionRequirement {
                        action: "call",
                        required: &["server", "tool"],
                    }],
                )
            })
        });
        &SCHEMA
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        MCP_ROOT_NAME
    }

    fn description(&self) -> &str {
        "Call tools exposed by configured MCP servers: 'call' invokes a remote tool, \
         'list' and 'status' report discovery results and errors, and 'refresh' forces rediscovery. \
         Supports hot config reload from workspace/mcp.json and dynamic aliases."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tools::schema::publish_flat_schema(self.execution_schema())
    }

    fn execution_parameters_schema(&self) -> Option<std::sync::Arc<serde_json::Value>> {
        Some(std::sync::Arc::clone(self.execution_schema()))
    }

    fn specs(&self) -> Vec<ToolSpec> {
        vec![self.spec()]
    }

    fn catalog_specs(&self) -> Vec<ToolSpec> {
        let state = self.state.read();
        let mut specs = Vec::with_capacity(state.capability_specs.len() + 1);
        specs.push(self.spec());
        specs.extend(state.capability_specs.iter().map(|item| item.spec.clone()));
        specs
    }

    fn shared_catalog_snapshot(&self) -> Option<super::SharedToolCatalogSnapshot> {
        Some(self.state.read().catalog_snapshot.clone())
    }

    fn catalog_availability_is_configured(&self) -> bool {
        let state = self.state.read();
        state.config_error.is_some() || !state.effective_config.servers.is_empty()
    }

    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        self.state
            .read()
            .catalog_snapshot
            .entries()
            .iter()
            .filter_map(|entry| entry.binding_digest().map(|digest| (entry.spec().name.clone(), digest)))
            .collect()
    }

    fn spec_for_name(&self, name: &str) -> Option<ToolSpec> {
        self.state
            .read()
            .catalog_snapshot
            .entry(name)
            .map(|entry| entry.spec().clone())
    }

    fn supports_name(&self, name: &str) -> bool {
        if name == MCP_ROOT_NAME {
            return true;
        }

        let state = self.state.read();
        Self::parse_alias_name(&state.effective_config, name).is_some()
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        self.refresh_runtime_state(false).await;
        Ok(())
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_named(MCP_ROOT_NAME, args).await
    }

    async fn execute_named(&self, name: &str, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if name == MCP_ROOT_NAME {
            crate::tools::schema::validate_execution_contract(self.name(), self.execution_schema(), &mut args)?;
        }
        let action = if name == MCP_ROOT_NAME {
            args.get("action").and_then(serde_json::Value::as_str).unwrap_or("call")
        } else {
            "call"
        };
        if name == MCP_ROOT_NAME && matches!(action, "list" | "status" | "refresh") {
            self.refresh_runtime_state(action == "refresh").await;
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "initialized": self.state.read().initialized,
                    "config_error": self.config_error(),
                    "servers": self.server_runtime_info(),
                }))?,
                error: None,
            });
        }
        if action != "call" {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unknown MCP action '{action}'")),
            });
        }
        if !crate::tools::effective_autonomy::call_policy(&self.security).can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        self.refresh_runtime_state(false).await;

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
            let arguments = args.get("arguments").and_then(serde_json::Value::as_object).cloned();
            (server_name, tool_name, arguments)
        } else {
            let state = self.state.read();
            let (server_name, tool_name) = Self::parse_alias_name(&state.effective_config, name)
                .ok_or_else(|| anyhow::anyhow!("Unknown MCP alias tool '{name}'"))?;
            drop(state);

            let arguments = args
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("MCP alias tool '{name}' expects object arguments"))?;
            (server_name, tool_name, Some(arguments))
        };

        // `args` itself is left intact — the SideEffectGate below still reads
        // the runtime approval grant out of it.
        let arguments = Self::scrub_forwarded_arguments(arguments);

        // SideEffectGate: forwarding a tool call to an external MCP server runs
        // arbitrary, potentially irreversible side effects on an untrusted
        // process/endpoint — the largest un-gated surface (reaudit P0-C). Gate it
        // as a High-risk resource operation bound to the exact `{server}:{tool}`
        // target so an approval for server-A can never authorize server-B
        // (op naming per design d03).
        let operation_name = op_id::op_id(self.name(), "call", &[&server_name, &tool_name]);
        // The grant names the capability that was approved: the alias public
        // name for a discovered tool, the root name for a root call. The
        // operation id stays root-based so one approval never crosses targets.
        let approval_grant = ApprovalGrant::from_runtime_args(name, &args);
        if let Err(error) = SideEffectGate::new(&crate::tools::effective_autonomy::call_policy(&self.security))
            .authorize_resource_operation(name, &operation_name, ResourceRiskLevel::High, approval_grant.as_ref())
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let call_result = self.validate_and_call(name, server_name, tool_name, arguments).await;

        match call_result {
            Ok((success, output)) => Ok(ToolResult {
                success,
                output,
                error: None,
            }),
            // A transport error can quote the server URL; the model never
            // sees its userinfo (D-050).
            Err(err) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(crate::security::redaction::redact_provider_text(&format!("{err:#}"))),
            }),
        }
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        let state = self.state.read();
        if state.config_error.is_some() {
            return crate::capability::CapabilityAvailability::configured("workspace MCP configuration is invalid");
        }
        if state.effective_config.servers.is_empty() {
            return crate::capability::CapabilityAvailability::declared("no MCP servers are configured");
        }
        if !state.discovery_errors.is_empty() {
            return crate::capability::CapabilityAvailability::configured(format!(
                "MCP configuration/discovery degraded: {} server error(s)",
                state.discovery_errors.len(),
            ));
        }
        if state.initialized {
            crate::capability::CapabilityAvailability::healthy(format!(
                "{} MCP server(s) discovered successfully",
                state.discovered_tools.len()
            ))
        } else {
            crate::capability::CapabilityAvailability::configured(format!(
                "{} MCP server(s) configured; discovery pending",
                state.effective_config.servers.len()
            ))
        }
    }
}

#[async_trait]
impl Tool for McpStatusTool {
    fn name(&self) -> &str {
        "mcp_status"
    }

    fn description(&self) -> &str {
        "Discover configured MCP servers and report tools, transport, health, and the latest error without invoking a remote tool."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"refresh": {"type": "boolean", "default": false}},
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let force = args
            .get("refresh")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        self.mcp.refresh_runtime_state(force).await;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "config_error": self.mcp.config_error(),
                "servers": self.mcp.server_runtime_info(),
                "workspace_servers": self.mcp.workspace_server_notes(),
                "alias_exposure": "turn_scoped",
            }))?,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        crate::capability::CapabilityAvailability::ready(
            "local MCP diagnostics are executable even when no remote server is configured",
        )
    }
}

#[cfg(test)]
mod tests;

pub(super) mod level_view;

#[cfg(test)]
mod dispatch_binding_tests;
