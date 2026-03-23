use crate::auth::codex_auth::default_codex_auth_json_path;
use crate::config::files::{build_split_tables, read_merged_toml};
use crate::providers::{is_glm_alias, is_zai_alias};
use crate::security::AutonomyLevel;
use anyhow::{Context, Result};
use directories::UserDirs;
use parking_lot::RwLock;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(unix)]
use tokio::fs::File;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

const SUPPORTED_PROXY_SERVICE_KEYS: &[&str] = &[
    "provider.anthropic",
    "provider.compatible",
    "provider.copilot",
    "provider.gemini",
    "provider.glm",
    "provider.ollama",
    "provider.openai",
    "provider.openrouter",
    "channel.dingtalk",
    "channel.discord",
    "channel.lark",
    "channel.matrix",
    "channel.mattermost",
    "channel.nextcloud_talk",
    "channel.qq",
    "channel.signal",
    "channel.slack",
    "channel.telegram",
    "channel.whatsapp",
    "tool.browser",
    "tool.composio",
    "tool.http_request",
    "tool.pushover",
    "memory.embeddings",
    "tunnel.custom",
];

const SUPPORTED_PROXY_SERVICE_SELECTORS: &[&str] = &["provider.*", "channel.*", "tool.*", "memory.*", "tunnel.*"];

static RUNTIME_PROXY_CONFIG: OnceLock<RwLock<ProxyConfig>> = OnceLock::new();
static RUNTIME_PROXY_CLIENT_CACHE: OnceLock<RwLock<HashMap<String, reqwest::Client>>> = OnceLock::new();

// ── Top-level config ──────────────────────────────────────────────

/// Top-level OpenPRX configuration, loaded from `config.toml`.
///
/// Resolution order: `OPENPRX_WORKSPACE` (legacy: `OPENPRX_WORKSPACE`) env
/// → `active_workspace.toml` marker
/// → `~/.openprx/config.toml` (fallback `~/.openprx/config.toml`).
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    /// Workspace directory - computed from home, not serialized
    #[serde(skip)]
    pub workspace_dir: PathBuf,
    /// Path to config.toml - computed from home, not serialized
    #[serde(skip)]
    pub config_path: PathBuf,
    /// API key for the selected provider. Overridden by `OPENPRX_API_KEY`, `OPENPRX_API_KEY / OPENPRX_API_KEY`, or `API_KEY` env vars.
    pub api_key: Option<String>,
    /// Base URL override for provider API (e.g. "http://10.0.0.1:11434" for remote Ollama)
    pub api_url: Option<String>,
    /// Default provider ID or alias (e.g. `"openrouter"`, `"ollama"`, `"anthropic"`). Default: `"openrouter"`.
    pub default_provider: Option<String>,
    /// Default model routed through the selected provider (e.g. `"anthropic/claude-sonnet-4-6"`).
    pub default_model: Option<String>,
    /// Default model temperature (0.0–2.0). Default: `0.7`.
    pub default_temperature: f64,

    /// Observability backend configuration (`[observability]`).
    #[serde(default)]
    pub observability: ObservabilityConfig,

    /// Autonomy and security policy configuration (`[autonomy]`).
    #[serde(default)]
    pub autonomy: AutonomyConfig,

    /// Runtime adapter configuration (`[runtime]`). Controls native vs Docker execution.
    #[serde(default)]
    pub runtime: RuntimeConfig,

    /// Reliability settings: retries, fallback providers, backoff (`[reliability]`).
    #[serde(default)]
    pub reliability: ReliabilityConfig,

    /// Scheduler configuration for periodic task execution (`[scheduler]`).
    #[serde(default)]
    pub scheduler: SchedulerConfig,

    /// Agent orchestration settings (`[agent]`).
    #[serde(default)]
    pub agent: AgentConfig,

    /// Session spawning configuration (`[sessions_spawn]`).
    #[serde(default)]
    pub sessions_spawn: SessionsSpawnConfig,

    /// Self-system experimental automation controls (`[self_system]`).
    #[serde(default)]
    pub self_system: SelfSystemConfig,

    /// Skills loading and community repository behavior (`[skills]`).
    #[serde(default)]
    pub skills: SkillsConfig,

    /// Dynamic skill retrieval settings (`[skill_rag]`).
    #[serde(default)]
    pub skill_rag: SkillRagConfig,

    /// Model routing rules — route `hint:<name>` to specific provider+model combos.
    #[serde(default)]
    pub model_routes: Vec<ModelRouteConfig>,

    /// Embedding routing rules — route `hint:<name>` to specific provider+model combos.
    #[serde(default)]
    pub embedding_routes: Vec<EmbeddingRouteConfig>,

    /// Automatic query classification — maps user messages to model hints.
    #[serde(default)]
    pub query_classification: QueryClassificationConfig,

    /// Task routing configuration — classifies work by intent before the main agent loop.
    #[serde(default)]
    pub task_routing: TaskRoutingConfig,

    /// Heuristic LLM router configuration (`[router]`).
    #[serde(default)]
    pub router: RouterConfig,

    /// Heartbeat configuration for periodic health pings (`[heartbeat]`).
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,

    /// Xin (心) autonomous task engine configuration (`[xin]`).
    #[serde(default)]
    pub xin: crate::xin::XinConfig,

    /// Cron job configuration (`[cron]`).
    #[serde(default)]
    pub cron: CronConfig,

    /// Channel configurations: Telegram, Discord, Slack, etc. (`[channels_config]`).
    #[serde(default)]
    pub channels_config: ChannelsConfig,

    /// Memory backend configuration: sqlite, markdown, embeddings (`[memory]`).
    #[serde(default)]
    pub memory: MemoryConfig,

    /// Static identity bindings loaded at startup (`[[identity_bindings]]`).
    #[serde(default)]
    pub identity_bindings: Vec<IdentityBindingConfig>,

    /// Static user policy records loaded at startup (`[[user_policies]]`).
    #[serde(default)]
    pub user_policies: Vec<UserPolicyConfig>,

    /// Persistent storage provider configuration (`[storage]`).
    #[serde(default)]
    pub storage: StorageConfig,

    /// Tunnel configuration for exposing the gateway publicly (`[tunnel]`).
    #[serde(default)]
    pub tunnel: TunnelConfig,

    /// Gateway server configuration: host, port, pairing, rate limits (`[gateway]`).
    #[serde(default)]
    pub gateway: GatewayConfig,

    /// Standalone webhook receiver for external event -> topic synchronization (`[webhook]`).
    #[serde(default)]
    pub webhook: MemoryWebhookConfig,

    /// Composio managed OAuth tools integration (`[composio]`).
    #[serde(default)]
    pub composio: ComposioConfig,

    /// Secrets encryption configuration (`[secrets]`).
    #[serde(default)]
    pub mcp: McpConfig,

    /// Auth profile and external credential import settings (`[auth]`).
    #[serde(default)]
    pub auth: AuthConfig,

    #[serde(default)]
    pub secrets: SecretsConfig,

    /// Browser automation configuration (`[browser]`).
    #[serde(default)]
    pub browser: BrowserConfig,

    /// HTTP request tool configuration (`[http_request]`).
    #[serde(default)]
    pub http_request: HttpRequestConfig,

    /// Multimodal (image) handling configuration (`[multimodal]`).
    #[serde(default)]
    pub multimodal: MultimodalConfig,

    /// Web search tool configuration (`[web_search]`).
    #[serde(default)]
    pub web_search: WebSearchConfig,

    /// Proxy configuration for outbound HTTP/HTTPS/SOCKS5 traffic (`[proxy]`).
    #[serde(default)]
    pub proxy: ProxyConfig,

    /// Identity format configuration: OpenClaw or AIEOS (`[identity]`).
    #[serde(default)]
    pub identity: IdentityConfig,

    /// Cost tracking and budget enforcement configuration (`[cost]`).
    #[serde(default)]
    pub cost: CostConfig,

    /// Remote node proxy configuration (`[nodes]`).
    #[serde(default)]
    pub nodes: NodesConfig,

    /// Delegate agent configurations for multi-agent workflows.
    #[serde(default)]
    pub agents: HashMap<String, DelegateAgentConfig>,

    /// Media understanding configuration (`[media]` section).
    /// Controls audio STT and video frame extraction for incoming attachments.
    #[serde(default)]
    pub media: MediaConfig,

    /// Security configuration: sandboxing, resource limits, audit, tool policy (`[security]`).
    #[serde(default)]
    pub security: SecurityConfig,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("workspace_dir", &self.workspace_dir)
            .field("config_path", &self.config_path)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("api_url", &self.api_url)
            .field("default_provider", &self.default_provider)
            .field("default_model", &self.default_model)
            .field("default_temperature", &self.default_temperature)
            .field("observability", &self.observability)
            .field("autonomy", &self.autonomy)
            .field("runtime", &self.runtime)
            .field("reliability", &self.reliability)
            .field("scheduler", &self.scheduler)
            .field("agent", &self.agent)
            .field("sessions_spawn", &self.sessions_spawn)
            .field("self_system", &self.self_system)
            .field("skills", &self.skills)
            .field("skill_rag", &self.skill_rag)
            .field("model_routes", &self.model_routes)
            .field("embedding_routes", &self.embedding_routes)
            .field("query_classification", &self.query_classification)
            .field("task_routing", &self.task_routing)
            .field("router", &self.router)
            .field("heartbeat", &self.heartbeat)
            .field("xin", &self.xin)
            .field("cron", &self.cron)
            .field("channels_config", &self.channels_config)
            .field("memory", &self.memory)
            .field("identity_bindings", &self.identity_bindings)
            .field("user_policies", &self.user_policies)
            .field("storage", &self.storage)
            .field("tunnel", &self.tunnel)
            .field("gateway", &self.gateway)
            .field("webhook", &self.webhook)
            .field("composio", &self.composio)
            .field("mcp", &self.mcp)
            .field("auth", &self.auth)
            .field("secrets", &self.secrets)
            .field("browser", &self.browser)
            .field("http_request", &self.http_request)
            .field("multimodal", &self.multimodal)
            .field("web_search", &self.web_search)
            .field("proxy", &self.proxy)
            .field("identity", &self.identity)
            .field("cost", &self.cost)
            .field("nodes", &self.nodes)
            .field("agents", &self.agents)
            .field("media", &self.media)
            .field("security", &self.security)
            .finish()
    }
}

// ── Delegate Agents ──────────────────────────────────────────────

/// Configuration for a delegate sub-agent used by the `delegate` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DelegateAgentConfig {
    /// Provider name (e.g. "ollama", "openrouter", "anthropic")
    pub provider: String,
    /// Model name
    pub model: String,
    /// Optional system prompt for the sub-agent
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Optional API key override
    #[serde(default)]
    pub api_key: Option<String>,
    /// Temperature override
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Max recursion depth for nested delegation
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Enable agentic sub-agent mode (multi-turn tool-call loop).
    #[serde(default)]
    pub agentic: bool,
    /// Allowlist of tool names available to the sub-agent in agentic mode.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Maximum tool-call iterations in agentic mode.
    #[serde(default = "default_max_tool_iterations")]
    pub max_iterations: usize,
    /// Optional identity files directory relative to workspace root.
    #[serde(default)]
    pub identity_dir: Option<String>,
    /// Optional memory scope for spawned sessions: "shared" (default) or "isolated".
    #[serde(default)]
    pub memory_scope: Option<String>,
    /// Whether this agent is allowed to be targeted by sessions_spawn (default: true).
    #[serde(default)]
    pub spawn_enabled: Option<bool>,
}

const fn default_max_depth() -> u32 {
    3
}

const fn default_max_tool_iterations() -> usize {
    50
}

const fn default_router_alpha() -> f32 {
    0.0
}

const fn default_router_beta() -> f32 {
    0.5
}

const fn default_router_gamma() -> f32 {
    0.3
}

const fn default_router_delta() -> f32 {
    0.1
}

const fn default_router_epsilon() -> f32 {
    0.1
}

const fn default_router_knn_min_records() -> usize {
    10
}

const fn default_router_knn_k() -> usize {
    7
}

const fn default_router_max_context() -> usize {
    128_000
}

const fn default_router_latency() -> u32 {
    2_000
}

const fn default_router_elo() -> f32 {
    1_000.0
}

const fn default_automix_confidence_threshold() -> f32 {
    0.7
}

/// Adaptive escalation policy for cheap-first routing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutomixConfig {
    /// Enable Automix escalation. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Escalate when confidence falls below this threshold. Default: `0.7`.
    #[serde(default = "default_automix_confidence_threshold")]
    pub confidence_threshold: f32,
    /// Tier markers that identify cheap-first models.
    #[serde(default)]
    pub cheap_model_tiers: Vec<String>,
    /// Premium model target used for escalation.
    #[serde(default)]
    pub premium_model_id: String,
}

impl Default for AutomixConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            confidence_threshold: default_automix_confidence_threshold(),
            cheap_model_tiers: Vec::new(),
            premium_model_id: String::new(),
        }
    }
}

impl AutomixConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(0.0..=1.0).contains(&self.confidence_threshold) {
            anyhow::bail!("automix.confidence_threshold must be in [0,1]");
        }
        if self.enabled && self.premium_model_id.trim().is_empty() {
            anyhow::bail!("automix.premium_model_id must not be empty when automix.enabled=true");
        }
        Ok(())
    }
}

/// Heuristic LLM router configuration (`[router]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RouterConfig {
    /// Enable heuristic routing. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Similarity score weight. Phase 1 keeps this at `0.0`.
    #[serde(default = "default_router_alpha")]
    pub alpha: f32,
    /// Capability score weight.
    #[serde(default = "default_router_beta")]
    pub beta: f32,
    /// Elo score weight.
    #[serde(default = "default_router_gamma")]
    pub gamma: f32,
    /// Cost penalty coefficient.
    #[serde(default = "default_router_delta")]
    pub delta: f32,
    /// Latency penalty coefficient.
    #[serde(default = "default_router_epsilon")]
    pub epsilon: f32,
    /// Enable KNN-based semantic router history. Default: `false`.
    #[serde(default)]
    pub knn_enabled: bool,
    /// Minimum history records before KNN affects routing. Default: `10`.
    #[serde(default = "default_router_knn_min_records")]
    pub knn_min_records: usize,
    /// Number of nearest neighbors considered for voting. Default: `7`.
    #[serde(default = "default_router_knn_k")]
    pub knn_k: usize,
    /// Cheap-first adaptive escalation policy.
    #[serde(default)]
    pub automix: AutomixConfig,
    /// Candidate model registry.
    #[serde(default)]
    pub models: Vec<RouterModelConfig>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            alpha: default_router_alpha(),
            beta: default_router_beta(),
            gamma: default_router_gamma(),
            delta: default_router_delta(),
            epsilon: default_router_epsilon(),
            knn_enabled: false,
            knn_min_records: default_router_knn_min_records(),
            knn_k: default_router_knn_k(),
            automix: AutomixConfig::default(),
            models: Vec::new(),
        }
    }
}

impl RouterConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.automix.validate()?;
        for (name, weight) in [
            ("alpha", self.alpha),
            ("beta", self.beta),
            ("gamma", self.gamma),
            ("delta", self.delta),
            ("epsilon", self.epsilon),
        ] {
            if weight < 0.0 {
                anyhow::bail!("router.{name} must be non-negative");
            }
        }
        Ok(())
    }
}

/// Static router metadata for one model candidate.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RouterModelConfig {
    /// Model identifier without provider prefix.
    pub model_id: String,
    /// Provider identifier.
    pub provider: String,
    /// USD cost per million tokens.
    #[serde(default)]
    pub cost_per_million_tokens: f32,
    /// Maximum context window in tokens.
    #[serde(default = "default_router_max_context")]
    pub max_context: usize,
    /// Average latency in milliseconds.
    #[serde(default = "default_router_latency")]
    pub latency_ms: u32,
    /// Capability categories such as `conversation`, `code`, or `analysis`.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Initial Elo rating.
    #[serde(default = "default_router_elo")]
    pub elo_rating: f32,
}

/// Remote node proxy configuration for core-side RPC calls and node daemon defaults.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NodesConfig {
    /// Enable remote node features.
    #[serde(default)]
    pub enabled: bool,
    /// Default request timeout for node RPC calls.
    #[serde(default = "default_nodes_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Default max retries for node RPC calls.
    #[serde(default = "default_nodes_retry_max")]
    pub retry_max: u8,
    /// Configured remote nodes.
    #[serde(default)]
    pub nodes: Vec<RemoteNodeConfig>,
    /// Embedded node server defaults used by `prx-node`.
    #[serde(default)]
    pub server: NodeServerConfig,
}

const fn default_nodes_timeout_ms() -> u64 {
    15_000
}

const fn default_nodes_retry_max() -> u8 {
    2
}

impl Default for NodesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            request_timeout_ms: default_nodes_timeout_ms(),
            retry_max: default_nodes_retry_max(),
            nodes: Vec::new(),
            server: NodeServerConfig::default(),
        }
    }
}

/// A remote node target for core-side node proxy calls.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct RemoteNodeConfig {
    /// Stable node ID used by the nodes tool.
    #[serde(alias = "name")]
    pub id: String,
    /// Base endpoint (e.g. "http://127.0.0.1:8787").
    pub endpoint: String,
    /// Bearer token for node RPC authentication.
    #[serde(alias = "token")]
    pub bearer_token: String,
    /// Optional request signing key (HMAC-SHA256).
    #[serde(default)]
    pub hmac_secret: Option<String>,
    /// Whether this node is enabled.
    #[serde(default = "default_nodes_enabled")]
    pub enabled: bool,
    /// Optional per-node timeout override.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Optional per-node retry override.
    #[serde(default, alias = "max_retries")]
    pub retry_max: Option<u8>,
}

impl std::fmt::Debug for RemoteNodeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteNodeConfig")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .field("bearer_token", &"[REDACTED]")
            .field("hmac_secret", &self.hmac_secret.as_ref().map(|_| "[REDACTED]"))
            .field("enabled", &self.enabled)
            .field("timeout_ms", &self.timeout_ms)
            .field("retry_max", &self.retry_max)
            .finish()
    }
}

const fn default_nodes_enabled() -> bool {
    true
}

/// Node daemon runtime config used by `prx-node`.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct NodeServerConfig {
    /// Listen address for node daemon.
    #[serde(default = "default_node_server_listen_addr", alias = "bind")]
    pub listen_addr: String,
    /// Required bearer token for /rpc.
    #[serde(default, alias = "token")]
    pub bearer_token: String,
    /// Optional HMAC signing secret.
    #[serde(default)]
    pub hmac_secret: Option<String>,
    /// Sandbox root directory for read/write/cwd constraints.
    #[serde(default = "default_node_server_sandbox_root")]
    pub sandbox_root: String,
    /// Default command timeout in milliseconds.
    #[serde(default = "default_node_server_exec_timeout_ms")]
    pub exec_timeout_ms: u64,
    /// Maximum combined stdout/stderr bytes captured per command.
    #[serde(default = "default_node_server_max_output_bytes")]
    pub max_output_bytes: usize,
    /// Maximum number of concurrent async command tasks.
    #[serde(default = "default_node_server_max_concurrent_tasks")]
    pub max_concurrent_tasks: usize,
    /// TTL for completed task results in milliseconds.
    #[serde(default = "default_node_server_task_result_ttl_ms")]
    pub task_result_ttl_ms: u64,
    /// Command allowlist matched on first token.
    ///
    /// In restricted mode this list is required unless explicitly set to `["*"]`.
    #[serde(default = "default_node_server_allowed_commands")]
    pub allowed_commands: Vec<String>,
    /// Command denylist matched on first token. Always enforced.
    #[serde(default, alias = "command_blacklist")]
    pub blocked_commands: Vec<String>,
    /// Require TLS for non-loopback binds.
    ///
    /// When true, node server must either bind to loopback or provide cert/key.
    #[serde(default = "default_node_server_tls_required")]
    pub tls_required: bool,
    /// TLS certificate path (PEM).
    #[serde(default)]
    pub tls_cert: Option<String>,
    /// TLS private key path (PEM).
    #[serde(default)]
    pub tls_key: Option<String>,
}

impl std::fmt::Debug for NodeServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeServerConfig")
            .field("listen_addr", &self.listen_addr)
            .field("bearer_token", &"[REDACTED]")
            .field("hmac_secret", &self.hmac_secret.as_ref().map(|_| "[REDACTED]"))
            .field("sandbox_root", &self.sandbox_root)
            .field("exec_timeout_ms", &self.exec_timeout_ms)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("max_concurrent_tasks", &self.max_concurrent_tasks)
            .field("task_result_ttl_ms", &self.task_result_ttl_ms)
            .field("allowed_commands", &self.allowed_commands)
            .field("blocked_commands", &self.blocked_commands)
            .field("tls_required", &self.tls_required)
            .field("tls_cert", &self.tls_cert)
            .field("tls_key", &self.tls_key)
            .finish()
    }
}

fn default_node_server_listen_addr() -> String {
    "127.0.0.1:8787".to_string()
}

fn default_node_server_sandbox_root() -> String {
    ".".to_string()
}

const fn default_node_server_exec_timeout_ms() -> u64 {
    15_000
}

const fn default_node_server_max_output_bytes() -> usize {
    1_048_576
}

const fn default_node_server_max_concurrent_tasks() -> usize {
    8
}

const fn default_node_server_task_result_ttl_ms() -> u64 {
    3_600_000
}

fn default_node_server_allowed_commands() -> Vec<String> {
    vec!["echo".to_string()]
}

const fn default_node_server_tls_required() -> bool {
    true
}

impl Default for NodeServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_node_server_listen_addr(),
            bearer_token: String::new(),
            hmac_secret: None,
            sandbox_root: default_node_server_sandbox_root(),
            exec_timeout_ms: default_node_server_exec_timeout_ms(),
            max_output_bytes: default_node_server_max_output_bytes(),
            max_concurrent_tasks: default_node_server_max_concurrent_tasks(),
            task_result_ttl_ms: default_node_server_task_result_ttl_ms(),
            allowed_commands: default_node_server_allowed_commands(),
            blocked_commands: Vec::new(),
            tls_required: default_node_server_tls_required(),
            tls_cert: None,
            tls_key: None,
        }
    }
}

/// Agent orchestration configuration (`[agent]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentConfig {
    /// When true: bootstrap_max_chars=6000, rag_chunk_limit=2. Use for 13B or smaller models.
    #[serde(default)]
    pub compact_context: bool,
    /// Maximum tool-call loop turns per user message. Default: `10`.
    /// Setting to `0` falls back to the safe default of `10`.
    #[serde(default = "default_agent_max_tool_iterations")]
    pub max_tool_iterations: usize,
    /// Maximum conversation history messages retained per session. Default: `50`.
    #[serde(default = "default_agent_max_history_messages")]
    pub max_history_messages: usize,
    /// Enable parallel tool execution within a single iteration. Default: `false`.
    #[serde(default)]
    pub parallel_tools: bool,
    /// Tool dispatch strategy (e.g. `"auto"`). Default: `"auto"`.
    #[serde(default = "default_agent_tool_dispatcher")]
    pub tool_dispatcher: String,
    /// Max concurrent read-only tool calls in an iteration batch.
    /// Defaults to 2 to preserve existing scheduler behavior.
    #[serde(default = "default_read_only_tool_concurrency_window")]
    pub read_only_tool_concurrency_window: usize,
    /// Timeout in seconds for each read-only tool call in a parallel batch.
    /// Defaults to 30s to preserve existing scheduler behavior.
    #[serde(default = "default_read_only_tool_timeout_secs")]
    pub read_only_tool_timeout_secs: u64,
    /// Enables priority scheduling so foreground tool calls run before background batches.
    /// Defaults to false to preserve existing scheduler behavior.
    #[serde(default)]
    pub priority_scheduling_enabled: bool,
    /// Tool names treated as low-priority when `priority_scheduling_enabled = true`.
    #[serde(default = "default_agent_low_priority_tools")]
    pub low_priority_tools: Vec<String>,
    /// Global kill switch. When enabled, forces tool scheduling to serial mode.
    #[serde(default)]
    pub concurrency_kill_switch_force_serial: bool,
    /// Rollout stage for read-only parallel scheduling.
    /// Allowed values: `off`, `stage_a`, `stage_b`, `stage_c`, `full`.
    #[serde(default = "default_concurrency_rollout_stage")]
    pub concurrency_rollout_stage: String,
    /// Optional rollout sample percentage (0-100). When zero, stage defaults apply.
    #[serde(default)]
    pub concurrency_rollout_sample_percent: u8,
    /// Optional channel allowlist for concurrency rollout. Empty means all channels.
    #[serde(default)]
    pub concurrency_rollout_channels: Vec<String>,
    /// Enable automatic fallback to serial mode when rollback thresholds are exceeded.
    #[serde(default = "default_true")]
    pub concurrency_auto_rollback_enabled: bool,
    /// Timeout rate threshold (0.0-1.0) for triggering serial fallback.
    #[serde(default = "default_concurrency_rollback_threshold")]
    pub concurrency_rollback_timeout_rate_threshold: f64,
    /// Cancellation rate threshold (0.0-1.0) for triggering serial fallback.
    #[serde(default = "default_concurrency_rollback_threshold")]
    pub concurrency_rollback_cancel_rate_threshold: f64,
    /// Error rate threshold (0.0-1.0) for triggering serial fallback.
    #[serde(default = "default_concurrency_rollback_threshold")]
    pub concurrency_rollback_error_rate_threshold: f64,
    /// Context compaction controls (`[agent.compaction]`).
    #[serde(default)]
    pub compaction: AgentCompactionConfig,
}

/// Compaction mode for long conversation contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentCompactionMode {
    /// Disable proactive compaction.
    Off,
    /// Conservative compaction that prefers summary replacement.
    #[default]
    Safeguard,
    /// Aggressive truncation mode for tighter contexts.
    Aggressive,
}

/// Agent context compaction configuration (`[agent.compaction]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentCompactionConfig {
    /// Compaction strategy (`off`, `safeguard`, `aggressive`).
    #[serde(default)]
    pub mode: AgentCompactionMode,
    /// Tokens reserved for the next model response.
    #[serde(default = "default_agent_compaction_reserve_tokens")]
    pub reserve_tokens: usize,
    /// Number of recent non-system messages to keep after compaction.
    #[serde(default = "default_agent_compaction_keep_recent_messages")]
    pub keep_recent_messages: usize,
    /// Run memory flush extraction before compacting.
    #[serde(default = "default_true")]
    pub memory_flush: bool,
    /// Token threshold that triggers compaction.
    #[serde(default = "default_agent_compaction_max_context_tokens")]
    pub max_context_tokens: usize,
}

impl Default for AgentCompactionConfig {
    fn default() -> Self {
        Self {
            mode: AgentCompactionMode::Safeguard,
            reserve_tokens: default_agent_compaction_reserve_tokens(),
            keep_recent_messages: default_agent_compaction_keep_recent_messages(),
            memory_flush: true,
            max_context_tokens: default_agent_compaction_max_context_tokens(),
        }
    }
}

/// Sessions spawn configuration (`[sessions_spawn]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionsSpawnConfig {
    /// Default execution mode for sessions_spawn (`task` or `process`).
    #[serde(default = "default_sessions_spawn_mode")]
    pub default_mode: String,
    /// Optional root directory for process-mode worker workspaces.
    ///
    /// When unset, falls back to `<workspace>/workers`.
    #[serde(default)]
    pub worker_workspace_root: Option<String>,
    /// Remove worker workspace directory after process-mode completion.
    #[serde(default = "default_sessions_spawn_cleanup_on_complete")]
    pub cleanup_on_complete: bool,
    /// Maximum concurrent running sub-agent processes/tasks globally.
    #[serde(default = "default_sessions_spawn_max_concurrent")]
    pub max_concurrent: usize,
    /// Maximum nested spawn depth (`sessions_spawn` from a spawned sub-agent).
    #[serde(default = "default_sessions_spawn_max_spawn_depth")]
    pub max_spawn_depth: usize,
    /// Maximum concurrent child runs allowed per parent session.
    #[serde(default = "default_sessions_spawn_max_children_per_agent")]
    pub max_children_per_agent: usize,
}

impl Default for SessionsSpawnConfig {
    fn default() -> Self {
        Self {
            default_mode: default_sessions_spawn_mode(),
            worker_workspace_root: None,
            cleanup_on_complete: default_sessions_spawn_cleanup_on_complete(),
            max_concurrent: default_sessions_spawn_max_concurrent(),
            max_spawn_depth: default_sessions_spawn_max_spawn_depth(),
            max_children_per_agent: default_sessions_spawn_max_children_per_agent(),
        }
    }
}

fn default_sessions_spawn_mode() -> String {
    "task".to_string()
}

const fn default_sessions_spawn_cleanup_on_complete() -> bool {
    true
}

const fn default_sessions_spawn_max_concurrent() -> usize {
    4
}

const fn default_sessions_spawn_max_spawn_depth() -> usize {
    2
}

const fn default_sessions_spawn_max_children_per_agent() -> usize {
    5
}

/// Self-system experimental automation config (`[self_system]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SelfSystemConfig {
    /// Enable periodic self-system fitness worker in daemon runtime.
    /// Deprecated as a global evolution switch: use `evolution_enabled` for scheduler control.
    #[serde(default)]
    pub enabled: bool,
    /// Fitness report interval in hours.
    #[serde(default = "default_self_system_fitness_interval_hours")]
    pub fitness_interval_hours: u64,
    /// Enable evolution scheduler cycles in daemon runtime.
    #[serde(default)]
    pub evolution_enabled: bool,
    /// Evolution scheduler interval in hours (daemon mode).
    #[serde(default = "default_self_system_evolution_interval_hours")]
    pub evolution_interval_hours: u32,
}

const fn default_self_system_fitness_interval_hours() -> u64 {
    24
}

const fn default_self_system_evolution_interval_hours() -> u32 {
    24
}

impl Default for SelfSystemConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fitness_interval_hours: default_self_system_fitness_interval_hours(),
            evolution_enabled: true,
            evolution_interval_hours: default_self_system_evolution_interval_hours(),
        }
    }
}

