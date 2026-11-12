use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// The proxy service key of this tool (`proxy.services`).
const PROXY_SERVICE_KEY: &str = "tool.http_request";

/// HTTP request tool for API interactions.
/// Supports GET, POST, PUT, DELETE methods with configurable security.
pub struct HttpRequestTool {
    allowed_domains: Vec<String>,
    max_response_size: usize,
    timeout_secs: u64,
}

impl HttpRequestTool {
    pub fn new(
        _security: Arc<crate::security::SecurityPolicy>,
        allowed_domains: Vec<String>,
        max_response_size: usize,
        timeout_secs: u64,
    ) -> Self {
        Self {
            allowed_domains: super::web_fetch::normalize_allowed_domains(allowed_domains),
            max_response_size: max_response_size.max(1),
            timeout_secs: timeout_secs.max(1),
        }
    }

    fn validate_url(&self, raw_url: &str) -> anyhow::Result<String> {
        let url = raw_url.trim();

        if url.is_empty() {
            anyhow::bail!("URL cannot be empty");
        }

        if url.chars().any(char::is_whitespace) {
            anyhow::bail!("URL cannot contain whitespace");
        }

        if !url.starts_with("http://") && !url.starts_with("https://") {
            anyhow::bail!("Only http:// and https:// URLs are allowed");
        }

        let host = extract_host(url)?;
        if is_private_or_local_host(&host) {
            anyhow::bail!("Blocked local/private host: {host}");
        }
        if !self.allowed_domains.is_empty() && !super::web_fetch::host_matches_allowlist(&host, &self.allowed_domains) {
            anyhow::bail!("Host '{host}' is not in browser.allowed_domains");
        }

        Ok(url.to_string())
    }

    fn validate_method(&self, method: &str) -> anyhow::Result<reqwest::Method> {
        match method.to_uppercase().as_str() {
            "GET" => Ok(reqwest::Method::GET),
            "POST" => Ok(reqwest::Method::POST),
            "PUT" => Ok(reqwest::Method::PUT),
            "DELETE" => Ok(reqwest::Method::DELETE),
            "PATCH" => Ok(reqwest::Method::PATCH),
            "HEAD" => Ok(reqwest::Method::HEAD),
            "OPTIONS" => Ok(reqwest::Method::OPTIONS),
            _ => anyhow::bail!(
                "Unsupported HTTP method: {method}. Supported: GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS"
            ),
        }
    }

    fn parse_headers(&self, headers: &serde_json::Value) -> Vec<(String, String)> {
        let mut result = Vec::new();
        if let Some(obj) = headers.as_object() {
            for (key, value) in obj {
                if let Some(str_val) = value.as_str() {
                    result.push((key.clone(), str_val.to_string()));
                }
            }
        }
        result
    }

    #[allow(clippy::indexing_slicing)]
    #[cfg(test)]
    fn redact_headers_for_display(headers: &[(String, String)]) -> Vec<(String, String)> {
        headers
            .iter()
            .map(|(key, value)| {
                let lower = key.to_lowercase();
                let is_sensitive = lower.contains("authorization")
                    || lower.contains("api-key")
                    || lower.contains("apikey")
                    || lower.contains("token")
                    || lower.contains("secret");
                if is_sensitive {
                    (key.clone(), "***REDACTED***".into())
                } else {
                    (key.clone(), value.clone())
                }
            })
            .collect()
    }

    async fn execute_request(
        &self,
        url: &str,
        method: reqwest::Method,
        headers: Vec<(String, String)>,
        body: Option<&str>,
    ) -> anyhow::Result<reqwest::Response> {
        // Resolve once, refuse any non-global address, and pin the checked
        // addresses for this host: the client must not resolve the name a
        // second time (DNS rebinding). Redirects are never followed, so this
        // is the only hop.
        let target = resolve_pinned_target(url).await?;
        let client = self.client_for(&target, &crate::config::runtime_proxy_config())?;

        let mut request = client.request(method, url);

        for (key, value) in headers {
            request = request.header(&key, &value);
        }

        if let Some(body_str) = body {
            request = request.body(body_str.to_string());
        }

        Ok(request.send().await?)
    }

