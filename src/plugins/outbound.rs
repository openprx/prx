//! Outbound network policy for WASM plugins (`http-outbound`, `websocket-outbound`).
//!
//! Three independent layers, all of which must pass:
//!
//! 1. **Allowlist.** A plugin manifest lists URL patterns. An empty list denies
//!    everything. A pattern is `scheme://host[:port][/path-prefix][*]`: the
//!    scheme is compared exactly, the host exactly (or, with a leading `*.`, as
//!    a subdomain on a label boundary), the port exactly (the scheme default
//!    when the pattern names none) and the path as a prefix on a segment
//!    boundary. `https://api.x.com/*` therefore never matches
//!    `https://api.x.com.evil/` or `https://api.x.com@evil/`.
//! 2. **Address guard.** Loopback, link-local, private (RFC 1918 / ULA), CGNAT,
//!    multicast, reserved and cloud-metadata addresses are refused, for
//!    literal IP hosts, for every address a name resolves to (at connect time,
//!    so a rebinding answer cannot slip between check and use), and for every
//!    redirect hop.
//! 3. **Budget.** Enforced by `HostState` (requests per invocation and an
//!    optional per-minute window).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;

/// Largest number of redirect hops a plugin request may follow.
pub const MAX_REDIRECT_HOPS: usize = 5;

/// Host part of a [`UrlPattern`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostPattern {
    /// The host must equal this string (lowercase; IPv6 keeps its brackets).
    Exact(String),
    /// The host must be a strict subdomain of this suffix (`*.suffix`).
    Subdomain(String),
}

/// One parsed allowlist entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlPattern {
    scheme: String,
    host: HostPattern,
    port: u16,
    /// Path prefix without a trailing slash; empty matches every path.
    path_prefix: String,
}

const fn default_port(scheme: &str) -> Option<u16> {
    match scheme.as_bytes() {
        b"http" | b"ws" => Some(80),
        b"https" | b"wss" => Some(443),
        _ => None,
    }
}

impl UrlPattern {
    /// Parse one allowlist entry.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| format!("allowlist pattern '{raw}' must start with a scheme such as https://"))?;
        let scheme = scheme.to_ascii_lowercase();
        let Some(scheme_default_port) = default_port(&scheme) else {
            return Err(format!(
                "allowlist pattern '{raw}' has unsupported scheme '{scheme}' (use http, https, ws or wss)"
            ));
        };
        let (authority, path) = rest.find('/').map_or((rest, ""), |index| rest.split_at(index));
        if authority.is_empty() {
            return Err(format!("allowlist pattern '{raw}' has no host"));
        }
        if authority.contains(['@', '?', '#', ' ', '\\']) || path.contains(['?', '#', '\\']) {
            return Err(format!(
                "allowlist pattern '{raw}' must not contain user info, a query, a fragment or spaces"
            ));
        }

        let (host_text, port) = split_host_port(authority, raw)?;
        let port = port.unwrap_or(scheme_default_port);
        let host = parse_host_pattern(&host_text, raw)?;

        let path = path.strip_suffix('*').unwrap_or(path);
        if path.contains('*') {
            return Err(format!(
                "allowlist pattern '{raw}' may use '*' only as the host prefix '*.' or as the final character"
            ));
        }
        let path_prefix = path.trim_end_matches('/').to_string();
        if path_prefix.split('/').any(|segment| segment == "." || segment == "..") {
            return Err(format!(
                "allowlist pattern '{raw}' must not contain '.' or '..' segments"
            ));
        }
        Ok(Self {
            scheme,
            host,
            port,
            path_prefix,
        })
    }

    /// Whether `url` falls inside this pattern.
    #[must_use]
    pub fn matches(&self, url: &Url) -> bool {
        if url.scheme() != self.scheme || !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        if url.port_or_known_default() != Some(self.port) {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        let host_ok = match &self.host {
            HostPattern::Exact(expected) => &host == expected,
            HostPattern::Subdomain(suffix) => {
                matches!(host_kind(url), HostKind::Domain(_))
                    && host
                        .strip_suffix(suffix.as_str())
                        .is_some_and(|front| front.len() > 1 && front.ends_with('.'))
            }
        };
        if !host_ok {
            return false;
        }
        if self.path_prefix.is_empty() {
            return true;
        }
        let path = url.path();
        path == self.path_prefix
            || path
                .strip_prefix(self.path_prefix.as_str())
                .is_some_and(|tail| tail.starts_with('/'))
    }
}