const fn default_agent_max_tool_iterations() -> usize {
    50
}

const fn default_agent_max_history_messages() -> usize {
    50
}

fn default_agent_tool_dispatcher() -> String {
    "auto".into()
}

const fn default_read_only_tool_concurrency_window() -> usize {
    2
}

const fn default_read_only_tool_timeout_secs() -> u64 {
    30
}

fn default_agent_low_priority_tools() -> Vec<String> {
    vec!["sessions_spawn", "delegate", "cron_run"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn default_concurrency_rollout_stage() -> String {
    "off".to_string()
}

const fn default_concurrency_rollback_threshold() -> f64 {
    0.20
}

const fn default_agent_compaction_reserve_tokens() -> usize {
    4096
}

const fn default_agent_compaction_keep_recent_messages() -> usize {
    12
}

const fn default_agent_compaction_max_context_tokens() -> usize {
    128_000
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            compact_context: false,
            max_tool_iterations: default_agent_max_tool_iterations(),
            max_history_messages: default_agent_max_history_messages(),
            parallel_tools: false,
            tool_dispatcher: default_agent_tool_dispatcher(),
            read_only_tool_concurrency_window: default_read_only_tool_concurrency_window(),
            read_only_tool_timeout_secs: default_read_only_tool_timeout_secs(),
            priority_scheduling_enabled: false,
            low_priority_tools: default_agent_low_priority_tools(),
            concurrency_kill_switch_force_serial: false,
            concurrency_rollout_stage: default_concurrency_rollout_stage(),
            concurrency_rollout_sample_percent: 0,
            concurrency_rollout_channels: Vec::new(),
            concurrency_auto_rollback_enabled: true,
            concurrency_rollback_timeout_rate_threshold: default_concurrency_rollback_threshold(),
            concurrency_rollback_cancel_rate_threshold: default_concurrency_rollback_threshold(),
            concurrency_rollback_error_rate_threshold: default_concurrency_rollback_threshold(),
            compaction: AgentCompactionConfig::default(),
        }
    }
}

/// Skills loading configuration (`[skills]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillsConfig {
    /// Enable loading and syncing the community open-skills repository.
    /// Default: `false` (opt-in).
    #[serde(default)]
    pub open_skills_enabled: bool,
    /// Optional path to a local open-skills repository.
    /// If unset, defaults to `$HOME/open-skills` when enabled.
    #[serde(default)]
    pub open_skills_dir: Option<String>,
    /// Enable cloning and loading OpenClaw skills from GitHub (sparse checkout of `skills/` dir).
    /// Default: `false` (opt-in).
    #[serde(default)]
    pub openclaw_skills_enabled: bool,
    /// Optional path to a local openclaw-skills clone directory.
    /// If unset, defaults to `$HOME/.openprx/openclaw-skills/` when enabled.
    #[serde(default)]
    pub openclaw_skills_dir: Option<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            open_skills_enabled: false,
            open_skills_dir: None,
            openclaw_skills_enabled: false,
            openclaw_skills_dir: None,
        }
    }
}

/// Dynamic skill retrieval configuration (`[skill_rag]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillRagConfig {
    /// Enable query-aware skill selection instead of full-skill injection.
    #[serde(default = "default_skill_rag_enabled")]
    pub enabled: bool,
    /// Maximum number of relevant skills injected when skill RAG is enabled.
    #[serde(default = "default_skill_rag_top_k")]
    pub top_k: usize,
}

const fn default_skill_rag_enabled() -> bool {
    true
}

const fn default_skill_rag_top_k() -> usize {
    5
}

impl Default for SkillRagConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            top_k: default_skill_rag_top_k(),
        }
    }
}

/// Multimodal (image) handling configuration (`[multimodal]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MultimodalConfig {
    /// Maximum number of image attachments accepted per request.
    #[serde(default = "default_multimodal_max_images")]
    pub max_images: usize,
    /// Maximum image payload size in MiB before base64 encoding.
    #[serde(default = "default_multimodal_max_image_size_mb")]
    pub max_image_size_mb: usize,
    /// Allow fetching remote image URLs (http/https). Disabled by default.
    #[serde(default)]
    pub allow_remote_fetch: bool,
}

const fn default_multimodal_max_images() -> usize {
    4
}

const fn default_multimodal_max_image_size_mb() -> usize {
    5
}

impl MultimodalConfig {
    /// Clamp configured values to safe runtime bounds.
    pub fn effective_limits(&self) -> (usize, usize) {
        let max_images = self.max_images.clamp(1, 16);
        let max_image_size_mb = self.max_image_size_mb.clamp(1, 20);
        (max_images, max_image_size_mb)
    }
}

impl Default for MultimodalConfig {
    fn default() -> Self {
        Self {
            max_images: default_multimodal_max_images(),
            max_image_size_mb: default_multimodal_max_image_size_mb(),
            allow_remote_fetch: false,
        }
    }
}

// ── Media Understanding ──────────────────────────────────────────

/// Media understanding configuration (`[media]` section).
///
/// Controls STT transcription for audio and video frame extraction.
/// Aligns with OpenClaw's media-understanding architecture.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MediaConfig {
    /// Audio STT provider: "ollama" | "cli" | "none". Default: "none".
    #[serde(default = "default_audio_provider")]
    pub audio_provider: String,

    /// Audio model name (Ollama model or OpenAI model). Default: "whisper".
    #[serde(default = "default_audio_model")]
    pub audio_model: String,

    /// Ollama base URL for audio transcription. Default: "http://localhost:11434".
    #[serde(default = "default_audio_ollama_url")]
    pub audio_ollama_url: String,

    /// Video provider: "frames" (ffmpeg frame extraction) | "none". Default: "none".
    #[serde(default = "default_video_provider")]
    pub video_provider: String,

    /// Maximum video frames to extract per video. Default: 4.
    #[serde(default = "default_video_max_frames")]
    pub video_max_frames: usize,

    /// Maximum audio attachment size in MiB. Default: 20.
    #[serde(default = "default_max_audio_size_mb")]
    pub max_audio_size_mb: usize,

    /// Maximum video attachment size in MiB. Default: 50.
    #[serde(default = "default_max_video_size_mb")]
    pub max_video_size_mb: usize,
}

fn default_audio_provider() -> String {
    "none".into()
}
fn default_audio_model() -> String {
    "whisper".into()
}
fn default_audio_ollama_url() -> String {
    "http://localhost:11434".into()
}
fn default_video_provider() -> String {
    "none".into()
}
const fn default_video_max_frames() -> usize {
    4
}
const fn default_max_audio_size_mb() -> usize {
    20
}
const fn default_max_video_size_mb() -> usize {
    50
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            audio_provider: default_audio_provider(),
            audio_model: default_audio_model(),
            audio_ollama_url: default_audio_ollama_url(),
            video_provider: default_video_provider(),
            video_max_frames: default_video_max_frames(),
            max_audio_size_mb: default_max_audio_size_mb(),
            max_video_size_mb: default_max_video_size_mb(),
        }
    }
}

// ── Identity (AIEOS / OpenClaw format) ──────────────────────────

/// Identity format configuration (`[identity]` section).
///
/// Supports `"openclaw"` (default) or `"aieos"` identity documents.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IdentityConfig {
    /// Identity format: "openclaw" (default) or "aieos"
    #[serde(default = "default_identity_format")]
    pub format: String,
    /// Path to AIEOS JSON file (relative to workspace)
    #[serde(default)]
    pub aieos_path: Option<String>,
    /// Inline AIEOS JSON (alternative to file path)
    #[serde(default)]
    pub aieos_inline: Option<String>,
}

fn default_identity_format() -> String {
    "openclaw".into()
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            format: default_identity_format(),
            aieos_path: None,
            aieos_inline: None,
        }
    }
}

// ── Cost tracking and budget enforcement ───────────────────────────

/// Cost tracking and budget enforcement configuration (`[cost]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CostConfig {
    /// Enable cost tracking (default: false)
    #[serde(default)]
    pub enabled: bool,

    /// Daily spending limit in USD (default: 10.00)
    #[serde(default = "default_daily_limit")]
    pub daily_limit_usd: f64,

    /// Monthly spending limit in USD (default: 100.00)
    #[serde(default = "default_monthly_limit")]
    pub monthly_limit_usd: f64,

    /// Warn when spending reaches this percentage of limit (default: 80)
    #[serde(default = "default_warn_percent")]
    pub warn_at_percent: u8,

    /// Allow requests to exceed budget with --override flag (default: false)
    #[serde(default)]
    pub allow_override: bool,

    /// Per-model pricing (USD per 1M tokens)
    #[serde(default)]
    pub prices: std::collections::HashMap<String, ModelPricing>,
}

/// Per-model pricing entry (USD per 1M tokens).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelPricing {
    /// Input price per 1M tokens
    #[serde(default)]
    pub input: f64,

    /// Output price per 1M tokens
    #[serde(default)]
    pub output: f64,
}

const fn default_daily_limit() -> f64 {
    10.0
}

const fn default_monthly_limit() -> f64 {
    100.0
}

const fn default_warn_percent() -> u8 {
    80
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            daily_limit_usd: default_daily_limit(),
            monthly_limit_usd: default_monthly_limit(),
            warn_at_percent: default_warn_percent(),
            allow_override: false,
            prices: get_default_pricing(),
        }
    }
}

/// Default pricing for popular models (USD per 1M tokens)
fn get_default_pricing() -> std::collections::HashMap<String, ModelPricing> {
    let mut prices = std::collections::HashMap::new();

    // Anthropic models
    prices.insert(
        "anthropic/claude-sonnet-4-20250514".into(),
        ModelPricing {
            input: 3.0,
            output: 15.0,
        },
    );
    prices.insert(
        "anthropic/claude-opus-4-20250514".into(),
        ModelPricing {
            input: 15.0,
            output: 75.0,
        },
    );
    prices.insert(
        "anthropic/claude-3.5-sonnet".into(),
        ModelPricing {
            input: 3.0,
            output: 15.0,
        },
    );
    prices.insert(
        "anthropic/claude-3-haiku".into(),
        ModelPricing {
            input: 0.25,
            output: 1.25,
        },
    );

    // OpenAI models
    prices.insert(
        "openai/gpt-4o".into(),
        ModelPricing {
            input: 5.0,
            output: 15.0,
        },
    );
    prices.insert(
        "openai/gpt-4o-mini".into(),
        ModelPricing {
            input: 0.15,
            output: 0.60,
        },
    );
    prices.insert(
        "openai/o1-preview".into(),
        ModelPricing {
            input: 15.0,
            output: 60.0,
        },
    );

    // Google models
    prices.insert(
        "google/gemini-2.0-flash".into(),
        ModelPricing {
            input: 0.10,
            output: 0.40,
        },
    );
    prices.insert(
        "google/gemini-1.5-pro".into(),
        ModelPricing {
            input: 1.25,
            output: 5.0,
        },
    );

    prices
}

// ── Gateway security ─────────────────────────────────────────────

/// Gateway server configuration (`[gateway]` section).
///
/// Controls the HTTP gateway for webhook and pairing endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GatewayConfig {
    /// Gateway port (default: 16830)
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// Gateway host (default: 127.0.0.1)
    #[serde(default = "default_gateway_host")]
    pub host: String,
    /// Require pairing before accepting requests (default: true)
    #[serde(default = "default_true")]
    pub require_pairing: bool,
    /// Allow binding to non-localhost without a tunnel (default: false)
    #[serde(default)]
    pub allow_public_bind: bool,
    /// Paired bearer tokens (managed automatically, not user-edited)
    #[serde(default)]
    pub paired_tokens: Vec<String>,

    /// Max `/pair` requests per minute per client key.
    #[serde(default = "default_pair_rate_limit")]
    pub pair_rate_limit_per_minute: u32,

    /// Max `/webhook` requests per minute per client key.
    #[serde(default = "default_webhook_rate_limit")]
    pub webhook_rate_limit_per_minute: u32,

    /// Max `/api/*` requests per minute per authenticated token.
    #[serde(default = "default_api_rate_limit")]
    pub api_rate_limit_per_minute: u32,

    /// Trust proxy-forwarded client IP headers (`X-Forwarded-For`, `X-Real-IP`).
    /// Disabled by default; enable only behind a trusted reverse proxy.
    #[serde(default)]
    pub trust_forwarded_headers: bool,

    /// Maximum distinct client keys tracked by gateway rate limiter maps.
    #[serde(default = "default_gateway_rate_limit_max_keys")]
    pub rate_limit_max_keys: usize,

    /// TTL for webhook idempotency keys.
    #[serde(default = "default_idempotency_ttl_secs")]
    pub idempotency_ttl_secs: u64,

    /// Maximum distinct idempotency keys retained in memory.
    #[serde(default = "default_gateway_idempotency_max_keys")]
    pub idempotency_max_keys: usize,
    /// Request timeout in seconds for gateway HTTP handlers.
    #[serde(default = "default_gateway_request_timeout_secs")]
    pub request_timeout_secs: u64,
}

const fn default_gateway_port() -> u16 {
    16830
}

fn default_gateway_host() -> String {
    "127.0.0.1".into()
}

const fn default_pair_rate_limit() -> u32 {
    10
}

const fn default_webhook_rate_limit() -> u32 {
    60
}

const fn default_api_rate_limit() -> u32 {
    60
}

const fn default_idempotency_ttl_secs() -> u64 {
    300
}

const fn default_gateway_rate_limit_max_keys() -> usize {
    10_000
}

const fn default_gateway_idempotency_max_keys() -> usize {
    10_000
}

const fn default_gateway_request_timeout_secs() -> u64 {
    60
}

const fn default_true() -> bool {
    true
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            port: default_gateway_port(),
            host: default_gateway_host(),
            require_pairing: true,
            allow_public_bind: false,
            paired_tokens: Vec::new(),
            pair_rate_limit_per_minute: default_pair_rate_limit(),
            webhook_rate_limit_per_minute: default_webhook_rate_limit(),
            api_rate_limit_per_minute: default_api_rate_limit(),
            trust_forwarded_headers: false,
            rate_limit_max_keys: default_gateway_rate_limit_max_keys(),
            idempotency_ttl_secs: default_idempotency_ttl_secs(),
            idempotency_max_keys: default_gateway_idempotency_max_keys(),
            request_timeout_secs: default_gateway_request_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemoryWebhookConfig {
    /// Enable the standalone webhook receiver.
    #[serde(default)]
    pub enabled: bool,
    /// Socket address for the receiver (e.g. "0.0.0.0:16899").
    #[serde(default = "default_memory_webhook_bind")]
    pub bind: String,
    /// Required bearer token for incoming webhook events.
    #[serde(default)]
    pub token: Option<String>,
}

fn default_memory_webhook_bind() -> String {
    "0.0.0.0:16899".to_string()
}

impl Default for MemoryWebhookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_memory_webhook_bind(),
            token: None,
        }
    }
}

// ── Composio (managed tool surface) ─────────────────────────────

/// Composio managed OAuth tools integration (`[composio]` section).
///
/// Provides access to 1000+ OAuth-connected tools via the Composio platform.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposioConfig {
    /// Enable Composio integration for 1000+ OAuth tools
    #[serde(default, alias = "enable")]
    pub enabled: bool,
    /// Composio API key (stored encrypted when secrets.encrypt = true)
    #[serde(default)]
    pub api_key: Option<String>,
    /// Default entity ID for multi-user setups
    #[serde(default = "default_entity_id")]
    pub entity_id: String,
}

fn default_entity_id() -> String {
    "default".into()
}

impl Default for ComposioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            entity_id: default_entity_id(),
        }
    }
}

// ── MCP (Model Context Protocol) ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpConfig {
    /// Enable MCP client integration
    #[serde(default)]
    pub enabled: bool,
    /// Named MCP server definitions
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            servers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
}

impl Default for McpTransport {
    fn default() -> Self {
        Self::Stdio
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpServerConfig {
    /// Per-server enable switch
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Transport type: stdio or http
    #[serde(default)]
    pub transport: McpTransport,
    /// Command for stdio mode
    #[serde(default)]
    pub command: Option<String>,
    /// Command args for stdio mode
    #[serde(default)]
    pub args: Vec<String>,
    /// URL for HTTP mode (streamable HTTP endpoint)
    #[serde(default)]
    pub url: Option<String>,
    /// Environment variables for stdio mode
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Startup timeout in milliseconds
    #[serde(default = "default_mcp_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    /// Request timeout in milliseconds
    #[serde(default = "default_mcp_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Prefix used in tool naming and routing
    #[serde(default = "default_mcp_tool_name_prefix")]
    pub tool_name_prefix: String,
    /// Optional allow-list of exposed tools (empty = all)
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Optional deny-list of exposed tools
    #[serde(default)]
    pub deny_tools: Vec<String>,
}

const fn default_mcp_startup_timeout_ms() -> u64 {
    10_000
}

const fn default_mcp_request_timeout_ms() -> u64 {
    30_000
}

fn default_mcp_tool_name_prefix() -> String {
    "mcp".into()
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: McpTransport::Stdio,
            command: None,
            args: Vec::new(),
            url: None,
            env: HashMap::new(),
            startup_timeout_ms: default_mcp_startup_timeout_ms(),
            request_timeout_ms: default_mcp_request_timeout_ms(),
            tool_name_prefix: default_mcp_tool_name_prefix(),
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
        }
    }
}

// ── Secrets (encrypted credential store) ────────────────────────

/// Secrets encryption configuration (`[secrets]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretsConfig {
    /// Enable encryption for API keys and tokens in config.toml
    #[serde(default = "default_true")]
    pub encrypt: bool,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self { encrypt: true }
    }
}

/// Authentication and external credential import configuration (`[auth]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AuthConfig {
    /// Automatically import `openai-codex` OAuth credentials from Codex CLI auth.json.
    #[serde(default = "default_true")]
    pub codex_auth_json_auto_import: bool,
    /// Source path for Codex CLI auth.json import.
    #[serde(default = "default_codex_auth_json_path")]
    pub codex_auth_json_path: PathBuf,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            codex_auth_json_auto_import: true,
            codex_auth_json_path: default_codex_auth_json_path(),
        }
    }
}

// ── Browser (friendly-service browsing only) ───────────────────

/// Computer-use sidecar configuration (`[browser.computer_use]` section).
///
/// Delegates OS-level mouse, keyboard, and screenshot actions to a local sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BrowserComputerUseConfig {
    /// Sidecar endpoint for computer-use actions (OS-level mouse/keyboard/screenshot)
    #[serde(default = "default_browser_computer_use_endpoint")]
    pub endpoint: String,
    /// Optional bearer token for computer-use sidecar
    #[serde(default)]
    pub api_key: Option<String>,
    /// Per-action request timeout in milliseconds
    #[serde(default = "default_browser_computer_use_timeout_ms")]
    pub timeout_ms: u64,
    /// Allow remote/public endpoint for computer-use sidecar (default: false)
    #[serde(default)]
    pub allow_remote_endpoint: bool,
    /// Optional window title/process allowlist forwarded to sidecar policy
    #[serde(default)]
    pub window_allowlist: Vec<String>,
    /// Optional X-axis boundary for coordinate-based actions
    #[serde(default)]
    pub max_coordinate_x: Option<i64>,
    /// Optional Y-axis boundary for coordinate-based actions
    #[serde(default)]
    pub max_coordinate_y: Option<i64>,
}

fn default_browser_computer_use_endpoint() -> String {
    "http://127.0.0.1:8787/v1/actions".into()
}

const fn default_browser_computer_use_timeout_ms() -> u64 {
    15_000
}

impl Default for BrowserComputerUseConfig {
    fn default() -> Self {
        Self {
            endpoint: default_browser_computer_use_endpoint(),
            api_key: None,
            timeout_ms: default_browser_computer_use_timeout_ms(),
            allow_remote_endpoint: false,
            window_allowlist: Vec::new(),
            max_coordinate_x: None,
            max_coordinate_y: None,
        }
    }
}

/// Browser automation configuration (`[browser]` section).
///
/// Controls the `browser_open` tool and browser automation backends.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BrowserConfig {
    /// Enable `browser_open` tool (opens URLs in Brave without scraping)
    #[serde(default)]
    pub enabled: bool,
    /// Allowed domains for `browser_open` (exact or subdomain match)
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Browser session name (for agent-browser automation)
    #[serde(default)]
    pub session_name: Option<String>,
    /// Browser automation backend: "agent_browser" | "rust_native" | "computer_use" | "auto"
    #[serde(default = "default_browser_backend")]
    pub backend: String,
    /// Headless mode for rust-native backend
    #[serde(default = "default_true")]
    pub native_headless: bool,
    /// WebDriver endpoint URL for rust-native backend (e.g. http://127.0.0.1:9515)
    #[serde(default = "default_browser_webdriver_url")]
    pub native_webdriver_url: String,
    /// Optional Chrome/Chromium executable path for rust-native backend
    #[serde(default)]
    pub native_chrome_path: Option<String>,
    /// Computer-use sidecar configuration
    #[serde(default)]
    pub computer_use: BrowserComputerUseConfig,
}

fn default_browser_backend() -> String {
    "agent_browser".into()
}

fn default_browser_webdriver_url() -> String {
    "http://127.0.0.1:9515".into()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_domains: Vec::new(),
            session_name: None,
            backend: default_browser_backend(),
            native_headless: default_true(),
            native_webdriver_url: default_browser_webdriver_url(),
            native_chrome_path: None,
            computer_use: BrowserComputerUseConfig::default(),
        }
    }
}

// ── HTTP request tool ───────────────────────────────────────────

/// HTTP request tool configuration (`[http_request]` section).
///
/// Deny-by-default: if `allowed_domains` is empty, all HTTP requests are rejected.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct HttpRequestConfig {
    /// Enable `http_request` tool for API interactions
    #[serde(default)]
    pub enabled: bool,
    /// Allowed domains for HTTP requests (exact or subdomain match)
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Maximum response size in bytes (default: 1MB)
    #[serde(default = "default_http_max_response_size")]
    pub max_response_size: usize,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_http_timeout_secs")]
    pub timeout_secs: u64,
}

const fn default_http_max_response_size() -> usize {
    1_000_000 // 1MB
}

const fn default_http_timeout_secs() -> u64 {
    30
}

// ── Web search ───────────────────────────────────────────────────

/// Web search tool configuration (`[web_search]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebSearchConfig {
    /// Enable `web_search_tool` for web searches
    #[serde(default)]
    pub enabled: bool,
    /// Search provider: "duckduckgo" (free, no API key) or "brave" (requires API key)
    #[serde(default = "default_web_search_provider")]
    pub provider: String,
    /// Brave Search API key (required if provider is "brave")
    #[serde(default)]
    pub brave_api_key: Option<String>,
    /// Maximum results per search (1-10)
    #[serde(default = "default_web_search_max_results")]
    pub max_results: usize,
    /// Request timeout in seconds
    #[serde(default = "default_web_search_timeout_secs")]
    pub timeout_secs: u64,
    /// Enable `web_fetch` tool (fetch and extract readable content from a URL)
    #[serde(default = "default_web_fetch_enabled")]
    pub fetch_enabled: bool,
    /// Maximum characters returned by `web_fetch` (default 10000)
    #[serde(default = "default_web_fetch_max_chars")]
    pub fetch_max_chars: usize,
}

fn default_web_search_provider() -> String {
    "duckduckgo".into()
}

const fn default_web_search_max_results() -> usize {
    5
}

const fn default_web_search_timeout_secs() -> u64 {
    15
}

const fn default_web_fetch_enabled() -> bool {
    true
}

const fn default_web_fetch_max_chars() -> usize {
    10_000
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_web_search_provider(),
            brave_api_key: None,
            max_results: default_web_search_max_results(),
            timeout_secs: default_web_search_timeout_secs(),
            fetch_enabled: default_web_fetch_enabled(),
            fetch_max_chars: default_web_fetch_max_chars(),
        }
    }
}

// ── Proxy ───────────────────────────────────────────────────────

/// Proxy application scope — determines which outbound traffic uses the proxy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProxyScope {
    /// Use system environment proxy variables only.
    Environment,
    /// Apply proxy to all OpenPRX-managed HTTP traffic (default).
    #[default]
    Zeroclaw,
    /// Apply proxy only to explicitly listed service selectors.
    Services,
}

/// Proxy configuration for outbound HTTP/HTTPS/SOCKS5 traffic (`[proxy]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProxyConfig {
    /// Enable proxy support for selected scope.
    #[serde(default)]
    pub enabled: bool,
    /// Proxy URL for HTTP requests (supports http, https, socks5, socks5h).
    #[serde(default)]
    pub http_proxy: Option<String>,
    /// Proxy URL for HTTPS requests (supports http, https, socks5, socks5h).
    #[serde(default)]
    pub https_proxy: Option<String>,
    /// Fallback proxy URL for all schemes.
    #[serde(default)]
    pub all_proxy: Option<String>,
    /// No-proxy bypass list. Same format as NO_PROXY.
    #[serde(default)]
    pub no_proxy: Vec<String>,
    /// Proxy application scope.
    #[serde(default)]
    pub scope: ProxyScope,
    /// Service selectors used when scope = "services".
    #[serde(default)]
    pub services: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_proxy: None,
            https_proxy: None,
            all_proxy: None,
            no_proxy: Vec::new(),
            scope: ProxyScope::Zeroclaw,
            services: Vec::new(),
        }
    }
}

impl ProxyConfig {
    pub const fn supported_service_keys() -> &'static [&'static str] {
        SUPPORTED_PROXY_SERVICE_KEYS
    }

    pub const fn supported_service_selectors() -> &'static [&'static str] {
        SUPPORTED_PROXY_SERVICE_SELECTORS
    }

    pub fn has_any_proxy_url(&self) -> bool {
        normalize_proxy_url_option(self.http_proxy.as_deref()).is_some()
            || normalize_proxy_url_option(self.https_proxy.as_deref()).is_some()
            || normalize_proxy_url_option(self.all_proxy.as_deref()).is_some()
    }

    pub fn normalized_services(&self) -> Vec<String> {
        normalize_service_list(self.services.clone())
    }

    pub fn normalized_no_proxy(&self) -> Vec<String> {
        normalize_no_proxy_list(self.no_proxy.clone())
    }

    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("http_proxy", self.http_proxy.as_deref()),
            ("https_proxy", self.https_proxy.as_deref()),
            ("all_proxy", self.all_proxy.as_deref()),
        ] {
            if let Some(url) = normalize_proxy_url_option(value) {
                validate_proxy_url(field, &url)?;
            }
        }

        for selector in self.normalized_services() {
            if !is_supported_proxy_service_selector(&selector) {
                anyhow::bail!(
                    "Unsupported proxy service selector '{selector}'. Use tool `proxy_config` action `list_services` for valid values"
                );
            }
        }

        if self.enabled && !self.has_any_proxy_url() {
            anyhow::bail!(
                "Proxy is enabled but no proxy URL is configured. Set at least one of http_proxy, https_proxy, or all_proxy"
            );
        }

        if self.enabled && self.scope == ProxyScope::Services && self.normalized_services().is_empty() {
            anyhow::bail!("proxy.scope='services' requires a non-empty proxy.services list when proxy is enabled");
        }

        Ok(())
    }

    pub fn should_apply_to_service(&self, service_key: &str) -> bool {
        if !self.enabled {
            return false;
        }

        match self.scope {
            ProxyScope::Environment => false,
            ProxyScope::Zeroclaw => true,
            ProxyScope::Services => {
                let service_key = service_key.trim().to_ascii_lowercase();
                if service_key.is_empty() {
                    return false;
                }

                self.normalized_services()
                    .iter()
                    .any(|selector| service_selector_matches(selector, &service_key))
            }
        }
    }

    pub fn apply_to_reqwest_builder(
        &self,
        mut builder: reqwest::ClientBuilder,
        service_key: &str,
    ) -> reqwest::ClientBuilder {
        if !self.should_apply_to_service(service_key) {
            return builder;
        }

        let no_proxy = self.no_proxy_value();

        if let Some(url) = normalize_proxy_url_option(self.all_proxy.as_deref()) {
            match reqwest::Proxy::all(&url) {
                Ok(proxy) => {
                    builder = builder.proxy(apply_no_proxy(proxy, no_proxy.clone()));
                }
                Err(error) => {
                    tracing::warn!(
                        proxy_url = %redact_url_credentials(&url),
                        service_key,
                        "Ignoring invalid all_proxy URL: {error}"
                    );
                }
            }
        }

        if let Some(url) = normalize_proxy_url_option(self.http_proxy.as_deref()) {
            match reqwest::Proxy::http(&url) {
                Ok(proxy) => {
                    builder = builder.proxy(apply_no_proxy(proxy, no_proxy.clone()));
                }
                Err(error) => {
                    tracing::warn!(
                        proxy_url = %redact_url_credentials(&url),
                        service_key,
                        "Ignoring invalid http_proxy URL: {error}"
                    );
                }
            }
        }

        if let Some(url) = normalize_proxy_url_option(self.https_proxy.as_deref()) {
            match reqwest::Proxy::https(&url) {
                Ok(proxy) => {
                    builder = builder.proxy(apply_no_proxy(proxy, no_proxy));
                }
                Err(error) => {
                    tracing::warn!(
                        proxy_url = %redact_url_credentials(&url),
                        service_key,
                        "Ignoring invalid https_proxy URL: {error}"
                    );
                }
            }
        }

        builder
    }

    pub fn apply_to_process_env(&self) {
        set_proxy_env_pair("HTTP_PROXY", self.http_proxy.as_deref());
        set_proxy_env_pair("HTTPS_PROXY", self.https_proxy.as_deref());
        set_proxy_env_pair("ALL_PROXY", self.all_proxy.as_deref());

        let no_proxy_joined = {
            let list = self.normalized_no_proxy();
            (!list.is_empty()).then(|| list.join(","))
        };
        set_proxy_env_pair("NO_PROXY", no_proxy_joined.as_deref());
    }

    pub fn clear_process_env() {
        clear_proxy_env_pair("HTTP_PROXY");
        clear_proxy_env_pair("HTTPS_PROXY");
        clear_proxy_env_pair("ALL_PROXY");
        clear_proxy_env_pair("NO_PROXY");
    }

    fn no_proxy_value(&self) -> Option<reqwest::NoProxy> {
        let joined = {
            let list = self.normalized_no_proxy();
            (!list.is_empty()).then(|| list.join(","))
        };
        joined.as_deref().and_then(reqwest::NoProxy::from_string)
    }
}

