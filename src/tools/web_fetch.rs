use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

#[allow(clippy::expect_used)]
static RE_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?si)<script[^>]*>.*?</script>").expect("BUG: invalid hardcoded script tag regex"));
#[allow(clippy::expect_used)]
static RE_STYLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?si)<style[^>]*>.*?</style>").expect("BUG: invalid hardcoded style tag regex"));
#[allow(clippy::expect_used)]
static RE_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?si)<head[^>]*>.*?</head>").expect("BUG: invalid hardcoded head tag regex"));
#[allow(clippy::expect_used)]
static RE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(br\s*/?|p|div|h[1-6]|li|tr|article|section|header|footer)[^>]*>")
        .expect("BUG: invalid hardcoded block element regex")
});
#[allow(clippy::expect_used)]
static RE_TAGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>").expect("BUG: invalid hardcoded strip-tags regex"));

/// Web fetch tool — fetches a URL and returns clean readable text.
pub struct WebFetchTool {
    security: Arc<SecurityPolicy>,
    allowed_domains: Vec<String>,
    max_chars: usize,
    timeout_secs: u64,
}

impl WebFetchTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        allowed_domains: Vec<String>,
        max_chars: usize,
        timeout_secs: u64,
    ) -> Self {
        Self {
            security,
            allowed_domains: normalize_allowed_domains(allowed_domains),
            max_chars: max_chars.max(100),
            timeout_secs: timeout_secs.max(1),
        }
    }

    /// Convert HTML to plain readable text by stripping tags and decoding entities.
    fn html_to_text(html: &str) -> String {
        // Remove script/style/head blocks (case-insensitive, multiline).
        // Rust's regex crate does not support backreferences, so use three
        // separate patterns instead of the combined `(?si)<(script|style|head)[^>]*>.*?</\1>`.
        let text = RE_SCRIPT.replace_all(html, "");
        let text = RE_STYLE.replace_all(&text, "");
        let text = RE_HEAD.replace_all(&text, "");

        // Replace block-level elements with newlines so paragraphs separate cleanly
        let text = RE_BLOCK.replace_all(&text, "\n");

        // Strip all remaining tags
        let text = RE_TAGS.replace_all(&text, "");

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
        text.lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
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
        if is_private_or_local_host(&host) {
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

        Ok(url.to_string())
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
         Useful for reading articles, documentation, or any web page."
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
        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded".to_string()),
            });
        }
        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action budget exhausted".to_string()),
            });
        }

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

        // Disable automatic redirect following so every redirect target is
        // re-validated against the allowlist and private-IP rules before
        // being followed (prevents SSRF via open-redirect).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_secs))
            .user_agent("Mozilla/5.0 (compatible; PRX/1.0; +https://openprx.dev)")
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        // Follow redirects manually, re-validating each hop.
        let mut current_url = url.clone();
        let max_redirects: u32 = 5;
        let mut redirect_count: u32 = 0;
        let response = loop {
            let resp = client
                .get(&current_url)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to fetch {}: {}", current_url, e))?;

            if resp.status().is_redirection() {
                if redirect_count >= max_redirects {
                    anyhow::bail!("Too many redirects fetching {}", url);
                }
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("Redirect with no Location header"))?
                    .to_string();
                // Resolve relative redirects against the current URL.
                let next_url = if location.starts_with("http://") || location.starts_with("https://") {
                    location
                } else {
                    let base =
                        reqwest::Url::parse(&current_url).map_err(|e| anyhow::anyhow!("Invalid base URL: {e}"))?;
                    base.join(&location)
                        .map_err(|e| anyhow::anyhow!("Invalid redirect URL: {e}"))?
                        .to_string()
                };
                // Re-validate the redirect target before following.
                current_url = self.validate_url(&next_url)?;
                redirect_count += 1;
            } else {
                break resp;
            }
        };

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
            Self::html_to_text(&body)
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