    /// The client of one request to a checked target: pinned to the checked
    /// addresses, never following redirects, and connecting directly. A
    /// general or inherited proxy is switched off, because a proxy resolves
    /// the name itself and the pin would not bind the final address. Only an
    /// operator who lists this tool in `proxy.services` (scope `services`)
    /// routes it through the proxy; the local checks have run all the same.
    fn client_for(
        &self,
        target: &PinnedTarget,
        proxy: &crate::config::ProxyConfig,
    ) -> reqwest::Result<reqwest::Client> {
        let builder = target.pin(
            reqwest::Client::builder()
                .user_agent(crate::PRX_USER_AGENT)
                .timeout(Duration::from_secs(self.timeout_secs))
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none()),
        );
        proxy
            .apply_explicit_selection_to_reqwest_builder(builder, PROXY_SERVICE_KEY)
            .build()
    }

    async fn read_bounded_response(&self, mut response: reqwest::Response) -> anyhow::Result<String> {
        let mut bytes = Vec::with_capacity(self.max_response_size.min(64 * 1024));
        let mut truncated = false;

        while let Some(chunk) = response.chunk().await? {
            let remaining = self.max_response_size.saturating_sub(bytes.len());
            if chunk.len() > remaining {
                if let Some(prefix) = chunk.get(..remaining) {
                    bytes.extend_from_slice(prefix);
                }
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
            if bytes.len() == self.max_response_size {
                truncated = true;
                break;
            }
        }

        Ok(Self::format_response_bytes(&bytes, truncated))
    }

    fn format_response_bytes(bytes: &[u8], truncated: bool) -> String {
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        if truncated {
            text.push_str("\n\n... [Response truncated due to size limit] ...");
        }
        text
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn description(&self) -> &str {
        "Make unrestricted HTTP requests. Supports GET, POST, PUT, DELETE, PATCH, HEAD, and OPTIONS methods. \
         Connects directly to the checked public address of the host (private and local addresses are refused, \
         redirects are not followed); a proxy is used only when the operator lists tool.http_request in proxy.services."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "url": {
                    "type": "string",
                    "description": "HTTP or HTTPS URL to request"
                },
                "method": {
                    "type": "string",
                    "enum": ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"],
                    "description": "HTTP method (GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS)",
                    "default": "GET"
                },
                "headers": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "Optional HTTP headers as key-value pairs (e.g., {\"Authorization\": \"Bearer token\", \"Content-Type\": \"application/json\"})",
                    "default": {}
                },
                "body": {
                    "type": "string",
                    "description": "Optional request body (for POST, PUT, PATCH requests)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'url' parameter"))?;

        let method_str = args.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
        let headers_val = args.get("headers").cloned().unwrap_or_else(|| json!({}));
        let body = args.get("body").and_then(|v| v.as_str());

        let url = match self.validate_url(url) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let method = match self.validate_method(method_str) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };
        let request_headers = self.parse_headers(&headers_val);

        match self.execute_request(&url, method, request_headers, body).await {
            Ok(response) => {
                let status = response.status();
                let status_code = status.as_u16();

                // Get response headers (redact sensitive ones)
                let response_headers = response.headers().iter();
                let headers_text = response_headers
                    .map(|(k, v)| {
                        let lower = k.as_str().to_lowercase();
                        let is_sensitive = lower.contains("cookie")
                            || lower.contains("authorization")
                            || lower.contains("x-api-key")
                            || lower.contains("x-auth-token")
                            || lower.contains("proxy-auth")
                            || lower.contains("www-authenticate")
                            || lower.contains("x-csrf")
                            || lower.contains("x-xsrf");
                        if is_sensitive {
                            format!("{}: ***REDACTED***", k.as_str())
                        } else {
                            format!("{}: {:?}", k.as_str(), v.to_str().unwrap_or("<non-utf8>"))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");

                // Get response body with size limit
                let response_text = match self.read_bounded_response(response).await {
                    Ok(text) => text,
                    Err(e) => format!("[Failed to read response body: {e}]"),
                };

                let output = format!(
                    "Status: {} {}\nResponse Headers: {}\n\nResponse Body:\n{}",
                    status_code,
                    status.canonical_reason().unwrap_or("Unknown"),
                    headers_text,
                    response_text
                );

                Ok(ToolResult {
                    success: status.is_success(),
                    output,
                    error: if status.is_client_error() || status.is_server_error() {
                        Some(format!("HTTP {}", status_code))
                    } else {
                        None
                    },
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("HTTP request failed: {e}")),
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::WebBrowsing]
    }
}

// Helper functions for HTTP domain validation

pub(crate) fn extract_host(url: &str) -> anyhow::Result<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| anyhow::anyhow!("Only http:// and https:// URLs are allowed"))?;

    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .ok_or_else(|| anyhow::anyhow!("Invalid URL"))?;

    if authority.is_empty() {
        anyhow::bail!("URL must include a host");
    }

    if authority.contains('@') {
        anyhow::bail!("URL userinfo is not allowed");
    }

    if authority.starts_with('[') {
        anyhow::bail!("IPv6 hosts are not supported in http_request");
    }

    let host = authority
        .split(':')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_lowercase();

    if host.is_empty() {
        anyhow::bail!("URL must include a valid host");
    }

    Ok(host)
}

pub(crate) fn is_private_or_local_host(host: &str) -> bool {
    // Strip brackets from IPv6 addresses like [::1]
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);

    let has_local_tld = bare.rsplit('.').next().is_some_and(|label| label == "local");

    if bare == "localhost" || bare.ends_with(".localhost") || has_local_tld {
        return true;
    }

    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => is_non_global_v4(v4),
            std::net::IpAddr::V6(v6) => is_non_global_v6(v6),
        };
    }
    if let Some(ip) = parse_legacy_ipv4(bare) {
        return is_non_global_v4(ip);
    }

    // DNS rebinding defense: resolve the hostname and check if any resolved
    // IP is private/local. This catches hostnames that pass string checks
    // but resolve to internal addresses (e.g. attacker-controlled DNS that
    // alternates between public and private IPs).
    resolves_to_non_global(host)
}

/// Whether any address `host` resolves to is loopback, unspecified or
/// otherwise not globally routable. A name that does not resolve is not
/// reported here (the request then fails to connect).
pub(crate) fn resolves_to_non_global(host: &str) -> bool {
    resolved_addresses(host).into_iter().any(is_non_global_ip)
}

/// Whether `ip` is loopback, unspecified or otherwise not globally routable.
pub(crate) fn is_non_global_ip(ip: std::net::IpAddr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || match ip {
            std::net::IpAddr::V4(v4) => is_non_global_v4(v4),
            std::net::IpAddr::V6(v6) => is_non_global_v6(v6),
        }
}