fn apply_no_proxy(proxy: reqwest::Proxy, no_proxy: Option<reqwest::NoProxy>) -> reqwest::Proxy {
    proxy.no_proxy(no_proxy)
}

/// Redact credentials (user:pass) from a proxy URL before logging.
///
/// Turns `http://user:pass@host:port/path` into `http://[REDACTED]@host:port/path`.
/// Returns the original string unchanged if no credentials are present.
fn redact_url_credentials(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(scheme_end) = url.find("://") {
            let scheme = &url[..scheme_end + 3];
            let after_at = &url[at_pos + 1..];
            return format!("{scheme}[REDACTED]@{after_at}");
        }
    }
    url.to_string()
}

fn normalize_proxy_url_option(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn normalize_no_proxy_list(values: Vec<String>) -> Vec<String> {
    normalize_comma_values(values)
}

fn normalize_service_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = normalize_comma_values(values)
        .into_iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn normalize_comma_values(values: Vec<String>) -> Vec<String> {
    let mut output = Vec::new();
    for value in values {
        for part in value.split(',') {
            let normalized = part.trim();
            if normalized.is_empty() {
                continue;
            }
            output.push(normalized.to_string());
        }
    }
    output.sort_unstable();
    output.dedup();
    output
}

fn is_supported_proxy_service_selector(selector: &str) -> bool {
    if SUPPORTED_PROXY_SERVICE_KEYS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(selector))
    {
        return true;
    }

    SUPPORTED_PROXY_SERVICE_SELECTORS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(selector))
}

fn service_selector_matches(selector: &str, service_key: &str) -> bool {
    if selector == service_key {
        return true;
    }

    if let Some(prefix) = selector.strip_suffix(".*") {
        return service_key.starts_with(prefix)
            && service_key
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('.'));
    }

    false
}

fn validate_proxy_url(field: &str, url: &str) -> Result<()> {
    let redacted = redact_url_credentials(url);
    let parsed =
        reqwest::Url::parse(url).with_context(|| format!("Invalid {field} URL: '{redacted}' is not a valid URL"))?;

    match parsed.scheme() {
        "http" | "https" | "socks5" | "socks5h" => {}
        scheme => {
            anyhow::bail!("Invalid {field} URL scheme '{scheme}'. Allowed: http, https, socks5, socks5h");
        }
    }

    if parsed.host_str().is_none() {
        anyhow::bail!("Invalid {field} URL: host is required");
    }

    Ok(())
}

#[allow(unsafe_code)]
fn set_proxy_env_pair(key: &str, value: Option<&str>) {
    let lowercase_key = key.to_ascii_lowercase();
    if let Some(value) = value.and_then(|candidate| normalize_proxy_url_option(Some(candidate))) {
        // SAFETY: Called during single-threaded config initialization (apply_env_overrides)
        // before any concurrent HTTP clients read these variables.
        unsafe {
            std::env::set_var(key, &value);
            std::env::set_var(lowercase_key, value);
        }
    } else {
        // SAFETY: Same single-threaded initialization context as set branch above.
        unsafe {
            std::env::remove_var(key);
            std::env::remove_var(lowercase_key);
        }
    }
}

#[allow(unsafe_code)]
fn clear_proxy_env_pair(key: &str) {
    // SAFETY: Called during single-threaded config initialization to clear stale
    // proxy env vars before any concurrent HTTP clients are created.
    unsafe {
        std::env::remove_var(key);
        std::env::remove_var(key.to_ascii_lowercase());
    }
}

fn runtime_proxy_state() -> &'static RwLock<ProxyConfig> {
    RUNTIME_PROXY_CONFIG.get_or_init(|| RwLock::new(ProxyConfig::default()))
}

fn runtime_proxy_client_cache() -> &'static RwLock<HashMap<String, reqwest::Client>> {
    RUNTIME_PROXY_CLIENT_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn clear_runtime_proxy_client_cache() {
    runtime_proxy_client_cache().write().clear();
}

fn runtime_proxy_cache_key(service_key: &str, timeout_secs: Option<u64>, connect_timeout_secs: Option<u64>) -> String {
    format!(
        "{}|timeout={}|connect_timeout={}",
        service_key.trim().to_ascii_lowercase(),
        timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string()),
        connect_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string())
    )
}

fn runtime_proxy_cached_client(cache_key: &str) -> Option<reqwest::Client> {
    runtime_proxy_client_cache().read().get(cache_key).cloned()
}

fn set_runtime_proxy_cached_client(cache_key: String, client: reqwest::Client) {
    runtime_proxy_client_cache().write().insert(cache_key, client);
}

pub fn set_runtime_proxy_config(config: ProxyConfig) {
    *runtime_proxy_state().write() = config;
    clear_runtime_proxy_client_cache();
}

pub fn runtime_proxy_config() -> ProxyConfig {
    runtime_proxy_state().read().clone()
}

pub fn apply_runtime_proxy_to_builder(builder: reqwest::ClientBuilder, service_key: &str) -> reqwest::ClientBuilder {
    runtime_proxy_config().apply_to_reqwest_builder(builder, service_key)
}

pub fn build_runtime_proxy_client(service_key: &str) -> Result<reqwest::Client> {
    let cache_key = runtime_proxy_cache_key(service_key, None, None);
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return Ok(client);
    }

    let builder = apply_runtime_proxy_to_builder(reqwest::Client::builder(), service_key);
    let client = builder
        .build()
        .map_err(|e| anyhow::anyhow!("proxy client build failed for {service_key}: {e}"))?;
    set_runtime_proxy_cached_client(cache_key, client.clone());
    Ok(client)
}

pub fn build_runtime_proxy_client_with_timeouts(
    service_key: &str,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> Result<reqwest::Client> {
    let cache_key = runtime_proxy_cache_key(service_key, Some(timeout_secs), Some(connect_timeout_secs));
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return Ok(client);
    }

    let builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs));
    let builder = apply_runtime_proxy_to_builder(builder, service_key);
    let client = builder
        .build()
        .map_err(|e| anyhow::anyhow!("proxy client build failed for {service_key}: {e}"))?;
    set_runtime_proxy_cached_client(cache_key, client.clone());
    Ok(client)
}

fn parse_proxy_scope(raw: &str) -> Option<ProxyScope> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "environment" | "env" => Some(ProxyScope::Environment),
        "prx" | "internal" | "core" => Some(ProxyScope::Zeroclaw),
        "services" | "service" => Some(ProxyScope::Services),
        _ => None,
    }
}

fn parse_proxy_enabled(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
// ── Memory ───────────────────────────────────────────────────

/// Persistent storage configuration (`[storage]` section).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct StorageConfig {
    /// Storage provider settings (e.g. sqlite, postgres).
    #[serde(default)]
    pub provider: StorageProviderSection,
}

/// Wrapper for the storage provider configuration section.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct StorageProviderSection {
    /// Storage provider backend settings.
    #[serde(default)]
    pub config: StorageProviderConfig,
}

/// Storage provider backend configuration (e.g. postgres connection details).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StorageProviderConfig {
    /// Storage engine key (e.g. "postgres", "sqlite").
    #[serde(default)]
    pub provider: String,

    /// Connection URL for remote providers.
    /// Accepts legacy aliases: dbURL, database_url, databaseUrl.
    #[serde(default, alias = "dbURL", alias = "database_url", alias = "databaseUrl")]
    pub db_url: Option<String>,

    /// Database schema for SQL backends.
    #[serde(default = "default_storage_schema")]
    pub schema: String,

    /// Table name for memory entries.
    #[serde(default = "default_storage_table")]
    pub table: String,

    /// Optional connection timeout in seconds for remote providers.
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,
}

fn default_storage_schema() -> String {
    "public".into()
}

fn default_storage_table() -> String {
    "memories".into()
}

impl Default for StorageProviderConfig {
    fn default() -> Self {
        Self {
            provider: String::new(),
            db_url: None,
            schema: default_storage_schema(),
            table: default_storage_table(),
            connect_timeout_secs: None,
        }
    }
}

/// Memory backend configuration (`[memory]` section).
///
/// Controls conversation memory storage, embeddings, hybrid search, response caching,
/// and memory snapshot/hydration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)]
pub struct MemoryConfig {
    /// "sqlite" | "lucid" | "postgres" | "markdown" | "none" (`none` = explicit no-op memory)
    ///
    /// `postgres` requires `[storage.provider.config]` with `db_url` (`dbURL` alias supported).
    pub backend: String,
    /// Auto-save user-stated conversation input to memory (assistant output is excluded)
    pub auto_save: bool,
    /// Feature gate for memory ACL enforcement. Phase 0 keeps this disabled.
    #[serde(default)]
    pub acl_enabled: bool,
    /// Run memory/session hygiene (archiving + retention cleanup)
    #[serde(default = "default_hygiene_enabled")]
    pub hygiene_enabled: bool,
    /// Archive daily/session files older than this many days
    #[serde(default = "default_archive_after_days")]
    pub archive_after_days: u32,
    /// Purge archived files older than this many days
    #[serde(default = "default_purge_after_days")]
    pub purge_after_days: u32,
    /// For sqlite backend: prune conversation rows older than this many days
    #[serde(default = "default_conversation_retention_days")]
    pub conversation_retention_days: u32,
    /// For sqlite backend: prune daily rows older than this many days
    #[serde(default = "default_daily_retention_days")]
    pub daily_retention_days: u32,
    /// Embedding provider: "none" | "openai" | "custom:URL"
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// Embedding model name (e.g. "text-embedding-3-small")
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Embedding vector dimensions
    #[serde(default = "default_embedding_dims")]
    pub embedding_dimensions: usize,
    /// Weight for vector similarity in hybrid search (0.0–1.0)
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    /// Weight for keyword BM25 in hybrid search (0.0–1.0)
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    /// Minimum hybrid score (0.0–1.0) for a memory to be included in context.
    /// Memories scoring below this threshold are dropped to prevent irrelevant
    /// context from bleeding into conversations. Default: 0.4
    #[serde(default = "default_min_relevance_score")]
    pub min_relevance_score: f64,
    /// Max embedding cache entries before LRU eviction
    #[serde(default = "default_cache_size")]
    pub embedding_cache_size: usize,

    // ── Memory Snapshot (soul backup to Markdown) ─────────────
    /// Enable periodic export of core memories to MEMORY_SNAPSHOT.md
    #[serde(default)]
    pub snapshot_enabled: bool,
    /// Run snapshot during hygiene passes (heartbeat-driven)
    #[serde(default)]
    pub snapshot_on_hygiene: bool,
    /// Auto-hydrate from MEMORY_SNAPSHOT.md when brain.db is missing
    #[serde(default = "default_true")]
    pub auto_hydrate: bool,

    // ── SQLite backend options ─────────────────────────────────
    /// For sqlite backend: max seconds to wait when opening the DB (e.g. file locked).
    /// None = wait indefinitely (default). Recommended max: 300.
    #[serde(default)]
    pub sqlite_open_timeout_secs: Option<u64>,
}

fn default_embedding_provider() -> String {
    "none".into()
}
const fn default_hygiene_enabled() -> bool {
    true
}
const fn default_archive_after_days() -> u32 {
    7
}
const fn default_purge_after_days() -> u32 {
    30
}
const fn default_conversation_retention_days() -> u32 {
    3
}
const fn default_daily_retention_days() -> u32 {
    7
}
fn default_embedding_model() -> String {
    "text-embedding-3-small".into()
}
const fn default_embedding_dims() -> usize {
    1536
}
const fn default_vector_weight() -> f64 {
    0.7
}
const fn default_keyword_weight() -> f64 {
    0.3
}
const fn default_min_relevance_score() -> f64 {
    0.4
}
const fn default_cache_size() -> usize {
    10_000
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: "sqlite".into(),
            auto_save: true,
            acl_enabled: false,
            hygiene_enabled: default_hygiene_enabled(),
            archive_after_days: default_archive_after_days(),
            purge_after_days: default_purge_after_days(),
            conversation_retention_days: default_conversation_retention_days(),
            daily_retention_days: default_daily_retention_days(),
            embedding_provider: default_embedding_provider(),
            embedding_model: default_embedding_model(),
            embedding_dimensions: default_embedding_dims(),
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            min_relevance_score: default_min_relevance_score(),
            embedding_cache_size: default_cache_size(),
            snapshot_enabled: false,
            snapshot_on_hygiene: false,
            auto_hydrate: true,
            sqlite_open_timeout_secs: None,
        }
    }
}

/// Startup identity binding entry (`[[identity_bindings]]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IdentityBindingConfig {
    pub user_id: String,
    pub channel: String,
    pub channel_account: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Startup user policy entry (`[[user_policies]]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UserPolicyConfig {
    pub user_id: String,
    #[serde(default = "default_user_policy_role")]
    pub role: String,
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default = "default_user_policy_visibility_ceiling")]
    pub visibility_ceiling: String,
    #[serde(default)]
    pub blocked_patterns: Vec<String>,
}

fn default_user_policy_role() -> String {
    "guest".into()
}

fn default_user_policy_visibility_ceiling() -> String {
    "private".into()
}

// ── Observability ─────────────────────────────────────────────────

/// Observability backend configuration (`[observability]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ObservabilityConfig {
    /// "none" | "log" | "prometheus" | "otel"
    pub backend: String,

    /// OTLP endpoint (e.g. "http://localhost:4318"). Only used when backend = "otel".
    #[serde(default)]
    pub otel_endpoint: Option<String>,

    /// Service name reported to the OTel collector. Defaults to "prx".
    #[serde(default)]
    pub otel_service_name: Option<String>,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            backend: "none".into(),
            otel_endpoint: None,
            otel_service_name: None,
        }
    }
}

// ── Autonomy / Security ──────────────────────────────────────────

/// A single scope rule for tool access control.
///
/// Rules are evaluated top-to-bottom; the first matching rule wins.
/// Deny always takes priority over allow within a rule.
/// A rule matches when ALL specified criteria match (logical AND).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ScopeRule {
    /// Match by sender identity: UUID (e.g. "uuid:xxx"), phone number, or "*" for any sender.
    pub user: Option<String>,
    /// Match by channel name: "signal", "telegram", "discord", etc.
    pub channel: Option<String>,
    /// Match by chat type: "direct", "group", or "*" for any type.
    pub chat_type: Option<String>,
    /// Tool whitelist: only these tools are allowed (empty = all tools permitted).
    #[serde(default)]
    pub tools_allow: Vec<String>,
    /// Tool blacklist: these tools are denied regardless of allow list.
    #[serde(default)]
    pub tools_deny: Vec<String>,
}

fn default_scope_action() -> String {
    "allow".to_string()
}

/// Scope-based tool access control configuration (`[autonomy.scopes]`).
///
/// Controls which tools are available per-user, per-channel, and per-chat-type.
///
/// Example config:
/// ```toml
/// [autonomy.scopes]
/// default = "allow"
///
/// [[autonomy.scopes.rules]]
/// channel = "signal"
/// chat_type = "group"
/// tools_deny = ["shell", "file_write"]
///
/// [[autonomy.scopes.rules]]
/// user = "uuid:untrusted-user-uuid"
/// tools_allow = ["memory_recall"]   # whitelist-only
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScopeConfig {
    /// Default action when no rule matches: "allow" (default) or "deny".
    #[serde(default = "default_scope_action")]
    pub default: String,
    /// Ordered list of scope rules evaluated top-to-bottom.
    #[serde(default)]
    pub rules: Vec<ScopeRule>,
}

impl Default for ScopeConfig {
    fn default() -> Self {
        Self {
            default: default_scope_action(),
            rules: Vec::new(),
        }
    }
}

/// Autonomy and security policy configuration (`[autonomy]` section).
///
/// Controls what the agent is allowed to do: shell commands, filesystem access,
/// risk approval gates, and per-policy budgets.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutonomyConfig {
    /// Autonomy level: `read_only`, `supervised` (default), or `full`.
    pub level: AutonomyLevel,
    /// Restrict file writes and command paths to the workspace directory. Default: `true`.
    pub workspace_only: bool,
    /// Allowlist of executable names permitted for shell execution.
    pub allowed_commands: Vec<String>,
    /// Explicit path denylist. Default includes system-critical paths.
    pub forbidden_paths: Vec<String>,
    /// Maximum actions allowed per hour per policy. Default: `100`.
    pub max_actions_per_hour: u32,
    /// Maximum cost per day in cents per policy. Default: `1000`.
    pub max_cost_per_day_cents: u32,

    /// Require explicit approval for medium-risk shell commands.
    #[serde(default = "default_true")]
    pub require_approval_for_medium_risk: bool,

    /// Block high-risk shell commands even if allowlisted.
    #[serde(default = "default_true")]
    pub block_high_risk_commands: bool,

    /// Tools that never require approval (e.g. read-only tools).
    #[serde(default = "default_auto_approve")]
    pub auto_approve: Vec<String>,

    /// Tools that always require interactive approval, even after "Always".
    #[serde(default = "default_always_ask")]
    pub always_ask: Vec<String>,

    /// Scope-based tool access control: per-user/channel/chat-type allow/deny rules.
    #[serde(default)]
    pub scopes: ScopeConfig,
}

fn default_auto_approve() -> Vec<String> {
    vec!["file_read".into(), "memory_recall".into()]
}

const fn default_always_ask() -> Vec<String> {
    vec![]
}

impl Default for AutonomyConfig {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Supervised,
            workspace_only: true,
            allowed_commands: vec![
                "git".into(),
                "npm".into(),
                "cargo".into(),
                "ls".into(),
                "cat".into(),
                "grep".into(),
                "find".into(),
                "echo".into(),
                "pwd".into(),
                "wc".into(),
                "head".into(),
                "tail".into(),
            ],
            forbidden_paths: vec![
                "/etc".into(),
                "/root".into(),
                "/home".into(),
                "/usr".into(),
                "/bin".into(),
                "/sbin".into(),
                "/lib".into(),
                "/opt".into(),
                "/boot".into(),
                "/dev".into(),
                "/proc".into(),
                "/sys".into(),
                "/var".into(),
                "/tmp".into(),
                "~/.ssh".into(),
                "~/.gnupg".into(),
                "~/.aws".into(),
                "~/.config".into(),
            ],
            max_actions_per_hour: 20,
            max_cost_per_day_cents: 500,
            require_approval_for_medium_risk: true,
            block_high_risk_commands: true,
            auto_approve: default_auto_approve(),
            always_ask: default_always_ask(),
            scopes: ScopeConfig::default(),
        }
    }
}

// ── Runtime ──────────────────────────────────────────────────────

/// Runtime adapter configuration (`[runtime]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeConfig {
    /// Runtime kind (`native` | `docker`).
    #[serde(default = "default_runtime_kind")]
    pub kind: String,

    /// Docker runtime settings (used when `kind = "docker"`).
    #[serde(default)]
    pub docker: DockerRuntimeConfig,

    /// Global reasoning override for providers that expose explicit controls.
    /// - `None`: provider default behavior
    /// - `Some(true)`: request reasoning/thinking when supported
    /// - `Some(false)`: disable reasoning/thinking when supported
    #[serde(default)]
    pub reasoning_enabled: Option<bool>,
}

/// Docker runtime configuration (`[runtime.docker]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DockerRuntimeConfig {
    /// Runtime image used to execute shell commands.
    #[serde(default = "default_docker_image")]
    pub image: String,

    /// Docker network mode (`none`, `bridge`, etc.).
    #[serde(default = "default_docker_network")]
    pub network: String,

    /// Optional memory limit in MB (`None` = no explicit limit).
    #[serde(default = "default_docker_memory_limit_mb")]
    pub memory_limit_mb: Option<u64>,

    /// Optional CPU limit (`None` = no explicit limit).
    #[serde(default = "default_docker_cpu_limit")]
    pub cpu_limit: Option<f64>,

    /// Mount root filesystem as read-only.
    #[serde(default = "default_true")]
    pub read_only_rootfs: bool,

    /// Mount configured workspace into `/workspace`.
    #[serde(default = "default_true")]
    pub mount_workspace: bool,

    /// Optional workspace root allowlist for Docker mount validation.
    #[serde(default)]
    pub allowed_workspace_roots: Vec<String>,
}

fn default_runtime_kind() -> String {
    "native".into()
}

fn default_docker_image() -> String {
    "alpine:3.20".into()
}

fn default_docker_network() -> String {
    "none".into()
}

const fn default_docker_memory_limit_mb() -> Option<u64> {
    Some(512)
}

const fn default_docker_cpu_limit() -> Option<f64> {
    Some(1.0)
}

impl Default for DockerRuntimeConfig {
    fn default() -> Self {
        Self {
            image: default_docker_image(),
            network: default_docker_network(),
            memory_limit_mb: default_docker_memory_limit_mb(),
            cpu_limit: default_docker_cpu_limit(),
            read_only_rootfs: true,
            mount_workspace: true,
            allowed_workspace_roots: Vec::new(),
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            kind: default_runtime_kind(),
            docker: DockerRuntimeConfig::default(),
            reasoning_enabled: None,
        }
    }
}

// ── Reliability / supervision ────────────────────────────────────

/// Reliability and supervision configuration (`[reliability]` section).
///
/// Controls provider retries, fallback chains, API key rotation, and channel restart backoff.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReliabilityConfig {
    /// Retries per provider before failing over.
    #[serde(default = "default_provider_retries")]
    pub provider_retries: u32,
    /// Base backoff (ms) for provider retry delay.
    #[serde(default = "default_provider_backoff_ms")]
    pub provider_backoff_ms: u64,
    /// Fallback provider chain (e.g. `["anthropic", "openai"]`).
    #[serde(default)]
    pub fallback_providers: Vec<String>,
    /// Additional API keys for round-robin rotation on rate-limit (429) errors.
    /// The primary `api_key` is always tried first; these are extras.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Per-model fallback chains. When a model fails, try these alternatives in order.
    /// Example: `{ "claude-opus-4-20250514" = ["claude-sonnet-4-20250514", "gpt-4o"] }`
    #[serde(default)]
    pub model_fallbacks: std::collections::HashMap<String, Vec<String>>,
    /// Initial backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_secs")]
    pub channel_initial_backoff_secs: u64,
    /// Max backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_max_secs")]
    pub channel_max_backoff_secs: u64,
    /// Scheduler polling cadence in seconds.
    #[serde(default = "default_scheduler_poll_secs")]
    pub scheduler_poll_secs: u64,
    /// Max retries for cron job execution attempts.
    #[serde(default = "default_scheduler_retries")]
    pub scheduler_retries: u32,
}

const fn default_provider_retries() -> u32 {
    2
}

const fn default_provider_backoff_ms() -> u64 {
    500
}

const fn default_channel_backoff_secs() -> u64 {
    2
}

const fn default_channel_backoff_max_secs() -> u64 {
    60
}

const fn default_scheduler_poll_secs() -> u64 {
    15
}

const fn default_scheduler_retries() -> u32 {
    2
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            provider_retries: default_provider_retries(),
            provider_backoff_ms: default_provider_backoff_ms(),
            fallback_providers: Vec::new(),
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: default_channel_backoff_secs(),
            channel_max_backoff_secs: default_channel_backoff_max_secs(),
            scheduler_poll_secs: default_scheduler_poll_secs(),
            scheduler_retries: default_scheduler_retries(),
        }
    }
}

// ── Scheduler ────────────────────────────────────────────────────

/// Scheduler configuration for periodic task execution (`[scheduler]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SchedulerConfig {
    /// Enable the built-in scheduler loop.
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,
    /// Maximum number of persisted scheduled tasks.
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    /// Maximum tasks executed per scheduler polling cycle.
    #[serde(default = "default_scheduler_max_concurrent")]
    pub max_concurrent: usize,
}

const fn default_scheduler_enabled() -> bool {
    true
}

const fn default_scheduler_max_tasks() -> usize {
    64
}

const fn default_scheduler_max_concurrent() -> usize {
    4
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: default_scheduler_enabled(),
            max_tasks: default_scheduler_max_tasks(),
            max_concurrent: default_scheduler_max_concurrent(),
        }
    }
}

// ── Model routing ────────────────────────────────────────────────

/// Route a task hint to a specific provider + model.
///
/// ```toml
/// [[model_routes]]
/// hint = "reasoning"
/// provider = "openrouter"
/// model = "anthropic/claude-opus-4-20250514"
///
/// [[model_routes]]
/// hint = "fast"
/// provider = "groq"
/// model = "llama-3.3-70b-versatile"
/// ```
///
/// Usage: pass `hint:reasoning` as the model parameter to route the request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelRouteConfig {
    /// Task hint name (e.g. "reasoning", "fast", "code", "summarize")
    pub hint: String,
    /// Provider to route to (must match a known provider name)
    pub provider: String,
    /// Model to use with that provider
    pub model: String,
    /// Optional API key override for this route's provider
    #[serde(default)]
    pub api_key: Option<String>,
}

// ── Embedding routing ───────────────────────────────────────────

/// Route an embedding hint to a specific provider + model.
///
/// ```toml
/// [[embedding_routes]]
/// hint = "semantic"
/// provider = "openai"
/// model = "text-embedding-3-small"
/// dimensions = 1536
///
/// [memory]
/// embedding_model = "hint:semantic"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmbeddingRouteConfig {
    /// Route hint name (e.g. "semantic", "archive", "faq")
    pub hint: String,
    /// Embedding provider (`none`, `openai`, or `custom:<url>`)
    pub provider: String,
    /// Embedding model to use with that provider
    pub model: String,
    /// Optional embedding dimension override for this route
    #[serde(default)]
    pub dimensions: Option<usize>,
    /// Optional API key override for this route's provider
    #[serde(default)]
    pub api_key: Option<String>,
}

// ── Query Classification ─────────────────────────────────────────

/// Automatic query classification — classifies user messages by keyword/pattern
/// and routes to the appropriate model hint. Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct QueryClassificationConfig {
    /// Enable automatic query classification. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Classification rules evaluated in priority order.
    #[serde(default)]
    pub rules: Vec<ClassificationRule>,
}

/// A single classification rule mapping message patterns to a model hint.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct ClassificationRule {
    /// Must match a `[[model_routes]]` hint value.
    pub hint: String,
    /// Case-insensitive substring matches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Case-sensitive literal matches (for "```", "fn ", etc.).
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Only match if message length >= N chars.
    #[serde(default)]
    pub min_length: Option<usize>,
    /// Only match if message length <= N chars.
    #[serde(default)]
    pub max_length: Option<usize>,
    /// Higher priority rules are checked first.
    #[serde(default)]
    pub priority: i32,
}

/// Task-routing default and rule intent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TaskRoutingIntentConfig {
    #[default]
    Simple,
    Delegate,
    Stream,
}

/// Lightweight pre-LLM task routing.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct TaskRoutingConfig {
    /// Enable task intent routing before entering the main tool loop.
    #[serde(default)]
    pub enabled: bool,
    /// Fallback intent when no rule matches.
    #[serde(default)]
    pub default_intent: TaskRoutingIntentConfig,
    /// Rules evaluated in descending priority order.
    #[serde(default)]
    pub rules: Vec<TaskRoutingRule>,
}

/// A single task-routing rule.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct TaskRoutingRule {
    /// Case-insensitive substring matches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Route intent to apply when a keyword matches.
    #[serde(default)]
    pub intent: TaskRoutingIntentConfig,
    /// Optional model hint/raw model for non-delegated requests.
    #[serde(default)]
    pub model_hint: Option<String>,
    /// Optional raw sub-agent model override for delegated tasks.
    #[serde(default)]
    pub sub_agent_model: Option<String>,
    /// Higher priority rules are checked first.
    #[serde(default)]
    pub priority: i32,
}

// ── Heartbeat ────────────────────────────────────────────────────

/// Heartbeat configuration for periodic health pings (`[heartbeat]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HeartbeatConfig {
    /// Enable periodic heartbeat pings. Default: `false`.
    pub enabled: bool,
    /// Interval in minutes between heartbeat pings. Default: `30`.
    pub interval_minutes: u32,
    /// Inclusive active hour window `[start_hour, end_hour]` in local time.
    /// Heartbeat ticks outside this range are skipped.
    #[serde(default = "default_heartbeat_active_hours")]
    pub active_hours: Vec<u8>,
    /// Prompt used by heartbeat worker when dispatching tasks.
    #[serde(default = "default_heartbeat_prompt")]
    pub prompt: String,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 30,
            active_hours: default_heartbeat_active_hours(),
            prompt: default_heartbeat_prompt(),
        }
    }
}

fn default_heartbeat_active_hours() -> Vec<u8> {
    vec![8, 23]
}

fn default_heartbeat_prompt() -> String {
    "Check HEARTBEAT.md and follow instructions.".to_string()
}

/// Policy for direct-message handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    /// Future pairing handshake flow (placeholder; runtime currently falls back to allowlist).
    Pairing,
    /// Accept only senders listed in `allowed_from`.
    #[default]
    Allowlist,
    /// Accept any direct sender.
    Open,
    /// Ignore direct messages.
    Disabled,
}

/// Policy for group-message handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GroupPolicy {
    /// Accept only groups listed in `group_allow_from`.
    #[default]
    Allowlist,
    /// Accept any group.
    Open,
    /// Ignore group messages.
    Disabled,
}

// ── Cron ────────────────────────────────────────────────────────

/// Cron job configuration (`[cron]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronConfig {
    /// Enable the cron subsystem. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum number of historical cron run records to retain. Default: `50`.
    #[serde(default = "default_max_run_history")]
    pub max_run_history: u32,
}

const fn default_max_run_history() -> u32 {
    50
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_run_history: default_max_run_history(),
        }
    }
}

// ── Tunnel ──────────────────────────────────────────────────────

