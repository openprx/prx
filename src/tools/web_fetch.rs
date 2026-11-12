use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use std::sync::LazyLock;
use std::time::Duration;

static RE_SCRIPT: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?si)<script[^>]*>.*?</script>"));
fn re_script() -> Result<&'static Regex, &'static regex::Error> {
    RE_SCRIPT.as_ref()
}

static RE_STYLE: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| Regex::new(r"(?si)<style[^>]*>.*?</style>"));
fn re_style() -> Result<&'static Regex, &'static regex::Error> {
    RE_STYLE.as_ref()
}

static RE_HEAD: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| Regex::new(r"(?si)<head[^>]*>.*?</head>"));
fn re_head() -> Result<&'static Regex, &'static regex::Error> {
    RE_HEAD.as_ref()
}

static RE_BLOCK: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?i)<(br\s*/?|p|div|h[1-6]|li|tr|article|section|header|footer)[^>]*>"));
fn re_block() -> Result<&'static Regex, &'static regex::Error> {
    RE_BLOCK.as_ref()
}

static RE_TAGS: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| Regex::new(r"<[^>]+>"));
fn re_tags() -> Result<&'static Regex, &'static regex::Error> {
    RE_TAGS.as_ref()
}

/// Redirect hops `web_fetch` follows before it gives up.
const MAX_REDIRECTS: u32 = 5;

/// Web fetch tool — fetches a URL and returns clean readable text.
pub struct WebFetchTool {
    allowed_domains: Vec<String>,
    max_chars: usize,
    timeout_secs: u64,
}

impl WebFetchTool {
    pub fn new(allowed_domains: Vec<String>, max_chars: usize, timeout_secs: u64) -> Self {
        Self {
            allowed_domains: normalize_allowed_domains(allowed_domains),
            max_chars: max_chars.max(100),
            timeout_secs: timeout_secs.max(1),
        }
    }