/// Why a request target could not be pinned to a checked address.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PinError {
    #[error("Invalid request URL: {0}")]
    InvalidUrl(String),
    #[error("DNS resolution failed for {host}: {source}")]
    Lookup {
        host: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Host {host} did not resolve to any address")]
    Unresolved { host: String },
    #[error("Blocked local/private host: {host} resolves to {address}")]
    NonGlobal { host: String, address: std::net::IpAddr },
}

/// A request host together with the addresses it resolved to, every one of
/// them checked to be globally routable. A client built through [`Self::pin`]
/// connects to exactly these addresses and never resolves the name again, so
/// a hostile DNS server cannot answer public for the check and private for
/// the connection (DNS rebinding). TLS SNI and the `Host` header keep the
/// name. Same pattern as `media::artifact::prepare_remote_target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinnedTarget {
    pub(crate) host: String,
    pub(crate) addrs: Vec<std::net::SocketAddr>,
}

impl PinnedTarget {
    /// The target of `host` when it resolved to `addrs`: refused when the
    /// list is empty or when any address is not globally routable (one
    /// private address among public ones is enough to refuse).
    pub(crate) fn from_resolved(host: &str, addrs: Vec<std::net::SocketAddr>) -> Result<Self, PinError> {
        if addrs.is_empty() {
            return Err(PinError::Unresolved { host: host.to_string() });
        }
        if let Some(blocked) = addrs.iter().find(|addr| is_non_global_ip(addr.ip())) {
            return Err(PinError::NonGlobal {
                host: host.to_string(),
                address: blocked.ip(),
            });
        }
        Ok(Self {
            host: host.to_string(),
            addrs,
        })
    }

    /// `builder` with this host pinned to the checked addresses.
    pub(crate) fn pin(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder.resolve_to_addrs(&self.host, &self.addrs)
    }
}

/// Resolve the host of `url` once and return it pinned to the checked
/// addresses. The host is the one `reqwest` itself parses from the URL, so
/// the name that is checked is the name that is connected to. One hop only:
/// a caller that follows redirects calls this again for every hop.
pub(crate) async fn resolve_pinned_target(url: &str) -> Result<PinnedTarget, PinError> {
    let parsed = reqwest::Url::parse(url).map_err(|error| PinError::InvalidUrl(error.to_string()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| PinError::InvalidUrl("missing host".to_string()))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| PinError::InvalidUrl("missing port".to_string()))?;
    let addrs = lookup_host(host, port).await.map_err(|source| PinError::Lookup {
        host: host.to_string(),
        source,
    })?;
    PinnedTarget::from_resolved(host, addrs)
}

/// Resolve `host:port` for a connection, through the system resolver. Every
/// production lookup of a user-supplied host (media downloads, node
/// callbacks, the doctor's gateway probe) goes through this, so under test it
/// reads the fixed table instead and never touches the network (INT-2B
/// FLAKE-DNS).
pub(crate) async fn lookup_host(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    let host = unbracketed(host);
    #[cfg(not(test))]
    {
        tokio::net::lookup_host((host, port)).await.map(|addrs| addrs.collect())
    }
    #[cfg(test)]
    {
        std::future::ready(test_lookup_host(host, port)).await
    }
}

/// [`lookup_host`] for synchronous callers.
pub(crate) fn lookup_host_blocking(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    let host = unbracketed(host);
    #[cfg(not(test))]
    {
        std::net::ToSocketAddrs::to_socket_addrs(&(host, port)).map(|addrs| addrs.collect())
    }
    #[cfg(test)]
    {
        test_lookup_host(host, port)
    }
}

/// `host` without the brackets a URL or `host:port` spelling puts around an
/// IPv6 literal (`[::1]`). The system resolvers take the bare literal only,
/// so a configured `gateway.host = "[::1]"` resolves (D-063).
fn unbracketed(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host)
}

/// Test support: accept one connection on `listener`, read one request head
/// (bounded), answer with `response`, and return the head that was received.
#[cfg(test)]
pub(crate) async fn serve_one_request(listener: &tokio::net::TcpListener, response: &str) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const MAX_HEAD_BYTES: usize = 8 * 1024;
    let (mut stream, _) = listener.accept().await?;
    let mut head = Vec::new();
    let mut buffer = [0_u8; 512];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        if head.len() >= MAX_HEAD_BYTES {
            return Err(std::io::Error::other("test server: request head exceeds the bound"));
        }
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(buffer.get(..read).unwrap_or_default());
    }
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Under test: an IP literal is itself, a name comes from the fixed table,
/// and any other name does not resolve. Like the system resolvers it does
/// not accept a bracketed literal, so callers must pass the bare form.
#[cfg(test)]
fn test_lookup_host(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    let addresses = host
        .parse::<std::net::IpAddr>()
        .map_or_else(|_| resolved_addresses(&host.to_ascii_lowercase()), |ip| vec![ip]);
    if addresses.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("test resolver: `{host}` is not in the table"),
        ));
    }
    Ok(addresses
        .into_iter()
        .map(|ip| std::net::SocketAddr::new(ip, port))
        .collect())
}

