//! Webhook hook actions: POST the event envelope to an HTTP endpoint.
//!
//! A webhook is an observer like a command hook. It runs on the dispatcher
//! (never on a turn), is bounded by the action's timeout, and its outcome
//! changes nothing: a failure is logged as typed metadata (error class and
//! status class, never the URL, a header value or the body).

use std::collections::BTreeMap;
use std::time::Duration;

use hmac::{Hmac, Mac};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use sha2::Sha256;

use super::config::{HookAction, bounded_timeout};
use super::event::{HookEvent, redacted_hook_payload};
use super::hook_core::HookCore;
use super::MAX_PAYLOAD_BYTES;

/// The service key of the runtime proxy settings for webhook hooks.
const PROXY_SERVICE_KEY: &str = "hook.webhook";
/// Same header and `sha256=<hex>` form the inbound webhook verifies.
const SIGNATURE_HEADER: &str = "x-webhook-signature";
const TIMESTAMP_HEADER: &str = "x-webhook-timestamp";
/// One retry, after this pause, for a transport failure, a 5xx or a 429; the
/// action's timeout bounds both attempts together.
const RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_HEADERS_PER_ACTION: usize = 32;

/// Why a webhook delivery failed. The class names are the only thing logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WebhookError {
    PayloadTooLarge,
    InvalidTarget,
    SecretUnavailable,
    ClientUnavailable,
    Timeout,
    Connect,
    Transport,
    Status(u16),
}

impl WebhookError {
    pub(super) const fn class(self) -> &'static str {
        match self {
            Self::PayloadTooLarge => "payload_too_large",
            Self::InvalidTarget => "invalid_target",
            Self::SecretUnavailable => "secret_unavailable",
            Self::ClientUnavailable => "client_unavailable",
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Transport => "transport",
            Self::Status(_) => "status",
        }
    }

    pub(super) const fn status_class(self) -> &'static str {
        match self {
            Self::Status(300..=399) => "3xx",
            Self::Status(400..=499) => "4xx",
            Self::Status(500..=599) => "5xx",
            Self::Status(_) => "other",
            _ => "none",
        }
    }

    const fn retryable(self) -> bool {
        matches!(self, Self::Connect | Self::Transport | Self::Status(429 | 500..=599))
    }
}

fn classify(error: &reqwest::Error) -> WebhookError {
    if error.is_timeout() {
        WebhookError::Timeout
    } else if error.is_connect() {
        WebhookError::Connect
    } else if error.is_builder() {
        WebhookError::InvalidTarget
    } else {
        WebhookError::Transport
    }
}

fn parse_url(url: &str) -> Option<reqwest::Url> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    (matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some()).then_some(parsed)
}

/// `url` as safe to show: no credentials, query or fragment.
pub fn display_url(url: &str) -> String {
    let Some(mut parsed) = parse_url(url) else {
        return "invalid-url".to_string();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

fn header_pairs(headers: &BTreeMap<String, String>) -> Option<Vec<(HeaderName, HeaderValue)>> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = HeaderName::from_bytes(name.trim().as_bytes()).ok()?;
            let mut value = HeaderValue::from_str(value).ok()?;
            value.set_sensitive(true);
            Some((name, value))
        })
        .collect()
}

/// Integrity checks of a webhook action. Messages never quote the URL, a
/// header value or the secret.
pub(super) fn validate_action(
    url: &str,
    headers: &BTreeMap<String, String>,
    signing_secret: Option<&str>,
) -> anyhow::Result<()> {
    if parse_url(url).is_none() {
        anyhow::bail!("hook `url` must be an http or https URL with a host");
    }
    if headers.len() > MAX_HEADERS_PER_ACTION {
        anyhow::bail!("hook action has too many headers");
    }
    if header_pairs(headers).is_none() {
        anyhow::bail!("hook `headers` holds an invalid header name or value");
    }
    if signing_secret.is_some_and(|secret| secret.trim().is_empty()) {
        anyhow::bail!("hook `signing_secret` is empty");
    }
    Ok(())
}

fn sign(secret: &str, body: &[u8]) -> Option<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(body);
    Some(format!("sha256={}", hex::encode(mac.finalize().into_bytes())))
}

impl HookCore {
    /// The signing secret of an action: a plain value, or an `enc2:` value
    /// decrypted with the key beside config.toml.
    fn webhook_secret(&self, configured: &str) -> Result<String, WebhookError> {
        if !crate::security::SecretStore::is_encrypted(configured) {
            return Ok(configured.to_string());
        }
        let (_, launch_root) = self.workspace_policy();
        let dir = launch_root
            .as_deref()
            .and_then(std::path::Path::parent)
            .ok_or(WebhookError::SecretUnavailable)?;
        crate::security::SecretStore::with_existing_key(dir)
            .decrypt(configured)
            .map_err(|_| WebhookError::SecretUnavailable)
    }

    pub(super) async fn run_webhook(
        &self,
        event: HookEvent,
        envelope: &Value,
        default_timeout_ms: u64,
        action: &HookAction,
    ) -> Result<(), WebhookError> {
        let url = action
            .url
            .as_deref()
            .and_then(parse_url)
            .ok_or(WebhookError::InvalidTarget)?;
        let timeout_ms = bounded_timeout(action.timeout_ms.unwrap_or(default_timeout_ms))
            .map_err(|_| WebhookError::InvalidTarget)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

        // The same redaction a command hook's payload goes through.
        let body = redacted_hook_payload(envelope).to_string();
        if body.len() > MAX_PAYLOAD_BYTES {
            return Err(WebhookError::PayloadTooLarge);
        }

        let mut headers = HeaderMap::new();
        for (name, value) in header_pairs(&action.headers).ok_or(WebhookError::InvalidTarget)? {
            // The body framing is PRX's own.
            if matches!(name.as_str(), "host" | "content-length" | "transfer-encoding") {
                continue;
            }
            headers.insert(name, value);
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(configured) = action.signing_secret.as_deref() {
            let secret = self.webhook_secret(configured)?;
            let signature = sign(&secret, body.as_bytes()).ok_or(WebhookError::SecretUnavailable)?;
            let mut signature = HeaderValue::from_str(&signature).map_err(|_| WebhookError::SecretUnavailable)?;
            signature.set_sensitive(true);
            headers.insert(HeaderName::from_static(SIGNATURE_HEADER), signature);
            headers.insert(
                HeaderName::from_static(TIMESTAMP_HEADER),
                HeaderValue::from(chrono::Utc::now().timestamp()),
            );
        }

        let client = crate::config::build_runtime_proxy_client(PROXY_SERVICE_KEY)
            .map_err(|_| WebhookError::ClientUnavailable)?;
        let mut attempt = 0_u8;
        loop {
            attempt += 1;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(WebhookError::Timeout);
            }
            let sent = client
                .post(url.clone())
                .headers(headers.clone())
                .body(body.clone())
                .timeout(remaining)
                .send()
                .await;
            let failure = match sent {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => WebhookError::Status(response.status().as_u16()),
                Err(error) => classify(&error),
            };
            let time_left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if attempt >= 2 || !failure.retryable() || time_left <= RETRY_DELAY {
                tracing::debug!(event = event.as_str(), attempt, "webhook hook attempt failed");
                return Err(failure);
            }
            tokio::time::sleep(RETRY_DELAY).await;
        }
    }
}
