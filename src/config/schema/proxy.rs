use super::{Context, Deserialize, HashMap, JsonSchema, OnceLock, Result, RwLock, Serialize};

pub(super) const SUPPORTED_PROXY_SERVICE_KEYS: &[&str] = &[
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
    "decision.compatible_http",
];

/// Selectors of removed channels. An old config may still name them: they are
/// dropped with a warning at validation and never route traffic. Any other
/// unknown selector is still refused.
pub(super) const RETIRED_PROXY_SERVICE_SELECTORS: &[&str] = &["channel.matrix"];

pub(super) const SUPPORTED_PROXY_SERVICE_SELECTORS: &[&str] = &[
    "provider.*",
    "channel.*",
    "tool.*",
    "memory.*",
    "tunnel.*",
    "decision.*",
];

pub(super) static RUNTIME_PROXY_CONFIG: OnceLock<RwLock<ProxyConfig>> = OnceLock::new();
pub(super) static RUNTIME_PROXY_CLIENT_CACHE: OnceLock<RwLock<HashMap<String, reqwest::Client>>> = OnceLock::new();

// ── Proxy ───────────────────────────────────────────────────────

// PRX never writes the process environment (`HTTP_PROXY` and friends): a
// `setenv` next to a concurrent `getenv` in another thread is undefined
// behaviour, and no single-threaded startup path ever needed it. Runtime
// proxy changes go to the in-process state (`set_runtime_proxy_config`) that
// client builders read explicitly. `scope = "environment"` is therefore
// inactive: a saved configuration that has it still loads, with one warning
// (`ProxyConfig::ENVIRONMENT_SCOPE_INACTIVE`), and its URLs route no traffic. (Plain
// comments: the doc comments below are published in the config JSON schema.)

static ENVIRONMENT_SCOPE_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProxyConfig {
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
            if is_retired_proxy_service_selector(&selector) {
                tracing::warn!(
                    selector = %selector,
                    "Ignoring removed proxy service selector: Matrix support was removed; the selector can be deleted from proxy.services"
                );
                continue;
            }
            if !is_supported_proxy_service_selector(&selector) {
                anyhow::bail!(
                    "Unsupported proxy service selector '{selector}'. Use tool `proxy_config` action `list_services` for valid values"
                );
            }
        }

        if self.has_any_proxy_url() && self.scope == ProxyScope::Services && self.normalized_services().is_empty() {
            anyhow::bail!("proxy.scope='services' requires a non-empty proxy.services list");
        }

        // An old configuration with the environment scope must still load:
        // it is reported once per process, never refused.
        if let Some(notice) = self.environment_scope_notice()
            && !ENVIRONMENT_SCOPE_WARNED.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            tracing::warn!("{notice}");
        }

        Ok(())
    }

    /// What an operator is told when a loaded configuration still has
    /// `proxy.scope = "environment"`, and what the `proxy_config` tool
    /// answers when asked to set it.
    pub const ENVIRONMENT_SCOPE_INACTIVE: &str = "proxy.scope = \"environment\" is inactive: PRX never writes the \
         process environment, so the configured proxy URLs route no traffic. Migrate to proxy.scope = \"zeroclaw\" \
         (all PRX traffic; tool `proxy_config` scope 'prx') or to proxy.scope = \"services\" with a proxy.services \
         list; to keep using inherited variables, start PRX with HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY already set";

    /// The migration notice of a configuration whose scope is `environment`
    /// (inactive since PRX stopped writing the process environment), and
    /// `None` for every other scope.
    pub const fn environment_scope_notice(&self) -> Option<&'static str> {
        match self.scope {
            ProxyScope::Environment => Some(Self::ENVIRONMENT_SCOPE_INACTIVE),
            ProxyScope::Zeroclaw | ProxyScope::Services => None,
        }
    }

    /// Whether the operator selected `service_key` for proxying by name:
    /// the scope is `services` and a `proxy.services` selector matches the
    /// key. A proxy that merely applies to everything (scope `zeroclaw`) or
    /// that is inherited from the environment is not an explicit selection.
    pub fn explicitly_selects_service(&self, service_key: &str) -> bool {
        self.scope == ProxyScope::Services && self.should_apply_to_service(service_key)
    }

    /// The proxy settings of a client that connects to an address PRX has
    /// checked and pinned (`http_request`, `web_fetch`). Through a proxy the
    /// proxy resolves the target name, so the pin would not bind the address
    /// that is finally connected to. Such a client therefore connects
    /// directly: inherited environment proxies and a general proxy are
    /// switched off. Only an explicit selection of `service_key`
    /// ([`Self::explicitly_selects_service`]) routes it through the proxy;
    /// the proxy must then enforce its own egress rules.
    pub fn apply_explicit_selection_to_reqwest_builder(
        &self,
        builder: reqwest::ClientBuilder,
        service_key: &str,
    ) -> reqwest::ClientBuilder {
        let direct = builder.no_proxy();
        if self.explicitly_selects_service(service_key) {
            self.apply_to_reqwest_builder(direct, service_key)
        } else {
            direct
        }
    }

    pub fn should_apply_to_service(&self, service_key: &str) -> bool {
        if !self.has_any_proxy_url() {
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
                    .filter(|selector| !is_retired_proxy_service_selector(selector))
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

    pub(super) fn no_proxy_value(&self) -> Option<reqwest::NoProxy> {
        let joined = {
            let list = self.normalized_no_proxy();
            (!list.is_empty()).then(|| list.join(","))
        };
        joined.as_deref().and_then(reqwest::NoProxy::from_string)
    }
}

pub(super) fn apply_no_proxy(proxy: reqwest::Proxy, no_proxy: Option<reqwest::NoProxy>) -> reqwest::Proxy {
    proxy.no_proxy(no_proxy)
}

/// Redact credentials (user:pass) from a proxy URL before logging.
///
/// Turns `http://user:pass@host:port/path` into `http://[REDACTED]@host:port/path`.
/// Returns the original string unchanged if no credentials are present.
pub(super) fn redact_url_credentials(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(scheme_end) = url.find("://") {
            let scheme = &url[..scheme_end + 3];
            let after_at = &url[at_pos + 1..];
            return format!("{scheme}[REDACTED]@{after_at}");
        }
    }
    url.to_string()
}