/// The addresses `host` resolves to, through the system resolver.
#[cfg(not(test))]
fn resolved_addresses(host: &str) -> Vec<std::net::IpAddr> {
    std::net::ToSocketAddrs::to_socket_addrs(&(host, 80))
        .map(|addrs| addrs.map(|addr| addr.ip()).collect())
        .unwrap_or_default()
}

/// The addresses `host` resolves to under test: a fixed table, never the
/// network (INT-2B FLAKE-DNS), so no test depends on live DNS. The
/// `example.com` names resolve to a public address, `rebind.test` to a
/// private one (a rebinding host), `mixed.test` to one public and one private
/// address, `localhost` to loopback, and every other name to nothing.
#[cfg(test)]
fn resolved_addresses(host: &str) -> Vec<std::net::IpAddr> {
    match host {
        "localhost" => vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
        "example.com" | "api.example.com" | "mcp.example.com" | "www.example.com" => {
            vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(93, 184, 215, 14))]
        }
        "rebind.test" => vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1))],
        "mixed.test" => vec![
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(93, 184, 215, 14)),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
        ],
        _ => Vec::new(),
    }
}

/// Parse the historical IPv4 forms accepted by libc and many HTTP stacks
/// (octal, hexadecimal, integer, and shortened dotted notation). Rust's
/// `IpAddr` deliberately rejects them, so checking them explicitly prevents a
/// validator/client interpretation gap.
fn parse_legacy_ipv4(host: &str) -> Option<std::net::Ipv4Addr> {
    fn component(value: &str) -> Option<u32> {
        if value.is_empty() {
            return None;
        }
        if let Some(hex) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
            return u32::from_str_radix(hex, 16).ok();
        }
        if value.len() > 1 && value.starts_with('0') {
            return u32::from_str_radix(&value[1..], 8).ok();
        }
        value.parse::<u32>().ok()
    }

    let parts = host.split('.').map(component).collect::<Option<Vec<_>>>()?;
    let raw = match parts.as_slice() {
        [value] => *value,
        [a, b] if *a <= 0xff && *b <= 0x00ff_ffff => (*a << 24) | *b,
        [a, b, c] if *a <= 0xff && *b <= 0xff && *c <= 0xffff => (*a << 24) | (*b << 16) | *c,
        [a, b, c, d] if [a, b, c, d].iter().all(|value| **value <= 0xff) => (*a << 24) | (*b << 16) | (*c << 8) | *d,
        _ => return None,
    };
    Some(std::net::Ipv4Addr::from(raw))
}

/// Returns true if the IPv4 address is not globally routable.
fn is_non_global_v4(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()                       // 127.0.0.0/8
        || v4.is_private()                 // 10/8, 172.16/12, 192.168/16
        || v4.is_link_local()              // 169.254.0.0/16
        || v4.is_unspecified()             // 0.0.0.0
        || v4.is_broadcast()              // 255.255.255.255
        || v4.is_multicast()              // 224.0.0.0/4
        || (a == 100 && (64..=127).contains(&b)) // Shared address space (RFC 6598)
        || a >= 240                        // Reserved (240.0.0.0/4, except broadcast)
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // IETF assignments + TEST-NET-1
        || (a == 198 && b == 51)           // Documentation (198.51.100.0/24)
        || (a == 203 && b == 0)            // Documentation (203.0.113.0/24)
        || (a == 198 && (18..=19).contains(&b)) // Benchmarking (198.18.0.0/15)
}