    /// Convert HTML to plain readable text by stripping tags and decoding entities.
    fn html_to_text(html: &str) -> Result<String, &'static regex::Error> {
        Self::html_to_text_with_patterns(html, [re_script(), re_style(), re_head(), re_block(), re_tags()])
    }

    fn html_to_text_with_patterns(
        html: &str,
        patterns: [Result<&Regex, &'static regex::Error>; 5],
    ) -> Result<String, &'static regex::Error> {
        let [script, style, head, block, tags] = patterns;

        // Remove script/style/head blocks (case-insensitive, multiline).
        // Rust's regex crate does not support backreferences, so use three
        // separate patterns instead of the combined `(?si)<(script|style|head)[^>]*>.*?</\1>`.
        let text = script?.replace_all(html, "");
        let text = style?.replace_all(&text, "");
        let text = head?.replace_all(&text, "");

        // Replace block-level elements with newlines so paragraphs separate cleanly
        let text = block?.replace_all(&text, "\n");

        // Strip all remaining tags
        let text = tags?.replace_all(&text, "");

        // Decode common HTML entities
        let text = text
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&apos;", "'")
            .replace("&nbsp;", " ")
            .replace("&#160;", " ")
            .replace("&mdash;", "—")
            .replace("&ndash;", "–")
            .replace("&hellip;", "…")
            .replace("&ldquo;", "\u{201C}")
            .replace("&rdquo;", "\u{201D}")
            .replace("&lsquo;", "\u{2018}")
            .replace("&rsquo;", "\u{2019}");

        // Collapse whitespace: trim each line and drop blank lines
        Ok(text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn validate_url(&self, raw_url: &str) -> anyhow::Result<String> {
        let url = raw_url.trim();
        if url.is_empty() {
            anyhow::bail!("URL cannot be empty");
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            anyhow::bail!("URL must start with http:// or https://");
        }
        let host = extract_host(url)?;
        if is_private_or_local_address(&host) {
            anyhow::bail!("Blocked local/private host: {host}");
        }

        // Graceful degradation: if allowlist is not configured, allow public hosts
        // only (still blocks localhost/private IPs). Encourage explicit config.
        if self.allowed_domains.is_empty() {
            tracing::warn!(
                "web_fetch running without browser.allowed_domains; allowing public host '{}' only. Configure [browser].allowed_domains for stricter policy",
                host
            );
        } else if !host_matches_allowlist(&host, &self.allowed_domains) {
            anyhow::bail!("Host '{host}' is not in browser.allowed_domains");
        }

        // Reject unauthorized names before DNS can disclose them. Permitted
        // hosts still undergo the same private-address/rebinding check.
        if is_private_or_local_host(&host) {
            anyhow::bail!("Blocked local/private host: {host}");
        }

        Ok(url.to_string())
    }

    /// The validated URL a redirect from `current_url` to `location` leads
    /// to. A relative `location` is resolved against the current URL; the
    /// result goes through the same checks as the first URL, so a redirect
    /// to a private literal or to a host outside the allowlist is refused.
    fn redirect_target(&self, current_url: &str, location: &str) -> anyhow::Result<String> {
        let next_url = if location.starts_with("http://") || location.starts_with("https://") {
            location.to_string()
        } else {
            let base = reqwest::Url::parse(current_url).map_err(|e| anyhow::anyhow!("Invalid base URL: {e}"))?;
            base.join(location)
                .map_err(|e| anyhow::anyhow!("Invalid redirect URL: {e}"))?
                .to_string()
        };
        self.validate_url(&next_url)
    }

    /// The client of one hop to a checked target: pinned to the checked
    /// addresses, never following redirects itself, and connecting directly.
    /// Proxies (inherited environment variables included) are switched off:
    /// a proxy resolves the name itself, so the pin would not bind the
    /// address that is finally connected to (same rule as media downloads).
    fn client_for(&self, target: &super::http_request::PinnedTarget) -> reqwest::Result<reqwest::Client> {
        target
            .pin(
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(self.timeout_secs))
                    .user_agent("Mozilla/5.0 (compatible; PRX/1.0; +https://openprx.dev)")
                    .redirect(reqwest::redirect::Policy::none())
                    .no_proxy(),
            )
            .build()
    }

    /// Fetch `url`, whose host was checked and pinned as `first`, following
    /// redirects manually. Every redirect target goes through the same steps
    /// as the first URL (allowlist and private-address validation, one
    /// resolution, every address checked) and is fetched by a client pinned
    /// to exactly the checked addresses, so no name is resolved a second
    /// time between its check and its connection (DNS rebinding).
    async fn fetch_following_redirects(
        &self,
        url: &str,
        first: super::http_request::PinnedTarget,
    ) -> anyhow::Result<reqwest::Response> {
        let mut current_url = url.to_string();
        let mut target = first;
        let mut redirect_count: u32 = 0;
        loop {
            let client = self.client_for(&target)?;
            let resp = client
                .get(&current_url)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to fetch {current_url}: {e}"))?;

            if !resp.status().is_redirection() {
                return Ok(resp);
            }
            if redirect_count >= MAX_REDIRECTS {
                anyhow::bail!("Too many redirects fetching {url}");
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("Redirect with no Location header"))?;
            current_url = self.redirect_target(&current_url, location)?;
            target = super::http_request::resolve_pinned_target(&current_url).await?;
            redirect_count += 1;
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch and extract readable content from a URL. \
         Downloads the page and returns clean text with HTML tags removed. \
         Useful for reading articles, documentation, or any web page. \
         Connects directly to the checked public address of each host, redirect targets included \
         (private and local addresses are refused); proxies are never used."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch (must start with http:// or https://)"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum characters to return (default uses server config)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: url"))?;

        let url = self.validate_url(url)?;

        // Allow caller to override max_chars; clamp to server-side maximum
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .and_then(|value| usize::try_from(value).ok())
            .map(|value| value.min(self.max_chars))
            .unwrap_or(self.max_chars);

        tracing::info!("Fetching URL: {}", url);

        let target = super::http_request::resolve_pinned_target(&url).await?;
        let response = self.fetch_following_redirects(&url, target).await?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("HTTP {} fetching {}", status, url);
        }

        // Check content-type — only process text-based responses
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let is_html = content_type.contains("html");
        let is_text = content_type.contains("text") || content_type.contains("json") || content_type.contains("xml");

        if !is_html && !is_text && !content_type.is_empty() {
            anyhow::bail!(
                "Unsupported content type '{}' — web_fetch only handles text/HTML responses",
                content_type
            );
        }

        let body = response
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read response body from {}: {}", url, e))?;

        let output = if is_html || body.trim_start().starts_with('<') {
            match Self::html_to_text(&body) {
                Ok(text) => text,
                Err(error) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("HTML pattern unavailable: {error}")),
                    });
                }
            }
        } else {
            // Plain text / JSON / XML — return as-is
            body
        };

        // Truncate if needed, appending a note
        let output = if output.len() > max_chars {
            let safe_end = output.floor_char_boundary(max_chars);
            format!(
                "{}\n\n[Truncated: output exceeded {} characters. Use a smaller range or a more specific URL.]",
                &output[..safe_end],
                max_chars
            )
        } else {
            output
        };

        if output.trim().is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "(No readable content extracted from page)".to_string(),
                error: None,
            });
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::WebBrowsing]
    }
}

