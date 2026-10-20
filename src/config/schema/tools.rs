use super::{Deserialize, JsonSchema, Serialize};

// ── Browser (friendly-service browsing only) ───────────────────

/// Computer-use sidecar configuration (`[browser.computer_use]` section).
///
/// The computer-use / agent-browser automation tools were removed (agents use
/// `shell` instead). Only `api_key` is retained, because the config secret
/// encryption/migration path still references it.
#[derive(Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct BrowserComputerUseConfig {
    /// Optional bearer token (retained only for config secret handling).
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Browser configuration (`[browser]` section).
///
/// The `browser` / `browser_open` automation tools were removed. This section
/// now only carries `allowed_domains`, consumed by the `web_fetch` tool for
/// domain validation; `computer_use.api_key` is kept for config secret handling.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct BrowserConfig {
    /// Allowed domains for `web_fetch` (exact or subdomain match).
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Computer-use sidecar config (retained only for `api_key` secret handling).
    #[serde(default)]
    pub computer_use: BrowserComputerUseConfig,
}

// ── HTTP request tool ───────────────────────────────────────────

/// HTTP request tool configuration (`[http_request]` section).
///
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HttpRequestConfig {
    /// Maximum response size in bytes (default: 1MB)
    #[serde(default = "default_http_max_response_size")]
    pub max_response_size: usize,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_http_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for HttpRequestConfig {
    fn default() -> Self {
        Self {
            max_response_size: default_http_max_response_size(),
            timeout_secs: default_http_timeout_secs(),
        }
    }
}

pub(super) const fn default_http_max_response_size() -> usize {
    1_000_000 // 1MB
}

pub(super) const fn default_http_timeout_secs() -> u64 {
    30
}

// ── Web search ───────────────────────────────────────────────────

/// Web search tool configuration (`[web_search]` section).
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebSearchConfig {
    /// Search provider: "duckduckgo" (free, no API key) or "brave" (requires API key)
    #[serde(default = "default_web_search_provider")]
    pub provider: String,
    /// Brave Search API key (required if provider is "brave")
    #[serde(default)]
    pub brave_api_key: Option<String>,
    /// Maximum results per search (1-20)
    #[serde(default = "default_web_search_max_results")]
    pub max_results: usize,
    /// Request timeout in seconds
    #[serde(default = "default_web_search_timeout_secs")]
    pub timeout_secs: u64,
    /// Maximum characters returned by `web_fetch` (default 10000)
    #[serde(default = "default_web_fetch_max_chars")]
    pub fetch_max_chars: usize,
}

pub(super) fn default_web_search_provider() -> String {
    "duckduckgo".into()
}

pub(super) const fn default_web_search_max_results() -> usize {
    // Behavior-limits Phase 1: raised 5 -> 15 to return more search results.
    15
}

pub(super) const fn default_web_search_timeout_secs() -> u64 {
    15
}

pub(super) const fn default_web_fetch_max_chars() -> usize {
    // Behavior-limits Phase 1: raised 10K -> 50K so web_fetch returns more of a
    // page. Still a finite OOM guard (not removed).
    50_000
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: default_web_search_provider(),
            brave_api_key: None,
            max_results: default_web_search_max_results(),
            timeout_secs: default_web_search_timeout_secs(),
            fetch_max_chars: default_web_fetch_max_chars(),
        }
    }
}