/// Returns true if the IPv6 address is not globally routable.
fn is_non_global_v6(v6: std::net::Ipv6Addr) -> bool {
    let segs = v6.segments();
    v6.is_loopback()                       // ::1
        || v6.is_unspecified()             // ::
        || v6.is_multicast()              // ff00::/8
        || (segs[0] & 0xfe00) == 0xfc00   // Unique-local (fc00::/7)
        || (segs[0] & 0xffc0) == 0xfe80   // Link-local (fe80::/10)
        || (segs[0] == 0x2001 && segs[1] == 0x0db8) // Documentation (2001:db8::/32)
        || v6.to_ipv4_mapped().is_some_and(is_non_global_v4)
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_tool(allowed_domains: Vec<&str>) -> HttpRequestTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        HttpRequestTool::new(
            security,
            allowed_domains.into_iter().map(String::from).collect(),
            1_000_000,
            30,
        )
    }

    #[test]
    fn validate_accepts_http() {
        let tool = test_tool(vec!["example.com"]);
        assert!(tool.validate_url("http://example.com").is_ok());
    }

    #[test]
    fn validate_rejects_whitespace() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("https://example.com/hello world")
            .unwrap_err()
            .to_string();
        assert!(err.contains("whitespace"));
    }

    #[test]
    fn validate_blocks_local_and_private_hosts() {
        let tool = test_tool(Vec::new());
        for url in [
            "http://localhost:8080/secret",
            "http://127.0.0.1/secret",
            "http://10.0.0.1/secret",
            "http://169.254.169.254/latest/meta-data",
        ] {
            let error = tool.validate_url(url).expect_err("private target must be blocked");
            assert!(
                error.to_string().contains("Blocked local/private host"),
                "{url}: {error}"
            );
        }
    }

    #[test]
    fn validate_enforces_configured_domain_allowlist() {
        let tool = test_tool(vec!["example.com"]);
        assert!(tool.validate_url("https://api.example.com/v1").is_ok());
        let error = tool
            .validate_url("https://example.org/v1")
            .expect_err("host outside allowlist must be blocked");
        assert!(error.to_string().contains("browser.allowed_domains"));
    }

    #[test]
    fn validate_accepts_valid_methods() {
        let tool = test_tool(vec!["example.com"]);
        assert!(tool.validate_method("GET").is_ok());
        assert!(tool.validate_method("POST").is_ok());
        assert!(tool.validate_method("PUT").is_ok());
        assert!(tool.validate_method("DELETE").is_ok());
        assert!(tool.validate_method("PATCH").is_ok());
        assert!(tool.validate_method("HEAD").is_ok());
        assert!(tool.validate_method("OPTIONS").is_ok());
    }

    #[test]
    fn validate_rejects_invalid_method() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool.validate_method("INVALID").unwrap_err().to_string();
        assert!(err.contains("Unsupported HTTP method"));
    }

    #[test]
    fn schema_constrains_methods_and_header_values() {
        let tool = test_tool(vec!["example.com"]);
        let schema = tool.parameters_schema();

        assert!(
            crate::tools::schema::validate_tool_arguments(
                &schema,
                &json!({"url": "https://example.com", "method": "POST", "headers": {"x-id": "1"}})
            )
            .is_empty(),
            "expected an empty value"
        );
        assert!(
            !crate::tools::schema::validate_tool_arguments(
                &schema,
                &json!({"url": "https://example.com", "method": "TRACE"})
            )
            .is_empty(),
            "expected a non-empty value"
        );
        assert!(
            !crate::tools::schema::validate_tool_arguments(
                &schema,
                &json!({"url": "https://example.com", "headers": {"x-id": 1}})
            )
            .is_empty(),
            "expected a non-empty value"
        );
    }

    #[test]
    fn blocks_multicast_ipv4() {
        assert!(is_private_or_local_host("224.0.0.1"));
        assert!(is_private_or_local_host("239.255.255.255"));
    }

    #[test]
    fn blocks_broadcast() {
        assert!(is_private_or_local_host("255.255.255.255"));
    }

    #[test]
    fn blocks_reserved_ipv4() {
        assert!(is_private_or_local_host("240.0.0.1"));
        assert!(is_private_or_local_host("250.1.2.3"));
    }

    #[test]
    fn blocks_documentation_ranges() {
        assert!(is_private_or_local_host("192.0.2.1")); // TEST-NET-1
        assert!(is_private_or_local_host("198.51.100.1")); // TEST-NET-2
        assert!(is_private_or_local_host("203.0.113.1")); // TEST-NET-3
    }

    #[test]
    fn blocks_benchmarking_range() {
        assert!(is_private_or_local_host("198.18.0.1"));
        assert!(is_private_or_local_host("198.19.255.255"));
    }

    #[test]
    fn blocks_ipv6_localhost() {
        assert!(is_private_or_local_host("::1"));
        assert!(is_private_or_local_host("[::1]"));
    }

    #[test]
    fn blocks_ipv6_multicast() {
        assert!(is_private_or_local_host("ff02::1"));
    }

    #[test]
    fn blocks_ipv6_link_local() {
        assert!(is_private_or_local_host("fe80::1"));
    }

    #[test]
    fn blocks_ipv6_unique_local() {
        assert!(is_private_or_local_host("fd00::1"));
    }

    #[test]
    fn blocks_ipv4_mapped_ipv6() {
        assert!(is_private_or_local_host("::ffff:127.0.0.1"));
        assert!(is_private_or_local_host("::ffff:192.168.1.1"));
        assert!(is_private_or_local_host("::ffff:10.0.0.1"));
    }

    #[test]
    fn allows_public_ipv4() {
        assert!(!is_private_or_local_host("8.8.8.8"));
        assert!(!is_private_or_local_host("1.1.1.1"));
        assert!(!is_private_or_local_host("93.184.216.34"));
    }

    #[test]
    fn blocks_ipv6_documentation_range() {
        assert!(is_private_or_local_host("2001:db8::1"));
    }

    #[test]
    fn allows_public_ipv6() {
        assert!(!is_private_or_local_host("2607:f8b0:4004:800::200e"));
    }

    #[test]
    fn blocks_shared_address_space() {
        assert!(is_private_or_local_host("100.64.0.1"));
        assert!(is_private_or_local_host("100.127.255.255"));
        assert!(!is_private_or_local_host("100.63.0.1")); // Just below range
        assert!(!is_private_or_local_host("100.128.0.1")); // Just above range
    }

    #[test]
    fn bounded_response_within_limit() {
        assert_eq!(
            HttpRequestTool::format_response_bytes(b"hello world", false),
            "hello world"
        );
    }

    #[test]
    fn bounded_response_marks_truncation() {
        let truncated = HttpRequestTool::format_response_bytes(b"hello worl", true);
        assert!(truncated.len() <= 10 + 60); // limit + message
        assert!(truncated.contains("[Response truncated"));
    }

    #[test]
    fn parse_headers_preserves_original_values() {
        let tool = test_tool(vec!["example.com"]);
        let headers = json!({
            "Authorization": "Bearer secret",
            "Content-Type": "application/json",
            "X-API-Key": "my-key"
        });
        let parsed = tool.parse_headers(&headers);
        assert_eq!(parsed.len(), 3);
        assert!(parsed.iter().any(|(k, v)| k == "Authorization" && v == "Bearer secret"));
        assert!(parsed.iter().any(|(k, v)| k == "X-API-Key" && v == "my-key"));
        assert!(
            parsed
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "application/json")
        );
    }

    #[test]
    fn redact_headers_for_display_redacts_sensitive() {
        let headers = vec![
            ("Authorization".into(), "Bearer secret".into()),
            ("Content-Type".into(), "application/json".into()),
            ("X-API-Key".into(), "my-key".into()),
            ("X-Secret-Token".into(), "tok-123".into()),
        ];
        let redacted = HttpRequestTool::redact_headers_for_display(&headers);
        assert_eq!(redacted.len(), 4);
        assert!(
            redacted
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "***REDACTED***")
        );
        assert!(redacted.iter().any(|(k, v)| k == "X-API-Key" && v == "***REDACTED***"));
        assert!(
            redacted
                .iter()
                .any(|(k, v)| k == "X-Secret-Token" && v == "***REDACTED***")
        );
        assert!(
            redacted
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "application/json")
        );
    }

    #[test]
    fn redact_headers_does_not_alter_original() {
        let headers = vec![("Authorization".into(), "Bearer real-token".into())];
        let _ = HttpRequestTool::redact_headers_for_display(&headers);
        assert_eq!(headers[0].1, "Bearer real-token");
    }

    // ── SSRF: alternate IP notation bypass defense-in-depth ─────────
    //
    // Rust's IpAddr::parse() rejects non-standard notations (octal, hex,
    // decimal integer, zero-padded). These tests document that property
    // so regressions are caught if the parsing strategy ever changes.

    #[test]
    fn ssrf_octal_loopback_caught_by_dns_resolution() {
        // 0177.0.0.1 is octal for 127.0.0.1; glibc's getaddrinfo resolves
        // this to loopback, so the DNS rebinding defense catches it.
        assert!(is_private_or_local_host("0177.0.0.1"));
    }

    #[test]
    fn ssrf_hex_loopback_caught_by_dns_resolution() {
        // 0x7f000001 is hex for 127.0.0.1; caught by DNS resolution.
        assert!(is_private_or_local_host("0x7f000001"));
    }

    #[test]
    fn ssrf_decimal_loopback_caught_by_dns_resolution() {
        // 2130706433 is decimal for 127.0.0.1; caught by DNS resolution.
        assert!(is_private_or_local_host("2130706433"));
    }

    #[test]
    fn ssrf_zero_padded_loopback_caught_by_dns_resolution() {
        // 127.000.000.001 uses zero-padded octets; caught by DNS resolution.
        assert!(is_private_or_local_host("127.000.000.001"));
    }

    #[test]
    fn redirect_policy_is_none() {
        // Structural test: the tool should be buildable with redirect-safe config.
        // The actual Policy::none() enforcement is in execute_request's client builder.
        let tool = test_tool(vec!["example.com"]);
        assert_eq!(tool.name(), "http_request");
    }

    // ── §1.4 DNS rebinding / SSRF defense-in-depth tests ─────

    #[test]
    fn ssrf_blocks_loopback_127_range() {
        assert!(is_private_or_local_host("127.0.0.1"));
        assert!(is_private_or_local_host("127.0.0.2"));
        assert!(is_private_or_local_host("127.255.255.255"));
    }

    #[test]
    fn ssrf_blocks_rfc1918_10_range() {
        assert!(is_private_or_local_host("10.0.0.1"));
        assert!(is_private_or_local_host("10.255.255.255"));
    }

    #[test]
    fn ssrf_blocks_rfc1918_172_range() {
        assert!(is_private_or_local_host("172.16.0.1"));
        assert!(is_private_or_local_host("172.31.255.255"));
    }

    #[test]
    fn ssrf_blocks_unspecified_address() {
        assert!(is_private_or_local_host("0.0.0.0"));
    }

    #[test]
    fn ssrf_blocks_dot_localhost_subdomain() {
        assert!(is_private_or_local_host("evil.localhost"));
        assert!(is_private_or_local_host("a.b.localhost"));
    }

    #[test]
    fn ssrf_blocks_dot_local_tld() {
        assert!(is_private_or_local_host("service.local"));
    }

    #[test]
    fn ssrf_ipv6_unspecified() {
        assert!(is_private_or_local_host("::"));
    }

    #[test]
    fn validate_rejects_ftp_scheme() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool.validate_url("ftp://example.com").unwrap_err().to_string();
        assert!(err.contains("http://") || err.contains("https://"));
    }

    #[test]
    fn validate_rejects_empty_url() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool.validate_url("").unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    /// INT-2B FLAKE-DNS, RED before: the DNS-rebinding check resolved the
    /// host through the system resolver, so these tests depended on live DNS
    /// (`example.com` once resolved to a blocked address mid-run and four
    /// tests failed). After: under test the check reads a fixed table and
    /// never the network. `rebind.test` (a reserved name no real resolver
    /// answers) resolves to a private address there, so the rebinding branch
    /// is covered; a name outside the table resolves to nothing.
    #[test]
    fn hostname_checks_use_the_test_resolver_and_never_the_network() {
        assert!(is_private_or_local_host("rebind.test"));
        assert!(!is_private_or_local_host("example.com"));
        assert!(!is_private_or_local_host("api.example.com"));
        assert!(!is_private_or_local_host("unlisted.test"));
    }

    /// INT-2B B2f F-6: the connection lookups (media downloads, node
    /// callbacks, the doctor probe) read the same test table: an IP literal
    /// is itself, a listed name resolves, anything else fails without asking
    /// a real resolver (`unlisted.test` would otherwise go to DNS).
    #[tokio::test]
    async fn connection_lookups_use_the_test_resolver_and_never_the_network() {
        let public = lookup_host("example.com", 443).await.expect("test: table name");
        assert_eq!(public.len(), 1);
        assert_eq!(public.first().map(std::net::SocketAddr::port), Some(443));
        let literal = lookup_host("[::1]", 8080).await.expect("test: literal");
        assert_eq!(literal.first().map(|addr| addr.ip().is_loopback()), Some(true));
        let error = lookup_host("unlisted.test", 80)
            .await
            .expect_err("test: not in the table");
        assert!(error.to_string().contains("test resolver"), "{error}");
        let blocking = lookup_host_blocking("rebind.test", 80).expect("test: table name");
        assert_eq!(
            blocking.first().map(|addr| addr.ip().to_string()),
            Some("10.0.0.1".to_string())
        );
    }

    fn socket(ip: [u8; 4], port: u16) -> std::net::SocketAddr {
        std::net::SocketAddr::from((ip, port))
    }

    /// M2: the pinned target carries exactly the checked addresses, so the
    /// client connects to them and never resolves the name again.
    #[test]
    fn pinned_target_keeps_the_host_and_every_checked_address() {
        let addrs = vec![socket([93, 184, 215, 14], 443), socket([8, 8, 8, 8], 443)];
        let target = PinnedTarget::from_resolved("example.com", addrs.clone()).expect("test: public addresses pin");
        assert_eq!(target.host, "example.com", "the pin must be keyed by the request host");
        assert_eq!(target.addrs, addrs, "the pin must hold exactly the checked addresses");
        target
            .pin(reqwest::Client::builder())
            .build()
            .expect("test: a pinned client builds");
    }

    /// M2: one private address among public ones refuses the whole host.
    #[test]
    fn pinned_target_refuses_a_host_with_one_public_and_one_private_address() {
        let error = PinnedTarget::from_resolved(
            "mixed.test",
            vec![socket([93, 184, 215, 14], 443), socket([10, 0, 0, 1], 443)],
        )
        .expect_err("test: a private address must refuse the host");
        assert!(
            matches!(&error, PinError::NonGlobal { host, address }
                if host == "mixed.test" && address.to_string() == "10.0.0.1"),
            "expected NonGlobal for 10.0.0.1, got {error:?}"
        );
        assert!(
            error.to_string().contains("Blocked local/private host"),
            "the refusal must read as an SSRF block: {error}"
        );
    }

    #[test]
    fn pinned_target_refuses_a_host_without_addresses() {
        let error = PinnedTarget::from_resolved("unlisted.test", Vec::new()).expect_err("test: nothing to pin");
        assert!(
            matches!(error, PinError::Unresolved { .. }),
            "expected Unresolved, got {error:?}"
        );
    }

    /// Deadline of every await in these tests (AGENTS §3.9).
    const TEST_DEADLINE: Duration = Duration::from_secs(10);
    /// How long a listener is watched for a connection that must not come.
    const NO_CONNECTION_WINDOW: Duration = Duration::from_millis(300);
    const OK_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
    /// A name no resolver answers (RFC 6761): only a pinned client reaches it.
    const PIN_PROOF_HOST: &str = "pin-proof.invalid";

    async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(TEST_DEADLINE, future)
            .await
            .expect("test: the step must finish within the deadline")
    }

    async fn loopback_listener() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let listener = within(tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("test: bind an owned loopback listener");
        let address = listener.local_addr().expect("test: listener address");
        (listener, address)
    }

    fn proof_target(address: std::net::SocketAddr) -> PinnedTarget {
        // The layer below the address check: the list is given as already
        // validated, exactly as `resolve_pinned_target` hands it over.
        PinnedTarget {
            host: PIN_PROOF_HOST.to_string(),
            addrs: vec![address],
        }
    }

    fn proxy_config(
        scope: crate::config::ProxyScope,
        services: &[&str],
        proxy: std::net::SocketAddr,
    ) -> crate::config::ProxyConfig {
        crate::config::ProxyConfig {
            http_proxy: Some(format!("http://{proxy}")),
            scope,
            services: services.iter().map(|service| (*service).to_string()).collect(),
            ..crate::config::ProxyConfig::default()
        }
    }

    /// M2: the per-hop step resolves once (test table, never the network),
    /// checks every address and pins them with the URL's port.
    #[tokio::test]
    async fn resolve_pinned_target_checks_every_resolved_address() {
        let target = within(resolve_pinned_target("https://example.com/v1"))
            .await
            .expect("test: public table name pins");
        assert_eq!(target.host, "example.com", "host comes from the parsed URL");
        assert_eq!(
            target.addrs,
            vec![socket([93, 184, 215, 14], 443)],
            "the checked address is pinned with the scheme's port"
        );

        for url in [
            "https://mixed.test/",
            "https://rebind.test/",
            "http://10.0.0.1/secret",
            "http://127.0.0.1:8080/",
        ] {
            let error = within(resolve_pinned_target(url))
                .await
                .expect_err("test: a non-global target must not pin");
            assert!(
                matches!(error, PinError::NonGlobal { .. }),
                "{url}: expected NonGlobal, got {error:?}"
            );
        }

        let error = within(resolve_pinned_target("https://unlisted.test/"))
            .await
            .expect_err("test: an unresolvable name must not pin");
        assert!(
            matches!(error, PinError::Lookup { .. }),
            "expected Lookup, got {error:?}"
        );
        let error = within(resolve_pinned_target("not a url"))
            .await
            .expect_err("test: a malformed URL must not pin");
        assert!(
            matches!(error, PinError::InvalidUrl(_)),
            "expected InvalidUrl, got {error:?}"
        );
    }

    /// Review r1 finding 5 (a), real wire: the production client of a
    /// checked target connects to the pinned address although the name
    /// cannot resolve, and the request keeps the name in its `Host` header.
    /// Break-the-fix: drop `resolve_to_addrs` in `PinnedTarget::pin` and the
    /// send fails with a DNS error.
    #[tokio::test]
    async fn the_pinned_client_reaches_the_pinned_address_with_the_host_header() {
        let (listener, address) = loopback_listener().await;
        let server = tokio::spawn(async move { serve_one_request(&listener, OK_RESPONSE).await });
        let client = test_tool(Vec::new())
            .client_for(&proof_target(address), &crate::config::ProxyConfig::default())
            .expect("test: the pinned client builds");

        let url = format!("http://{PIN_PROOF_HOST}:{}/probe", address.port());
        let response = within(client.get(&url).send())
            .await
            .expect("test: the request must reach the pinned listener");
        assert_eq!(response.status().as_u16(), 200, "the listener's answer comes back");

        let head = within(server)
            .await
            .expect("test: server task joins")
            .expect("test: the listener served one request");
        assert!(
            head.starts_with("GET /probe HTTP/1.1\r\n"),
            "the listener must receive the request line, got {head}"
        );
        let host_header = format!("host: {PIN_PROOF_HOST}:{}\r\n", address.port());
        assert!(
            head.to_ascii_lowercase().contains(&host_header),
            "the Host header must keep the name, got {head}"
        );
    }

    /// Review r1 finding 1, real wire: with a general (not selected) proxy
    /// configured, the tool's client still connects directly to the pinned
    /// address and the proxy sees no connection.
    #[tokio::test]
    async fn a_general_proxy_is_not_used_by_the_pinned_client() {
        let (target_listener, target_address) = loopback_listener().await;
        let (proxy_listener, proxy_address) = loopback_listener().await;
        let server = tokio::spawn(async move { serve_one_request(&target_listener, OK_RESPONSE).await });
        let general = proxy_config(crate::config::ProxyScope::Zeroclaw, &[], proxy_address);
        assert!(
            general.should_apply_to_service(PROXY_SERVICE_KEY),
            "test setup: ordinary clients would use this proxy"
        );
        let client = test_tool(Vec::new())
            .client_for(&proof_target(target_address), &general)
            .expect("test: the pinned client builds");

        let url = format!("http://{PIN_PROOF_HOST}:{}/direct", target_address.port());
        within(client.get(&url).send())
            .await
            .expect("test: the request must go directly to the pinned listener");
        let head = within(server)
            .await
            .expect("test: server task joins")
            .expect("test: the target served one request");
        assert!(
            head.starts_with("GET /direct HTTP/1.1\r\n"),
            "the target must receive an origin-form request, got {head}"
        );
        assert!(
            tokio::time::timeout(NO_CONNECTION_WINDOW, proxy_listener.accept())
                .await
                .is_err(),
            "the general proxy must not be connected to"
        );
    }

    /// Review r1 finding 1, real wire: an operator who lists the tool in
    /// `proxy.services` routes it through the proxy (absolute-form request
    /// at the proxy, nothing at the target).
    #[tokio::test]
    async fn an_explicit_services_selection_routes_the_client_through_the_proxy() {
        let (target_listener, target_address) = loopback_listener().await;
        let (proxy_listener, proxy_address) = loopback_listener().await;
        let proxy_server = tokio::spawn(async move { serve_one_request(&proxy_listener, OK_RESPONSE).await });
        let selected = proxy_config(crate::config::ProxyScope::Services, &[PROXY_SERVICE_KEY], proxy_address);
        let client = test_tool(Vec::new())
            .client_for(&proof_target(target_address), &selected)
            .expect("test: the proxied client builds");

        let url = format!("http://{PIN_PROOF_HOST}:{}/via-proxy", target_address.port());
        within(client.get(&url).send())
            .await
            .expect("test: the request must be answered by the proxy");
        let head = within(proxy_server)
            .await
            .expect("test: proxy task joins")
            .expect("test: the proxy served one request");
        assert!(
            head.starts_with(&format!("GET {url} HTTP/1.1\r\n")),
            "the proxy must receive the absolute-form request, got {head}"
        );
        assert!(
            tokio::time::timeout(NO_CONNECTION_WINDOW, target_listener.accept())
                .await
                .is_err(),
            "the target must not be connected to directly"
        );
    }

    /// M2: `execute_request` refuses before any connection when the host
    /// resolves to a private address, even if only one of several is private.
    #[tokio::test]
    async fn execute_request_refuses_a_host_that_resolves_privately() {
        let tool = test_tool(Vec::new());
        let error = within(tool.execute_request("http://mixed.test/", reqwest::Method::GET, Vec::new(), None))
            .await
            .expect_err("test: a mixed public/private host must be refused");
        assert!(
            error.to_string().contains("Blocked local/private host"),
            "expected an SSRF block, got {error}"
        );
    }
}
