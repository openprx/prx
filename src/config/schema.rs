mod codex_effort;
pub use codex_effort::InvalidCodexReasoningEffort;
mod recursive_spawn;

use crate::auth::codex_auth::default_codex_auth_json_path;
use crate::config::files::{build_split_tables, read_merged_toml_with_gate};
use crate::security::AutonomyLevel;
use anyhow::{Context, Result};
use parking_lot::RwLock;
use schemars::JsonSchema;
use serde::de::IntoDeserializer;
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

// ── Top-level config ──────────────────────────────────────────────

/// Every set of duplicate `[[embedding_routes]]` hints already reported by
/// `validate`, so a configuration that is validated again warns once.
static REPORTED_DUPLICATE_EMBEDDING_HINTS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashSet<Vec<String>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

/// One sentence per `[[embedding_routes]]` entry whose hint an earlier entry
/// already uses, with the fix. Empty when every hint names one route.
#[must_use]
pub fn duplicate_embedding_route_hints(config: &Config) -> Vec<String> {
    config
        .embedding_routes
        .iter()
        .enumerate()
        .filter_map(|(i, route)| {
            config
                .embedding_routes
                .iter()
                .take(i)
                .position(|earlier| earlier.hint.trim() == route.hint.trim())
                .map(|first| {
                    format!(
                        "embedding_routes[{i}] and embedding_routes[{first}] both use hint '{}'; only the first is \
                         used. Give each route its own hint, or remove one",
                        route.hint.trim()
                    )
                })
        })
        .collect()
}

/// Whether the `[[providers.tool_dialects]]` deprecation was reported in
/// this process (once is enough; every load and reload validates).
static REPORTED_TOOL_DIALECTS_DEPRECATION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Every set of unused `[[providers.tool_dialects]]` entries (index and
/// redacted provider) already reported by `validate`, so a configuration that
/// is validated again warns once.
static REPORTED_UNUSED_TOOL_DIALECTS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashSet<Vec<(usize, String)>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

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
    /// API key for the selected provider. No environment variable sets or
    /// overrides it; when it is unset, the key stored for the provider with
    /// `prx auth paste-token --provider <name>` is used.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Base URL override for provider API (e.g. "http://10.0.0.1:11434" for remote Ollama)
    #[serde(default)]
    pub api_url: Option<String>,
    /// Default provider ID or alias (e.g. `"openrouter"`, `"ollama"`, `"anthropic"`). Default: `"openrouter"`.
    #[serde(default = "default_provider_from_catalog")]
    pub default_provider: Option<String>,
    /// Default model routed through the selected provider (e.g. `"anthropic/claude-sonnet-4-6"`).
    #[serde(default = "default_model_from_catalog")]
    pub default_model: Option<String>,
    /// Default model temperature (0.0–2.0). Default: `0.7`.
    #[serde(default = "default_temperature")]
    pub default_temperature: f64,

    /// Catalog model levels and explicit parameters, keyed by canonical release ID.
    #[serde(default)]
    pub model_settings: std::collections::BTreeMap<String, crate::config::model_settings::ModelSettings>,

    /// Provider-specific runtime options (`[providers.<name>]`).
    #[serde(default)]
    pub providers: ProvidersConfig,

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

    /// Interactive chat UI configuration (`[chat]`).
    #[serde(default)]
    pub chat: ChatConfig,

    /// Self-system experimental automation controls (`[self_system]`).
    #[serde(default)]
    pub self_system: SelfSystemConfig,

    /// Skills loading and community repository behavior (`[skills]`).
    #[serde(default)]
    pub skills: SkillsConfig,

    /// Dynamic skill retrieval settings (`[skill_rag]`).
    #[serde(default)]
    pub skill_rag: SkillRagConfig,

    /// WASM plugin runtime switch (`[plugins]`), off by default.
    #[serde(default)]
    pub plugins: crate::config::plugins::PluginsConfig,

    /// Skill lifecycle (`[skills_lifecycle]`), off by default.
    #[serde(default)]
    pub skills_lifecycle: crate::config::skills_lifecycle::SkillsLifecycleConfig,

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

    /// Smart group-reply pre-gate configuration (`[smart_group]`).
    #[serde(default)]
    pub smart_group: SmartGroupConfig,

    /// Heartbeat configuration for periodic health pings (`[heartbeat]`).
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,

    /// Xin autonomous task engine configuration (`[xin]`).
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

    /// MCP server compatibility configuration (`[mcp_server]`).
    #[serde(default)]
    pub mcp_server: McpServerRuntimeConfig,

    /// Agent-to-Agent compatibility configuration (`[a2a]`).
    #[serde(default)]
    pub a2a: A2aConfig,

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

    /// Decision-plane configuration (`[causal_tree]`).
    /// Default: enabled locally; remote processing remains explicit opt-in.
    /// A versioned `enabled = false` preserves an explicit disable.
    #[serde(default)]
    pub causal_tree: crate::causal_tree::CausalTreeConfig,

    /// Security configuration: sandboxing, resource limits, audit, tool policy (`[security]`).
    #[serde(default)]
    pub security: SecurityConfig,

    /// Operator-owned compliance controls (`[compliance]`).
    #[serde(default)]
    pub compliance: ComplianceConfig,

    /// Tool tiering configuration — controls which tools are exposed based on intent (`[tool_tiering]`).
    #[serde(default)]
    pub tool_tiering: ToolTieringConfig,
}