fn split_host_port(authority: &str, raw: &str) -> Result<(String, Option<u16>), String> {
    let (host, port_text) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (inside, after) = bracketed
            .split_once(']')
            .ok_or_else(|| format!("allowlist pattern '{raw}' has an unterminated IPv6 literal"))?;
        let port_text = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| format!("allowlist pattern '{raw}' has trailing text after the IPv6 literal"))?,
            ),
        };
        (format!("[{}]", inside.to_ascii_lowercase()), port_text)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port_text)) => (host.to_ascii_lowercase(), Some(port_text)),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    let port = port_text
        .map(|text| {
            text.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| format!("allowlist pattern '{raw}' has an invalid port '{text}'"))
        })
        .transpose()?;
    Ok((host, port))
}

fn parse_host_pattern(host: &str, raw: &str) -> Result<HostPattern, String> {
    if let Some(suffix) = host.strip_prefix("*.") {
        if suffix.contains('*') || !suffix.contains('.') || suffix.starts_with('.') || suffix.ends_with('.') {
            return Err(format!(
                "allowlist pattern '{raw}': a '*.' host needs a concrete parent domain with at least two labels"
            ));
        }
        if suffix.parse::<IpAddr>().is_ok() || suffix.starts_with('[') {
            return Err(format!(
                "allowlist pattern '{raw}': '*.' cannot be applied to an IP address"
            ));
        }
        return Ok(HostPattern::Subdomain(suffix.to_string()));
    }
    if host.is_empty() || host.contains('*') {
        return Err(format!(
            "allowlist pattern '{raw}': '*' is allowed only as a leading '*.' host label"
        ));
    }
    Ok(HostPattern::Exact(host.to_string()))
}

/// A parsed, immutable plugin outbound allowlist.
#[derive(Debug, Clone, Default)]
pub struct OutboundAllowlist {
    patterns: Vec<UrlPattern>,
}

impl OutboundAllowlist {
    /// Parse every entry, failing on the first invalid one. Used at manifest
    /// load so a typo is a load error instead of a silently dropped rule.
    pub fn parse_all(entries: &[String]) -> Result<Self, String> {
        let patterns = entries
            .iter()
            .map(|entry| UrlPattern::parse(entry))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { patterns })
    }

    /// Parse the entries that are valid. An invalid entry is dropped, which can
    /// only narrow what the plugin may reach.
    #[must_use]
    pub fn from_entries_lossy(entries: &[String]) -> Self {
        let mut patterns = Vec::with_capacity(entries.len());
        for entry in entries {
            match UrlPattern::parse(entry) {
                Ok(pattern) => patterns.push(pattern),
                Err(error) => tracing::warn!(error = %error, "ignoring invalid plugin outbound allowlist entry"),
            }
        }
        Self { patterns }
    }

    /// Whether `url` matches an entry. An empty allowlist matches nothing.
    #[must_use]
    pub fn permits(&self, url: &Url) -> bool {
        self.patterns.iter().any(|pattern| pattern.matches(url))
    }

    /// Whether the list has no usable entry.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Whether an IPv4 address must never be reached from a plugin.
#[must_use]
pub const fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // 169.254.0.0/16, includes 169.254.169.254
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (b & 0xC0) == 64) // CGNAT 100.64.0.0/10
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b & 0xFE) == 18) // benchmarking 198.18.0.0/15
        || a >= 240 // reserved
}