pub(super) fn normalize_proxy_url_option(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub(super) fn normalize_no_proxy_list(values: Vec<String>) -> Vec<String> {
    normalize_comma_values(values)
}

pub(super) fn normalize_service_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = normalize_comma_values(values)
        .into_iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

pub(super) fn normalize_comma_values(values: Vec<String>) -> Vec<String> {
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

pub(super) fn is_retired_proxy_service_selector(selector: &str) -> bool {
    RETIRED_PROXY_SERVICE_SELECTORS
        .iter()
        .any(|retired| retired.eq_ignore_ascii_case(selector))
}

pub(super) fn is_supported_proxy_service_selector(selector: &str) -> bool {
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

pub(super) fn service_selector_matches(selector: &str, service_key: &str) -> bool {
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

pub(super) fn validate_proxy_url(field: &str, url: &str) -> Result<()> {
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

pub(super) fn runtime_proxy_state() -> &'static RwLock<ProxyConfig> {
    RUNTIME_PROXY_CONFIG.get_or_init(|| RwLock::new(ProxyConfig::default()))
}

pub(super) fn runtime_proxy_client_cache() -> &'static RwLock<HashMap<String, reqwest::Client>> {
    RUNTIME_PROXY_CLIENT_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

pub(super) fn clear_runtime_proxy_client_cache() {
    runtime_proxy_client_cache().write().clear();
}

pub(super) fn runtime_proxy_cache_key(
    service_key: &str,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
) -> String {
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

pub(super) fn runtime_proxy_cached_client(cache_key: &str) -> Option<reqwest::Client> {
    runtime_proxy_client_cache().read().get(cache_key).cloned()
}

pub(super) fn set_runtime_proxy_cached_client(cache_key: String, client: reqwest::Client) {
    runtime_proxy_client_cache().write().insert(cache_key, client);
}

pub(crate) fn set_runtime_proxy_config(config: ProxyConfig) {
    *runtime_proxy_state().write() = config;
    clear_runtime_proxy_client_cache();
    RUNTIME_PROXY_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Bumped by every [`set_runtime_proxy_config`]: a client cache outside
/// this module keys its clients by it, so a proxy change rebuilds them.
pub(super) static RUNTIME_PROXY_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The runtime proxy generation, bumped by every runtime proxy change.
pub fn runtime_proxy_generation() -> u64 {
    RUNTIME_PROXY_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn runtime_proxy_config() -> ProxyConfig {
    runtime_proxy_state().read().clone()
}

pub fn apply_runtime_proxy_to_builder(builder: reqwest::ClientBuilder, service_key: &str) -> reqwest::ClientBuilder {
    runtime_proxy_config().apply_to_reqwest_builder(builder.user_agent(crate::PRX_USER_AGENT), service_key)
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