// Every serialized field is rendered, masked, under `settings`; the lint
// cannot see through the serde rendering.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for Config {
    /// The serialized settings with every secret path of the registry masked
    /// (`crate::config::secret_paths`), plus the two path fields that are not
    /// serialized.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("Config");
        debug
            .field("workspace_dir", &self.workspace_dir)
            .field("config_path", &self.config_path);
        match serde_json::to_value(self) {
            Ok(mut settings) => {
                crate::config::secret_paths::redact(&mut settings);
                debug.field("settings", &settings);
            }
            // Never fall back to the derived field output: it would print
            // secrets.
            Err(_) => {
                debug.field("settings", &"<unserializable>");
            }
        }
        debug.finish()
    }
}

/// `Debug` for configuration sub-structs that hold a secret or an endpoint
/// URL: the serialized settings with every secret and URL userinfo masked
/// (`secret_paths::debug_redacted`), never the derived field output. The
/// path is where the struct sits in the configuration.
macro_rules! redacted_debug {
    ($($ty:ident => $path:literal),* $(,)?) => {
        $(
            impl std::fmt::Debug for $ty {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    crate::config::secret_paths::debug_redacted(f, stringify!($ty), self, $path)
                }
            }
        )*
    };
}

redacted_debug! {
    DelegateAgentConfig => "agents.<name>",
    ModelRouteConfig => "model_routes[]",
    EmbeddingRouteConfig => "embedding_routes[]",
    ReliabilityConfig => "reliability",
    ComposioConfig => "composio",
    BrowserComputerUseConfig => "browser.computer_use",
    WebSearchConfig => "web_search",
    StorageProviderConfig => "storage.provider.config",
    GatewayConfig => "gateway",
    McpServerConfig => "mcp.servers.<name>",
    TelegramConfig => "channels_config.telegram",
    DiscordConfig => "channels_config.discord",
    SlackConfig => "channels_config.slack",
    MattermostConfig => "channels_config.mattermost",
    DingTalkConfig => "channels_config.dingtalk",
    IrcConfig => "channels_config.irc",
    LarkConfig => "channels_config.lark",
    LinqConfig => "channels_config.linq",
    MatrixConfig => "channels_config.matrix",
    NextcloudTalkConfig => "channels_config.nextcloud_talk",
    QQConfig => "channels_config.qq",
    WacliConfig => "channels_config.wacli",
    WebhookConfig => "channels_config.webhook",
    WhatsAppConfig => "channels_config.whatsapp",
    SignalConfig => "channels_config.signal",
    MemoryWebhookConfig => "webhook",
    CloudflareTunnelConfig => "tunnel.cloudflare",
    NgrokTunnelConfig => "tunnel.ngrok",
    CustomTunnelConfig => "tunnel.custom",
    ChatDaemonConfig => "chat.daemon",
    RemoteNodeConfig => "nodes.nodes[]",
    NodeServerConfig => "nodes.server",
    ProxyConfig => "proxy",
    ObservabilityConfig => "observability",
    MediaConfig => "media",
    McpServerRuntimeConfig => "mcp_server",
    A2aConfig => "a2a",
    RouterModelConfig => "router.models[]",
    ProviderModelConfig => "providers.models[]",
    ProviderToolDialectConfig => "providers.tool_dialects[]",
    MemoryConfig => "memory",
    SmartGroupConfig => "smart_group",
    CostConfig => "cost",
}

