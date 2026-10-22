use super::{Deserialize, HashMap, JsonSchema, Serialize, default_true};

// ── Composio (managed tool surface) ─────────────────────────────

/// Composio managed OAuth tools integration (`[composio]` section).
///
/// Provides access to 1000+ OAuth-connected tools via the Composio platform.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposioConfig {
    /// Composio API key (stored encrypted when secrets.encrypt = true)
    #[serde(default)]
    pub api_key: Option<String>,
    /// Default entity ID for multi-user setups
    #[serde(default = "default_entity_id")]
    pub entity_id: String,
}

pub(super) fn default_entity_id() -> String {
    "default".into()
}

impl Default for ComposioConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            entity_id: default_entity_id(),
        }
    }
}

impl ComposioConfig {
    pub fn configured(&self) -> bool {
        self.api_key.as_deref().is_some_and(|value| !value.trim().is_empty())
    }
}

// ── MCP (Model Context Protocol) ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpConfig {
    /// Named MCP server definitions
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
    /// Which new servers a workspace `mcp.json` may add (D-085 F-1). A
    /// workspace file may always narrow a server defined here (leave it out,
    /// or expose fewer of its tools); anything else it says about such a
    /// server is ignored. `off` (the default) adds nothing, `http_only` adds
    /// HTTP servers, `stdio` adds HTTP and stdio servers (a stdio server is a
    /// program run as PRX's user). Widening this is a high-risk change.
    #[serde(default, skip_serializing_if = "WorkspaceMcpServers::is_off")]
    pub workspace_servers: WorkspaceMcpServers,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: HashMap::new(),
            workspace_servers: WorkspaceMcpServers::Off,
        }
    }
}

/// Which new MCP servers a workspace `mcp.json` may add
/// (`mcp.workspace_servers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMcpServers {
    /// None: a workspace file may only narrow the servers defined in
    /// `config.toml`.
    #[default]
    Off,
    /// HTTP servers (still refused when they target a private address).
    HttpOnly,
    /// HTTP servers and stdio servers started with a bare launcher from the
    /// fixed list, in the cleared launch environment.
    Stdio,
}

