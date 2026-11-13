//! Literal loopback detection and endpoint checks for decision endpoints
//! (CDP-PLAN 1.7; SP-CDP-A 5.3 rule 1; shared with the factory and the HTTP
//! client, AG-17).
//!
//! A decision endpoint may use plain `http` only when its host is written as
//! a literal loopback address. Nothing is resolved, and the check reads the
//! host exactly as the operator wrote it: a URL parser normalizes
//! `127.1`, `0x7f000001` and `2130706433` to `127.0.0.1`, and those
//! spellings are exactly what must be refused, so the parsed host is never
//! consulted for this decision.
//!
//! Accepted: a dotted quad `127.0.0.1` to `127.255.255.254` (four decimal
//! parts, no leading zeros), `[::1]`, and the exact name `localhost`, which
//! the transport maps to `127.0.0.1` without DNS. Refused: every other
//! spelling, including `127.1`, `0x7f000001`, `2130706433`,
//! `[::ffff:127.0.0.1]`, `localhost.` and `*.localhost`.

use reqwest::Url;

/// Where a checked decision endpoint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointLocality {
    /// A literal loopback address: `http` or `https`.
    Loopback,
    /// Anything else: `https` only.
    Remote,
}

/// Why a decision endpoint is refused. No variant carries the endpoint
/// text, so an error never echoes a credential written into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    /// Not an absolute URL.
    #[error("is not an absolute URL")]
    Unparseable,
    /// `user:password@` in the URL. Credentials go through `credential_ref`.
    #[error("carries user information; credentials go through `causal_tree.backend.credential_ref`, never the URL")]
    Userinfo,
    /// A query or a fragment.
    #[error("carries a query or a fragment")]
    QueryOrFragment,
    /// A scheme other than `http` or `https`.
    #[error("must use https (or http on a literal loopback address)")]
    Scheme,
    /// Plain `http` to a host that is not a literal loopback address.
    #[error(
        "uses http to a host that is not a literal loopback address (127.0.0.1 to 127.255.255.254, [::1] or \
         localhost); a remote endpoint needs https"
    )]
    PlainHttpRemote,
}

/// Whether `endpoint`'s host, as written, is a literal loopback address.
///
/// This is a spelling test only: call [`check_endpoint`] first, which also
/// refuses user information, a query, a fragment and other schemes.
#[must_use]
pub fn is_literal_loopback(endpoint: &str) -> bool {
    written_host(endpoint)
        .is_some_and(|host| host.eq_ignore_ascii_case("localhost") || host == "[::1]" || is_loopback_dotted_quad(host))
}

/// The port of an endpoint whose host is written `localhost` (any case),
/// so the client can resolve it to `127.0.0.1` itself with no DNS lookup;
/// `None` for any other host.
#[must_use]
pub fn localhost_port(endpoint: &str) -> Option<u16> {
    let host = written_host(endpoint)?;
    if !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    Url::parse(endpoint.trim()).ok()?.port_or_known_default()
}

