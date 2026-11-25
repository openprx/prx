//! App identification headers and per-endpoint static headers.
//!
//! OpenRouter and some gateways list the calling application from two
//! headers. PRX sends them to OpenRouter and to `custom:` OpenAI-compatible
//! endpoints unless `[providers] app_identification = false`. The
//! `User-Agent` (`prx/<version>`) is separate and always sent.

use super::ProviderId;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// Value of the `HTTP-Referer` header.
pub const APP_REFERER: &str = "https://github.com/openprx/prx";
/// Value of the `X-Title` header.
pub const APP_TITLE: &str = "OpenPRX";

/// The two app identification headers, or none when identification is off.
#[must_use]
pub fn app_identity_headers(enabled: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if enabled {
        headers.insert("HTTP-Referer", HeaderValue::from_static(APP_REFERER));
        headers.insert("X-Title", HeaderValue::from_static(APP_TITLE));
    }
    headers
}

/// `[providers.endpoint_headers]`: static headers per custom endpoint.
///
/// The values are credentials in practice (a gateway's app code or key), so
/// `Debug` never shows them, the headers built from them are marked
/// sensitive, and nothing logs them.
#[derive(Clone, Default)]
pub struct EndpointHeaders(Vec<(String, Vec<(String, String)>)>);

impl std::fmt::Debug for EndpointHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointHeaders")
            .field("endpoints", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl EndpointHeaders {
    /// Build from the configured map (endpoint -> header name -> value).
    #[must_use]
    pub fn from_config(map: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>) -> Self {
        Self(
            map.iter()
                .map(|(endpoint, headers)| {
                    (
                        endpoint.trim().to_string(),
                        headers
                            .iter()
                            .map(|(name, value)| (name.clone(), value.clone()))
                            .collect(),
                    )
                })
                .collect(),
        )
    }

    /// The headers declared for `provider` (a `custom:<url>` label), matched by
    /// provider identity. A header that is not a valid HTTP header is skipped
    /// with a warning that names the header, never its value.
    #[must_use]
    pub fn for_endpoint(&self, provider: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if self.0.is_empty() {
            return headers;
        }
        let id = ProviderId::of(provider);
        for (endpoint, entries) in &self.0 {
            if ProviderId::of(endpoint) != id {
                continue;
            }
            for (name, value) in entries {
                match (
                    HeaderName::from_bytes(name.trim().as_bytes()),
                    HeaderValue::from_str(value.trim()),
                ) {
                    (Ok(name), Ok(mut value)) => {
                        value.set_sensitive(true);
                        headers.insert(name, value);
                    }
                    _ => {
                        tracing::warn!(header = %name, "providers.endpoint_headers entry is not a valid HTTP header; skipped")
                    }
                }
            }
        }
        headers
    }
}