/// Tunnel configuration for exposing the gateway publicly (`[tunnel]` section).
///
/// Supported providers: `"none"` (default), `"cloudflare"`, `"tailscale"`, `"ngrok"`, `"custom"`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TunnelConfig {
    /// Tunnel provider: `"none"`, `"cloudflare"`, `"tailscale"`, `"ngrok"`, or `"custom"`. Default: `"none"`.
    pub provider: String,

    /// Cloudflare Tunnel configuration (used when `provider = "cloudflare"`).
    #[serde(default)]
    pub cloudflare: Option<CloudflareTunnelConfig>,

    /// Tailscale Funnel/Serve configuration (used when `provider = "tailscale"`).
    #[serde(default)]
    pub tailscale: Option<TailscaleTunnelConfig>,

    /// ngrok tunnel configuration (used when `provider = "ngrok"`).
    #[serde(default)]
    pub ngrok: Option<NgrokTunnelConfig>,

    /// Custom tunnel command configuration (used when `provider = "custom"`).
    #[serde(default)]
    pub custom: Option<CustomTunnelConfig>,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            provider: "none".into(),
            cloudflare: None,
            tailscale: None,
            ngrok: None,
            custom: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloudflareTunnelConfig {
    /// Cloudflare Tunnel token (from Zero Trust dashboard)
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TailscaleTunnelConfig {
    /// Use Tailscale Funnel (public internet) vs Serve (tailnet only)
    #[serde(default)]
    pub funnel: bool,
    /// Optional hostname override
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NgrokTunnelConfig {
    /// ngrok auth token
    pub auth_token: String,
    /// Optional custom domain
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CustomTunnelConfig {
    /// Command template to start the tunnel. Use {port} and {host} placeholders.
    /// Example: "bore local {port} --to bore.pub"
    pub start_command: String,
    /// Optional URL to check tunnel health
    pub health_url: Option<String>,
    /// Optional regex to extract public URL from command stdout
    pub url_pattern: Option<String>,
}

// ── Channels ─────────────────────────────────────────────────────

/// Top-level channel configurations (`[channels_config]` section).
///
/// Each channel sub-section (e.g. `telegram`, `discord`) is optional;
/// setting it to `Some(...)` enables that channel.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChannelsConfig {
    /// Enable the CLI interactive channel. Default: `true`.
    pub cli: bool,
    /// Telegram bot channel configuration.
    pub telegram: Option<TelegramConfig>,
    /// Discord bot channel configuration.
    pub discord: Option<DiscordConfig>,
    /// Slack bot channel configuration.
    pub slack: Option<SlackConfig>,
    /// Mattermost bot channel configuration.
    pub mattermost: Option<MattermostConfig>,
    /// Webhook channel configuration.
    pub webhook: Option<WebhookConfig>,
    /// iMessage channel configuration (macOS only).
    pub imessage: Option<IMessageConfig>,
    /// Matrix channel configuration.
    pub matrix: Option<MatrixConfig>,
    /// Signal channel configuration.
    pub signal: Option<SignalConfig>,
    /// WhatsApp channel configuration (Cloud API or Web mode).
    pub whatsapp: Option<WhatsAppConfig>,
    /// wacli JSON-RPC daemon channel configuration.
    #[serde(default)]
    pub wacli: Option<WacliConfig>,
    /// Linq Partner API channel configuration.
    pub linq: Option<LinqConfig>,
    /// Nextcloud Talk bot channel configuration.
    pub nextcloud_talk: Option<NextcloudTalkConfig>,
    /// Email channel configuration.
    pub email: Option<crate::channels::email_channel::EmailConfig>,
    /// IRC channel configuration.
    pub irc: Option<IrcConfig>,
    /// Lark/Feishu channel configuration.
    pub lark: Option<LarkConfig>,
    /// DingTalk channel configuration.
    pub dingtalk: Option<DingTalkConfig>,
    /// QQ Official Bot channel configuration.
    pub qq: Option<QQConfig>,
    /// Base timeout in seconds for processing a single channel message (LLM + tools).
    /// Runtime uses this as a per-turn budget that scales with tool-loop depth
    /// (up to 4x, capped) so one slow/retried model call does not consume the
    /// entire conversation budget.
    /// Default: 300s for on-device LLMs (Ollama) which are slower than cloud APIs.
    #[serde(default = "default_channel_message_timeout_secs")]
    pub message_timeout_secs: u64,
}

const fn default_channel_message_timeout_secs() -> u64 {
    300
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            cli: true,
            telegram: None,
            discord: None,
            slack: None,
            mattermost: None,
            webhook: None,
            imessage: None,
            matrix: None,
            signal: None,
            whatsapp: None,
            wacli: None,
            linq: None,
            nextcloud_talk: None,
            email: None,
            irc: None,
            lark: None,
            dingtalk: None,
            qq: None,
            message_timeout_secs: default_channel_message_timeout_secs(),
        }
    }
}

/// Streaming mode for channels that support progressive message updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    /// No streaming -- send the complete response as a single message (default).
    #[default]
    Off,
    /// Update a draft message with every flush interval.
    Partial,
}

const fn default_draft_update_interval_ms() -> u64 {
    1000
}

/// Telegram bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TelegramConfig {
    /// Telegram Bot API token (from @BotFather).
    pub bot_token: String,
    /// Allowed Telegram user IDs or usernames. Empty = deny all.
    pub allowed_users: Vec<String>,
    /// Streaming mode for progressive response delivery via message edits.
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Minimum interval (ms) between draft message edits to avoid rate limits.
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// When true, a newer Telegram message from the same sender in the same chat
    /// cancels the in-flight request and starts a fresh response with preserved history.
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// When true, only respond to messages that @-mention the bot in groups.
    /// Direct messages are always processed.
    #[serde(default)]
    pub mention_only: bool,
}

/// Discord bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiscordConfig {
    /// Discord bot token (from Discord Developer Portal).
    pub bot_token: String,
    /// Optional guild (server) ID to restrict the bot to a single guild.
    pub guild_id: Option<String>,
    /// Allowed Discord user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, process messages from other bots (not just humans).
    /// The bot still ignores its own messages to prevent feedback loops.
    #[serde(default)]
    pub listen_to_bots: bool,
    /// When true, only respond to messages that @-mention the bot.
    /// Other messages in the guild are silently ignored.
    #[serde(default)]
    pub mention_only: bool,
}

/// Slack bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SlackConfig {
    /// Slack bot OAuth token (xoxb-...).
    pub bot_token: String,
    /// Slack app-level token for Socket Mode (xapp-...).
    pub app_token: Option<String>,
    /// Optional channel ID to restrict the bot to a single channel.
    pub channel_id: Option<String>,
    /// Allowed Slack user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

/// Mattermost bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MattermostConfig {
    /// Mattermost server URL (e.g. `"https://mattermost.example.com"`).
    pub url: String,
    /// Mattermost bot access token.
    pub bot_token: String,
    /// Optional channel ID to restrict the bot to a single channel.
    pub channel_id: Option<String>,
    /// Allowed Mattermost user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true (default), replies thread on the original post.
    /// When false, replies go to the channel root.
    #[serde(default)]
    pub thread_replies: Option<bool>,
    /// When true, only respond to messages that @-mention the bot.
    /// Other messages in the channel are silently ignored.
    #[serde(default)]
    pub mention_only: Option<bool>,
}

/// Webhook channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebhookConfig {
    /// Port to listen on for incoming webhooks.
    pub port: u16,
    /// Optional shared secret for webhook signature verification.
    pub secret: Option<String>,
}

/// iMessage channel configuration (macOS only).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IMessageConfig {
    /// Allowed iMessage contacts (phone numbers or email addresses). Empty = deny all.
    pub allowed_contacts: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

/// Matrix channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MatrixConfig {
    /// Matrix homeserver URL (e.g. `"https://matrix.org"`).
    pub homeserver: String,
    /// Matrix access token for the bot account.
    pub access_token: String,
    /// Optional Matrix user ID (e.g. `"@bot:matrix.org"`).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Optional Matrix device ID.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Matrix room ID to listen in (e.g. `"!abc123:matrix.org"`).
    pub room_id: String,
    /// Allowed Matrix user IDs. Empty = deny all.
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignalStormProtectionConfig {
    /// Deduplication TTL in seconds for `(channel + sender + event_type + normalized_content)`.
    #[serde(default = "default_signal_storm_dedupe_ttl_secs")]
    pub dedupe_ttl_secs: u64,
    /// Minimum interval in seconds between replies for the same `(channel + reply_target)`.
    #[serde(default = "default_signal_storm_min_reply_interval_secs")]
    pub min_reply_interval_secs: u64,
    /// Non-user event threshold within `abnormal_window_secs` before tripping the breaker.
    #[serde(default = "default_signal_storm_abnormal_threshold")]
    pub abnormal_threshold: usize,
    /// Sliding window in seconds for non-user event counting.
    #[serde(default = "default_signal_storm_abnormal_window_secs")]
    pub abnormal_window_secs: u64,
    /// Circuit breaker duration in seconds after threshold is reached.
    #[serde(default = "default_signal_storm_breaker_duration_secs")]
    pub breaker_duration_secs: u64,
}

const fn default_signal_storm_dedupe_ttl_secs() -> u64 {
    60
}

const fn default_signal_storm_min_reply_interval_secs() -> u64 {
    2
}

const fn default_signal_storm_abnormal_threshold() -> usize {
    10
}

const fn default_signal_storm_abnormal_window_secs() -> u64 {
    60
}

const fn default_signal_storm_breaker_duration_secs() -> u64 {
    300
}

impl Default for SignalStormProtectionConfig {
    fn default() -> Self {
        Self {
            dedupe_ttl_secs: default_signal_storm_dedupe_ttl_secs(),
            min_reply_interval_secs: default_signal_storm_min_reply_interval_secs(),
            abnormal_threshold: default_signal_storm_abnormal_threshold(),
            abnormal_window_secs: default_signal_storm_abnormal_window_secs(),
            breaker_duration_secs: default_signal_storm_breaker_duration_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignalConfig {
    /// Base URL for the signal-cli HTTP daemon (e.g. "http://127.0.0.1:16866").
    /// In "native" mode this is ignored; the daemon is spawned on `daemon_http_port`.
    #[serde(default = "default_signal_http_url")]
    pub http_url: String,
    /// E.164 phone number of the signal-cli account (e.g. "+1234567890").
    pub account: String,
    /// Channel mode: "native" (spawn signal-cli daemon locally) or "rest" (external daemon/Docker).
    /// Default: "rest" for backward compatibility.
    #[serde(default)]
    pub mode: Option<String>,
    /// Path to signal-cli binary. Only used in native mode. Default: "signal-cli" (PATH lookup).
    #[serde(default)]
    pub cli_path: Option<String>,
    /// signal-cli data directory. Only used in native mode.
    /// Default: $HOME/.local/share/signal-cli (signal-cli's standard XDG location).
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Local HTTP port for the spawned signal-cli daemon. Only used in native mode. Default: 16866.
    #[serde(default)]
    pub daemon_http_port: Option<u16>,
    /// Deprecated: ignored. Accepted for backward compatibility with older config files.
    #[serde(default)]
    pub daemon_mode: Option<bool>,
    /// Optional group ID to filter messages.
    /// - `None` or omitted: accept all messages (DMs and groups)
    /// - `"dm"`: only accept direct messages
    /// - Specific group ID: only accept messages from that group
    #[serde(default)]
    pub group_id: Option<String>,
    /// Allowed sender phone numbers (E.164) or "*" for all.
    #[serde(default)]
    pub allowed_from: Vec<String>,
    /// Skip messages that are attachment-only (no text body).
    #[serde(default)]
    pub ignore_attachments: bool,
    /// Skip incoming story messages.
    #[serde(default)]
    pub ignore_stories: bool,
    /// Signal ingress storm protection controls.
    #[serde(default)]
    pub storm_protection: SignalStormProtectionConfig,
    /// Startup readiness timeout in milliseconds for native `signal-cli` daemon mode.
    #[serde(default = "default_signal_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    /// Direct-message policy.
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Group-message policy.
    #[serde(default)]
    pub group_policy: GroupPolicy,
    /// Allowlisted group IDs used when `group_policy = "allowlist"`.
    #[serde(default)]
    pub group_allow_from: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

fn default_signal_http_url() -> String {
    "http://localhost:8080".to_string()
}

const fn default_signal_startup_timeout_ms() -> u64 {
    30_000
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            http_url: default_signal_http_url(),
            account: String::new(),
            mode: None,
            cli_path: None,
            data_dir: None,
            daemon_http_port: None,
            daemon_mode: None,
            group_id: None,
            allowed_from: Vec::new(),
            ignore_attachments: false,
            ignore_stories: false,
            storm_protection: SignalStormProtectionConfig::default(),
            startup_timeout_ms: default_signal_startup_timeout_ms(),
            dm_policy: DmPolicy::default(),
            group_policy: GroupPolicy::default(),
            group_allow_from: Vec::new(),
            mention_only: false,
        }
    }
}

impl SignalConfig {
    /// Returns the effective HTTP URL for the signal-cli daemon.
    /// In native mode, uses the local daemon port (ignoring `http_url`).
    /// In rest mode, returns `http_url` as-is.
    pub fn effective_http_url(&self) -> String {
        if self.mode.as_deref() == Some("native") {
            let port = self.daemon_http_port.unwrap_or(16866);
            format!("http://127.0.0.1:{port}")
        } else {
            self.http_url.clone()
        }
    }

    /// Returns true if native mode is configured.
    pub fn is_native_mode(&self) -> bool {
        self.mode.as_deref() == Some("native")
    }
}

/// WhatsApp channel configuration (Cloud API or Web mode).
///
/// Set `phone_number_id` for Cloud API mode, or `session_path` for Web mode.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WhatsAppConfig {
    /// Access token from Meta Business Suite (Cloud API mode)
    #[serde(default)]
    pub access_token: Option<String>,
    /// Phone number ID from Meta Business API (Cloud API mode)
    #[serde(default)]
    pub phone_number_id: Option<String>,
    /// Webhook verify token (you define this, Meta sends it back for verification)
    /// Only used in Cloud API mode
    #[serde(default)]
    pub verify_token: Option<String>,
    /// App secret from Meta Business Suite (for webhook signature verification)
    /// Can also be set via `OPENPRX_WHATSAPP_APP_SECRET` (legacy: `OPENPRX_WHATSAPP_APP_SECRET`) environment variable
    /// Only used in Cloud API mode
    #[serde(default)]
    pub app_secret: Option<String>,
    /// Session database path for WhatsApp Web client (Web mode)
    /// When set, enables native WhatsApp Web mode with wa-rs
    #[serde(default)]
    pub session_path: Option<String>,
    /// Phone number for pair code linking (Web mode, optional)
    /// Format: country code + number (e.g., "15551234567")
    /// If not set, QR code pairing will be used
    #[serde(default)]
    pub pair_phone: Option<String>,
    /// Custom pair code for linking (Web mode, optional)
    /// Leave empty to let WhatsApp generate one
    #[serde(default)]
    pub pair_code: Option<String>,
    /// Allowed phone numbers (E.164 format: +1234567890) or "*" for all
    #[serde(default, alias = "allowed_from")]
    pub allowed_numbers: Vec<String>,
    /// Direct-message policy.
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Group-message policy.
    #[serde(default)]
    pub group_policy: GroupPolicy,
    /// Allowlisted group IDs used when `group_policy = "allowlist"`.
    #[serde(default)]
    pub group_allow_from: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinqConfig {
    /// Linq Partner API token (Bearer auth)
    pub api_token: String,
    /// Phone number to send from (E.164 format)
    pub from_phone: String,
    /// Webhook signing secret for signature verification
    #[serde(default)]
    pub signing_secret: Option<String>,
    /// Allowed sender handles (phone numbers) or "*" for all
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

/// Nextcloud Talk bot configuration (webhook receive + OCS send API).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NextcloudTalkConfig {
    /// Nextcloud base URL (e.g. "https://cloud.example.com").
    pub base_url: String,
    /// Bot app token used for OCS API bearer auth.
    pub app_token: String,
    /// Shared secret for webhook signature verification.
    ///
    /// Can also be set via `OPENPRX_NEXTCLOUD_TALK_WEBHOOK_SECRET` (legacy: `OPENPRX_NEXTCLOUD_TALK_WEBHOOK_SECRET`).
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// Allowed Nextcloud actor IDs (`[]` = deny all, `"*"` = allow all).
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

impl WhatsAppConfig {
    /// Detect which backend to use based on config fields.
    /// Returns "cloud" if phone_number_id is set, "web" if session_path is set.
    pub const fn backend_type(&self) -> &'static str {
        if self.phone_number_id.is_some() {
            "cloud"
        } else if self.session_path.is_some() {
            "web"
        } else {
            // Default to Cloud API for backward compatibility
            "cloud"
        }
    }

    /// Check if this is a valid Cloud API config
    pub const fn is_cloud_config(&self) -> bool {
        self.phone_number_id.is_some() && self.access_token.is_some() && self.verify_token.is_some()
    }

    /// Check if this is a valid Web config
    pub const fn is_web_config(&self) -> bool {
        self.session_path.is_some()
    }

    /// Returns true when both Cloud and Web selectors are present.
    ///
    /// Runtime currently prefers Cloud mode in this case for backward compatibility.
    pub const fn is_ambiguous_config(&self) -> bool {
        self.phone_number_id.is_some() && self.session_path.is_some()
    }
}

/// wacli JSON-RPC daemon channel configuration.
///
/// Connect OpenPRX to WhatsApp via the `wacli` daemon (JSON-RPC over TCP).
/// The daemon must be running before OpenPRX starts.
///
/// Example TOML:
/// ```toml
/// [channels_config.wacli]
/// enabled = true
/// host = "127.0.0.1"
/// port = 16867
/// allowed_from = ["*"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WacliConfig {
    /// Enable the wacli channel.
    #[serde(default)]
    pub enabled: bool,
    /// Host where the wacli daemon listens (default: "127.0.0.1").
    #[serde(default = "default_wacli_host")]
    pub host: String,
    /// Port for the wacli daemon's JSON-RPC TCP listener (default: 16867).
    #[serde(default = "default_wacli_port")]
    pub port: u16,
    /// Sender JID allowlist. Use `["*"]` to accept all senders.
    /// JIDs look like `1234567890@s.whatsapp.net` for individuals or
    /// `1234567890-1234567890@g.us` for groups.
    #[serde(default = "default_wacli_allowed_from")]
    pub allowed_from: Vec<String>,
    /// Path to the `wacli` binary (for future auto-start support).
    #[serde(default)]
    pub cli_path: Option<String>,
    /// Path to the wacli store directory (for future auto-start support).
    #[serde(default)]
    pub store_dir: Option<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

fn default_wacli_host() -> String {
    "127.0.0.1".to_string()
}

const fn default_wacli_port() -> u16 {
    16867
}

fn default_wacli_allowed_from() -> Vec<String> {
    vec!["*".to_string()]
}

impl Default for WacliConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_wacli_host(),
            port: default_wacli_port(),
            allowed_from: default_wacli_allowed_from(),
            cli_path: None,
            store_dir: None,
            mention_only: false,
        }
    }
}

/// IRC channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IrcConfig {
    /// IRC server hostname
    pub server: String,
    /// IRC server port (default: 6697 for TLS)
    #[serde(default = "default_irc_port")]
    pub port: u16,
    /// Bot nickname
    pub nickname: String,
    /// Username (defaults to nickname if not set)
    pub username: Option<String>,
    /// Channels to join on connect
    #[serde(default)]
    pub channels: Vec<String>,
    /// Allowed nicknames (case-insensitive) or "*" for all
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process channel/group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
    /// Server password (for bouncers like ZNC)
    pub server_password: Option<String>,
    /// NickServ IDENTIFY password
    pub nickserv_password: Option<String>,
    /// SASL PLAIN password (IRCv3)
    pub sasl_password: Option<String>,
    /// Verify TLS certificate (default: true)
    pub verify_tls: Option<bool>,
}

const fn default_irc_port() -> u16 {
    6697
}

/// How OpenPRX receives events from Feishu / Lark.
///
/// - `websocket` (default) — persistent WSS long-connection; no public URL required.
/// - `webhook`             — HTTP callback server; requires a public HTTPS endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LarkReceiveMode {
    #[default]
    Websocket,
    Webhook,
}

/// Lark/Feishu configuration for messaging integration.
/// Lark is the international version; Feishu is the Chinese version.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LarkConfig {
    /// App ID from Lark/Feishu developer console
    pub app_id: String,
    /// App Secret from Lark/Feishu developer console
    pub app_secret: String,
    /// Encrypt key for webhook message decryption (optional)
    #[serde(default)]
    pub encrypt_key: Option<String>,
    /// Verification token for webhook validation (optional)
    #[serde(default)]
    pub verification_token: Option<String>,
    /// Allowed user IDs or union IDs (empty = deny all, "*" = allow all)
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Whether to use the Feishu (Chinese) endpoint instead of Lark (International)
    #[serde(default)]
    pub use_feishu: bool,
    /// Event receive mode: "websocket" (default) or "webhook"
    #[serde(default)]
    pub receive_mode: LarkReceiveMode,
    /// HTTP port for webhook mode only. Must be set when receive_mode = "webhook".
    /// Not required (and ignored) for websocket mode.
    #[serde(default)]
    pub port: Option<u16>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

// ── Security Config ─────────────────────────────────────────────────

/// Tool policy configuration (`[security.tool_policy]` section).
///
/// Defines multi-layer allow/deny policies for tool execution.
///
/// Example:
/// ```toml
/// [security.tool_policy]
/// default = "allow"
///
/// [security.tool_policy.groups]
/// hardware = "deny"
/// sessions = "allow"
///
/// [security.tool_policy.tools]
/// shell = "supervised"
/// gateway = "allow"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolPolicyConfig {
    /// Default policy when no more-specific rule matches.
    /// "allow" (default), "deny", or "supervised".
    #[serde(default = "default_tool_policy_default")]
    pub default: String,

    /// Group-level policies: maps group name → policy string.
    /// Known groups: sessions, automation, ui, hardware.
    #[serde(default)]
    pub groups: std::collections::HashMap<String, String>,

    /// Per-tool policies: maps tool name → policy string.
    #[serde(default)]
    pub tools: std::collections::HashMap<String, String>,
}

fn default_tool_policy_default() -> String {
    "allow".into()
}

impl ToolPolicyConfig {
    /// Returns `true` when the global default permits execution.
    pub fn default_allow(&self) -> bool {
        !matches!(self.default.trim().to_ascii_lowercase().as_str(), "deny")
    }
}

impl Default for ToolPolicyConfig {
    fn default() -> Self {
        Self {
            default: default_tool_policy_default(),
            groups: std::collections::HashMap::new(),
            tools: std::collections::HashMap::new(),
        }
    }
}

/// Security configuration for sandboxing, resource limits, and audit logging
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct SecurityConfig {
    /// Sandbox configuration
    #[serde(default)]
    pub sandbox: SandboxConfig,

    /// Resource limits
    #[serde(default)]
    pub resources: ResourceLimitsConfig,

    /// Audit logging configuration
    #[serde(default)]
    pub audit: AuditConfig,

    /// Tool policy pipeline configuration (`[security.tool_policy]`).
    #[serde(default)]
    pub tool_policy: ToolPolicyConfig,
}

/// Sandbox configuration for OS-level isolation
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SandboxConfig {
    /// Enable sandboxing (None = auto-detect, Some = explicit)
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Sandbox backend to use
    #[serde(default)]
    pub backend: SandboxBackend,

    /// Custom Firejail arguments (when backend = firejail)
    #[serde(default)]
    pub firejail_args: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: None, // Auto-detect
            backend: SandboxBackend::Auto,
            firejail_args: Vec::new(),
        }
    }
}

/// Sandbox backend selection
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxBackend {
    /// Auto-detect best available (default)
    #[default]
    Auto,
    /// Landlock (Linux kernel LSM, native)
    Landlock,
    /// Firejail (user-space sandbox)
    Firejail,
    /// Bubblewrap (user namespaces)
    Bubblewrap,
    /// Docker container isolation
    Docker,
    /// No sandboxing (application-layer only)
    None,
}

/// Resource limits for command execution
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResourceLimitsConfig {
    /// Maximum memory in MB per command
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u32,

    /// Maximum CPU time in seconds per command
    #[serde(default = "default_max_cpu_time_seconds")]
    pub max_cpu_time_seconds: u64,

    /// Maximum number of subprocesses
    #[serde(default = "default_max_subprocesses")]
    pub max_subprocesses: u32,

    /// Enable memory monitoring
    #[serde(default = "default_memory_monitoring_enabled")]
    pub memory_monitoring: bool,
}

const fn default_max_memory_mb() -> u32 {
    512
}

const fn default_max_cpu_time_seconds() -> u64 {
    60
}

const fn default_max_subprocesses() -> u32 {
    10
}

const fn default_memory_monitoring_enabled() -> bool {
    true
}

impl Default for ResourceLimitsConfig {
    fn default() -> Self {
        Self {
            max_memory_mb: default_max_memory_mb(),
            max_cpu_time_seconds: default_max_cpu_time_seconds(),
            max_subprocesses: default_max_subprocesses(),
            memory_monitoring: default_memory_monitoring_enabled(),
        }
    }
}

/// Audit logging configuration
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AuditConfig {
    /// Enable audit logging
    #[serde(default = "default_audit_enabled")]
    pub enabled: bool,

    /// Path to audit log file (relative to openprx dir)
    #[serde(default = "default_audit_log_path")]
    pub log_path: String,

    /// Maximum log size in MB before rotation
    #[serde(default = "default_audit_max_size_mb")]
    pub max_size_mb: u32,

    /// Sign events with HMAC for tamper evidence
    #[serde(default)]
    pub sign_events: bool,
}

const fn default_audit_enabled() -> bool {
    true
}

fn default_audit_log_path() -> String {
    "audit.log".to_string()
}

const fn default_audit_max_size_mb() -> u32 {
    100
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_audit_enabled(),
            log_path: default_audit_log_path(),
            max_size_mb: default_audit_max_size_mb(),
            sign_events: false,
        }
    }
}

/// DingTalk configuration for Stream Mode messaging
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DingTalkConfig {
    /// Client ID (AppKey) from DingTalk developer console
    pub client_id: String,
    /// Client Secret (AppSecret) from DingTalk developer console
    pub client_secret: String,
    /// Allowed user IDs (staff IDs). Empty = deny all, "*" = allow all
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

/// QQ Official Bot configuration (Tencent QQ Bot SDK)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QQConfig {
    /// App ID from QQ Bot developer console
    pub app_id: String,
    /// App Secret from QQ Bot developer console
    pub app_secret: String,
    /// Allowed user IDs. Empty = deny all, "*" = allow all
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, only process group messages that mention the bot.
    #[serde(default)]
    pub mention_only: bool,
}

// ── Config impl ──────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        let home = UserDirs::new().map_or_else(|| PathBuf::from("."), |u| u.home_dir().to_path_buf());
        let openprx_dir = preferred_user_config_dir(&home);

        Self {
            workspace_dir: openprx_dir.join("workspace"),
            config_path: openprx_dir.join("config.toml"),
            api_key: None,
            api_url: None,
            default_provider: Some("openrouter".to_string()),
            default_model: Some("anthropic/claude-sonnet-4.6".to_string()),
            default_temperature: 0.7,
            observability: ObservabilityConfig::default(),
            autonomy: AutonomyConfig::default(),
            runtime: RuntimeConfig::default(),
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            agent: AgentConfig::default(),
            sessions_spawn: SessionsSpawnConfig::default(),
            self_system: SelfSystemConfig::default(),
            skills: SkillsConfig::default(),
            skill_rag: SkillRagConfig::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            task_routing: TaskRoutingConfig::default(),
            router: RouterConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            xin: crate::xin::XinConfig::default(),
            cron: CronConfig::default(),
            channels_config: ChannelsConfig::default(),
            memory: MemoryConfig::default(),
            identity_bindings: Vec::new(),
            user_policies: Vec::new(),
            storage: StorageConfig::default(),
            tunnel: TunnelConfig::default(),
            gateway: GatewayConfig::default(),
            webhook: MemoryWebhookConfig::default(),
            composio: ComposioConfig::default(),
            mcp: McpConfig::default(),
            auth: AuthConfig::default(),
            secrets: SecretsConfig::default(),
            browser: BrowserConfig::default(),
            http_request: HttpRequestConfig::default(),
            multimodal: MultimodalConfig::default(),
            web_search: WebSearchConfig::default(),
            proxy: ProxyConfig::default(),
            identity: IdentityConfig::default(),
            cost: CostConfig::default(),
            nodes: NodesConfig::default(),
            agents: HashMap::new(),
            query_classification: QueryClassificationConfig::default(),
            media: MediaConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

fn default_config_and_workspace_dirs() -> Result<(PathBuf, PathBuf)> {
    let config_dir = default_config_dir()?;
    Ok((config_dir.clone(), config_dir.join("workspace")))
}

const ACTIVE_WORKSPACE_STATE_FILE: &str = "active_workspace.toml";
const PRIMARY_CONFIG_DIR_NAME: &str = ".openprx";
const LEGACY_CONFIG_DIR_NAME: &str = ".openprx";

#[derive(Debug, Serialize, Deserialize)]
struct ActiveWorkspaceState {
    config_dir: String,
}

fn default_config_dir() -> Result<PathBuf> {
    let home = UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .context("Could not find home directory")?;
    Ok(preferred_user_config_dir(&home))
}

fn preferred_user_config_dir(home: &Path) -> PathBuf {
    let primary = home.join(PRIMARY_CONFIG_DIR_NAME);
    if primary.exists() {
        return primary;
    }

    let legacy = home.join(LEGACY_CONFIG_DIR_NAME);
    if legacy.exists() {
        return legacy;
    }

    primary
}

fn active_workspace_state_path(default_dir: &Path) -> PathBuf {
    default_dir.join(ACTIVE_WORKSPACE_STATE_FILE)
}

async fn load_persisted_workspace_dirs(default_config_dir: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
    let state_path = active_workspace_state_path(default_config_dir);
    if !state_path.exists() {
        return Ok(None);
    }

    let contents = match fs::read_to_string(&state_path).await {
        Ok(contents) => contents,
        Err(error) => {
            tracing::warn!(
                "Failed to read active workspace marker {}: {error}",
                state_path.display()
            );
            return Ok(None);
        }
    };

    let state: ActiveWorkspaceState = match toml::from_str(&contents) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                "Failed to parse active workspace marker {}: {error}",
                state_path.display()
            );
            return Ok(None);
        }
    };

    let raw_config_dir = state.config_dir.trim();
    if raw_config_dir.is_empty() {
        tracing::warn!(
            "Ignoring active workspace marker {} because config_dir is empty",
            state_path.display()
        );
        return Ok(None);
    }

    let parsed_dir = PathBuf::from(raw_config_dir);
    let config_dir = if parsed_dir.is_absolute() {
        parsed_dir
    } else {
        default_config_dir.join(parsed_dir)
    };
    Ok(Some((config_dir.clone(), config_dir.join("workspace"))))
}