// ── Config impl ──────────────────────────────────────────────────

/// The catalog's default provider, or `None` when the embedded catalog cannot
/// be read.
///
/// Shared by `#[serde(default)]` and `impl Default` so a `config.toml` that
/// names no provider resolves the same one either way.
fn default_provider_from_catalog() -> Option<String> {
    crate::providers::catalog::ReleaseCatalog::embedded_ref()
        .ok()
        .map(|catalog| catalog.default_provider().to_string())
}

/// The catalog's global default model, or `None` when the embedded catalog
/// cannot be read.
fn default_model_from_catalog() -> Option<String> {
    crate::providers::catalog::ReleaseCatalog::embedded_ref()
        .ok()
        .and_then(crate::providers::catalog::ReleaseCatalog::global_default_model)
        .map(str::to_string)
}

/// The default sampling temperature.
const fn default_temperature() -> f64 {
    0.7
}

impl Default for Config {
    fn default() -> Self {
        let home = user_home_dir().unwrap_or_else(|| PathBuf::from("."));
        let openprx_dir = preferred_user_config_dir(&home);

        Self {
            workspace_dir: openprx_dir.join("workspace"),
            config_path: openprx_dir.join("config.toml"),
            api_key: None,
            api_url: None,
            default_provider: default_provider_from_catalog(),
            default_model: default_model_from_catalog(),
            default_temperature: default_temperature(),
            model_settings: Default::default(),
            providers: ProvidersConfig::default(),
            observability: ObservabilityConfig::default(),
            autonomy: AutonomyConfig::default(),
            runtime: RuntimeConfig::default(),
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            agent: AgentConfig::default(),
            sessions_spawn: SessionsSpawnConfig::default(),
            chat: ChatConfig::default(),
            self_system: SelfSystemConfig::default(),
            skills: SkillsConfig::default(),
            skill_rag: SkillRagConfig::default(),
            plugins: crate::config::plugins::PluginsConfig::default(),
            skills_lifecycle: crate::config::skills_lifecycle::SkillsLifecycleConfig::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            task_routing: TaskRoutingConfig::default(),
            router: RouterConfig::default(),
            smart_group: SmartGroupConfig::default(),
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
            mcp_server: McpServerRuntimeConfig::default(),
            a2a: A2aConfig::default(),
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
            causal_tree: crate::causal_tree::CausalTreeConfig::default(),
            security: SecurityConfig::default(),
            compliance: ComplianceConfig::default(),
            tool_tiering: ToolTieringConfig::default(),
        }
    }
}
mod legacy;
#[cfg(test)]
use legacy::insert_toml_path_if_absent;
use legacy::migrate_legacy_config_keys;

mod providers;
pub use providers::*;

mod nodes;
pub use nodes::*;

mod agent;
pub use agent::*;

mod media;
pub use media::*;

mod cost;
pub use cost::*;

mod gateway;
pub use gateway::*;

mod integrations;
pub use integrations::*;

mod secrets;
pub use secrets::*;

mod tools;
pub use tools::*;

mod proxy;
pub use proxy::*;

mod memory;
pub use memory::*;

mod observability;
pub use observability::*;

mod autonomy;
pub use autonomy::*;

mod runtime;
pub use runtime::*;

mod routing;
pub use routing::*;

mod messaging;
pub use messaging::*;

mod tunnel;
pub use tunnel::*;

mod channels;
pub use channels::*;

mod security;
pub use security::*;

mod chat;
pub use chat::*;

mod paths;
pub use paths::*;

mod credentials;
pub use credentials::*;

mod lifecycle;

mod atomic;
pub(crate) use atomic::*;

#[cfg(test)]
mod tests;

/// The decision-plane configuration through the real load, validate and save
/// paths (CDP-03; SP-CDP-A 5.3 red-before rows; V8.06).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod causal_tree_config_tests;
