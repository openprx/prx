use super::{Deserialize, JsonSchema, Serialize};

/// Remote node proxy configuration for core-side RPC calls and node daemon defaults.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NodesConfig {
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

pub(super) const fn default_nodes_timeout_ms() -> u64 {
    15_000
}

pub(super) const fn default_nodes_retry_max() -> u8 {
    2
}

impl Default for NodesConfig {
    fn default() -> Self {
        Self {
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
    /// Optional per-node timeout override.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Optional per-node retry override.
    #[serde(default, alias = "max_retries")]
    pub retry_max: Option<u8>,
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
    /// Maximum combined stdout/stderr bytes captured per command and maximum
    /// file read/write payload accepted by the node RPC server.
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

pub(super) fn default_node_server_listen_addr() -> String {
    "127.0.0.1:8787".to_string()
}

pub(super) fn default_node_server_sandbox_root() -> String {
    ".".to_string()
}

pub(super) const fn default_node_server_exec_timeout_ms() -> u64 {
    15_000
}

pub(super) const fn default_node_server_max_output_bytes() -> usize {
    1_048_576
}

pub(super) const fn default_node_server_max_concurrent_tasks() -> usize {
    8
}

pub(super) const fn default_node_server_task_result_ttl_ms() -> u64 {
    3_600_000
}

pub(super) fn default_node_server_allowed_commands() -> Vec<String> {
    vec!["echo".to_string()]
}

pub(super) const fn default_node_server_tls_required() -> bool {
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