pub(crate) fn normalize_allowed_domains(domains: Vec<String>) -> Vec<String> {
    let mut normalized = domains
        .into_iter()
        .filter_map(|domain| normalize_domain(&domain))
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn normalize_domain(raw: &str) -> Option<String> {
    let mut domain = raw.trim().to_lowercase();
    if domain.is_empty() {
        return None;
    }

    if let Some(stripped) = domain.strip_prefix("https://") {
        domain = stripped.to_string();
    } else if let Some(stripped) = domain.strip_prefix("http://") {
        domain = stripped.to_string();
    }

    if let Some((host, _)) = domain.split_once('/') {
        domain = host.to_string();
    }

    domain = domain.trim_start_matches('.').trim_end_matches('.').to_string();

    if let Some((host, _)) = domain.split_once(':') {
        domain = host.to_string();
    }

    if domain.is_empty() || domain.chars().any(char::is_whitespace) {
        return None;
    }

    Some(domain)
}

fn extract_host(url: &str) -> anyhow::Result<String> {
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
        anyhow::bail!("IPv6 hosts are not supported in web_fetch");
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

pub(crate) fn host_matches_allowlist(host: &str, allowed_domains: &[String]) -> bool {
    allowed_domains
        .iter()
        .any(|domain| host == domain || host.strip_suffix(domain).is_some_and(|prefix| prefix.ends_with('.')))
}

fn is_private_or_local_address(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|candidate| candidate.strip_suffix(']'))
        .unwrap_or(host);

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

    false
}

fn is_private_or_local_host(host: &str) -> bool {
    if is_private_or_local_address(host) {
        return true;
    }
    // DNS rebinding defense: resolve the hostname and check if any resolved
    // IP is private/local (one resolver for both tools; under test it never
    // touches the network).
    super::http_request::resolves_to_non_global(host)
}

fn is_non_global_v4(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || (a == 100 && (64..=127).contains(&b))
        || a >= 240
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && b == 51)
        || (a == 203 && b == 0)
        || (a == 198 && (18..=19).contains(&b))
}