/// Whether an IPv6 address must never be reached from a plugin.
#[must_use]
pub fn is_forbidden_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_forbidden_ipv4(mapped);
    }
    let [s0, s1, s2, s3, s4, s5, s6, s7] = ip.segments();
    let embedded = |high: u16, low: u16| {
        let [a, b] = high.to_be_bytes();
        let [c, d] = low.to_be_bytes();
        is_forbidden_ipv4(Ipv4Addr::new(a, b, c, d))
    };
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s0 | s1 | s2 | s3 | s4 | s5) == 0 // ::/96 IPv4-compatible
        || (s0 & 0xfe00) == 0xfc00 // fc00::/7 unique local, includes fd00:ec2::254
        || (s0 & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (s0 & 0xffc0) == 0xfec0 // fec0::/10 site-local (deprecated)
        || (s0 == 0x0100 && (s1 | s2 | s3) == 0) // 100::/64 discard
        || (s0 == 0x2001 && s1 == 0) // Teredo
        || (s0 == 0x0064 && s1 == 0xff9b && s2 == 1) // 64:ff9b:1::/48 local NAT64
        || (s0 == 0x0064 && s1 == 0xff9b && (s2 | s3 | s4 | s5) == 0 && embedded(s6, s7)) // NAT64 of a private v4
        || (s0 == 0x2002 && embedded(s1, s2)) // 6to4 of a private v4
}

/// Whether `ip` must never be reached from a plugin.
#[must_use]
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4(v4),
        IpAddr::V6(v6) => is_forbidden_ipv6(v6),
    }
}

/// What the host of a URL is, after the URL parser has normalised numeric
/// spellings such as `0x7f.1` or `2130706433` to dotted form.
enum HostKind {
    Ip(IpAddr),
    Domain(String),
    Missing,
}

fn host_kind(url: &Url) -> HostKind {
    let Some(host) = url.host_str() else {
        return HostKind::Missing;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>()
        .map_or_else(|_| HostKind::Domain(host.to_ascii_lowercase()), HostKind::Ip)
}

/// Refuse a URL whose host is a forbidden IP literal or a loopback name.
/// Names that resolve to forbidden addresses are caught by the resolver guard.
pub fn check_literal_target(url: &Url) -> Result<(), String> {
    match host_kind(url) {
        HostKind::Ip(ip) if is_forbidden_ip(ip) => Err(format!("address {ip} is not reachable from plugins")),
        HostKind::Ip(_) => Ok(()),
        HostKind::Domain(name) => {
            let name = name.trim_end_matches('.');
            if name == "localhost" || name.ends_with(".localhost") {
                Err(format!("host {name} is not reachable from plugins"))
            } else {
                Ok(())
            }
        }
        HostKind::Missing => Err("URL has no host".to_string()),
    }
}

/// Parse `raw` and apply the scheme, user-info, allowlist and literal-address
/// checks. The returned URL is the exact value the request must use.
pub fn authorize_url(allowlist: &OutboundAllowlist, raw: &str, schemes: &[&str]) -> Result<Url, String> {
    if allowlist.is_empty() {
        return Err("plugin has an empty outbound allowlist; all outbound URLs are denied".to_string());
    }
    let url = Url::parse(raw).map_err(|error| format!("invalid URL: {error}"))?;
    if !schemes.contains(&url.scheme()) {
        return Err(format!("URL scheme '{}' is not allowed here", url.scheme()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with user info are not allowed".to_string());
    }
    if !allowlist.permits(&url) {
        return Err(format!("URL not in allowlist: {raw}"));
    }
    check_literal_target(&url)?;
    Ok(url)
}

/// Decide whether a redirect to `next` may be followed. Every hop must pass the
/// same allowlist and address checks as the first request.
pub fn check_redirect_hop(allowlist: &OutboundAllowlist, next: &Url, previous_hops: usize) -> Result<(), String> {
    if previous_hops >= MAX_REDIRECT_HOPS {
        return Err(format!("too many redirects (limit {MAX_REDIRECT_HOPS})"));
    }
    if !matches!(next.scheme(), "http" | "https") {
        return Err(format!("redirect to scheme '{}' refused", next.scheme()));
    }
    if !allowlist.permits(next) {
        return Err(format!("redirect target not in allowlist: {next}"));
    }
    check_literal_target(next).map_err(|error| format!("redirect refused: {error}"))
}

/// Keep the resolved addresses only when every one of them is reachable.
/// A single forbidden answer rejects the whole name, so a mixed answer cannot
/// steer the connection to the internal address.
pub fn guard_resolved(host: &str, addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    if addrs.is_empty() {
        return Err(format!("host {host} did not resolve"));
    }
    if let Some(bad) = addrs.iter().find(|addr| is_forbidden_ip(addr.ip())) {
        return Err(format!(
            "host {host} resolves to {}, which is not reachable from plugins",
            bad.ip()
        ));
    }
    Ok(addrs)
}

/// Resolve `host:port` and apply [`guard_resolved`]. An IP literal is checked
/// without a lookup.
pub async fn resolve_host_guarded(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return guard_resolved(host, vec![SocketAddr::new(ip, port)]);
    }
    let addrs = tokio::net::lookup_host((bare, port))
        .await
        .map_err(|error| format!("DNS lookup for {host} failed: {error}"))?
        .collect::<Vec<_>>();
    guard_resolved(host, addrs)
}

/// reqwest resolver that applies the address guard to every lookup it makes,
/// including the ones for redirect targets.
#[derive(Debug, Default, Clone, Copy)]
pub struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_host_guarded(&host, 0)
                .await
                .map_err(|error| Box::<dyn std::error::Error + Send + Sync>::from(error))?;
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Redirect policy that re-validates every hop.
#[must_use]
pub fn redirect_policy(allowlist: Arc<OutboundAllowlist>) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        match check_redirect_hop(&allowlist, attempt.url(), attempt.previous().len()) {
            Ok(()) => attempt.follow(),
            Err(error) => attempt.error(error),
        }
    })
}