pub(crate) async fn persist_active_workspace_config_dir(config_dir: &Path) -> Result<()> {
    let default_config_dir = default_config_dir()?;
    let state_path = active_workspace_state_path(&default_config_dir);

    if config_dir == default_config_dir {
        if state_path.exists() {
            fs::remove_file(&state_path)
                .await
                .with_context(|| format!("Failed to clear active workspace marker: {}", state_path.display()))?;
        }
        return Ok(());
    }

    fs::create_dir_all(&default_config_dir).await.with_context(|| {
        format!(
            "Failed to create default config directory: {}",
            default_config_dir.display()
        )
    })?;

    let state = ActiveWorkspaceState {
        config_dir: config_dir.to_string_lossy().into_owned(),
    };
    let serialized = toml::to_string_pretty(&state).context("Failed to serialize active workspace marker")?;

    let temp_path = default_config_dir.join(format!(".{ACTIVE_WORKSPACE_STATE_FILE}.tmp-{}", uuid::Uuid::new_v4()));
    fs::write(&temp_path, serialized).await.with_context(|| {
        format!(
            "Failed to write temporary active workspace marker: {}",
            temp_path.display()
        )
    })?;

    if let Err(error) = fs::rename(&temp_path, &state_path).await {
        let _ = fs::remove_file(&temp_path).await;
        anyhow::bail!(
            "Failed to atomically persist active workspace marker {}: {error}",
            state_path.display()
        );
    }

    sync_directory(&default_config_dir).await?;
    Ok(())
}

fn resolve_config_dir_for_workspace(workspace_dir: &Path) -> (PathBuf, PathBuf) {
    let workspace_config_dir = workspace_dir.to_path_buf();
    if workspace_config_dir.join("config.toml").exists() {
        return (workspace_config_dir.clone(), workspace_config_dir.join("workspace"));
    }

    let legacy_candidates = workspace_dir.parent().map(|parent| {
        [
            parent.join(PRIMARY_CONFIG_DIR_NAME),
            parent.join(LEGACY_CONFIG_DIR_NAME),
        ]
    });
    if let Some(candidates) = legacy_candidates {
        for legacy_dir in &candidates {
            if legacy_dir.join("config.toml").exists() {
                return (legacy_dir.clone(), workspace_config_dir);
            }
        }

        if workspace_dir
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new("workspace"))
        {
            return (candidates[0].clone(), workspace_config_dir);
        }
    }

    (workspace_config_dir.clone(), workspace_config_dir.join("workspace"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigResolutionSource {
    EnvConfigDir,
    EnvWorkspace,
    ActiveWorkspaceMarker,
    DefaultConfigDir,
}

impl ConfigResolutionSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EnvConfigDir => "OPENPRX_CONFIG_DIR",
            Self::EnvWorkspace => "OPENPRX_WORKSPACE",
            Self::ActiveWorkspaceMarker => "active_workspace.toml",
            Self::DefaultConfigDir => "default",
        }
    }
}

fn env_openprx(name_suffix: &str) -> std::result::Result<String, std::env::VarError> {
    std::env::var(format!("OPENPRX_{name_suffix}"))
}

async fn resolve_runtime_config_dirs(
    default_openprx_dir: &Path,
    default_workspace_dir: &Path,
) -> Result<(PathBuf, PathBuf, ConfigResolutionSource)> {
    if let Ok(custom_config_dir) = env_openprx("CONFIG_DIR") {
        let custom_config_dir = custom_config_dir.trim();
        if !custom_config_dir.is_empty() {
            let openprx_dir = PathBuf::from(custom_config_dir);
            return Ok((
                openprx_dir.clone(),
                openprx_dir.join("workspace"),
                ConfigResolutionSource::EnvConfigDir,
            ));
        }
    }

    if let Ok(custom_workspace) = env_openprx("WORKSPACE") {
        if !custom_workspace.is_empty() {
            let (openprx_dir, workspace_dir) = resolve_config_dir_for_workspace(&PathBuf::from(custom_workspace));
            return Ok((openprx_dir, workspace_dir, ConfigResolutionSource::EnvWorkspace));
        }
    }

    if let Some((openprx_dir, workspace_dir)) = load_persisted_workspace_dirs(default_openprx_dir).await? {
        return Ok((
            openprx_dir,
            workspace_dir,
            ConfigResolutionSource::ActiveWorkspaceMarker,
        ));
    }

    Ok((
        default_openprx_dir.to_path_buf(),
        default_workspace_dir.to_path_buf(),
        ConfigResolutionSource::DefaultConfigDir,
    ))
}

fn decrypt_optional_secret(
    store: &crate::security::SecretStore,
    value: &mut Option<String>,
    field_name: &str,
) -> Result<()> {
    if let Some(raw) = value.clone() {
        if crate::security::SecretStore::is_encrypted(&raw) {
            *value = Some(
                store
                    .decrypt(&raw)
                    .with_context(|| format!("Failed to decrypt {field_name}"))?,
            );
        }
    }
    Ok(())
}

fn encrypt_optional_secret(
    store: &crate::security::SecretStore,
    value: &mut Option<String>,
    field_name: &str,
) -> Result<()> {
    if let Some(raw) = value.clone() {
        if !crate::security::SecretStore::is_encrypted(&raw) {
            *value = Some(
                store
                    .encrypt(&raw)
                    .with_context(|| format!("Failed to encrypt {field_name}"))?,
            );
        }
    }
    Ok(())
}

fn config_dir_creation_error(path: &Path) -> String {
    format!(
        "Failed to create config directory: {}. If running as an OpenRC service, \
         ensure this path is writable by user 'prx'.",
        path.display()
    )
}

fn decrypt_config_secrets(config: &mut Config, openprx_dir: &Path) -> Result<()> {
    let store = crate::security::SecretStore::new(openprx_dir, config.secrets.encrypt);
    decrypt_optional_secret(&store, &mut config.api_key, "config.api_key")?;
    decrypt_optional_secret(&store, &mut config.composio.api_key, "config.composio.api_key")?;

    decrypt_optional_secret(
        &store,
        &mut config.browser.computer_use.api_key,
        "config.browser.computer_use.api_key",
    )?;

    decrypt_optional_secret(
        &store,
        &mut config.web_search.brave_api_key,
        "config.web_search.brave_api_key",
    )?;

    decrypt_optional_secret(
        &store,
        &mut config.storage.provider.config.db_url,
        "config.storage.provider.config.db_url",
    )?;

    for agent in config.agents.values_mut() {
        decrypt_optional_secret(&store, &mut agent.api_key, "config.agents.*.api_key")?;
    }

    Ok(())
}

impl Config {
    pub(crate) fn load_from_path(config_path: &Path, workspace_dir: PathBuf) -> Result<Self> {
        let merged = read_merged_toml(config_path)?;
        let mut config: Self = merged.try_into().context("Failed to deserialize merged config")?;
        config.config_path = config_path.to_path_buf();
        config.workspace_dir = workspace_dir;

        let openprx_dir = config_path
            .parent()
            .context("Config path must have a parent directory")?;
        decrypt_config_secrets(&mut config, openprx_dir)?;
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    pub async fn load_or_init() -> Result<Self> {
        let (default_openprx_dir, default_workspace_dir) = default_config_and_workspace_dirs()?;

        let (openprx_dir, workspace_dir, resolution_source) =
            resolve_runtime_config_dirs(&default_openprx_dir, &default_workspace_dir).await?;

        let config_path = openprx_dir.join("config.toml");

        fs::create_dir_all(&openprx_dir)
            .await
            .with_context(|| config_dir_creation_error(&openprx_dir))?;
        fs::create_dir_all(&workspace_dir)
            .await
            .context("Failed to create workspace directory")?;

        if config_path.exists() {
            // Warn if config file is world-readable (may contain API keys)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&config_path).await {
                    if meta.permissions().mode() & 0o004 != 0 {
                        tracing::warn!(
                            "Config file {:?} is world-readable (mode {:o}). \
                             Consider restricting with: chmod 600 {:?}",
                            config_path,
                            meta.permissions().mode() & 0o777,
                            config_path,
                        );
                    }
                }
            }

            let config = Self::load_from_path(&config_path, workspace_dir)?;
            tracing::info!(
                path = %config.config_path.display(),
                workspace = %config.workspace_dir.display(),
                source = resolution_source.as_str(),
                initialized = false,
                "Config loaded"
            );
            Ok(config)
        } else {
            let mut config = Self::default();
            config.config_path = config_path.clone();
            config.workspace_dir = workspace_dir;
            config.save().await?;

            // Restrict permissions on newly created config file (may contain API keys)
            #[cfg(unix)]
            {
                use std::{fs::Permissions, os::unix::fs::PermissionsExt};
                let _ = fs::set_permissions(&config_path, Permissions::from_mode(0o600)).await;
            }

            config.apply_env_overrides();
            config.validate()?;
            tracing::info!(
                path = %config.config_path.display(),
                workspace = %config.workspace_dir.display(),
                source = resolution_source.as_str(),
                initialized = true,
                "Config loaded"
            );
            Ok(config)
        }
    }

    /// Validate configuration values that would cause runtime failures.
    ///
    /// Called after TOML deserialization and env-override application to catch
    /// obviously invalid values early instead of failing at arbitrary runtime points.
    pub fn validate(&self) -> Result<()> {
        // Gateway
        if self.gateway.host.trim().is_empty() {
            anyhow::bail!("gateway.host must not be empty");
        }
        if self.webhook.enabled {
            if self.webhook.bind.trim().is_empty() {
                anyhow::bail!("webhook.bind must not be empty when webhook.enabled is true");
            }
            self.webhook
                .bind
                .parse::<std::net::SocketAddr>()
                .with_context(|| format!("invalid webhook.bind socket address: {}", self.webhook.bind))?;
            if self
                .webhook
                .token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                anyhow::bail!("webhook.token must be set when webhook.enabled is true");
            }
        }

        // Autonomy
        if self.autonomy.max_actions_per_hour == 0 {
            anyhow::bail!("autonomy.max_actions_per_hour must be greater than 0");
        }

        // Scheduler
        if self.scheduler.max_concurrent == 0 {
            anyhow::bail!("scheduler.max_concurrent must be greater than 0");
        }
        if self.scheduler.max_tasks == 0 {
            anyhow::bail!("scheduler.max_tasks must be greater than 0");
        }
        if !matches!(
            self.agent.concurrency_rollout_stage.as_str(),
            "off" | "stage_a" | "stage_b" | "stage_c" | "full"
        ) {
            anyhow::bail!("agent.concurrency_rollout_stage must be one of: off|stage_a|stage_b|stage_c|full");
        }
        if !(0.0..=1.0).contains(&self.agent.concurrency_rollback_timeout_rate_threshold) {
            anyhow::bail!("agent.concurrency_rollback_timeout_rate_threshold must be in [0,1]");
        }
        if !(0.0..=1.0).contains(&self.agent.concurrency_rollback_cancel_rate_threshold) {
            anyhow::bail!("agent.concurrency_rollback_cancel_rate_threshold must be in [0,1]");
        }
        if !(0.0..=1.0).contains(&self.agent.concurrency_rollback_error_rate_threshold) {
            anyhow::bail!("agent.concurrency_rollback_error_rate_threshold must be in [0,1]");
        }

        // Model routes
        for (i, route) in self.model_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].hint must not be empty");
            }
            if route.provider.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].provider must not be empty");
            }
            if route.model.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].model must not be empty");
            }
        }

        if self.router.enabled {
            self.router.validate()?;
        }

        // Embedding routes
        for (i, route) in self.embedding_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].hint must not be empty");
            }
            if route.provider.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].provider must not be empty");
            }
            if route.model.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].model must not be empty");
            }
        }

        // Proxy (delegate to existing validation)
        self.proxy.validate()?;

        Ok(())
    }

    /// Apply environment variable overrides to config
    pub fn apply_env_overrides(&mut self) {
        // API Key: OPENPRX_API_KEY / OPENPRX_API_KEY or API_KEY (generic)
        if let Ok(key) = env_openprx("API_KEY").or_else(|_| std::env::var("API_KEY")) {
            if !key.is_empty() {
                self.api_key = Some(key);
            }
        }
        // API Key: GLM_API_KEY overrides when provider is a GLM/Zhipu variant.
        if self.default_provider.as_deref().is_some_and(is_glm_alias) {
            if let Ok(key) = std::env::var("GLM_API_KEY") {
                if !key.is_empty() {
                    self.api_key = Some(key);
                }
            }
        }

        // API Key: ZAI_API_KEY overrides when provider is a Z.AI variant.
        if self.default_provider.as_deref().is_some_and(is_zai_alias) {
            if let Ok(key) = std::env::var("ZAI_API_KEY") {
                if !key.is_empty() {
                    self.api_key = Some(key);
                }
            }
        }

        // Provider override precedence:
        // 1) OPENPRX_PROVIDER / OPENPRX_PROVIDER always wins when set.
        // 2) Legacy PROVIDER is only honored when config still uses the
        //    default provider (openrouter) or provider is unset. This prevents
        //    container defaults from overriding explicit custom providers.
        if let Ok(provider) = env_openprx("PROVIDER") {
            if !provider.is_empty() {
                self.default_provider = Some(provider);
            }
        } else if let Ok(provider) = std::env::var("PROVIDER") {
            let should_apply_legacy_provider = self
                .default_provider
                .as_deref()
                .map_or(true, |configured| configured.trim().eq_ignore_ascii_case("openrouter"));
            if should_apply_legacy_provider && !provider.is_empty() {
                self.default_provider = Some(provider);
            }
        }

        // Model: OPENPRX_MODEL / OPENPRX_MODEL or MODEL
        if let Ok(model) = env_openprx("MODEL").or_else(|_| std::env::var("MODEL")) {
            if !model.is_empty() {
                self.default_model = Some(model);
            }
        }

        // Workspace directory: OPENPRX_WORKSPACE
        if let Ok(workspace) = env_openprx("WORKSPACE") {
            if !workspace.is_empty() {
                let (_, workspace_dir) = resolve_config_dir_for_workspace(&PathBuf::from(workspace));
                self.workspace_dir = workspace_dir;
            }
        }

        // Open-skills opt-in flag: OPENPRX_OPEN_SKILLS_ENABLED
        if let Ok(flag) = env_openprx("OPEN_SKILLS_ENABLED") {
            if !flag.trim().is_empty() {
                match flag.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "on" => self.skills.open_skills_enabled = true,
                    "0" | "false" | "no" | "off" => self.skills.open_skills_enabled = false,
                    _ => tracing::warn!(
                        "Ignoring invalid OPENPRX_OPEN_SKILLS_ENABLED (valid: 1|0|true|false|yes|no|on|off)"
                    ),
                }
            }
        }

        // Open-skills directory override: OPENPRX_OPEN_SKILLS_DIR
        if let Ok(path) = env_openprx("OPEN_SKILLS_DIR") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                self.skills.open_skills_dir = Some(trimmed.to_string());
            }
        }

        // OpenClaw skills opt-in flag: OPENPRX_OPENCLAW_SKILLS_ENABLED
        if let Ok(flag) = env_openprx("OPENCLAW_SKILLS_ENABLED") {
            if !flag.trim().is_empty() {
                match flag.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "on" => self.skills.openclaw_skills_enabled = true,
                    "0" | "false" | "no" | "off" => self.skills.openclaw_skills_enabled = false,
                    _ => tracing::warn!(
                        "Ignoring invalid OPENPRX_OPENCLAW_SKILLS_ENABLED (valid: 1|0|true|false|yes|no|on|off)"
                    ),
                }
            }
        }

        // OpenClaw skills directory override: OPENPRX_OPENCLAW_SKILLS_DIR
        if let Ok(path) = env_openprx("OPENCLAW_SKILLS_DIR") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                self.skills.openclaw_skills_dir = Some(trimmed.to_string());
            }
        }

        // Gateway port: OPENPRX_GATEWAY_PORT or PORT
        if let Ok(port_str) = env_openprx("GATEWAY_PORT").or_else(|_| std::env::var("PORT")) {
            if let Ok(port) = port_str.parse::<u16>() {
                self.gateway.port = port;
            }
        }

        // Gateway host: OPENPRX_GATEWAY_HOST or HOST
        if let Ok(host) = env_openprx("GATEWAY_HOST").or_else(|_| std::env::var("HOST")) {
            if !host.is_empty() {
                self.gateway.host = host;
            }
        }

        // Allow public bind: OPENPRX_ALLOW_PUBLIC_BIND
        if let Ok(val) = env_openprx("ALLOW_PUBLIC_BIND") {
            self.gateway.allow_public_bind = val == "1" || val.eq_ignore_ascii_case("true");
        }

        // Temperature: OPENPRX_TEMPERATURE
        if let Ok(temp_str) = env_openprx("TEMPERATURE") {
            if let Ok(temp) = temp_str.parse::<f64>() {
                if (0.0..=2.0).contains(&temp) {
                    self.default_temperature = temp;
                }
            }
        }

        // Agent read-only tool scheduler tuning.
        if let Ok(window) = env_openprx("READ_ONLY_TOOL_CONCURRENCY_WINDOW") {
            if let Ok(window) = window.parse::<usize>() {
                if window > 0 {
                    self.agent.read_only_tool_concurrency_window = window;
                }
            }
        }
        if let Ok(timeout_secs) = env_openprx("READ_ONLY_TOOL_TIMEOUT_SECS") {
            if let Ok(timeout_secs) = timeout_secs.parse::<u64>() {
                if timeout_secs > 0 {
                    self.agent.read_only_tool_timeout_secs = timeout_secs;
                }
            }
        }
        if let Ok(enabled) = env_openprx("PRIORITY_SCHEDULING_ENABLED") {
            self.agent.priority_scheduling_enabled = enabled == "1" || enabled.eq_ignore_ascii_case("true");
        }
        if let Ok(enabled) = env_openprx("CONCURRENCY_KILL_SWITCH_FORCE_SERIAL") {
            self.agent.concurrency_kill_switch_force_serial = enabled == "1" || enabled.eq_ignore_ascii_case("true");
        }
        if let Ok(stage) = env_openprx("CONCURRENCY_ROLLOUT_STAGE") {
            let stage = stage.trim().to_ascii_lowercase();
            if matches!(stage.as_str(), "off" | "stage_a" | "stage_b" | "stage_c" | "full") {
                self.agent.concurrency_rollout_stage = stage;
            }
        }
        if let Ok(percent) = env_openprx("CONCURRENCY_ROLLOUT_SAMPLE_PERCENT") {
            if let Ok(percent) = percent.parse::<u8>() {
                self.agent.concurrency_rollout_sample_percent = percent;
            }
        }
        if let Ok(channels) = env_openprx("CONCURRENCY_ROLLOUT_CHANNELS") {
            let parsed = channels
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            self.agent.concurrency_rollout_channels = parsed;
        }
        if let Ok(enabled) = env_openprx("CONCURRENCY_AUTO_ROLLBACK_ENABLED") {
            self.agent.concurrency_auto_rollback_enabled = enabled == "1" || enabled.eq_ignore_ascii_case("true");
        }
        if let Ok(threshold) = env_openprx("CONCURRENCY_ROLLBACK_TIMEOUT_RATE_THRESHOLD") {
            if let Ok(threshold) = threshold.parse::<f64>() {
                if (0.0..=1.0).contains(&threshold) {
                    self.agent.concurrency_rollback_timeout_rate_threshold = threshold;
                }
            }
        }
        if let Ok(threshold) = env_openprx("CONCURRENCY_ROLLBACK_CANCEL_RATE_THRESHOLD") {
            if let Ok(threshold) = threshold.parse::<f64>() {
                if (0.0..=1.0).contains(&threshold) {
                    self.agent.concurrency_rollback_cancel_rate_threshold = threshold;
                }
            }
        }
        if let Ok(threshold) = env_openprx("CONCURRENCY_ROLLBACK_ERROR_RATE_THRESHOLD") {
            if let Ok(threshold) = threshold.parse::<f64>() {
                if (0.0..=1.0).contains(&threshold) {
                    self.agent.concurrency_rollback_error_rate_threshold = threshold;
                }
            }
        }

        // Reasoning override: OPENPRX_REASONING_ENABLED or REASONING_ENABLED
        if let Ok(flag) = env_openprx("REASONING_ENABLED").or_else(|_| std::env::var("REASONING_ENABLED")) {
            let normalized = flag.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "1" | "true" | "yes" | "on" => self.runtime.reasoning_enabled = Some(true),
                "0" | "false" | "no" | "off" => self.runtime.reasoning_enabled = Some(false),
                _ => {}
            }
        }

        // Web search enabled: OPENPRX_WEB_SEARCH_ENABLED or WEB_SEARCH_ENABLED
        if let Ok(enabled) = env_openprx("WEB_SEARCH_ENABLED").or_else(|_| std::env::var("WEB_SEARCH_ENABLED")) {
            self.web_search.enabled = enabled == "1" || enabled.eq_ignore_ascii_case("true");
        }

        // Web search provider: OPENPRX_WEB_SEARCH_PROVIDER or WEB_SEARCH_PROVIDER
        if let Ok(provider) = env_openprx("WEB_SEARCH_PROVIDER").or_else(|_| std::env::var("WEB_SEARCH_PROVIDER")) {
            let provider = provider.trim();
            if !provider.is_empty() {
                self.web_search.provider = provider.to_string();
            }
        }

        // Brave API key: OPENPRX_BRAVE_API_KEY or BRAVE_API_KEY
        if let Ok(api_key) = env_openprx("BRAVE_API_KEY").or_else(|_| std::env::var("BRAVE_API_KEY")) {
            let api_key = api_key.trim();
            if !api_key.is_empty() {
                self.web_search.brave_api_key = Some(api_key.to_string());
            }
        }

        // Web search max results: OPENPRX_WEB_SEARCH_MAX_RESULTS or WEB_SEARCH_MAX_RESULTS
        if let Ok(max_results) =
            env_openprx("WEB_SEARCH_MAX_RESULTS").or_else(|_| std::env::var("WEB_SEARCH_MAX_RESULTS"))
        {
            if let Ok(max_results) = max_results.parse::<usize>() {
                if (1..=10).contains(&max_results) {
                    self.web_search.max_results = max_results;
                }
            }
        }

        // Web search timeout: OPENPRX_WEB_SEARCH_TIMEOUT_SECS or WEB_SEARCH_TIMEOUT_SECS
        if let Ok(timeout_secs) =
            env_openprx("WEB_SEARCH_TIMEOUT_SECS").or_else(|_| std::env::var("WEB_SEARCH_TIMEOUT_SECS"))
        {
            if let Ok(timeout_secs) = timeout_secs.parse::<u64>() {
                if timeout_secs > 0 {
                    self.web_search.timeout_secs = timeout_secs;
                }
            }
        }

        // Storage provider key (optional backend override): OPENPRX_STORAGE_PROVIDER
        if let Ok(provider) = env_openprx("STORAGE_PROVIDER") {
            let provider = provider.trim();
            if !provider.is_empty() {
                self.storage.provider.config.provider = provider.to_string();
            }
        }

        // Storage connection URL (for remote backends): OPENPRX_STORAGE_DB_URL
        if let Ok(db_url) = env_openprx("STORAGE_DB_URL") {
            let db_url = db_url.trim();
            if !db_url.is_empty() {
                self.storage.provider.config.db_url = Some(db_url.to_string());
            }
        }

        // Storage connect timeout: OPENPRX_STORAGE_CONNECT_TIMEOUT_SECS
        if let Ok(timeout_secs) = env_openprx("STORAGE_CONNECT_TIMEOUT_SECS") {
            if let Ok(timeout_secs) = timeout_secs.parse::<u64>() {
                if timeout_secs > 0 {
                    self.storage.provider.config.connect_timeout_secs = Some(timeout_secs);
                }
            }
        }
        // Proxy enabled flag: OPENPRX_PROXY_ENABLED
        let explicit_proxy_enabled = env_openprx("PROXY_ENABLED")
            .ok()
            .as_deref()
            .and_then(parse_proxy_enabled);
        if let Some(enabled) = explicit_proxy_enabled {
            self.proxy.enabled = enabled;
        }

        // Proxy URLs: OPENPRX_* wins, then generic *PROXY vars.
        let http_overridden = if let Ok(proxy_url) = env_openprx("HTTP_PROXY").or_else(|_| std::env::var("HTTP_PROXY"))
        {
            self.proxy.http_proxy = normalize_proxy_url_option(Some(&proxy_url));
            true
        } else {
            false
        };
        let https_overridden =
            if let Ok(proxy_url) = env_openprx("HTTPS_PROXY").or_else(|_| std::env::var("HTTPS_PROXY")) {
                self.proxy.https_proxy = normalize_proxy_url_option(Some(&proxy_url));
                true
            } else {
                false
            };
        let all_overridden = if let Ok(proxy_url) = env_openprx("ALL_PROXY").or_else(|_| std::env::var("ALL_PROXY")) {
            self.proxy.all_proxy = normalize_proxy_url_option(Some(&proxy_url));
            true
        } else {
            false
        };
        let proxy_url_overridden = http_overridden || https_overridden || all_overridden;
        if let Ok(no_proxy) = env_openprx("NO_PROXY").or_else(|_| std::env::var("NO_PROXY")) {
            self.proxy.no_proxy = normalize_no_proxy_list(vec![no_proxy]);
        }

        if explicit_proxy_enabled.is_none() && proxy_url_overridden && self.proxy.has_any_proxy_url() {
            self.proxy.enabled = true;
        }

        // Proxy scope and service selectors.
        if let Ok(scope_raw) = env_openprx("PROXY_SCOPE") {
            if let Some(scope) = parse_proxy_scope(&scope_raw) {
                self.proxy.scope = scope;
            } else {
                tracing::warn!(
                    scope = %scope_raw,
                    "Ignoring invalid OPENPRX_PROXY_SCOPE (valid: environment|prx|services)"
                );
            }
        }

        if let Ok(services_raw) = env_openprx("PROXY_SERVICES") {
            self.proxy.services = normalize_service_list(vec![services_raw]);
        }

        if let Err(error) = self.proxy.validate() {
            tracing::warn!("Invalid proxy configuration ignored: {error}");
            self.proxy.enabled = false;
        }

        if self.proxy.enabled && self.proxy.scope == ProxyScope::Environment {
            self.proxy.apply_to_process_env();
        }

        set_runtime_proxy_config(self.proxy.clone());
    }

    pub async fn save(&self) -> Result<()> {
        let toml_str = self.to_stored_toml_string()?;
        write_toml_string_atomic(&self.config_path, &toml_str).await
    }

    pub(crate) fn to_stored_toml_value(&self) -> Result<toml::Value> {
        let mut config_to_save = self.clone();
        let openprx_dir = self
            .config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let store = crate::security::SecretStore::new(openprx_dir, self.secrets.encrypt);

        encrypt_optional_secret(&store, &mut config_to_save.api_key, "config.api_key")?;
        encrypt_optional_secret(&store, &mut config_to_save.composio.api_key, "config.composio.api_key")?;

        encrypt_optional_secret(
            &store,
            &mut config_to_save.browser.computer_use.api_key,
            "config.browser.computer_use.api_key",
        )?;

        encrypt_optional_secret(
            &store,
            &mut config_to_save.web_search.brave_api_key,
            "config.web_search.brave_api_key",
        )?;

        encrypt_optional_secret(
            &store,
            &mut config_to_save.storage.provider.config.db_url,
            "config.storage.provider.config.db_url",
        )?;

        for agent in config_to_save.agents.values_mut() {
            encrypt_optional_secret(&store, &mut agent.api_key, "config.agents.*.api_key")?;
        }

        toml::Value::try_from(&config_to_save).context("Failed to convert config into TOML value")
    }

    pub(crate) fn to_stored_toml_string(&self) -> Result<String> {
        let value = self.to_stored_toml_value()?;
        toml::to_string_pretty(&value).context("Failed to serialize config")
    }

    pub fn to_split_toml_strings(&self) -> Result<(String, Vec<(String, String)>)> {
        let value = self.to_stored_toml_value()?;
        let (main_value, fragment_values) = build_split_tables(&value)?;
        let main = toml::to_string_pretty(&main_value).context("Failed to serialize main config")?;
        let mut fragments = Vec::with_capacity(fragment_values.len());
        for (name, value) in fragment_values {
            let rendered = toml::to_string_pretty(&value)
                .with_context(|| format!("Failed to serialize split fragment: {name}"))?;
            fragments.push((name, rendered));
        }
        Ok((main, fragments))
    }
}