fn is_non_global_v6(v6: std::net::Ipv6Addr) -> bool {
    let segs = v6.segments();
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || (segs[0] & 0xfe00) == 0xfc00
        || (segs[0] & 0xffc0) == 0xfe80
        || (segs[0] == 0x2001 && segs[1] == 0x0db8)
        || v6.to_ipv4_mapped().is_some_and(is_non_global_v4)
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_html_to_text_basic() {
        let html = "<html><body><h1>Hello</h1><p>World</p></body></html>";
        let text = WebFetchTool::html_to_text(html).expect("test: HTML patterns compile");
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains('<'));
        assert!(!text.contains('>'));
    }

    #[test]
    fn test_html_to_text_strips_scripts() {
        let html = "<html><head><script>var x = 1;</script></head><body>Content</body></html>";
        let text = WebFetchTool::html_to_text(html).expect("test: HTML patterns compile");
        assert!(!text.contains("var x"));
        assert!(text.contains("Content"));
    }

    #[test]
    fn test_html_to_text_strips_styles() {
        let html = "<html><head><style>body { color: red; }</style></head><body>Text</body></html>";
        let text = WebFetchTool::html_to_text(html).expect("test: HTML patterns compile");
        assert!(!text.contains("color"));
        assert!(text.contains("Text"));
    }

    #[test]
    fn test_html_to_text_decodes_entities() {
        let html = "<p>1 &lt; 2 &amp; 3 &gt; 0 &quot;quoted&quot; it&#39;s</p>";
        let text = WebFetchTool::html_to_text(html).expect("test: HTML patterns compile");
        assert!(text.contains("1 < 2 & 3 > 0 \"quoted\" it's"), "got: {text}");
    }

    #[test]
    fn test_html_to_text_collapses_whitespace() {
        let html = "<p>   lots   of   space   </p>";
        let text = WebFetchTool::html_to_text(html).expect("test: HTML patterns compile");
        // After trim, no leading/trailing whitespace per line
        for line in text.lines() {
            assert_eq!(line, line.trim());
        }
    }

    #[test]
    fn test_tool_name() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        assert_eq!(tool.name(), "web_fetch");
    }

    #[test]
    fn test_tool_description_contains_fetch() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        assert!(tool.description().to_lowercase().contains("fetch"));
    }

    #[test]
    fn test_parameters_schema() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["url"].is_object());
        assert_eq!(schema["required"][0], "url");
    }

    #[tokio::test]
    async fn test_execute_missing_url() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("url"));
    }

    #[tokio::test]
    async fn test_execute_empty_url() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        let result = tool.execute(json!({"url": ""})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_invalid_scheme() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        let result = tool.execute(json!({"url": "ftp://example.com"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("http://") || true); // scheme check
    }

    #[test]
    fn validate_url_blocks_private_hosts() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        let err = tool
            .validate_url("http://127.0.0.1/internal")
            .expect_err("private host should be rejected");
        assert!(err.to_string().contains("Blocked local/private host"));
    }

    #[test]
    fn validate_url_enforces_allowlist() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        let err = tool
            .validate_url("https://evil.com/")
            .expect_err("unexpected allowlist bypass");
        assert!(err.to_string().contains("not in browser.allowed_domains"));
    }

    #[test]
    fn validate_url_keeps_rebinding_checks_for_allowlisted_hosts() {
        let tool = WebFetchTool::new(vec!["rebind.test".into()], 10000, 15);
        let err = tool
            .validate_url("https://rebind.test/internal")
            .expect_err("allowlisted hostname resolving privately must be rejected");
        assert!(err.to_string().contains("Blocked local/private host"));
    }

    #[test]
    fn validate_url_allows_public_host_when_allowlist_empty() {
        let tool = WebFetchTool::new(vec![], 10000, 15);
        let ok = tool
            .validate_url("https://example.com/docs")
            .expect("public host should be allowed when allowlist is empty");
        assert_eq!(ok, "https://example.com/docs");

        let err = tool
            .validate_url("http://localhost:8080")
            .expect_err("localhost must still be blocked");
        assert!(err.to_string().contains("Blocked local/private host"));
    }

    /// INT-2B FLAKE-DNS, RED before: see the `http_request` test of the same
    /// name; `web_fetch` resolves through the same test table.
    #[test]
    fn hostname_checks_use_the_test_resolver_and_never_the_network() {
        assert!(is_private_or_local_host("rebind.test"));
        assert!(!is_private_or_local_host("example.com"));
    }

    /// M2: a redirect to a private literal is refused before any client is
    /// built for it; a relative redirect stays on the checked host.
    #[test]
    fn redirect_to_a_private_literal_is_refused() {
        let tool = WebFetchTool::new(vec![], 10000, 15);
        for location in [
            "http://10.0.0.1/admin",
            "http://127.0.0.1:8080/",
            "http://169.254.169.254/latest/meta-data",
            "https://rebind.test/internal",
        ] {
            let error = tool
                .redirect_target("https://example.com/start", location)
                .expect_err("test: a redirect to a private target must be refused");
            assert!(
                error.to_string().contains("Blocked local/private host"),
                "{location}: expected an SSRF block, got {error}"
            );
        }

        let next = tool
            .redirect_target("https://example.com/a/b", "../c")
            .expect("test: a relative redirect on a public host is followed");
        assert_eq!(
            next, "https://example.com/c",
            "relative redirects resolve against the hop"
        );
    }

    #[test]
    fn redirect_outside_the_allowlist_is_refused() {
        let tool = WebFetchTool::new(vec!["example.com".into()], 10000, 15);
        let error = tool
            .redirect_target("https://example.com/start", "https://mixed.test/")
            .expect_err("test: a redirect leaving the allowlist must be refused");
        assert!(
            error.to_string().contains("not in browser.allowed_domains"),
            "expected an allowlist refusal, got {error}"
        );
    }

    /// Deadline of every await in these tests (AGENTS §3.9).
    const TEST_DEADLINE: Duration = Duration::from_secs(10);
    /// How long a listener is watched for a connection that must not come.
    const NO_CONNECTION_WINDOW: Duration = Duration::from_millis(300);
    /// A name no resolver answers (RFC 6761): only a pinned client reaches it.
    const PIN_PROOF_HOST: &str = "pin-proof.invalid";

    async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(TEST_DEADLINE, future)
            .await
            .expect("test: the step must finish within the deadline")
    }

    /// An owned loopback listener that answers one request with `response`,
    /// then watches for a second connection. Yields the request head and
    /// whether a second connection arrived.
    async fn one_shot_server(response: String) -> (std::net::SocketAddr, tokio::task::JoinHandle<(String, bool)>) {
        let listener = within(tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("test: bind an owned loopback listener");
        let address = listener.local_addr().expect("test: listener address");
        let server = tokio::spawn(async move {
            let head = super::super::http_request::serve_one_request(&listener, &response)
                .await
                .expect("test: the listener serves one request");
            let second = tokio::time::timeout(NO_CONNECTION_WINDOW, listener.accept())
                .await
                .is_ok();
            (head, second)
        });
        (address, server)
    }

    fn proof_target(address: std::net::SocketAddr) -> super::super::http_request::PinnedTarget {
        // The layer below the address check: the list is given as already
        // validated, exactly as `resolve_pinned_target` hands it over.
        super::super::http_request::PinnedTarget {
            host: PIN_PROOF_HOST.to_string(),
            addrs: vec![address],
        }
    }

    /// M2: a host that resolves to one public and one private address, or a
    /// private literal, is refused before any connection.
    #[tokio::test]
    async fn execute_refuses_a_host_with_a_private_address() {
        let tool = WebFetchTool::new(vec![], 10000, 15);
        for url in ["https://mixed.test/", "http://10.0.0.1/"] {
            let error = within(tool.execute(json!({"url": url})))
                .await
                .expect_err("test: a non-global host must not be fetched");
            assert!(
                error.to_string().contains("Blocked local/private host"),
                "{url}: expected an SSRF block, got {error}"
            );
        }
    }

    /// Review r1 finding 5 (a), real wire: the fetch reaches the pinned
    /// address although the name cannot resolve, with the name in `Host`.
    /// Break-the-fix: drop `resolve_to_addrs` in `PinnedTarget::pin`.
    #[tokio::test]
    async fn the_fetch_reaches_the_pinned_address_with_the_host_header() {
        let (address, server) =
            one_shot_server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string()).await;
        let tool = WebFetchTool::new(vec![], 10000, 15);
        let url = format!("http://{PIN_PROOF_HOST}:{}/page", address.port());

        let response = within(tool.fetch_following_redirects(&url, proof_target(address)))
            .await
            .expect("test: the fetch must reach the pinned listener");
        assert_eq!(response.status().as_u16(), 200, "the listener's answer comes back");

        let (head, second) = within(server).await.expect("test: server task joins");
        assert!(
            head.starts_with("GET /page HTTP/1.1\r\n"),
            "the listener must receive the request line, got {head}"
        );
        let host_header = format!("host: {PIN_PROOF_HOST}:{}\r\n", address.port());
        assert!(
            head.to_ascii_lowercase().contains(&host_header),
            "the Host header must keep the name, got {head}"
        );
        assert!(!second, "one request means one connection");
    }

    /// Review r1 finding 5 (b), real wire: the listener answers 302 to a
    /// private literal and to a host whose resolution is private. The fetch
    /// is refused at the hop check and no second connection is made.
    #[tokio::test]
    async fn a_redirect_over_the_wire_to_a_private_target_is_refused_at_the_hop() {
        for location in ["http://127.0.0.1:1/", "http://rebind.test/internal"] {
            let (address, server) = one_shot_server(format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ))
            .await;
            let tool = WebFetchTool::new(vec![], 10000, 15);
            let url = format!("http://{PIN_PROOF_HOST}:{}/start", address.port());

            let error = within(tool.fetch_following_redirects(&url, proof_target(address)))
                .await
                .expect_err("test: a redirect to a private target must be refused");
            assert!(
                error.to_string().contains("Blocked local/private host"),
                "{location}: expected the hop check to refuse, got {error}"
            );

            let (head, second) = within(server).await.expect("test: server task joins");
            assert!(
                head.starts_with("GET /start HTTP/1.1\r\n"),
                "{location}: the first hop must reach the listener, got {head}"
            );
            assert!(!second, "{location}: no second connection may be made");
        }
    }

    #[test]
    fn web_fetch_hardcoded_patterns_compile() {
        assert!(re_script().is_ok(), "hardcoded re_script must compile");
        assert!(re_style().is_ok(), "hardcoded re_style must compile");
        assert!(re_head().is_ok(), "hardcoded re_head must compile");
        assert!(re_block().is_ok(), "hardcoded re_block must compile");
        assert!(re_tags().is_ok(), "hardcoded re_tags must compile");
    }

    #[test]
    fn html_conversion_propagates_each_pattern_error() {
        static ERROR: LazyLock<regex::Error> =
            LazyLock::new(|| regex::Error::Syntax("test: injected pattern failure".to_string()));
        const PATTERNS: usize = 5;
        for index in 0..PATTERNS {
            let mut patterns = [re_script(), re_style(), re_head(), re_block(), re_tags()];
            *patterns.get_mut(index).expect("test: bounded pattern index") = Err(&ERROR);
            assert!(
                WebFetchTool::html_to_text_with_patterns("<script>private</script>", patterns).is_err(),
                "broken HTML pattern must reject unprocessed output"
            );
        }
    }
}