impl WorkspaceMcpServers {
    /// Whether this is the default (`off`).
    #[must_use]
    pub fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

impl McpConfig {
    pub fn configured(&self) -> bool {
        !self.servers.is_empty()
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpServerRuntimeConfig {
    /// Advertised bind address. The gateway may still be started with a CLI port override.
    #[serde(default = "default_mcp_server_bind")]
    pub bind: String,
    /// Require an authenticated external identity for mutating calls.
    #[serde(default = "default_true")]
    pub require_auth: bool,
    /// Tools exposed to external MCP callers.
    ///
    /// The defaults are all read-only. Every external MCP caller runs
    /// owner-isolated, so only names on the owner-isolation-aware list
    /// (`crate::causal_tree::capability_policy::OWNER_ISOLATION_AWARE_TOOLS`) can run; any
    /// other name is not advertised, is refused with HTTP 403, and is warned
    /// about at config load. An admissible tool with side effects
    /// (`memory_forget`, `memory_store`, `document_ingest`) is advertised with its real effect by
    /// `GET /mcp/v1/tools/list`, and a retry-safe call of it needs an
    /// `Idempotency-Key`.
    #[serde(default = "default_mcp_server_exposed_tools")]
    pub exposed_tools: Vec<String>,
    /// Allowed browser origins for future streamable HTTP clients.
    #[serde(default = "default_mcp_allowed_origins")]
    pub allowed_origins: Vec<String>,
    /// Per-caller request budget, in requests per second, enforced on
    /// `POST /mcp/v1/tools/call` and keyed by the authenticated owner id.
    /// `0` disables the budget.
    #[serde(default = "default_mcp_server_rate_limit_rps")]
    pub rate_limit_rps: u32,
    /// Expected `iss` claim for inbound bearer JWTs. When unset, bearer JWT
    /// verification is disabled and bearer tokens are rejected.
    #[serde(default)]
    pub jwt_issuer: Option<String>,
    /// Expected `aud` claim for inbound bearer JWTs. When set, the token must
    /// carry a matching audience.
    #[serde(default)]
    pub jwt_audience: Option<String>,
    /// Inline JWKS document (RFC 7517 JSON) used to resolve signing keys by
    /// `kid`. Takes precedence over `jwt_jwks_uri` and `jwt_public_key_pem`.
    #[serde(default)]
    pub jwt_jwks: Option<String>,
    /// Remote JWKS endpoint (HTTPS URL) whose document is fetched and cached to
    /// resolve signing keys by `kid`. Used when no inline `jwt_jwks` is set.
    /// Fetching is fail-closed: a network failure with no still-valid cached
    /// copy rejects the token rather than accepting it.
    #[serde(default)]
    pub jwt_jwks_uri: Option<String>,
    /// Cache lifetime (seconds) for a fetched remote JWKS document. After this
    /// the document is refreshed on the next verification. Defaults to 300s.
    #[serde(default = "default_mcp_jwks_cache_ttl_secs")]
    pub jwt_jwks_cache_ttl_secs: u64,
    /// PEM-encoded public key (RSA/EC/Ed) used when no JWKS is configured.
    #[serde(default)]
    pub jwt_public_key_pem: Option<String>,
    /// Allowed JWT signing algorithms. Symmetric (`HS*`) and `none` are never
    /// accepted; only asymmetric algorithms in this list pass verification.
    #[serde(default = "default_mcp_jwt_algorithms")]
    pub jwt_algorithms: Vec<String>,
}

pub(super) fn default_mcp_jwt_algorithms() -> Vec<String> {
    vec!["RS256".to_string()]
}

pub(super) const fn default_mcp_jwks_cache_ttl_secs() -> u64 {
    300
}

pub(super) fn default_mcp_server_bind() -> String {
    "127.0.0.1:16831".into()
}

pub(super) fn default_mcp_server_exposed_tools() -> Vec<String> {
    [
        "memory_search",
        "memory_recall",
        "memory_get",
        "document_search",
        "document_get_chunk",
        "sessions_list",
        "sessions_history",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub(super) fn default_mcp_allowed_origins() -> Vec<String> {
    ["claude.ai", "openai.com", "localhost"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub(super) const fn default_mcp_server_rate_limit_rps() -> u32 {
    10
}

impl McpServerRuntimeConfig {
    /// Exposed tools that are not read-only.
    ///
    /// Exposing one is a deliberate operator action rather than a config error:
    /// the gateway MCP endpoint drives every call through the shared execution
    /// service, so such a tool is still policy-gated and, with an
    /// `Idempotency-Key`, ledger-protected. Approval is consulted only at
    /// `AutonomyLevel::Supervised` (and is then fail-closed, since a remote
    /// client cannot answer a prompt); at the default `Full` it is not, so for
    /// external callers the boundary is the owner-isolation-aware list — see
    /// [`Self::refused_exposed_tools`].
    #[must_use]
    pub fn side_effecting_exposed_tools(&self) -> Vec<&str> {
        self.exposed_tools
            .iter()
            .map(String::as_str)
            .filter(|name| !crate::security::policy::is_read_only_tool(name))
            .collect()
    }

    /// Exposed tools an external MCP caller can run: those on the
    /// owner-isolation-aware list, in configured order.
    #[must_use]
    pub fn admissible_exposed_tools(&self) -> Vec<&str> {
        self.exposed_tools
            .iter()
            .map(String::as_str)
            .filter(|name| crate::tools::execution::is_owner_isolation_aware_tool(name))
            .collect()
    }

    /// Exposed tools every external MCP caller is refused because they do not
    /// enforce per-owner isolation (V02-R9 / OQ-21). Such an entry is inert: it
    /// is not advertised and a call of it answers 403.
    #[must_use]
    pub fn refused_exposed_tools(&self) -> Vec<&str> {
        self.exposed_tools
            .iter()
            .map(String::as_str)
            .filter(|name| !crate::tools::execution::is_owner_isolation_aware_tool(name))
            .collect()
    }
}

impl Default for McpServerRuntimeConfig {
    fn default() -> Self {
        Self {
            bind: default_mcp_server_bind(),
            require_auth: true,
            exposed_tools: default_mcp_server_exposed_tools(),
            allowed_origins: default_mcp_allowed_origins(),
            rate_limit_rps: default_mcp_server_rate_limit_rps(),
            jwt_issuer: None,
            jwt_audience: None,
            jwt_jwks: None,
            jwt_jwks_uri: None,
            jwt_jwks_cache_ttl_secs: default_mcp_jwks_cache_ttl_secs(),
            jwt_public_key_pem: None,
            jwt_algorithms: default_mcp_jwt_algorithms(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct A2aConfig {
    /// Advertised bind address.
    #[serde(default = "default_a2a_bind")]
    pub bind: String,
    /// Stable PRX agent identifier advertised to peers.
    #[serde(default = "default_a2a_agent_id")]
    pub agent_id: String,
    /// Local SPIFFE-style identifier placeholder.
    #[serde(default = "default_a2a_spiffe_id")]
    pub spiffe_id: String,
    /// Whether to advertise into an external A2A registry.
    #[serde(default)]
    pub discovery_advertised: bool,
    /// Accepted external peer issuers.
    #[serde(default)]
    pub allowed_peer_issuers: Vec<String>,
    /// Replay window for future handoff delegation tokens.
    #[serde(default = "default_a2a_handoff_nonce_ttl_seconds")]
    pub handoff_nonce_ttl_seconds: u64,
    /// On-disk Ed25519 signing key for the published Agent Card (JWS). When set,
    /// the `/.well-known/agent.json` card is signed and verifiable via the JWKS
    /// endpoint. When unset, an ephemeral per-process key is used (card is still
    /// signed, but the public key only lives for the lifetime of the process).
    #[serde(default)]
    pub card_signing_key_path: Option<String>,
    /// Validity window of a published Agent Card, in seconds. Reflected in the
    /// card's `expires_at` and the response `Cache-Control: max-age`.
    #[serde(default = "default_a2a_card_ttl_seconds")]
    pub card_ttl_seconds: u64,
    /// Public JWKS URI advertised in the card signature block so peers know where
    /// to fetch the verification key. Defaults to the local JWKS endpoint.
    #[serde(default)]
    pub card_jwks_uri: Option<String>,
    /// Trust domains accepted when verifying a peer SPIFFE SVID. Empty means the
    /// `allowed_peer_issuers` allow-list is the sole gate (backward compatible).
    #[serde(default)]
    pub trusted_trust_domains: Vec<String>,
    /// Trust bundle CA certificate(s) in PEM, used to verify the signature chain
    /// of a peer-presented X.509 SVID. When unset, SVID chain verification is not
    /// performed (only header-asserted issuer allow-listing applies).
    #[serde(default)]
    pub spiffe_trust_bundle_pem: Option<String>,
    /// When true, a peer that presents an X.509 SVID header but fails chain /
    /// validity / trust-domain verification is rejected. When false (default),
    /// verification failures are logged but fall back to header-asserted trust
    /// (suitable for a trusted front proxy terminating mTLS).
    ///
    /// For MCP tool calls, SPIFFE headers (and the fall-back) are considered
    /// only when `gateway.trust_forwarded_headers` is true; otherwise they are
    /// ignored and the caller must authenticate with a bearer JWT.
    #[serde(default)]
    pub spiffe_strict_validation: bool,
}

pub(super) fn default_a2a_bind() -> String {
    "127.0.0.1:16832".into()
}

pub(super) fn default_a2a_agent_id() -> String {
    "prx-default".into()
}

pub(super) fn default_a2a_spiffe_id() -> String {
    "spiffe://prx-local/agent/prx-default".into()
}

pub(super) const fn default_a2a_handoff_nonce_ttl_seconds() -> u64 {
    3_600
}

pub(super) const fn default_a2a_card_ttl_seconds() -> u64 {
    3_600
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            bind: default_a2a_bind(),
            agent_id: default_a2a_agent_id(),
            spiffe_id: default_a2a_spiffe_id(),
            discovery_advertised: false,
            allowed_peer_issuers: Vec::new(),
            handoff_nonce_ttl_seconds: default_a2a_handoff_nonce_ttl_seconds(),
            card_signing_key_path: None,
            card_ttl_seconds: default_a2a_card_ttl_seconds(),
            card_jwks_uri: None,
            trusted_trust_domains: Vec::new(),
            spiffe_trust_bundle_pem: None,
            spiffe_strict_validation: false,
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

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpServerConfig {
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
    /// Working directory of a stdio server (absolute). Unset: a dedicated
    /// empty directory `mcp/<server>` beside `config.toml`, never the
    /// directory PRX was started from (D-088 C-3). Changing it is a launch
    /// change (high risk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

pub(super) const fn default_mcp_startup_timeout_ms() -> u64 {
    10_000
}

pub(super) const fn default_mcp_request_timeout_ms() -> u64 {
    30_000
}

pub(super) fn default_mcp_tool_name_prefix() -> String {
    "mcp".into()
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
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
            working_dir: None,
        }
    }
}