pub(crate) async fn write_toml_string_atomic(path: &Path, toml_str: &str) -> Result<()> {
    let parent_dir = path.parent().context("Config path must have a parent directory")?;

    if let Ok(metadata) = fs::symlink_metadata(path).await {
        if metadata.file_type().is_symlink() {
            anyhow::bail!("Refusing to replace config via symlink path: {}", path.display());
        }
    }

    fs::create_dir_all(parent_dir)
        .await
        .with_context(|| format!("Failed to create config directory: {}", parent_dir.display()))?;

    let file_name = path.file_name().and_then(|v| v.to_str()).unwrap_or("config.toml");
    let temp_path = parent_dir.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));
    let backup_path = parent_dir.join(format!("{file_name}.bak"));

    let mut open_options = OpenOptions::new();
    open_options.create_new(true).write(true);
    #[cfg(unix)]
    open_options.mode(0o600);

    let mut temp_file = open_options
        .open(&temp_path)
        .await
        .with_context(|| format!("Failed to create temporary config file: {}", temp_path.display()))?;
    temp_file
        .write_all(toml_str.as_bytes())
        .await
        .context("Failed to write temporary config contents")?;
    temp_file
        .sync_all()
        .await
        .context("Failed to fsync temporary config file")?;
    drop(temp_file);

    let had_existing_config = path.exists();
    if had_existing_config {
        fs::copy(path, &backup_path).await.with_context(|| {
            format!(
                "Failed to create config backup before atomic replace: {}",
                backup_path.display()
            )
        })?;
    }

    if let Err(e) = fs::rename(&temp_path, path).await {
        let _ = fs::remove_file(&temp_path).await;
        if had_existing_config && backup_path.exists() {
            fs::copy(&backup_path, path)
                .await
                .context("Failed to restore config backup")?;
        }
        anyhow::bail!("Failed to atomically replace config file: {e}");
    }

    sync_directory(parent_dir).await?;

    #[cfg(unix)]
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .await
        .with_context(|| format!("Failed to restrict permissions on {}", path.display()))?;

    if had_existing_config {
        let _ = fs::remove_file(&backup_path).await;
    }

    Ok(())
}

async fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = File::open(path)
            .await
            .with_context(|| format!("Failed to open directory for fsync: {}", path.display()))?;
        dir.sync_all()
            .await
            .with_context(|| format!("Failed to fsync directory metadata: {}", path.display()))?;
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tokio::sync::{Mutex, MutexGuard};
    use tokio::test;
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::ReadDirStream;

    /// Helper to set env vars in tests (unsafe in edition 2024).
    #[allow(unsafe_code)]
    fn test_set_env(key: impl AsRef<std::ffi::OsStr>, value: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: tests run single-threaded via serial test mutex or unique env keys.
        unsafe { std::env::set_var(key, value) }
    }

    /// Helper to remove env vars in tests (unsafe in edition 2024).
    #[allow(unsafe_code)]
    fn test_remove_env(key: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: tests run single-threaded via serial test mutex or unique env keys.
        unsafe { std::env::remove_var(key) }
    }

    // ── Defaults ─────────────────────────────────────────────

    #[test]
    async fn config_default_has_sane_values() {
        let c = Config::default();
        assert_eq!(c.default_provider.as_deref(), Some("openrouter"));
        assert!(c.default_model.as_deref().unwrap().contains("claude"));
        assert!((c.default_temperature - 0.7).abs() < f64::EPSILON);
        assert!(c.api_key.is_none());
        assert!(!c.skills.open_skills_enabled);
        assert!(c.workspace_dir.to_string_lossy().contains("workspace"));
        assert!(c.config_path.to_string_lossy().contains("config.toml"));
    }

    #[test]
    async fn config_dir_creation_error_mentions_openrc_and_path() {
        let msg = config_dir_creation_error(Path::new("/etc/prx"));
        assert!(msg.contains("/etc/prx"));
        assert!(msg.contains("OpenRC"));
        assert!(msg.contains("prx"));
    }

    #[test]
    async fn config_schema_export_contains_expected_contract_shape() {
        let schema = schemars::schema_for!(Config);
        let schema_json = serde_json::to_value(&schema).expect("schema should serialize to json");

        assert_eq!(
            schema_json.get("$schema").and_then(serde_json::Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema")
        );

        let properties = schema_json
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("schema should expose top-level properties");

        assert!(properties.contains_key("default_provider"));
        assert!(properties.contains_key("skills"));
        assert!(properties.contains_key("gateway"));
        assert!(properties.contains_key("channels_config"));
        assert!(!properties.contains_key("workspace_dir"));
        assert!(!properties.contains_key("config_path"));

        assert!(
            schema_json
                .get("$defs")
                .and_then(serde_json::Value::as_object)
                .is_some(),
            "schema should include reusable type definitions"
        );
    }

    #[test]
    async fn observability_config_default() {
        let o = ObservabilityConfig::default();
        assert_eq!(o.backend, "none");
    }

    #[test]
    async fn autonomy_config_default() {
        let a = AutonomyConfig::default();
        assert_eq!(a.level, AutonomyLevel::Supervised);
        assert!(a.workspace_only);
        assert!(a.allowed_commands.contains(&"git".to_string()));
        assert!(a.allowed_commands.contains(&"cargo".to_string()));
        assert!(a.forbidden_paths.contains(&"/etc".to_string()));
        assert_eq!(a.max_actions_per_hour, 20);
        assert_eq!(a.max_cost_per_day_cents, 500);
        assert!(a.require_approval_for_medium_risk);
        assert!(a.block_high_risk_commands);
    }

    #[test]
    async fn runtime_config_default() {
        let r = RuntimeConfig::default();
        assert_eq!(r.kind, "native");
        assert_eq!(r.docker.image, "alpine:3.20");
        assert_eq!(r.docker.network, "none");
        assert_eq!(r.docker.memory_limit_mb, Some(512));
        assert_eq!(r.docker.cpu_limit, Some(1.0));
        assert!(r.docker.read_only_rootfs);
        assert!(r.docker.mount_workspace);
    }

    #[test]
    async fn heartbeat_config_default() {
        let h = HeartbeatConfig::default();
        assert!(!h.enabled);
        assert_eq!(h.interval_minutes, 30);
    }

    #[test]
    async fn cron_config_default() {
        let c = CronConfig::default();
        assert!(c.enabled);
        assert_eq!(c.max_run_history, 50);
    }

    #[test]
    async fn cron_config_serde_roundtrip() {
        let c = CronConfig {
            enabled: false,
            max_run_history: 100,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: CronConfig = serde_json::from_str(&json).unwrap();
        assert!(!parsed.enabled);
        assert_eq!(parsed.max_run_history, 100);
    }

    #[test]
    async fn config_defaults_cron_when_section_missing() {
        let toml_str = r#"
workspace_dir = "/tmp/workspace"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;

        let parsed: Config = toml::from_str(toml_str).unwrap();
        assert!(parsed.cron.enabled);
        assert_eq!(parsed.cron.max_run_history, 50);
    }

    #[test]
    async fn memory_config_default_hygiene_settings() {
        let m = MemoryConfig::default();
        assert_eq!(m.backend, "sqlite");
        assert!(m.auto_save);
        assert!(!m.acl_enabled);
        assert!(m.hygiene_enabled);
        assert_eq!(m.archive_after_days, 7);
        assert_eq!(m.purge_after_days, 30);
        assert_eq!(m.conversation_retention_days, 3);
        assert_eq!(m.daily_retention_days, 7);
        assert!(m.sqlite_open_timeout_secs.is_none());
    }

    #[test]
    async fn nodes_config_defaults() {
        let nodes = NodesConfig::default();
        assert!(!nodes.enabled);
        assert_eq!(nodes.request_timeout_ms, 15_000);
        assert_eq!(nodes.retry_max, 2);
        assert_eq!(nodes.server.listen_addr, "127.0.0.1:8787");
        assert_eq!(nodes.server.max_concurrent_tasks, 8);
        assert_eq!(nodes.server.task_result_ttl_ms, 3_600_000);
    }

    #[test]
    async fn storage_provider_config_defaults() {
        let storage = StorageConfig::default();
        assert!(storage.provider.config.provider.is_empty());
        assert!(storage.provider.config.db_url.is_none());
        assert_eq!(storage.provider.config.schema, "public");
        assert_eq!(storage.provider.config.table, "memories");
        assert!(storage.provider.config.connect_timeout_secs.is_none());
    }

    #[test]
    async fn channels_config_default() {
        let c = ChannelsConfig::default();
        assert!(c.cli);
        assert!(c.telegram.is_none());
        assert!(c.discord.is_none());
    }

    // ── Serde round-trip ─────────────────────────────────────

    #[test]
    async fn config_toml_roundtrip() {
        let config = Config {
            workspace_dir: PathBuf::from("/tmp/test/workspace"),
            config_path: PathBuf::from("/tmp/test/config.toml"),
            api_key: Some("sk-test-key".into()),
            api_url: None,
            default_provider: Some("openrouter".into()),
            default_model: Some("gpt-4o".into()),
            default_temperature: 0.5,
            observability: ObservabilityConfig {
                backend: "log".into(),
                ..ObservabilityConfig::default()
            },
            autonomy: AutonomyConfig {
                level: AutonomyLevel::Full,
                workspace_only: false,
                allowed_commands: vec!["docker".into()],
                forbidden_paths: vec!["/secret".into()],
                max_actions_per_hour: 50,
                max_cost_per_day_cents: 1000,
                require_approval_for_medium_risk: false,
                block_high_risk_commands: true,
                auto_approve: vec!["file_read".into()],
                always_ask: vec![],
                scopes: ScopeConfig::default(),
            },
            runtime: RuntimeConfig {
                kind: "docker".into(),
                ..RuntimeConfig::default()
            },
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            sessions_spawn: SessionsSpawnConfig::default(),
            self_system: SelfSystemConfig::default(),
            skills: SkillsConfig::default(),
            skill_rag: SkillRagConfig::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            query_classification: QueryClassificationConfig::default(),
            task_routing: TaskRoutingConfig::default(),
            router: RouterConfig::default(),
            heartbeat: HeartbeatConfig {
                enabled: true,
                interval_minutes: 15,
                ..HeartbeatConfig::default()
            },
            cron: CronConfig::default(),
            xin: crate::xin::XinConfig::default(),
            channels_config: ChannelsConfig {
                cli: true,
                telegram: Some(TelegramConfig {
                    bot_token: "123:ABC".into(),
                    allowed_users: vec!["user1".into()],
                    stream_mode: StreamMode::default(),
                    draft_update_interval_ms: default_draft_update_interval_ms(),
                    interrupt_on_new_message: false,
                    mention_only: false,
                }),
                discord: None,
                slack: None,
                mattermost: None,
                webhook: None,
                imessage: None,
                matrix: None,
                signal: None,
                whatsapp: None,
                wacli: None,
                linq: None,
                nextcloud_talk: None,
                email: None,
                irc: None,
                lark: None,
                dingtalk: None,
                qq: None,
                message_timeout_secs: 300,
            },
            memory: MemoryConfig::default(),
            identity_bindings: Vec::new(),
            user_policies: Vec::new(),
            storage: StorageConfig::default(),
            tunnel: TunnelConfig::default(),
            gateway: GatewayConfig::default(),
            webhook: MemoryWebhookConfig::default(),
            composio: ComposioConfig::default(),
            mcp: McpConfig::default(),
            auth: AuthConfig::default(),
            secrets: SecretsConfig::default(),
            browser: BrowserConfig::default(),
            http_request: HttpRequestConfig::default(),
            multimodal: MultimodalConfig::default(),
            web_search: WebSearchConfig::default(),
            proxy: ProxyConfig::default(),
            agent: AgentConfig::default(),
            identity: IdentityConfig::default(),
            cost: CostConfig::default(),
            nodes: NodesConfig::default(),
            agents: HashMap::new(),
            media: MediaConfig::default(),
            security: SecurityConfig::default(),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.api_key, config.api_key);
        assert_eq!(parsed.default_provider, config.default_provider);
        assert_eq!(parsed.default_model, config.default_model);
        assert!((parsed.default_temperature - config.default_temperature).abs() < f64::EPSILON);
        assert_eq!(parsed.observability.backend, "log");
        assert_eq!(parsed.autonomy.level, AutonomyLevel::Full);
        assert!(!parsed.autonomy.workspace_only);
        assert_eq!(parsed.runtime.kind, "docker");
        assert!(parsed.heartbeat.enabled);
        assert_eq!(parsed.heartbeat.interval_minutes, 15);
        assert!(parsed.channels_config.telegram.is_some());
        assert_eq!(parsed.channels_config.telegram.unwrap().bot_token, "123:ABC");
    }

    #[test]
    async fn config_minimal_toml_uses_defaults() {
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(parsed.api_key.is_none());
        assert!(parsed.default_provider.is_none());
        assert_eq!(parsed.observability.backend, "none");
        assert_eq!(parsed.autonomy.level, AutonomyLevel::Supervised);
        assert_eq!(parsed.runtime.kind, "native");
        assert!(!parsed.heartbeat.enabled);
        assert!(parsed.channels_config.cli);
        assert!(parsed.memory.hygiene_enabled);
        assert!(!parsed.memory.acl_enabled);
        assert_eq!(parsed.memory.archive_after_days, 7);
        assert_eq!(parsed.memory.purge_after_days, 30);
        assert_eq!(parsed.memory.conversation_retention_days, 3);
        assert_eq!(parsed.memory.daily_retention_days, 7);
        assert_eq!(parsed.self_system.evolution_interval_hours, 24);
    }

    #[test]
    async fn self_system_evolution_interval_hours_deserializes() {
        let raw = r#"
default_temperature = 0.7

[self_system]
evolution_enabled = true
evolution_interval_hours = 12
"#;
        let parsed: Config = toml::from_str(raw).unwrap();
        assert!(parsed.self_system.evolution_enabled);
        assert_eq!(parsed.self_system.evolution_interval_hours, 12);
    }

    #[test]
    async fn config_parses_identity_bindings_and_user_policies() {
        let raw = r#"
default_temperature = 0.7

[[identity_bindings]]
user_id = "ak"
channel = "signal"
channel_account = "d26c8bda-58c5-4eb4-9997-0b011129fd58"

[[user_policies]]
user_id = "ak"
role = "owner"
projects = []
visibility_ceiling = "public"
blocked_patterns = []
"#;

        let parsed: Config = toml::from_str(raw).unwrap();
        assert_eq!(parsed.identity_bindings.len(), 1);
        assert_eq!(parsed.identity_bindings[0].user_id, "ak");
        assert_eq!(parsed.identity_bindings[0].channel, "signal");
        assert_eq!(parsed.user_policies.len(), 1);
        assert_eq!(parsed.user_policies[0].role, "owner");
        assert_eq!(parsed.user_policies[0].visibility_ceiling, "public");
    }

    #[test]
    async fn storage_provider_dburl_alias_deserializes() {
        let raw = r#"
default_temperature = 0.7

[storage.provider.config]
provider = "postgres"
dbURL = "postgres://postgres:postgres@localhost:5432/openprx"
schema = "public"
table = "memories"
connect_timeout_secs = 12
"#;

        let parsed: Config = toml::from_str(raw).unwrap();
        assert_eq!(parsed.storage.provider.config.provider, "postgres");
        assert_eq!(
            parsed.storage.provider.config.db_url.as_deref(),
            Some("postgres://postgres:postgres@localhost:5432/openprx")
        );
        assert_eq!(parsed.storage.provider.config.schema, "public");
        assert_eq!(parsed.storage.provider.config.table, "memories");
        assert_eq!(parsed.storage.provider.config.connect_timeout_secs, Some(12));
    }

    #[test]
    async fn runtime_reasoning_enabled_deserializes() {
        let raw = r#"
default_temperature = 0.7

[runtime]
reasoning_enabled = false
"#;

        let parsed: Config = toml::from_str(raw).unwrap();
        assert_eq!(parsed.runtime.reasoning_enabled, Some(false));
    }

    #[test]
    async fn agent_config_defaults() {
        let cfg = AgentConfig::default();
        assert!(!cfg.compact_context);
        assert_eq!(cfg.max_tool_iterations, 50);
        assert_eq!(cfg.max_history_messages, 50);
        assert!(!cfg.parallel_tools);
        assert_eq!(cfg.tool_dispatcher, "auto");
        assert_eq!(cfg.read_only_tool_concurrency_window, 2);
        assert_eq!(cfg.read_only_tool_timeout_secs, 30);
        assert!(!cfg.priority_scheduling_enabled);
        assert_eq!(cfg.low_priority_tools, default_agent_low_priority_tools());
        assert!(!cfg.concurrency_kill_switch_force_serial);
        assert_eq!(cfg.concurrency_rollout_stage, "off");
        assert_eq!(cfg.concurrency_rollout_sample_percent, 0);
        assert!(cfg.concurrency_rollout_channels.is_empty());
        assert!(cfg.concurrency_auto_rollback_enabled);
        assert!((cfg.concurrency_rollback_timeout_rate_threshold - 0.2).abs() < f64::EPSILON);
        assert!((cfg.concurrency_rollback_cancel_rate_threshold - 0.2).abs() < f64::EPSILON);
        assert!((cfg.concurrency_rollback_error_rate_threshold - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    async fn agent_config_deserializes() {
        let raw = r#"
default_temperature = 0.7
[agent]
compact_context = true
max_tool_iterations = 20
max_history_messages = 80
parallel_tools = true
tool_dispatcher = "xml"
read_only_tool_concurrency_window = 4
read_only_tool_timeout_secs = 45
priority_scheduling_enabled = true
low_priority_tools = ["sessions_spawn", "delegate"]
concurrency_kill_switch_force_serial = false
concurrency_rollout_stage = "stage_b"
concurrency_rollout_sample_percent = 25
concurrency_rollout_channels = ["telegram", "discord"]
concurrency_auto_rollback_enabled = true
concurrency_rollback_timeout_rate_threshold = 0.21
concurrency_rollback_cancel_rate_threshold = 0.22
concurrency_rollback_error_rate_threshold = 0.23
"#;
        let parsed: Config = toml::from_str(raw).unwrap();
        assert!(parsed.agent.compact_context);
        assert_eq!(parsed.agent.max_tool_iterations, 20);
        assert_eq!(parsed.agent.max_history_messages, 80);
        assert!(parsed.agent.parallel_tools);
        assert_eq!(parsed.agent.tool_dispatcher, "xml");
        assert_eq!(parsed.agent.read_only_tool_concurrency_window, 4);
        assert_eq!(parsed.agent.read_only_tool_timeout_secs, 45);
        assert!(parsed.agent.priority_scheduling_enabled);
        assert_eq!(parsed.agent.low_priority_tools, vec!["sessions_spawn", "delegate"]);
        assert!(!parsed.agent.concurrency_kill_switch_force_serial);
        assert_eq!(parsed.agent.concurrency_rollout_stage, "stage_b");
        assert_eq!(parsed.agent.concurrency_rollout_sample_percent, 25);
        assert_eq!(parsed.agent.concurrency_rollout_channels, vec!["telegram", "discord"]);
        assert!(parsed.agent.concurrency_auto_rollback_enabled);
        assert!((parsed.agent.concurrency_rollback_timeout_rate_threshold - 0.21).abs() < f64::EPSILON);
        assert!((parsed.agent.concurrency_rollback_cancel_rate_threshold - 0.22).abs() < f64::EPSILON);
        assert!((parsed.agent.concurrency_rollback_error_rate_threshold - 0.23).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn sync_directory_handles_existing_directory() {
        let dir = std::env::temp_dir().join(format!("openprx_test_sync_directory_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();

        sync_directory(&dir).await.unwrap();

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_save_and_load_tmpdir() {
        let dir = std::env::temp_dir().join("openprx_test_config");
        let _ = fs::remove_dir_all(&dir).await;
        fs::create_dir_all(&dir).await.unwrap();

        let config_path = dir.join("config.toml");
        let config = Config {
            workspace_dir: dir.join("workspace"),
            config_path: config_path.clone(),
            api_key: Some("sk-roundtrip".into()),
            api_url: None,
            default_provider: Some("openrouter".into()),
            default_model: Some("test-model".into()),
            default_temperature: 0.9,
            observability: ObservabilityConfig::default(),
            autonomy: AutonomyConfig::default(),
            runtime: RuntimeConfig::default(),
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            sessions_spawn: SessionsSpawnConfig::default(),
            self_system: SelfSystemConfig::default(),
            skills: SkillsConfig::default(),
            skill_rag: SkillRagConfig::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            query_classification: QueryClassificationConfig::default(),
            task_routing: TaskRoutingConfig::default(),
            router: RouterConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            xin: crate::xin::XinConfig::default(),
            cron: CronConfig::default(),
            channels_config: ChannelsConfig::default(),
            memory: MemoryConfig::default(),
            identity_bindings: Vec::new(),
            user_policies: Vec::new(),
            storage: StorageConfig::default(),
            tunnel: TunnelConfig::default(),
            gateway: GatewayConfig::default(),
            webhook: MemoryWebhookConfig::default(),
            composio: ComposioConfig::default(),
            mcp: McpConfig::default(),
            auth: AuthConfig::default(),
            secrets: SecretsConfig::default(),
            browser: BrowserConfig::default(),
            http_request: HttpRequestConfig::default(),
            multimodal: MultimodalConfig::default(),
            web_search: WebSearchConfig::default(),
            proxy: ProxyConfig::default(),
            agent: AgentConfig::default(),
            identity: IdentityConfig::default(),
            cost: CostConfig::default(),
            nodes: NodesConfig::default(),
            agents: HashMap::new(),
            media: MediaConfig::default(),
            security: SecurityConfig::default(),
        };

        config.save().await.unwrap();
        assert!(config_path.exists());

        let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
        let loaded: Config = toml::from_str(&contents).unwrap();
        assert!(
            loaded
                .api_key
                .as_deref()
                .is_some_and(crate::security::SecretStore::is_encrypted)
        );
        let store = crate::security::SecretStore::new(&dir, true);
        let decrypted = store.decrypt(loaded.api_key.as_deref().unwrap()).unwrap();
        assert_eq!(decrypted, "sk-roundtrip");
        assert_eq!(loaded.default_model.as_deref(), Some("test-model"));
        assert!((loaded.default_temperature - 0.9).abs() < f64::EPSILON);

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_load_from_path_supports_single_file() {
        let dir = std::env::temp_dir().join(format!("openprx_test_single_file_load_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();

        let config_path = dir.join("config.toml");
        fs::write(
            &config_path,
            r#"
default_temperature = 0.7
default_model = "single-file"
"#,
        )
        .await
        .unwrap();

        let loaded = Config::load_from_path(&config_path, dir.join("workspace")).unwrap();
        assert_eq!(loaded.default_model.as_deref(), Some("single-file"));
        assert_eq!(loaded.memory.backend, "sqlite");

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_load_from_path_merges_config_dir_and_replaces_arrays() {
        let dir = std::env::temp_dir().join(format!("openprx_test_config_merge_{}", uuid::Uuid::new_v4()));
        let config_dir = dir.join("config.d");
        fs::create_dir_all(&config_dir).await.unwrap();

        let config_path = dir.join("config.toml");
        fs::write(
            &config_path,
            r#"
default_temperature = 0.7
default_model = "base-model"

[[model_routes]]
hint = "alpha"
provider = "openrouter"
model = "base-alpha"

[memory]
backend = "sqlite"
auto_save = true
acl_enabled = false
hygiene_enabled = true
archive_after_days = 7
purge_after_days = 30
conversation_retention_days = 3
daily_retention_days = 7
embedding_provider = "base"
embedding_model = "text-embedding-3-small"
embedding_dimensions = 1536
vector_weight = 0.7
keyword_weight = 0.3
min_relevance_score = 0.4
embedding_cache_size = 1000
snapshot_enabled = false
snapshot_on_hygiene = false
auto_hydrate = true
"#,
        )
        .await
        .unwrap();
        fs::write(
            config_dir.join("00-memory.toml"),
            r#"
[memory]
embedding_provider = "override"
"#,
        )
        .await
        .unwrap();
        fs::write(
            config_dir.join("10-routes.toml"),
            r#"
default_model = "fragment-model"

[[model_routes]]
hint = "beta"
provider = "anthropic"
model = "override-beta"
"#,
        )
        .await
        .unwrap();

        let loaded = Config::load_from_path(&config_path, dir.join("workspace")).unwrap();
        assert_eq!(loaded.default_model.as_deref(), Some("fragment-model"));
        assert_eq!(loaded.model_routes.len(), 1);
        assert_eq!(loaded.model_routes[0].hint, "beta");
        assert_eq!(loaded.memory.embedding_provider, "override");

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_split_and_reload_roundtrip_matches_original() {
        let _env_guard = env_override_lock().await;
        let dir = std::env::temp_dir().join(format!("openprx_test_split_reload_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();

        let mut config = Config::default();
        config.workspace_dir = dir.join("workspace");
        config.config_path = dir.join("config.toml");
        config.default_model = Some("roundtrip-model".into());
        config.channels_config.telegram = Some(TelegramConfig {
            bot_token: "roundtrip-token".into(),
            allowed_users: vec!["zeroclaw_user".into()],
            stream_mode: StreamMode::default(),
            draft_update_interval_ms: default_draft_update_interval_ms(),
            interrupt_on_new_message: false,
            mention_only: false,
        });
        config.memory.backend = "markdown".into();
        config.storage.provider.config.provider = "postgres".into();
        config.storage.provider.config.db_url = Some("postgres://user:pw@host/db".into());
        // Keep test deterministic even if process env mutates proxy defaults elsewhere.
        config.proxy = ProxyConfig::default();
        config.security.resources.max_cpu_time_seconds = 120;
        config.autonomy.max_actions_per_hour = 42;
        config.agents.insert(
            "worker".into(),
            DelegateAgentConfig {
                provider: "openrouter".into(),
                model: "test-model".into(),
                system_prompt: Some("delegate".into()),
                api_key: Some("agent-secret".into()),
                temperature: Some(0.2),
                max_depth: 4,
                agentic: true,
                allowed_tools: vec!["shell".into()],
                max_iterations: 7,
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: Some(true),
            },
        );

        let expected = serde_json::to_value(&config).unwrap();
        crate::config::files::write_split_config(&config, false).await.unwrap();

        let reloaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).unwrap();
        let actual = serde_json::to_value(&reloaded).unwrap();
        assert_eq!(actual, expected);

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_split_preserves_unmanaged_fragments() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_dir = dir.path().join("config.d");
        fs::create_dir_all(&config_dir).await.unwrap();

        let mut config = Config::default();
        config.workspace_dir = dir.path().join("workspace");
        config.config_path = dir.path().join("config.toml");
        config.default_model = Some("preserve-fragments".into());
        config.memory.backend = "markdown".into();

        fs::write(config_dir.join("99-local.toml"), "default_temperature = 0.9\n")
            .await
            .unwrap();

        crate::config::files::write_split_config(&config, false).await.unwrap();

        assert!(config_dir.join("99-local.toml").exists());
    }

    #[tokio::test]
    async fn config_merge_refuses_unmanaged_fragments() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_dir = dir.path().join("config.d");
        fs::create_dir_all(&config_dir).await.unwrap();

        let mut config = Config::default();
        config.workspace_dir = dir.path().join("workspace");
        config.config_path = dir.path().join("config.toml");
        config.default_model = Some("merge-guard".into());
        config.save().await.unwrap();

        fs::write(config_dir.join("99-local.toml"), "default_temperature = 0.9\n")
            .await
            .unwrap();

        let error = crate::config::files::merge_split_config(&config).await.unwrap_err();
        assert!(
            error.to_string().contains("unmanaged config fragments"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn config_save_encrypts_nested_credentials() {
        let dir = std::env::temp_dir().join(format!("openprx_test_nested_credentials_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();

        let mut config = Config::default();
        config.workspace_dir = dir.join("workspace");
        config.config_path = dir.join("config.toml");
        config.api_key = Some("root-credential".into());
        config.composio.api_key = Some("composio-credential".into());
        config.browser.computer_use.api_key = Some("browser-credential".into());
        config.web_search.brave_api_key = Some("brave-credential".into());
        config.storage.provider.config.db_url = Some("postgres://user:pw@host/db".into());

        config.agents.insert(
            "worker".into(),
            DelegateAgentConfig {
                provider: "openrouter".into(),
                model: "model-test".into(),
                system_prompt: None,
                api_key: Some("agent-credential".into()),
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );

        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(config.config_path.clone()).await.unwrap();
        let stored: Config = toml::from_str(&contents).unwrap();
        let store = crate::security::SecretStore::new(&dir, true);

        let root_encrypted = stored.api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(root_encrypted));
        assert_eq!(store.decrypt(root_encrypted).unwrap(), "root-credential");

        let composio_encrypted = stored.composio.api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(composio_encrypted));
        assert_eq!(store.decrypt(composio_encrypted).unwrap(), "composio-credential");

        let browser_encrypted = stored.browser.computer_use.api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(browser_encrypted));
        assert_eq!(store.decrypt(browser_encrypted).unwrap(), "browser-credential");

        let web_search_encrypted = stored.web_search.brave_api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(web_search_encrypted));
        assert_eq!(store.decrypt(web_search_encrypted).unwrap(), "brave-credential");

        let worker = stored.agents.get("worker").unwrap();
        let worker_encrypted = worker.api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(worker_encrypted));
        assert_eq!(store.decrypt(worker_encrypted).unwrap(), "agent-credential");

        let storage_db_url = stored.storage.provider.config.db_url.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(storage_db_url));
        assert_eq!(store.decrypt(storage_db_url).unwrap(), "postgres://user:pw@host/db");

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_save_atomic_cleanup() {
        let dir = std::env::temp_dir().join(format!("openprx_test_config_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();

        let config_path = dir.join("config.toml");
        let mut config = Config::default();
        config.workspace_dir = dir.join("workspace");
        config.config_path = config_path.clone();
        config.default_model = Some("model-a".into());
        config.save().await.unwrap();
        assert!(config_path.exists());

        config.default_model = Some("model-b".into());
        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(contents.contains("model-b"));

        let names: Vec<String> = ReadDirStream::new(fs::read_dir(&dir).await.unwrap())
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect()
            .await;
        assert!(!names.iter().any(|name| name.contains(".tmp-")));
        assert!(!names.iter().any(|name| name.ends_with(".bak")));

        let _ = fs::remove_dir_all(&dir).await;
    }

    // ── Telegram / Discord config ────────────────────────────

    #[test]
    async fn telegram_config_serde() {
        let tc = TelegramConfig {
            bot_token: "123:XYZ".into(),
            allowed_users: vec!["alice".into(), "bob".into()],
            stream_mode: StreamMode::Partial,
            draft_update_interval_ms: 500,
            interrupt_on_new_message: true,
            mention_only: false,
        };
        let json = serde_json::to_string(&tc).unwrap();
        let parsed: TelegramConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.bot_token, "123:XYZ");
        assert_eq!(parsed.allowed_users.len(), 2);
        assert_eq!(parsed.stream_mode, StreamMode::Partial);
        assert_eq!(parsed.draft_update_interval_ms, 500);
        assert!(parsed.interrupt_on_new_message);
    }

    #[test]
    async fn telegram_config_defaults_stream_off() {
        let json = r#"{"bot_token":"tok","allowed_users":[]}"#;
        let parsed: TelegramConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.stream_mode, StreamMode::Off);
        assert_eq!(parsed.draft_update_interval_ms, 1000);
        assert!(!parsed.interrupt_on_new_message);
    }

    #[test]
    async fn discord_config_serde() {
        let dc = DiscordConfig {
            bot_token: "discord-token".into(),
            guild_id: Some("12345".into()),
            allowed_users: vec![],
            listen_to_bots: false,
            mention_only: false,
        };
        let json = serde_json::to_string(&dc).unwrap();
        let parsed: DiscordConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.bot_token, "discord-token");
        assert_eq!(parsed.guild_id.as_deref(), Some("12345"));
    }

    #[test]
    async fn discord_config_optional_guild() {
        let dc = DiscordConfig {
            bot_token: "tok".into(),
            guild_id: None,
            allowed_users: vec![],
            listen_to_bots: false,
            mention_only: false,
        };
        let json = serde_json::to_string(&dc).unwrap();
        let parsed: DiscordConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.guild_id.is_none());
    }

    // ── iMessage / Matrix config ────────────────────────────

    #[test]
    async fn imessage_config_serde() {
        let ic = IMessageConfig {
            allowed_contacts: vec!["+1234567890".into(), "user@icloud.com".into()],
            mention_only: false,
        };
        let json = serde_json::to_string(&ic).unwrap();
        let parsed: IMessageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.allowed_contacts.len(), 2);
        assert_eq!(parsed.allowed_contacts[0], "+1234567890");
    }

    #[test]
    async fn imessage_config_empty_contacts() {
        let ic = IMessageConfig {
            allowed_contacts: vec![],
            mention_only: false,
        };
        let json = serde_json::to_string(&ic).unwrap();
        let parsed: IMessageConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.allowed_contacts.is_empty());
    }

    #[test]
    async fn imessage_config_wildcard() {
        let ic = IMessageConfig {
            allowed_contacts: vec!["*".into()],
            mention_only: false,
        };
        let toml_str = toml::to_string(&ic).unwrap();
        let parsed: IMessageConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.allowed_contacts, vec!["*"]);
    }

    #[test]
    async fn matrix_config_serde() {
        let mc = MatrixConfig {
            homeserver: "https://matrix.org".into(),
            access_token: "syt_token_abc".into(),
            user_id: Some("@bot:matrix.org".into()),
            device_id: Some("DEVICE123".into()),
            room_id: "!room123:matrix.org".into(),
            allowed_users: vec!["@user:matrix.org".into()],
            mention_only: false,
        };
        let json = serde_json::to_string(&mc).unwrap();
        let parsed: MatrixConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.homeserver, "https://matrix.org");
        assert_eq!(parsed.access_token, "syt_token_abc");
        assert_eq!(parsed.user_id.as_deref(), Some("@bot:matrix.org"));
        assert_eq!(parsed.device_id.as_deref(), Some("DEVICE123"));
        assert_eq!(parsed.room_id, "!room123:matrix.org");
        assert_eq!(parsed.allowed_users.len(), 1);
    }

    #[test]
    async fn matrix_config_toml_roundtrip() {
        let mc = MatrixConfig {
            homeserver: "https://synapse.local:8448".into(),
            access_token: "tok".into(),
            user_id: None,
            device_id: None,
            room_id: "!abc:synapse.local".into(),
            allowed_users: vec!["@admin:synapse.local".into(), "*".into()],
            mention_only: false,
        };
        let toml_str = toml::to_string(&mc).unwrap();
        let parsed: MatrixConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.homeserver, "https://synapse.local:8448");
        assert_eq!(parsed.allowed_users.len(), 2);
    }

    #[test]
    async fn matrix_config_backward_compatible_without_session_hints() {
        let toml = r#"
homeserver = "https://matrix.org"
access_token = "tok"
room_id = "!ops:matrix.org"
allowed_users = ["@ops:matrix.org"]
"#;

        let parsed: MatrixConfig = toml::from_str(toml).unwrap();
        assert_eq!(parsed.homeserver, "https://matrix.org");
        assert!(parsed.user_id.is_none());
        assert!(parsed.device_id.is_none());
    }

    #[test]
    async fn signal_config_serde() {
        let sc = SignalConfig {
            http_url: "http://127.0.0.1:16866".into(),
            account: "+1234567890".into(),
            group_id: Some("group123".into()),
            allowed_from: vec!["+1111111111".into()],
            ignore_attachments: true,
            ignore_stories: false,
            storm_protection: SignalStormProtectionConfig {
                dedupe_ttl_secs: 90,
                min_reply_interval_secs: 3,
                abnormal_threshold: 12,
                abnormal_window_secs: 45,
                breaker_duration_secs: 120,
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&sc).unwrap();
        let parsed: SignalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.http_url, "http://127.0.0.1:16866");
        assert_eq!(parsed.account, "+1234567890");
        assert_eq!(parsed.group_id.as_deref(), Some("group123"));
        assert_eq!(parsed.allowed_from.len(), 1);
        assert!(parsed.ignore_attachments);
        assert!(!parsed.ignore_stories);
        assert_eq!(parsed.storm_protection.dedupe_ttl_secs, 90);
        assert_eq!(parsed.storm_protection.min_reply_interval_secs, 3);
        assert_eq!(parsed.storm_protection.abnormal_threshold, 12);
        assert_eq!(parsed.storm_protection.abnormal_window_secs, 45);
        assert_eq!(parsed.storm_protection.breaker_duration_secs, 120);
    }

    #[test]
    async fn signal_config_toml_roundtrip() {
        let sc = SignalConfig {
            http_url: "http://localhost:8080".into(),
            account: "+9876543210".into(),
            group_id: None,
            allowed_from: vec!["*".into()],
            ignore_attachments: false,
            ignore_stories: true,
            ..Default::default()
        };
        let toml_str = toml::to_string(&sc).unwrap();
        let parsed: SignalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.http_url, "http://localhost:8080");
        assert_eq!(parsed.account, "+9876543210");
        assert!(parsed.group_id.is_none());
        assert!(parsed.ignore_stories);
    }

    #[test]
    async fn signal_config_defaults() {
        let json = r#"{"http_url":"http://127.0.0.1:16866","account":"+1234567890"}"#;
        let parsed: SignalConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.group_id.is_none());
        assert!(parsed.allowed_from.is_empty());
        assert!(!parsed.ignore_attachments);
        assert!(!parsed.ignore_stories);
        assert_eq!(parsed.storm_protection.dedupe_ttl_secs, 60);
        assert_eq!(parsed.storm_protection.min_reply_interval_secs, 2);
        assert_eq!(parsed.storm_protection.abnormal_threshold, 10);
        assert_eq!(parsed.storm_protection.abnormal_window_secs, 60);
        assert_eq!(parsed.storm_protection.breaker_duration_secs, 300);
    }

    #[test]
    async fn channels_config_with_imessage_and_matrix() {
        let c = ChannelsConfig {
            cli: true,
            telegram: None,
            discord: None,
            slack: None,
            mattermost: None,
            webhook: None,
            imessage: Some(IMessageConfig {
                allowed_contacts: vec!["+1".into()],
                mention_only: false,
            }),
            matrix: Some(MatrixConfig {
                homeserver: "https://m.org".into(),
                access_token: "tok".into(),
                user_id: None,
                device_id: None,
                room_id: "!r:m".into(),
                allowed_users: vec!["@u:m".into()],
                mention_only: false,
            }),
            signal: None,
            whatsapp: None,
            wacli: None,
            linq: None,
            nextcloud_talk: None,
            email: None,
            irc: None,
            lark: None,
            dingtalk: None,
            qq: None,
            message_timeout_secs: 300,
        };
        let toml_str = toml::to_string_pretty(&c).unwrap();
        let parsed: ChannelsConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.imessage.is_some());
        assert!(parsed.matrix.is_some());
        assert_eq!(parsed.imessage.unwrap().allowed_contacts, vec!["+1"]);
        assert_eq!(parsed.matrix.unwrap().homeserver, "https://m.org");
    }

    #[test]
    async fn channels_config_default_has_no_imessage_matrix() {
        let c = ChannelsConfig::default();
        assert!(c.imessage.is_none());
        assert!(c.matrix.is_none());
    }

    // ── Edge cases: serde(default) for allowed_users ─────────

    #[test]
    async fn discord_config_deserializes_without_allowed_users() {
        // Old configs won't have allowed_users — serde(default) should fill vec![]
        let json = r#"{"bot_token":"tok","guild_id":"123"}"#;
        let parsed: DiscordConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.allowed_users.is_empty());
    }

    #[test]
    async fn discord_config_deserializes_with_allowed_users() {
        let json = r#"{"bot_token":"tok","guild_id":"123","allowed_users":["111","222"]}"#;
        let parsed: DiscordConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.allowed_users, vec!["111", "222"]);
    }

    #[test]
    async fn slack_config_deserializes_without_allowed_users() {
        let json = r#"{"bot_token":"xoxb-tok"}"#;
        let parsed: SlackConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.allowed_users.is_empty());
    }

    #[test]
    async fn slack_config_deserializes_with_allowed_users() {
        let json = r#"{"bot_token":"xoxb-tok","allowed_users":["U111"]}"#;
        let parsed: SlackConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.allowed_users, vec!["U111"]);
    }

    #[test]
    async fn discord_config_toml_backward_compat() {
        let toml_str = r#"
bot_token = "tok"
guild_id = "123"
"#;
        let parsed: DiscordConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.allowed_users.is_empty());
        assert_eq!(parsed.bot_token, "tok");
    }

    #[test]
    async fn slack_config_toml_backward_compat() {
        let toml_str = r#"
bot_token = "xoxb-tok"
channel_id = "C123"
"#;
        let parsed: SlackConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.allowed_users.is_empty());
        assert_eq!(parsed.channel_id.as_deref(), Some("C123"));
    }

    #[test]
    async fn webhook_config_with_secret() {
        let json = r#"{"port":8080,"secret":"my-secret-key"}"#;
        let parsed: WebhookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.secret.as_deref(), Some("my-secret-key"));
    }

    #[test]
    async fn webhook_config_without_secret() {
        let json = r#"{"port":8080}"#;
        let parsed: WebhookConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.secret.is_none());
        assert_eq!(parsed.port, 8080);
    }

    // ── WhatsApp config ──────────────────────────────────────

    #[test]
    async fn whatsapp_config_serde() {
        let wc = WhatsAppConfig {
            access_token: Some("EAABx...".into()),
            phone_number_id: Some("123456789".into()),
            verify_token: Some("my-verify-token".into()),
            app_secret: None,
            session_path: None,
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["+1234567890".into(), "+9876543210".into()],
            dm_policy: DmPolicy::default(),
            group_policy: GroupPolicy::default(),
            group_allow_from: vec![],
            mention_only: false,
        };
        let json = serde_json::to_string(&wc).unwrap();
        let parsed: WhatsAppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.access_token, Some("EAABx...".into()));
        assert_eq!(parsed.phone_number_id, Some("123456789".into()));
        assert_eq!(parsed.verify_token, Some("my-verify-token".into()));
        assert_eq!(parsed.allowed_numbers.len(), 2);
    }

    #[test]
    async fn whatsapp_config_toml_roundtrip() {
        let wc = WhatsAppConfig {
            access_token: Some("tok".into()),
            phone_number_id: Some("12345".into()),
            verify_token: Some("verify".into()),
            app_secret: Some("secret123".into()),
            session_path: None,
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["+1".into()],
            dm_policy: DmPolicy::default(),
            group_policy: GroupPolicy::default(),
            group_allow_from: vec![],
            mention_only: false,
        };
        let toml_str = toml::to_string(&wc).unwrap();
        let parsed: WhatsAppConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.phone_number_id, Some("12345".into()));
        assert_eq!(parsed.allowed_numbers, vec!["+1"]);
    }

    #[test]
    async fn whatsapp_config_deserializes_without_allowed_numbers() {
        let json = r#"{"access_token":"tok","phone_number_id":"123","verify_token":"ver"}"#;
        let parsed: WhatsAppConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.allowed_numbers.is_empty());
    }

    #[test]
    async fn whatsapp_config_wildcard_allowed() {
        let wc = WhatsAppConfig {
            access_token: Some("tok".into()),
            phone_number_id: Some("123".into()),
            verify_token: Some("ver".into()),
            app_secret: None,
            session_path: None,
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["*".into()],
            dm_policy: DmPolicy::default(),
            group_policy: GroupPolicy::default(),
            group_allow_from: vec![],
            mention_only: false,
        };
        let toml_str = toml::to_string(&wc).unwrap();
        let parsed: WhatsAppConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.allowed_numbers, vec!["*"]);
    }

    #[test]
    async fn whatsapp_config_backend_type_cloud_precedence_when_ambiguous() {
        let wc = WhatsAppConfig {
            access_token: Some("tok".into()),
            phone_number_id: Some("123".into()),
            verify_token: Some("ver".into()),
            app_secret: None,
            session_path: Some("~/.openprx/state/whatsapp-web/session.db".into()),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["+1".into()],
            dm_policy: DmPolicy::default(),
            group_policy: GroupPolicy::default(),
            group_allow_from: vec![],
            mention_only: false,
        };
        assert!(wc.is_ambiguous_config());
        assert_eq!(wc.backend_type(), "cloud");
    }

    #[test]
    async fn whatsapp_config_backend_type_web() {
        let wc = WhatsAppConfig {
            access_token: None,
            phone_number_id: None,
            verify_token: None,
            app_secret: None,
            session_path: Some("~/.openprx/state/whatsapp-web/session.db".into()),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec![],
            dm_policy: DmPolicy::default(),
            group_policy: GroupPolicy::default(),
            group_allow_from: vec![],
            mention_only: false,
        };
        assert!(!wc.is_ambiguous_config());
        assert_eq!(wc.backend_type(), "web");
    }

    #[test]
    async fn channels_config_with_whatsapp() {
        let c = ChannelsConfig {
            cli: true,
            telegram: None,
            discord: None,
            slack: None,
            mattermost: None,
            webhook: None,
            imessage: None,
            matrix: None,
            signal: None,
            whatsapp: Some(WhatsAppConfig {
                access_token: Some("tok".into()),
                phone_number_id: Some("123".into()),
                verify_token: Some("ver".into()),
                app_secret: None,
                session_path: None,
                pair_phone: None,
                pair_code: None,
                allowed_numbers: vec!["+1".into()],
                dm_policy: DmPolicy::default(),
                group_policy: GroupPolicy::default(),
                group_allow_from: vec![],
                mention_only: false,
            }),
            wacli: None,
            linq: None,
            nextcloud_talk: None,
            email: None,
            irc: None,
            lark: None,
            dingtalk: None,
            qq: None,
            message_timeout_secs: 300,
        };
        let toml_str = toml::to_string_pretty(&c).unwrap();
        let parsed: ChannelsConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.whatsapp.is_some());
        let wa = parsed.whatsapp.unwrap();
        assert_eq!(wa.phone_number_id, Some("123".into()));
        assert_eq!(wa.allowed_numbers, vec!["+1"]);
    }

    #[test]
    async fn channels_config_default_has_no_whatsapp() {
        let c = ChannelsConfig::default();
        assert!(c.whatsapp.is_none());
    }

    #[test]
    async fn channels_config_default_has_no_nextcloud_talk() {
        let c = ChannelsConfig::default();
        assert!(c.nextcloud_talk.is_none());
    }

    // ══════════════════════════════════════════════════════════
    // SECURITY CHECKLIST TESTS — Gateway config
    // ══════════════════════════════════════════════════════════

    #[test]
    async fn checklist_gateway_default_requires_pairing() {
        let g = GatewayConfig::default();
        assert!(g.require_pairing, "Pairing must be required by default");
    }

    #[test]
    async fn checklist_gateway_default_blocks_public_bind() {
        let g = GatewayConfig::default();
        assert!(!g.allow_public_bind, "Public bind must be blocked by default");
    }

    #[test]
    async fn checklist_gateway_default_no_tokens() {
        let g = GatewayConfig::default();
        assert!(g.paired_tokens.is_empty(), "No pre-paired tokens by default");
        assert_eq!(g.pair_rate_limit_per_minute, 10);
        assert_eq!(g.webhook_rate_limit_per_minute, 60);
        assert_eq!(g.api_rate_limit_per_minute, 60);
        assert!(!g.trust_forwarded_headers);
        assert_eq!(g.rate_limit_max_keys, 10_000);
        assert_eq!(g.idempotency_ttl_secs, 300);
        assert_eq!(g.idempotency_max_keys, 10_000);
    }

    #[test]
    async fn checklist_gateway_cli_default_host_is_localhost() {
        // The CLI default for --host is 127.0.0.1 (checked in main.rs)
        // Here we verify the config default matches
        let c = Config::default();
        assert!(c.gateway.require_pairing, "Config default must require pairing");
        assert!(!c.gateway.allow_public_bind, "Config default must block public bind");
    }

    #[test]
    async fn checklist_gateway_serde_roundtrip() {
        let g = GatewayConfig {
            port: 16830,
            host: "127.0.0.1".into(),
            require_pairing: true,
            allow_public_bind: false,
            paired_tokens: vec!["zc_test_token".into()],
            pair_rate_limit_per_minute: 12,
            webhook_rate_limit_per_minute: 80,
            api_rate_limit_per_minute: 90,
            trust_forwarded_headers: true,
            rate_limit_max_keys: 2048,
            idempotency_ttl_secs: 600,
            idempotency_max_keys: 4096,
            request_timeout_secs: 45,
        };
        let toml_str = toml::to_string(&g).unwrap();
        let parsed: GatewayConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.require_pairing);
        assert!(!parsed.allow_public_bind);
        assert_eq!(parsed.paired_tokens, vec!["zc_test_token"]);
        assert_eq!(parsed.pair_rate_limit_per_minute, 12);
        assert_eq!(parsed.webhook_rate_limit_per_minute, 80);
        assert_eq!(parsed.api_rate_limit_per_minute, 90);
        assert!(parsed.trust_forwarded_headers);
        assert_eq!(parsed.rate_limit_max_keys, 2048);
        assert_eq!(parsed.idempotency_ttl_secs, 600);
        assert_eq!(parsed.idempotency_max_keys, 4096);
        assert_eq!(parsed.request_timeout_secs, 45);
    }

    #[test]
    async fn checklist_gateway_backward_compat_no_gateway_section() {
        // Old configs without [gateway] should get secure defaults
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(
            parsed.gateway.require_pairing,
            "Missing [gateway] must default to require_pairing=true"
        );
        assert!(
            !parsed.gateway.allow_public_bind,
            "Missing [gateway] must default to allow_public_bind=false"
        );
    }

    #[test]
    async fn checklist_autonomy_default_is_workspace_scoped() {
        let a = AutonomyConfig::default();
        assert!(a.workspace_only, "Default autonomy must be workspace_only");
        assert!(a.forbidden_paths.contains(&"/etc".to_string()), "Must block /etc");
        assert!(a.forbidden_paths.contains(&"/proc".to_string()), "Must block /proc");
        assert!(a.forbidden_paths.contains(&"~/.ssh".to_string()), "Must block ~/.ssh");
    }

    // ══════════════════════════════════════════════════════════
    // COMPOSIO CONFIG TESTS
    // ══════════════════════════════════════════════════════════

    #[test]
    async fn composio_config_default_disabled() {
        let c = ComposioConfig::default();
        assert!(!c.enabled, "Composio must be disabled by default");
        assert!(c.api_key.is_none(), "No API key by default");
        assert_eq!(c.entity_id, "default");
    }

    #[test]
    async fn composio_config_serde_roundtrip() {
        let c = ComposioConfig {
            enabled: true,
            api_key: Some("comp-key-123".into()),
            entity_id: "user42".into(),
        };
        let toml_str = toml::to_string(&c).unwrap();
        let parsed: ComposioConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.api_key.as_deref(), Some("comp-key-123"));
        assert_eq!(parsed.entity_id, "user42");
    }

    #[test]
    async fn composio_config_backward_compat_missing_section() {
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(!parsed.composio.enabled, "Missing [composio] must default to disabled");
        assert!(parsed.composio.api_key.is_none());
    }

    #[test]
    async fn composio_config_partial_toml() {
        let toml_str = r"
enabled = true
";
        let parsed: ComposioConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.enabled);
        assert!(parsed.api_key.is_none());
        assert_eq!(parsed.entity_id, "default");
    }

    #[test]
    async fn composio_config_enable_alias_supported() {
        let toml_str = r"
enable = true
";
        let parsed: ComposioConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.enabled);
        assert!(parsed.api_key.is_none());
        assert_eq!(parsed.entity_id, "default");
    }

    // ══════════════════════════════════════════════════════════
    // SECRETS CONFIG TESTS
    // ══════════════════════════════════════════════════════════

    #[test]
    async fn secrets_config_default_encrypts() {
        let s = SecretsConfig::default();
        assert!(s.encrypt, "Encryption must be enabled by default");
    }

    #[test]
    async fn secrets_config_serde_roundtrip() {
        let s = SecretsConfig { encrypt: false };
        let toml_str = toml::to_string(&s).unwrap();
        let parsed: SecretsConfig = toml::from_str(&toml_str).unwrap();
        assert!(!parsed.encrypt);
    }

    #[test]
    async fn secrets_config_backward_compat_missing_section() {
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(parsed.secrets.encrypt, "Missing [secrets] must default to encrypt=true");
    }

    #[test]
    async fn auth_config_defaults_enable_codex_import() {
        let auth = AuthConfig::default();
        assert!(auth.codex_auth_json_auto_import);
        assert!(!auth.codex_auth_json_path.as_os_str().is_empty());
    }

    #[test]
    async fn auth_config_serde_roundtrip() {
        let auth = AuthConfig {
            codex_auth_json_auto_import: false,
            codex_auth_json_path: PathBuf::from("/tmp/custom-auth.json"),
        };
        let toml_str = toml::to_string(&auth).unwrap();
        let parsed: AuthConfig = toml::from_str(&toml_str).unwrap();
        assert!(!parsed.codex_auth_json_auto_import);
        assert_eq!(parsed.codex_auth_json_path, PathBuf::from("/tmp/custom-auth.json"));
    }

    #[test]
    async fn config_default_has_composio_and_secrets() {
        let c = Config::default();
        assert!(!c.composio.enabled);
        assert!(c.auth.codex_auth_json_auto_import);
        assert!(c.composio.api_key.is_none());
        assert!(c.secrets.encrypt);
        assert!(!c.browser.enabled);
        assert!(c.browser.allowed_domains.is_empty());
    }

    #[test]
    async fn browser_config_default_disabled() {
        let b = BrowserConfig::default();
        assert!(!b.enabled);
        assert!(b.allowed_domains.is_empty());
        assert_eq!(b.backend, "agent_browser");
        assert!(b.native_headless);
        assert_eq!(b.native_webdriver_url, "http://127.0.0.1:9515");
        assert!(b.native_chrome_path.is_none());
        assert_eq!(b.computer_use.endpoint, "http://127.0.0.1:8787/v1/actions");
        assert_eq!(b.computer_use.timeout_ms, 15_000);
        assert!(!b.computer_use.allow_remote_endpoint);
        assert!(b.computer_use.window_allowlist.is_empty());
        assert!(b.computer_use.max_coordinate_x.is_none());
        assert!(b.computer_use.max_coordinate_y.is_none());
    }

    #[test]
    async fn browser_config_serde_roundtrip() {
        let b = BrowserConfig {
            enabled: true,
            allowed_domains: vec!["example.com".into(), "docs.example.com".into()],
            session_name: None,
            backend: "auto".into(),
            native_headless: false,
            native_webdriver_url: "http://localhost:4444".into(),
            native_chrome_path: Some("/usr/bin/chromium".into()),
            computer_use: BrowserComputerUseConfig {
                endpoint: "https://computer-use.example.com/v1/actions".into(),
                api_key: Some("test-token".into()),
                timeout_ms: 8_000,
                allow_remote_endpoint: true,
                window_allowlist: vec!["Chrome".into(), "Visual Studio Code".into()],
                max_coordinate_x: Some(3840),
                max_coordinate_y: Some(2160),
            },
        };
        let toml_str = toml::to_string(&b).unwrap();
        let parsed: BrowserConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.allowed_domains.len(), 2);
        assert_eq!(parsed.allowed_domains[0], "example.com");
        assert_eq!(parsed.backend, "auto");
        assert!(!parsed.native_headless);
        assert_eq!(parsed.native_webdriver_url, "http://localhost:4444");
        assert_eq!(parsed.native_chrome_path.as_deref(), Some("/usr/bin/chromium"));
        assert_eq!(
            parsed.computer_use.endpoint,
            "https://computer-use.example.com/v1/actions"
        );
        assert_eq!(parsed.computer_use.api_key.as_deref(), Some("test-token"));
        assert_eq!(parsed.computer_use.timeout_ms, 8_000);
        assert!(parsed.computer_use.allow_remote_endpoint);
        assert_eq!(parsed.computer_use.window_allowlist.len(), 2);
        assert_eq!(parsed.computer_use.max_coordinate_x, Some(3840));
        assert_eq!(parsed.computer_use.max_coordinate_y, Some(2160));
    }

    #[test]
    async fn browser_config_backward_compat_missing_section() {
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(!parsed.browser.enabled);
        assert!(parsed.browser.allowed_domains.is_empty());
    }

    // ── Environment variable overrides (Docker support) ─────────

    async fn env_override_lock() -> MutexGuard<'static, ()> {
        static ENV_OVERRIDE_TEST_LOCK: Mutex<()> = Mutex::const_new(());
        ENV_OVERRIDE_TEST_LOCK.lock().await
    }

    fn clear_proxy_env_test_vars() {
        for key in [
            "OPENPRX_PROXY_ENABLED",
            "OPENPRX_HTTP_PROXY",
            "OPENPRX_HTTPS_PROXY",
            "OPENPRX_ALL_PROXY",
            "OPENPRX_NO_PROXY",
            "OPENPRX_PROXY_SCOPE",
            "OPENPRX_PROXY_SERVICES",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            test_remove_env(key);
        }
    }

    #[test]
    async fn env_override_api_key() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert!(config.api_key.is_none());

        test_set_env("OPENPRX_API_KEY", "sk-test-env-key");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("sk-test-env-key"));

        test_remove_env("OPENPRX_API_KEY");
    }

    #[test]
    async fn env_override_api_key_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_remove_env("OPENPRX_API_KEY");
        test_set_env("API_KEY", "sk-fallback-key");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("sk-fallback-key"));

        test_remove_env("API_KEY");
    }

    #[test]
    async fn env_override_provider() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_set_env("OPENPRX_PROVIDER", "anthropic");
        config.apply_env_overrides();
        assert_eq!(config.default_provider.as_deref(), Some("anthropic"));

        test_remove_env("OPENPRX_PROVIDER");
    }

    #[test]
    async fn env_override_open_skills_enabled_and_dir() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert!(!config.skills.open_skills_enabled);
        assert!(config.skills.open_skills_dir.is_none());

        test_set_env("OPENPRX_OPEN_SKILLS_ENABLED", "true");
        test_set_env("OPENPRX_OPEN_SKILLS_DIR", "/tmp/open-skills");
        config.apply_env_overrides();

        assert!(config.skills.open_skills_enabled);
        assert_eq!(config.skills.open_skills_dir.as_deref(), Some("/tmp/open-skills"));

        test_remove_env("OPENPRX_OPEN_SKILLS_ENABLED");
        test_remove_env("OPENPRX_OPEN_SKILLS_DIR");
    }

    #[test]
    async fn env_override_open_skills_enabled_invalid_value_keeps_existing_value() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        config.skills.open_skills_enabled = true;

        test_set_env("OPENPRX_OPEN_SKILLS_ENABLED", "maybe");
        config.apply_env_overrides();

        assert!(config.skills.open_skills_enabled);
        test_remove_env("OPENPRX_OPEN_SKILLS_ENABLED");
    }

    #[test]
    async fn env_override_provider_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_remove_env("OPENPRX_PROVIDER");
        test_set_env("PROVIDER", "openai");
        config.apply_env_overrides();
        assert_eq!(config.default_provider.as_deref(), Some("openai"));

        test_remove_env("PROVIDER");
    }

    #[test]
    async fn env_override_provider_fallback_does_not_replace_non_default_provider() {
        let _env_guard = env_override_lock().await;
        let mut config = Config {
            default_provider: Some("custom:https://proxy.example.com/v1".to_string()),
            ..Config::default()
        };

        test_remove_env("OPENPRX_PROVIDER");
        test_set_env("PROVIDER", "openrouter");
        config.apply_env_overrides();
        assert_eq!(
            config.default_provider.as_deref(),
            Some("custom:https://proxy.example.com/v1")
        );

        test_remove_env("PROVIDER");
    }

    #[test]
    async fn env_override_zero_claw_provider_overrides_non_default_provider() {
        let _env_guard = env_override_lock().await;
        let mut config = Config {
            default_provider: Some("custom:https://proxy.example.com/v1".to_string()),
            ..Config::default()
        };

        test_set_env("OPENPRX_PROVIDER", "openrouter");
        test_set_env("PROVIDER", "anthropic");
        config.apply_env_overrides();
        assert_eq!(config.default_provider.as_deref(), Some("openrouter"));

        test_remove_env("OPENPRX_PROVIDER");
        test_remove_env("PROVIDER");
    }

    #[test]
    async fn env_override_glm_api_key_for_regional_aliases() {
        let _env_guard = env_override_lock().await;
        let mut config = Config {
            default_provider: Some("glm-cn".to_string()),
            ..Config::default()
        };

        test_set_env("GLM_API_KEY", "glm-regional-key");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("glm-regional-key"));

        test_remove_env("GLM_API_KEY");
    }

    #[test]
    async fn env_override_zai_api_key_for_regional_aliases() {
        let _env_guard = env_override_lock().await;
        let mut config = Config {
            default_provider: Some("zai-cn".to_string()),
            ..Config::default()
        };

        test_set_env("ZAI_API_KEY", "zai-regional-key");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("zai-regional-key"));

        test_remove_env("ZAI_API_KEY");
    }

    #[test]
    async fn env_override_model() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_set_env("OPENPRX_MODEL", "gpt-4o");
        config.apply_env_overrides();
        assert_eq!(config.default_model.as_deref(), Some("gpt-4o"));

        test_remove_env("OPENPRX_MODEL");
    }

    #[test]
    async fn env_override_model_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_remove_env("OPENPRX_MODEL");
        test_set_env("MODEL", "anthropic/claude-3.5-sonnet");
        config.apply_env_overrides();
        assert_eq!(config.default_model.as_deref(), Some("anthropic/claude-3.5-sonnet"));

        test_remove_env("MODEL");
    }

    #[test]
    async fn env_override_workspace() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_set_env("OPENPRX_WORKSPACE", "/custom/workspace");
        config.apply_env_overrides();
        assert_eq!(config.workspace_dir, PathBuf::from("/custom/workspace"));

        test_remove_env("OPENPRX_WORKSPACE");
    }

    #[test]
    async fn resolve_runtime_config_dirs_uses_env_workspace_first() {
        let _env_guard = env_override_lock().await;
        let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let default_workspace_dir = default_config_dir.join("workspace");
        let workspace_dir = default_config_dir.join("profile-a");

        test_set_env("OPENPRX_WORKSPACE", &workspace_dir);
        let (config_dir, resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir)
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::EnvWorkspace);
        assert_eq!(config_dir, workspace_dir);
        assert_eq!(resolved_workspace_dir, workspace_dir.join("workspace"));

        test_remove_env("OPENPRX_WORKSPACE");
        let _ = fs::remove_dir_all(default_config_dir).await;
    }

    #[test]
    async fn resolve_runtime_config_dirs_uses_env_config_dir_first() {
        let _env_guard = env_override_lock().await;
        let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let default_workspace_dir = default_config_dir.join("workspace");
        let explicit_config_dir = default_config_dir.join("explicit-config");
        let marker_config_dir = default_config_dir.join("profiles").join("alpha");
        let state_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

        fs::create_dir_all(&default_config_dir).await.unwrap();
        let state = ActiveWorkspaceState {
            config_dir: marker_config_dir.to_string_lossy().into_owned(),
        };
        fs::write(&state_path, toml::to_string(&state).unwrap()).await.unwrap();

        test_set_env("OPENPRX_CONFIG_DIR", &explicit_config_dir);
        test_remove_env("OPENPRX_WORKSPACE");

        let (config_dir, resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir)
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::EnvConfigDir);
        assert_eq!(config_dir, explicit_config_dir);
        assert_eq!(resolved_workspace_dir, explicit_config_dir.join("workspace"));

        test_remove_env("OPENPRX_CONFIG_DIR");
        let _ = fs::remove_dir_all(default_config_dir).await;
    }

    #[test]
    async fn resolve_runtime_config_dirs_uses_active_workspace_marker() {
        let _env_guard = env_override_lock().await;
        let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let default_workspace_dir = default_config_dir.join("workspace");
        let marker_config_dir = default_config_dir.join("profiles").join("alpha");
        let state_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

        test_remove_env("OPENPRX_WORKSPACE");
        fs::create_dir_all(&default_config_dir).await.unwrap();
        let state = ActiveWorkspaceState {
            config_dir: marker_config_dir.to_string_lossy().into_owned(),
        };
        fs::write(&state_path, toml::to_string(&state).unwrap()).await.unwrap();

        let (config_dir, resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir)
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::ActiveWorkspaceMarker);
        assert_eq!(config_dir, marker_config_dir);
        assert_eq!(resolved_workspace_dir, marker_config_dir.join("workspace"));

        let _ = fs::remove_dir_all(default_config_dir).await;
    }

    #[test]
    async fn resolve_runtime_config_dirs_falls_back_to_default_layout() {
        let _env_guard = env_override_lock().await;
        let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let default_workspace_dir = default_config_dir.join("workspace");

        test_remove_env("OPENPRX_WORKSPACE");
        let (config_dir, resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir)
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::DefaultConfigDir);
        assert_eq!(config_dir, default_config_dir);
        assert_eq!(resolved_workspace_dir, default_workspace_dir);

        let _ = fs::remove_dir_all(default_config_dir).await;
    }

    #[test]
    async fn load_or_init_workspace_override_uses_workspace_root_for_config() {
        let _env_guard = env_override_lock().await;
        let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");

        let original_home = std::env::var("HOME").ok();
        test_set_env("HOME", &temp_home);
        test_set_env("OPENPRX_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.workspace_dir, workspace_dir.join("workspace"));
        assert_eq!(config.config_path, workspace_dir.join("config.toml"));
        assert!(workspace_dir.join("config.toml").exists());

        test_remove_env("OPENPRX_WORKSPACE");
        if let Some(home) = original_home {
            test_set_env("HOME", home);
        } else {
            test_remove_env("HOME");
        }
        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_workspace_suffix_uses_legacy_config_layout() {
        let _env_guard = env_override_lock().await;
        let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("workspace");
        let legacy_config_path = temp_home.join(".openprx").join("config.toml");

        let original_home = std::env::var("HOME").ok();
        test_set_env("HOME", &temp_home);
        test_set_env("OPENPRX_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.workspace_dir, workspace_dir);
        assert_eq!(config.config_path, legacy_config_path);
        assert!(config.config_path.exists());

        test_remove_env("OPENPRX_WORKSPACE");
        if let Some(home) = original_home {
            test_set_env("HOME", home);
        } else {
            test_remove_env("HOME");
        }
        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_workspace_override_keeps_existing_legacy_config() {
        let _env_guard = env_override_lock().await;
        let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("custom-workspace");
        let legacy_config_dir = temp_home.join(".openprx");
        let legacy_config_path = legacy_config_dir.join("config.toml");

        fs::create_dir_all(&legacy_config_dir).await.unwrap();
        fs::write(
            &legacy_config_path,
            r#"default_temperature = 0.7
default_model = "legacy-model"
"#,
        )
        .await
        .unwrap();

        let original_home = std::env::var("HOME").ok();
        test_set_env("HOME", &temp_home);
        test_set_env("OPENPRX_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.workspace_dir, workspace_dir);
        assert_eq!(config.config_path, legacy_config_path);
        assert_eq!(config.default_model.as_deref(), Some("legacy-model"));

        test_remove_env("OPENPRX_WORKSPACE");
        if let Some(home) = original_home {
            test_set_env("HOME", home);
        } else {
            test_remove_env("HOME");
        }
        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_uses_persisted_active_workspace_marker() {
        let _env_guard = env_override_lock().await;
        let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
        let custom_config_dir = temp_home.join("profiles").join("agent-alpha");

        fs::create_dir_all(&custom_config_dir).await.unwrap();
        fs::write(
            custom_config_dir.join("config.toml"),
            "default_temperature = 0.7\ndefault_model = \"persisted-profile\"\n",
        )
        .await
        .unwrap();

        let original_home = std::env::var("HOME").ok();
        test_set_env("HOME", &temp_home);
        test_remove_env("OPENPRX_WORKSPACE");

        persist_active_workspace_config_dir(&custom_config_dir).await.unwrap();

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.config_path, custom_config_dir.join("config.toml"));
        assert_eq!(config.workspace_dir, custom_config_dir.join("workspace"));
        assert_eq!(config.default_model.as_deref(), Some("persisted-profile"));

        if let Some(home) = original_home {
            test_set_env("HOME", home);
        } else {
            test_remove_env("HOME");
        }
        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_env_workspace_override_takes_priority_over_marker() {
        let _env_guard = env_override_lock().await;
        let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
        let marker_config_dir = temp_home.join("profiles").join("persisted-profile");
        let env_workspace_dir = temp_home.join("env-workspace");

        fs::create_dir_all(&marker_config_dir).await.unwrap();
        fs::write(
            marker_config_dir.join("config.toml"),
            "default_temperature = 0.7\ndefault_model = \"marker-model\"\n",
        )
        .await
        .unwrap();

        let original_home = std::env::var("HOME").ok();
        test_set_env("HOME", &temp_home);
        persist_active_workspace_config_dir(&marker_config_dir).await.unwrap();
        test_set_env("OPENPRX_WORKSPACE", &env_workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.workspace_dir, env_workspace_dir.join("workspace"));
        assert_eq!(config.config_path, env_workspace_dir.join("config.toml"));

        test_remove_env("OPENPRX_WORKSPACE");
        if let Some(home) = original_home {
            test_set_env("HOME", home);
        } else {
            test_remove_env("HOME");
        }
        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn persist_active_workspace_marker_is_cleared_for_default_config_dir() {
        let _env_guard = env_override_lock().await;
        let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
        let default_config_dir = temp_home.join(".openprx");
        let custom_config_dir = temp_home.join("profiles").join("custom-profile");
        let marker_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

        let original_home = std::env::var("HOME").ok();
        test_set_env("HOME", &temp_home);

        persist_active_workspace_config_dir(&custom_config_dir).await.unwrap();
        assert!(marker_path.exists());

        persist_active_workspace_config_dir(&default_config_dir).await.unwrap();
        assert!(!marker_path.exists());

        if let Some(home) = original_home {
            test_set_env("HOME", home);
        } else {
            test_remove_env("HOME");
        }
        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn env_override_empty_values_ignored() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        let original_provider = config.default_provider.clone();

        test_set_env("OPENPRX_PROVIDER", "");
        config.apply_env_overrides();
        assert_eq!(config.default_provider, original_provider);

        test_remove_env("OPENPRX_PROVIDER");
    }

    #[test]
    async fn env_override_gateway_port() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert_eq!(config.gateway.port, 16830);

        test_set_env("OPENPRX_GATEWAY_PORT", "8080");
        config.apply_env_overrides();
        assert_eq!(config.gateway.port, 8080);

        test_remove_env("OPENPRX_GATEWAY_PORT");
    }

    #[test]
    async fn env_override_port_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_remove_env("OPENPRX_GATEWAY_PORT");
        test_set_env("PORT", "9000");
        config.apply_env_overrides();
        assert_eq!(config.gateway.port, 9000);

        test_remove_env("PORT");
    }

    #[test]
    async fn env_override_gateway_host() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert_eq!(config.gateway.host, "127.0.0.1");

        test_set_env("OPENPRX_GATEWAY_HOST", "0.0.0.0");
        config.apply_env_overrides();
        assert_eq!(config.gateway.host, "0.0.0.0");

        test_remove_env("OPENPRX_GATEWAY_HOST");
    }

    #[test]
    async fn env_override_host_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_remove_env("OPENPRX_GATEWAY_HOST");
        test_set_env("HOST", "0.0.0.0");
        config.apply_env_overrides();
        assert_eq!(config.gateway.host, "0.0.0.0");

        test_remove_env("HOST");
    }

    #[test]
    async fn env_override_temperature() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_set_env("OPENPRX_TEMPERATURE", "0.5");
        config.apply_env_overrides();
        assert!((config.default_temperature - 0.5).abs() < f64::EPSILON);

        test_remove_env("OPENPRX_TEMPERATURE");
    }

    #[test]
    async fn env_override_temperature_out_of_range_ignored() {
        let _env_guard = env_override_lock().await;
        // Clean up any leftover env vars from other tests
        test_remove_env("OPENPRX_TEMPERATURE");

        let mut config = Config::default();
        let original_temp = config.default_temperature;

        // Temperature > 2.0 should be ignored
        test_set_env("OPENPRX_TEMPERATURE", "3.0");
        config.apply_env_overrides();
        assert!(
            (config.default_temperature - original_temp).abs() < f64::EPSILON,
            "Temperature 3.0 should be ignored (out of range)"
        );

        test_remove_env("OPENPRX_TEMPERATURE");
    }

    #[test]
    async fn env_override_reasoning_enabled() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert_eq!(config.runtime.reasoning_enabled, None);

        test_set_env("OPENPRX_REASONING_ENABLED", "false");
        config.apply_env_overrides();
        assert_eq!(config.runtime.reasoning_enabled, Some(false));

        test_set_env("OPENPRX_REASONING_ENABLED", "true");
        config.apply_env_overrides();
        assert_eq!(config.runtime.reasoning_enabled, Some(true));

        test_remove_env("OPENPRX_REASONING_ENABLED");
    }

    #[test]
    async fn env_override_reasoning_invalid_value_ignored() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        config.runtime.reasoning_enabled = Some(false);

        test_set_env("OPENPRX_REASONING_ENABLED", "maybe");
        config.apply_env_overrides();
        assert_eq!(config.runtime.reasoning_enabled, Some(false));

        test_remove_env("OPENPRX_REASONING_ENABLED");
    }

    #[test]
    async fn env_override_invalid_port_ignored() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        let original_port = config.gateway.port;

        test_set_env("PORT", "not_a_number");
        config.apply_env_overrides();
        assert_eq!(config.gateway.port, original_port);

        test_remove_env("PORT");
    }

    #[test]
    async fn env_override_web_search_config() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_set_env("WEB_SEARCH_ENABLED", "false");
        test_set_env("WEB_SEARCH_PROVIDER", "brave");
        test_set_env("WEB_SEARCH_MAX_RESULTS", "7");
        test_set_env("WEB_SEARCH_TIMEOUT_SECS", "20");
        test_set_env("BRAVE_API_KEY", "brave-test-key");

        config.apply_env_overrides();

        assert!(!config.web_search.enabled);
        assert_eq!(config.web_search.provider, "brave");
        assert_eq!(config.web_search.max_results, 7);
        assert_eq!(config.web_search.timeout_secs, 20);
        assert_eq!(config.web_search.brave_api_key.as_deref(), Some("brave-test-key"));

        test_remove_env("WEB_SEARCH_ENABLED");
        test_remove_env("WEB_SEARCH_PROVIDER");
        test_remove_env("WEB_SEARCH_MAX_RESULTS");
        test_remove_env("WEB_SEARCH_TIMEOUT_SECS");
        test_remove_env("BRAVE_API_KEY");
    }

    #[test]
    async fn env_override_web_search_invalid_values_ignored() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        let original_max_results = config.web_search.max_results;
        let original_timeout = config.web_search.timeout_secs;

        test_set_env("WEB_SEARCH_MAX_RESULTS", "99");
        test_set_env("WEB_SEARCH_TIMEOUT_SECS", "0");

        config.apply_env_overrides();

        assert_eq!(config.web_search.max_results, original_max_results);
        assert_eq!(config.web_search.timeout_secs, original_timeout);

        test_remove_env("WEB_SEARCH_MAX_RESULTS");
        test_remove_env("WEB_SEARCH_TIMEOUT_SECS");
    }

    #[test]
    async fn env_override_storage_provider_config() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_set_env("OPENPRX_STORAGE_PROVIDER", "postgres");
        test_set_env("OPENPRX_STORAGE_DB_URL", "postgres://example/db");
        test_set_env("OPENPRX_STORAGE_CONNECT_TIMEOUT_SECS", "15");

        config.apply_env_overrides();

        assert_eq!(config.storage.provider.config.provider, "postgres");
        assert_eq!(
            config.storage.provider.config.db_url.as_deref(),
            Some("postgres://example/db")
        );
        assert_eq!(config.storage.provider.config.connect_timeout_secs, Some(15));

        test_remove_env("OPENPRX_STORAGE_PROVIDER");
        test_remove_env("OPENPRX_STORAGE_DB_URL");
        test_remove_env("OPENPRX_STORAGE_CONNECT_TIMEOUT_SECS");
    }

    #[test]
    async fn env_override_agent_concurrency_controls() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        test_set_env("OPENPRX_CONCURRENCY_KILL_SWITCH_FORCE_SERIAL", "true");
        test_set_env("OPENPRX_CONCURRENCY_ROLLOUT_STAGE", "stage_c");
        test_set_env("OPENPRX_CONCURRENCY_ROLLOUT_SAMPLE_PERCENT", "40");
        test_set_env("OPENPRX_CONCURRENCY_ROLLOUT_CHANNELS", "telegram,discord");
        test_set_env("OPENPRX_CONCURRENCY_AUTO_ROLLBACK_ENABLED", "false");
        test_set_env("OPENPRX_CONCURRENCY_ROLLBACK_TIMEOUT_RATE_THRESHOLD", "0.31");
        test_set_env("OPENPRX_CONCURRENCY_ROLLBACK_CANCEL_RATE_THRESHOLD", "0.32");
        test_set_env("OPENPRX_CONCURRENCY_ROLLBACK_ERROR_RATE_THRESHOLD", "0.33");

        config.apply_env_overrides();

        assert!(config.agent.concurrency_kill_switch_force_serial);
        assert_eq!(config.agent.concurrency_rollout_stage, "stage_c");
        assert_eq!(config.agent.concurrency_rollout_sample_percent, 40);
        assert_eq!(config.agent.concurrency_rollout_channels, vec!["telegram", "discord"]);
        assert!(!config.agent.concurrency_auto_rollback_enabled);
        assert!((config.agent.concurrency_rollback_timeout_rate_threshold - 0.31).abs() < 1e-9);
        assert!((config.agent.concurrency_rollback_cancel_rate_threshold - 0.32).abs() < 1e-9);
        assert!((config.agent.concurrency_rollback_error_rate_threshold - 0.33).abs() < 1e-9);

        test_remove_env("OPENPRX_CONCURRENCY_KILL_SWITCH_FORCE_SERIAL");
        test_remove_env("OPENPRX_CONCURRENCY_ROLLOUT_STAGE");
        test_remove_env("OPENPRX_CONCURRENCY_ROLLOUT_SAMPLE_PERCENT");
        test_remove_env("OPENPRX_CONCURRENCY_ROLLOUT_CHANNELS");
        test_remove_env("OPENPRX_CONCURRENCY_AUTO_ROLLBACK_ENABLED");
        test_remove_env("OPENPRX_CONCURRENCY_ROLLBACK_TIMEOUT_RATE_THRESHOLD");
        test_remove_env("OPENPRX_CONCURRENCY_ROLLBACK_CANCEL_RATE_THRESHOLD");
        test_remove_env("OPENPRX_CONCURRENCY_ROLLBACK_ERROR_RATE_THRESHOLD");
    }

    #[test]
    async fn proxy_config_scope_services_requires_entries_when_enabled() {
        let proxy = ProxyConfig {
            enabled: true,
            http_proxy: Some("http://127.0.0.1:7890".into()),
            https_proxy: None,
            all_proxy: None,
            no_proxy: Vec::new(),
            scope: ProxyScope::Services,
            services: Vec::new(),
        };

        let error = proxy.validate().unwrap_err().to_string();
        assert!(error.contains("proxy.scope='services'"));
    }

    #[test]
    async fn env_override_proxy_scope_services() {
        let _env_guard = env_override_lock().await;
        clear_proxy_env_test_vars();

        let mut config = Config::default();
        test_set_env("OPENPRX_PROXY_ENABLED", "true");
        test_set_env("OPENPRX_HTTP_PROXY", "http://127.0.0.1:7890");
        test_set_env("OPENPRX_PROXY_SERVICES", "provider.openai, tool.http_request");
        test_set_env("OPENPRX_PROXY_SCOPE", "services");

        config.apply_env_overrides();

        assert!(config.proxy.enabled);
        assert_eq!(config.proxy.scope, ProxyScope::Services);
        assert_eq!(config.proxy.http_proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert!(config.proxy.should_apply_to_service("provider.openai"));
        assert!(config.proxy.should_apply_to_service("tool.http_request"));
        assert!(!config.proxy.should_apply_to_service("provider.anthropic"));

        clear_proxy_env_test_vars();
    }

    #[test]
    async fn env_override_proxy_scope_environment_applies_process_env() {
        let _env_guard = env_override_lock().await;
        clear_proxy_env_test_vars();

        let mut config = Config::default();
        test_set_env("OPENPRX_PROXY_ENABLED", "true");
        test_set_env("OPENPRX_PROXY_SCOPE", "environment");
        test_set_env("OPENPRX_HTTP_PROXY", "http://127.0.0.1:7890");
        test_set_env("OPENPRX_HTTPS_PROXY", "http://127.0.0.1:7891");
        test_set_env("OPENPRX_NO_PROXY", "localhost,127.0.0.1");

        config.apply_env_overrides();

        assert_eq!(config.proxy.scope, ProxyScope::Environment);
        assert_eq!(
            std::env::var("HTTP_PROXY").ok().as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            std::env::var("HTTPS_PROXY").ok().as_deref(),
            Some("http://127.0.0.1:7891")
        );
        assert!(
            std::env::var("NO_PROXY")
                .ok()
                .is_some_and(|value| value.contains("localhost"))
        );

        clear_proxy_env_test_vars();
    }

    #[test]
    async fn test_router_config_rejects_invalid_threshold() {
        let mut config = RouterConfig::default();
        config.automix.enabled = true;
        config.automix.confidence_threshold = 1.5;
        config.automix.premium_model_id = "openai/model-premium".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    async fn test_router_config_rejects_negative_weight() {
        let mut config = RouterConfig::default();
        config.alpha = -0.1;
        assert!(config.validate().is_err());
    }

    #[test]
    async fn test_router_config_rejects_empty_premium_model() {
        let mut config = RouterConfig::default();
        config.automix.enabled = true;
        config.automix.premium_model_id = String::new();
        assert!(config.validate().is_err());
    }

    fn runtime_proxy_cache_contains(cache_key: &str) -> bool {
        runtime_proxy_client_cache().read().contains_key(cache_key)
    }

    /// Serialise tests that mutate the global `RUNTIME_PROXY_CLIENT_CACHE` so
    /// they cannot race against each other.
    static PROXY_CACHE_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    async fn runtime_proxy_client_cache_reuses_default_profile_key() {
        let _guard = PROXY_CACHE_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let service_key = format!(
            "provider.cache_test.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        );
        let cache_key = runtime_proxy_cache_key(&service_key, None, None);

        clear_runtime_proxy_client_cache();
        assert!(!runtime_proxy_cache_contains(&cache_key));

        build_runtime_proxy_client(&service_key).expect("test: proxy client build");
        assert!(runtime_proxy_cache_contains(&cache_key));

        build_runtime_proxy_client(&service_key).expect("test: proxy client build (cached)");
        assert!(runtime_proxy_cache_contains(&cache_key));
    }

    #[test]
    async fn set_runtime_proxy_config_clears_runtime_proxy_client_cache() {
        let _guard = PROXY_CACHE_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let service_key = format!(
            "provider.cache_timeout_test.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        );
        let cache_key = runtime_proxy_cache_key(&service_key, Some(30), Some(5));

        clear_runtime_proxy_client_cache();
        build_runtime_proxy_client_with_timeouts(&service_key, 30, 5).expect("test: proxy client with timeouts build");
        assert!(runtime_proxy_cache_contains(&cache_key));

        set_runtime_proxy_config(ProxyConfig::default());
        assert!(!runtime_proxy_cache_contains(&cache_key));
    }

    #[test]
    async fn gateway_config_default_values() {
        let g = GatewayConfig::default();
        assert_eq!(g.port, 16830);
        assert_eq!(g.host, "127.0.0.1");
        assert!(g.require_pairing);
        assert!(!g.allow_public_bind);
        assert!(g.paired_tokens.is_empty());
        assert!(!g.trust_forwarded_headers);
        assert_eq!(g.rate_limit_max_keys, 10_000);
        assert_eq!(g.idempotency_max_keys, 10_000);
        assert_eq!(g.request_timeout_secs, 60);
    }

    #[test]
    async fn signal_policy_defaults_and_startup_timeout() {
        let parsed: SignalConfig = toml::from_str(r#"account = "+1234567890""#).unwrap();
        assert_eq!(parsed.startup_timeout_ms, 30_000);
        assert_eq!(parsed.dm_policy, DmPolicy::Allowlist);
        assert_eq!(parsed.group_policy, GroupPolicy::Allowlist);
    }

    #[test]
    async fn signal_dm_policy_unknown_variant_fails_deserialization() {
        let parsed = toml::from_str::<SignalConfig>(
            r#"
account = "+1234567890"
dm_policy = "unknown_policy"
"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    async fn whatsapp_allowed_from_alias_deserializes_into_allowed_numbers() {
        let parsed: WhatsAppConfig = toml::from_str(
            r#"
access_token = "tok"
phone_number_id = "id"
verify_token = "verify"
allowed_from = ["*"]
"#,
        )
        .unwrap();
        assert_eq!(parsed.allowed_numbers, vec!["*"]);
    }

    #[test]
    async fn heartbeat_defaults_include_active_hours_and_prompt() {
        let hb = HeartbeatConfig::default();
        assert_eq!(hb.active_hours, vec![8, 23]);
        assert_eq!(hb.prompt, "Check HEARTBEAT.md and follow instructions.");
    }

    #[test]
    async fn lark_config_serde() {
        let lc = LarkConfig {
            app_id: "cli_123456".into(),
            app_secret: "secret_abc".into(),
            encrypt_key: Some("encrypt_key".into()),
            verification_token: Some("verify_token".into()),
            allowed_users: vec!["user_123".into(), "user_456".into()],
            use_feishu: true,
            receive_mode: LarkReceiveMode::Websocket,
            port: None,
            mention_only: false,
        };
        let json = serde_json::to_string(&lc).unwrap();
        let parsed: LarkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.app_id, "cli_123456");
        assert_eq!(parsed.app_secret, "secret_abc");
        assert_eq!(parsed.encrypt_key.as_deref(), Some("encrypt_key"));
        assert_eq!(parsed.verification_token.as_deref(), Some("verify_token"));
        assert_eq!(parsed.allowed_users.len(), 2);
        assert!(parsed.use_feishu);
    }

    #[test]
    async fn lark_config_toml_roundtrip() {
        let lc = LarkConfig {
            app_id: "cli_123456".into(),
            app_secret: "secret_abc".into(),
            encrypt_key: Some("encrypt_key".into()),
            verification_token: Some("verify_token".into()),
            allowed_users: vec!["*".into()],
            use_feishu: false,
            receive_mode: LarkReceiveMode::Webhook,
            port: Some(9898),
            mention_only: false,
        };
        let toml_str = toml::to_string(&lc).unwrap();
        let parsed: LarkConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.app_id, "cli_123456");
        assert_eq!(parsed.app_secret, "secret_abc");
        assert!(!parsed.use_feishu);
    }

    #[test]
    async fn lark_config_deserializes_without_optional_fields() {
        let json = r#"{"app_id":"cli_123","app_secret":"secret"}"#;
        let parsed: LarkConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.encrypt_key.is_none());
        assert!(parsed.verification_token.is_none());
        assert!(parsed.allowed_users.is_empty());
        assert!(!parsed.use_feishu);
    }

    #[test]
    async fn lark_config_defaults_to_lark_endpoint() {
        let json = r#"{"app_id":"cli_123","app_secret":"secret"}"#;
        let parsed: LarkConfig = serde_json::from_str(json).unwrap();
        assert!(!parsed.use_feishu, "use_feishu should default to false (Lark)");
    }

    #[test]
    async fn lark_config_with_wildcard_allowed_users() {
        let json = r#"{"app_id":"cli_123","app_secret":"secret","allowed_users":["*"]}"#;
        let parsed: LarkConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.allowed_users, vec!["*"]);
    }

    #[test]
    async fn nextcloud_talk_config_serde() {
        let nc = NextcloudTalkConfig {
            base_url: "https://cloud.example.com".into(),
            app_token: "app-token".into(),
            webhook_secret: Some("webhook-secret".into()),
            allowed_users: vec!["user_a".into(), "*".into()],
            mention_only: false,
        };

        let json = serde_json::to_string(&nc).unwrap();
        let parsed: NextcloudTalkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.base_url, "https://cloud.example.com");
        assert_eq!(parsed.app_token, "app-token");
        assert_eq!(parsed.webhook_secret.as_deref(), Some("webhook-secret"));
        assert_eq!(parsed.allowed_users, vec!["user_a", "*"]);
    }

    #[test]
    async fn nextcloud_talk_config_defaults_optional_fields() {
        let json = r#"{"base_url":"https://cloud.example.com","app_token":"app-token"}"#;
        let parsed: NextcloudTalkConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.webhook_secret.is_none());
        assert!(parsed.allowed_users.is_empty());
    }

    // ── Config file permission hardening (Unix only) ───────────────

    #[cfg(unix)]
    #[test]
    async fn new_config_file_has_restricted_permissions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Create a config and save it
        let mut config = Config::default();
        config.config_path = config_path.clone();
        config.save().await.unwrap();

        let meta = fs::metadata(&config_path).await.unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "New config file should be owner-only (0600), got {mode:o}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_toml_string_atomic_rejects_symlink_target() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let real_path = tmp.path().join("real.toml");
        let symlink_path = tmp.path().join("config.toml");
        std::fs::write(&real_path, "default_temperature = 0.7\n").unwrap();
        symlink(&real_path, &symlink_path).unwrap();

        let error = write_toml_string_atomic(&symlink_path, "default_temperature = 1.0\n")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("symlink"), "unexpected error: {error}");
    }

    #[cfg(unix)]
    #[test]
    async fn world_readable_config_is_detectable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Create a config file with intentionally loose permissions
        std::fs::write(&config_path, "# test config").unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let meta = std::fs::metadata(&config_path).unwrap();
        let mode = meta.permissions().mode();
        assert!(
            mode & 0o004 != 0,
            "Test setup: file should be world-readable (mode {mode:o})"
        );
    }
}