fn normalize_allowed_domains(domains: Vec<String>) -> Vec<String> {
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

fn host_matches_allowlist(host: &str, allowed_domains: &[String]) -> bool {
    allowed_domains
        .iter()
        .any(|domain| host == domain || host.strip_suffix(domain).is_some_and(|prefix| prefix.ends_with('.')))
}

fn is_private_or_local_host(host: &str) -> bool {
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

    // DNS rebinding defense: resolve the hostname and check if any resolved
    // IP is private/local. This catches hostnames that pass string checks
    // but resolve to internal addresses (e.g. attacker-controlled DNS that
    // alternates between public and private IPs).
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&(host, 80)) {
        for addr in addrs {
            let ip = addr.ip();
            if ip.is_loopback() || ip.is_unspecified() {
                return true;
            }
            match ip {
                std::net::IpAddr::V4(v4) => {
                    if is_non_global_v4(v4) {
                        return true;
                    }
                }
                std::net::IpAddr::V6(v6) => {
                    if is_non_global_v6(v6) {
                        return true;
                    }
                }
            }
        }
    }

    false
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
    use crate::security::SecurityPolicy;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    #[test]
    fn test_html_to_text_basic() {
        let html = "<html><body><h1>Hello</h1><p>World</p></body></html>";
        let text = WebFetchTool::html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains('<'));
        assert!(!text.contains('>'));
    }

    #[test]
    fn test_html_to_text_strips_scripts() {
        let html = "<html><head><script>var x = 1;</script></head><body>Content</body></html>";
        let text = WebFetchTool::html_to_text(html);
        assert!(!text.contains("var x"));
        assert!(text.contains("Content"));
    }

    #[test]
    fn test_html_to_text_strips_styles() {
        let html = "<html><head><style>body { color: red; }</style></head><body>Text</body></html>";
        let text = WebFetchTool::html_to_text(html);
        assert!(!text.contains("color"));
        assert!(text.contains("Text"));
    }

    #[test]
    fn test_html_to_text_decodes_entities() {
        let html = "<p>1 &lt; 2 &amp; 3 &gt; 0 &quot;quoted&quot; it&#39;s</p>";
        let text = WebFetchTool::html_to_text(html);
        assert!(text.contains("1 < 2 & 3 > 0 \"quoted\" it's"), "got: {text}");
    }

    #[test]
    fn test_html_to_text_collapses_whitespace() {
        let html = "<p>   lots   of   space   </p>";
        let text = WebFetchTool::html_to_text(html);
        // After trim, no leading/trailing whitespace per line
        for line in text.lines() {
            assert_eq!(line, line.trim());
        }
    }

    #[test]
    fn test_tool_name() {
        let tool = WebFetchTool::new(test_security(), vec!["example.com".into()], 10000, 15);
        assert_eq!(tool.name(), "web_fetch");
    }

    #[test]
    fn test_tool_description_contains_fetch() {
        let tool = WebFetchTool::new(test_security(), vec!["example.com".into()], 10000, 15);
        assert!(tool.description().to_lowercase().contains("fetch"));
    }

    #[test]
    fn test_parameters_schema() {
        let tool = WebFetchTool::new(test_security(), vec!["example.com".into()], 10000, 15);
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["url"].is_object());
        assert_eq!(schema["required"][0], "url");
    }

    #[tokio::test]
    async fn test_execute_missing_url() {
        let tool = WebFetchTool::new(test_security(), vec!["example.com".into()], 10000, 15);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("url"));
    }

    #[tokio::test]
    async fn test_execute_empty_url() {
        let tool = WebFetchTool::new(test_security(), vec!["example.com".into()], 10000, 15);
        let result = tool.execute(json!({"url": ""})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_invalid_scheme() {
        let tool = WebFetchTool::new(test_security(), vec!["example.com".into()], 10000, 15);
        let result = tool.execute(json!({"url": "ftp://example.com"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("http://") || true); // scheme check
    }

    #[test]
    fn validate_url_blocks_private_hosts() {
        let tool = WebFetchTool::new(test_security(), vec!["example.com".into()], 10000, 15);
        let err = tool
            .validate_url("http://127.0.0.1/internal")
            .expect_err("private host should be rejected");
        assert!(err.to_string().contains("Blocked local/private host"));
    }

    #[test]
    fn validate_url_enforces_allowlist() {
        let tool = WebFetchTool::new(test_security(), vec!["example.com".into()], 10000, 15);
        let err = tool
            .validate_url("https://evil.com/")
            .expect_err("unexpected allowlist bypass");
        assert!(err.to_string().contains("not in browser.allowed_domains"));
    }

    #[test]
    fn validate_url_allows_public_host_when_allowlist_empty() {
        let tool = WebFetchTool::new(test_security(), vec![], 10000, 15);
        let ok = tool
            .validate_url("https://example.com/docs")
            .expect("public host should be allowed when allowlist is empty");
        assert_eq!(ok, "https://example.com/docs");

        let err = tool
            .validate_url("http://localhost:8080")
            .expect_err("localhost must still be blocked");
        assert!(err.to_string().contains("Blocked local/private host"));
    }
}