/// Build the one HTTP client a plugin request may use: guarded DNS, guarded
/// redirects, no ambient proxy (a proxy would resolve names itself and bypass
/// the address guard).
pub fn build_http_client(allowlist: Arc<OutboundAllowlist>, timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(GuardedResolver))
        .redirect(redirect_policy(allowlist))
        .timeout(timeout)
        .build()
        .map_err(|error| format!("failed to build the plugin HTTP client: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    use super::*;

    fn list(entries: &[&str]) -> OutboundAllowlist {
        OutboundAllowlist::parse_all(&entries.iter().map(ToString::to_string).collect::<Vec<_>>())
            .expect("test: valid allowlist")
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test: valid url")
    }

    #[test]
    fn an_empty_allowlist_denies_every_url() {
        let empty = OutboundAllowlist::default();
        assert!(authorize_url(&empty, "https://api.example.com/", &["https"]).is_err());
        assert!(!empty.permits(&url("https://api.example.com/")));
    }

    #[test]
    fn host_must_match_exactly_not_by_string_prefix() {
        let allowed = list(&["https://api.x.com/*"]);
        assert!(allowed.permits(&url("https://api.x.com/v1/a")));
        assert!(allowed.permits(&url("https://API.X.com/")));
        assert!(!allowed.permits(&url("https://api.x.com.evil/")));
        assert!(!allowed.permits(&url("https://api.x.com.evil.example/path")));
        assert!(!allowed.permits(&url("https://api.x.com@evil.example/")));
        assert!(!allowed.permits(&url("https://evil.example/api.x.com/")));
        assert!(!allowed.permits(&url("https://xapi.x.com/")));
        // A bare trailing star on the host is the old string-prefix form; it is invalid now.
        assert!(UrlPattern::parse("https://api.x.com*").is_err());
    }

    #[test]
    fn scheme_and_port_are_exact() {
        let allowed = list(&["https://api.x.com/*"]);
        assert!(!allowed.permits(&url("http://api.x.com/")));
        assert!(!allowed.permits(&url("https://api.x.com:8443/")));
        assert!(allowed.permits(&url("https://api.x.com:443/")));
        let with_port = list(&["https://api.x.com:8443/*"]);
        assert!(with_port.permits(&url("https://api.x.com:8443/a")));
        assert!(!with_port.permits(&url("https://api.x.com/a")));
        let ws = list(&["wss://stream.x.com/*"]);
        assert!(ws.permits(&url("wss://stream.x.com/feed")));
        assert!(!ws.permits(&url("ws://stream.x.com/feed")));
    }

    #[test]
    fn wildcard_subdomain_matches_on_label_boundaries() {
        let allowed = list(&["https://*.x.com/*"]);
        assert!(allowed.permits(&url("https://api.x.com/")));
        assert!(allowed.permits(&url("https://a.b.x.com/")));
        assert!(!allowed.permits(&url("https://x.com/")));
        assert!(!allowed.permits(&url("https://evilx.com/")));
        assert!(!allowed.permits(&url("https://api.x.com.evil/")));
        assert!(!allowed.permits(&url("https://notx.com/")));
    }

    #[test]
    fn path_prefix_matches_on_segment_boundaries() {
        let allowed = list(&["https://api.x.com/v1/*"]);
        assert!(allowed.permits(&url("https://api.x.com/v1")));
        assert!(allowed.permits(&url("https://api.x.com/v1/users")));
        assert!(!allowed.permits(&url("https://api.x.com/v10/users")));
        assert!(!allowed.permits(&url("https://api.x.com/v2/users")));
        // Dot segments are folded by the URL parser before matching.
        assert!(!allowed.permits(&url("https://api.x.com/v1/../v2/users")));
        assert!(!allowed.permits(&url("https://api.x.com/v1/%2e%2e/admin")));
    }

    #[test]
    fn user_info_never_matches() {
        let allowed = list(&["https://api.x.com/*"]);
        assert!(!allowed.permits(&url("https://user:pw@api.x.com/")));
        assert!(authorize_url(&allowed, "https://user@api.x.com/", &["https"]).is_err());
    }

    #[test]
    fn invalid_patterns_are_rejected_with_a_message() {
        for bad in [
            "api.x.com",
            "ftp://api.x.com/",
            "https://",
            "https://*/",
            "https://*.com/",
            "https://a*.x.com/",
            "https://api.x.com/a*b",
            "https://api.x.com:0/",
            "https://api.x.com:notaport/",
            "https://user@api.x.com/",
            "https://api.x.com/../x",
        ] {
            assert!(UrlPattern::parse(bad).is_err(), "{bad} should be rejected");
        }
        let error = UrlPattern::parse("api.x.com").unwrap_err();
        assert!(error.contains("scheme"), "{error}");
    }

    #[test]
    fn forbidden_ipv4_ranges() {
        for ip in [
            "127.0.0.1",
            "127.255.255.254",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
        ] {
            assert!(is_forbidden_ip(ip.parse().unwrap()), "{ip} must be forbidden");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "172.32.0.1", "100.128.0.1", "203.0.113.5"] {
            assert!(!is_forbidden_ip(ip.parse().unwrap()), "{ip} must be allowed");
        }
    }

    #[test]
    fn forbidden_ipv6_ranges() {
        for ip in [
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd00:ec2::254",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "fec0::1",
        ] {
            assert!(is_forbidden_ip(ip.parse().unwrap()), "{ip} must be forbidden");
        }
        for ip in ["2606:4700:4700::1111", "2001:4860:4860::8888", "::ffff:8.8.8.8"] {
            assert!(!is_forbidden_ip(ip.parse().unwrap()), "{ip} must be allowed");
        }
    }

    #[test]
    fn literal_private_hosts_are_refused_even_when_the_allowlist_names_them() {
        let allowed = list(&[
            "http://127.0.0.1/*",
            "http://169.254.169.254/*",
            "http://[fd00:ec2::254]/*",
            "http://localhost/*",
            "http://10.0.0.5/*",
            "http://metadata.localhost/*",
        ]);
        for target in [
            "http://127.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[fd00:ec2::254]/",
            "http://localhost/",
            "http://10.0.0.5/",
            "http://metadata.localhost/",
        ] {
            assert!(
                authorize_url(&allowed, target, &["http"]).is_err(),
                "{target} must be refused"
            );
        }
        // A public literal that the list names is fine.
        let public = list(&["http://203.0.113.5/*"]);
        assert!(authorize_url(&public, "http://203.0.113.5/x", &["http"]).is_ok());
    }

    #[test]
    fn resolved_addresses_are_checked_after_dns() {
        let ok = vec![SocketAddr::from(([93, 184, 216, 34], 0))];
        assert!(guard_resolved("example.com", ok).is_ok());

        let private = vec![SocketAddr::from(([10, 0, 0, 5], 0))];
        assert!(guard_resolved("rebind.example", private).is_err());

        let metadata = vec![SocketAddr::from(([169, 254, 169, 254], 0))];
        assert!(guard_resolved("meta.example", metadata).is_err());

        let mixed = vec![
            SocketAddr::from(([93, 184, 216, 34], 0)),
            SocketAddr::from(([127, 0, 0, 1], 0)),
        ];
        let error = guard_resolved("mixed.example", mixed).unwrap_err();
        assert!(error.contains("127.0.0.1"), "{error}");

        let v6 = vec![SocketAddr::new("fd00:ec2::254".parse().unwrap(), 0)];
        assert!(guard_resolved("meta6.example", v6).is_err());

        assert!(guard_resolved("empty.example", Vec::new()).is_err());
    }

    #[tokio::test]
    async fn the_system_resolver_answer_for_localhost_is_refused() {
        let error = resolve_host_guarded("localhost", 80).await.unwrap_err();
        assert!(error.contains("not reachable"), "{error}");
        assert!(resolve_host_guarded("127.0.0.1", 80).await.is_err());
        assert!(resolve_host_guarded("[::1]", 80).await.is_err());
        assert!(resolve_host_guarded("8.8.8.8", 443).await.is_ok());
    }

    #[test]
    fn redirect_hops_are_revalidated() {
        let allowed = list(&["https://api.x.com/*", "https://*.cdn.x.com/*"]);
        assert!(check_redirect_hop(&allowed, &url("https://api.x.com/next"), 0).is_ok());
        assert!(check_redirect_hop(&allowed, &url("https://a.cdn.x.com/file"), 1).is_ok());
        // Off-list host, downgrade scheme, private literal, metadata literal.
        assert!(check_redirect_hop(&allowed, &url("https://evil.example/"), 0).is_err());
        assert!(check_redirect_hop(&allowed, &url("http://api.x.com/"), 0).is_err());
        assert!(check_redirect_hop(&allowed, &url("https://127.0.0.1/"), 0).is_err());
        assert!(check_redirect_hop(&allowed, &url("http://169.254.169.254/"), 0).is_err());
        // Even an allowlisted name pointing at a private literal is refused.
        let internal = list(&["http://10.0.0.5/*"]);
        assert!(check_redirect_hop(&internal, &url("http://10.0.0.5/"), 0).is_err());
        // Hop limit.
        assert!(check_redirect_hop(&allowed, &url("https://api.x.com/"), MAX_REDIRECT_HOPS).is_err());
        // Non-http schemes.
        assert!(check_redirect_hop(&allowed, &url("file:///etc/passwd"), 0).is_err());
    }

    #[test]
    fn the_client_builds_with_the_guards_installed() {
        let client = build_http_client(Arc::new(list(&["https://api.x.com/*"])), Duration::from_secs(1));
        assert!(client.is_ok());
    }
}