/// Check a decision endpoint: an absolute `https` URL, or `http` on a
/// literal loopback address, with no user information, query or fragment.
///
/// # Errors
///
/// An [`EndpointError`] naming the rule; it never contains the endpoint.
pub fn check_endpoint(endpoint: &str) -> Result<EndpointLocality, EndpointError> {
    let endpoint = endpoint.trim();
    let url = Url::parse(endpoint).map_err(|_| EndpointError::Unparseable)?;
    if !url.username().is_empty() || url.password().is_some() || written_host(endpoint).is_none() {
        return Err(if url.username().is_empty() && url.password().is_none() {
            EndpointError::Unparseable
        } else {
            EndpointError::Userinfo
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(EndpointError::QueryOrFragment);
    }
    let loopback = is_literal_loopback(endpoint);
    match url.scheme() {
        "https" => Ok(if loopback {
            EndpointLocality::Loopback
        } else {
            EndpointLocality::Remote
        }),
        "http" if loopback => Ok(EndpointLocality::Loopback),
        "http" => Err(EndpointError::PlainHttpRemote),
        _ => Err(EndpointError::Scheme),
    }
}

/// The host of `endpoint` exactly as written: after `scheme://`, before the
/// path, query or fragment, without user information and port. A bracketed
/// IPv6 host keeps its brackets. `None` when there is no authority or it
/// carries user information.
fn written_host(endpoint: &str) -> Option<&str> {
    let (_, rest) = endpoint.trim().split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return None;
    }
    let host = if authority.starts_with('[') {
        let end = authority.find(']')?;
        authority.get(..=end)?
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then_some(host)
}

/// `127.x.y.z` written as four decimal parts without leading zeros, other
/// than the network (`127.0.0.0`) and broadcast (`127.255.255.255`)
/// addresses.
fn is_loopback_dotted_quad(host: &str) -> bool {
    let mut octets = [0_u8; 4];
    let mut count = 0_usize;
    for part in host.split('.') {
        let well_formed = !part.is_empty()
            && part.len() <= 3
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && !(part.len() > 1 && part.starts_with('0'));
        let Some(slot) = octets.get_mut(count) else {
            return false;
        };
        match part.parse::<u8>() {
            Ok(value) if well_formed => *slot = value,
            _ => return false,
        }
        count += 1;
    }
    count == 4 && octets[0] == 127 && octets != [127, 0, 0, 0] && octets != [127, 255, 255, 255]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_literal_loopback_spellings_are_loopback() {
        for endpoint in [
            "http://127.0.0.1:8080/v1",
            "http://127.0.0.2",
            "http://127.255.255.254/x",
            "http://[::1]:9/v1",
            "http://localhost:9/v1",
            "http://LOCALHOST",
        ] {
            assert!(is_literal_loopback(endpoint), "{endpoint}");
        }
        for endpoint in [
            "http://127.1/",
            "http://0x7f000001/",
            "http://2130706433/",
            "http://0177.0.0.1/",
            "http://127.00.0.1/",
            "http://127.0.0.0/",
            "http://127.255.255.255/",
            "http://[::ffff:127.0.0.1]/",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://localhost./",
            "http://api.localhost/",
            "http://10.0.0.1/",
            "http://example.com/",
            "http://user@127.0.0.1/",
            "not a url",
        ] {
            assert!(!is_literal_loopback(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn localhost_is_resolved_by_port_and_nothing_else_is() {
        assert_eq!(localhost_port("http://localhost:8900/v1"), Some(8900));
        assert_eq!(localhost_port("http://LOCALHOST/v1"), Some(80));
        assert_eq!(localhost_port("http://127.0.0.1:8900/v1"), None);
        assert_eq!(localhost_port("http://localhost.:8900/v1"), None);
        assert_eq!(localhost_port("https://d.example.com"), None);
    }

    /// SP-CDP-C V2.05 (endpoint rows): https anywhere, http only on literal
    /// loopback, and never user information, a query or a fragment. No error
    /// message repeats the endpoint.
    #[test]
    fn endpoints_are_checked_without_echoing_them() {
        assert_eq!(
            check_endpoint("https://decisions.example.com/v1"),
            Ok(EndpointLocality::Remote)
        );
        assert_eq!(check_endpoint("https://127.0.0.1:9/v1"), Ok(EndpointLocality::Loopback));
        assert_eq!(check_endpoint("http://127.0.0.1:9/v1"), Ok(EndpointLocality::Loopback));
        assert_eq!(check_endpoint("http://localhost:9"), Ok(EndpointLocality::Loopback));
        for (endpoint, expected) in [
            ("http://127.1:9/v1", EndpointError::PlainHttpRemote),
            ("http://2130706433/", EndpointError::PlainHttpRemote),
            ("http://decisions.example.com/v1", EndpointError::PlainHttpRemote),
            ("ftp://decisions.example.com/", EndpointError::Scheme),
            ("https://user:hunter2pw@decisions.example.com/", EndpointError::Userinfo),
            (
                "https://decisions.example.com/v1?token=abc",
                EndpointError::QueryOrFragment,
            ),
            ("https://decisions.example.com/v1#frag", EndpointError::QueryOrFragment),
            ("decisions.example.com", EndpointError::Unparseable),
        ] {
            let error = check_endpoint(endpoint).expect_err("refused");
            assert_eq!(error, expected, "{endpoint}");
            let message = error.to_string();
            assert!(
                !message.contains("hunter2pw") && !message.contains("example.com"),
                "{message}"
            );
        }
    }
}
