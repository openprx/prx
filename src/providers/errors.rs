//! Provider API error bodies, sanitising and tool schema refusals.

use super::{MAX_API_ERROR_CHARS, rate_limit, reliable, traits};

/// How much of an error text [`sanitize_api_error`] scrubs. It prints only
/// [`MAX_API_ERROR_CHARS`] characters, and no credential that starts within
/// them is this long, so scrubbing more would only cost time on a large or
/// crafted upstream body (R-C2BUj J-1).
pub(super) const MAX_SCRUBBED_ERROR_BYTES: usize = 16 * 1024;

/// How much of an upstream error response body PRX reads into memory. Error
/// bodies are shown sanitized and truncated, so the rest is never needed.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// How long PRX waits for an upstream error body. The streaming client has no
/// whole-request deadline, so without this bound an upstream that sends an
/// error status and then drips (or withholds) its body held the turn until
/// the body reached [`MAX_ERROR_BODY_BYTES`] or ended (R-C2BUk K-7).
pub(crate) const ERROR_BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Read at most [`MAX_ERROR_BODY_BYTES`] of an error response body, lossily
/// decoded, for at most [`ERROR_BODY_READ_TIMEOUT`]: when the time is up, what
/// was read is used. A body that cannot be read at all yields a fixed
/// placeholder.
pub(crate) async fn read_error_body(response: reqwest::Response) -> String {
    read_error_body_within(response, ERROR_BODY_READ_TIMEOUT).await
}

/// [`read_error_body`] with its time bound given: a provider that carries its
/// own bound (the default is [`ERROR_BODY_READ_TIMEOUT`]) passes it here.
pub(crate) async fn read_error_body_within(mut response: reqwest::Response, bound: std::time::Duration) -> String {
    let mut body: Vec<u8> = Vec::new();
    let mut failed = false;
    let read = async {
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let room = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
                    body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
                    if body.len() >= MAX_ERROR_BODY_BYTES {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
    };
    if tokio::time::timeout(bound, read).await.is_err() {
        tracing::warn!(
            timeout_ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX),
            "upstream error body did not finish in time; using what was read"
        );
    }
    if failed && body.is_empty() {
        return "<failed to read provider error body>".to_string();
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// Sanitize API error text by scrubbing secrets and truncating length.
///
/// Only the first [`MAX_SCRUBBED_ERROR_BYTES`] are scrubbed; the result is
/// truncated to [`MAX_API_ERROR_CHARS`] characters anyway.
pub fn sanitize_api_error(input: &str) -> String {
    let mut prefix_end = input.len().min(MAX_SCRUBBED_ERROR_BYTES);
    while !input.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let scrubbed = crate::security::redaction::redact_secrets(input.get(..prefix_end).unwrap_or_default());

    if scrubbed.chars().count() <= MAX_API_ERROR_CHARS {
        // A scrubbed prefix of a longer text is still marked as cut.
        return if prefix_end < input.len() {
            format!("{scrubbed}...")
        } else {
            scrubbed
        };
    }

    let mut end = MAX_API_ERROR_CHARS;
    while end > 0 && !scrubbed.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}...", &scrubbed[..end])
}

/// Build a sanitized provider error from a failed HTTP response.
///
/// The real `Retry-After` header is read *before* `response.text()` consumes the
/// body and, when present, attached as a structured
/// [`RetryAfterHint`](rate_limit::RetryAfterHint) source on the returned error.
/// The reliability layer recovers it with
/// [`retry_after_hint_ms`](rate_limit::retry_after_hint_ms) and honors the
/// upstream delay instead of falling back to its own exponential guess. The
/// `Display` form is unchanged, so every existing caller and log line keeps
/// exactly the message it had before.
pub async fn api_error(provider: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let retry_after_ms = traits::retry_after_ms_from_headers(response.headers());
    let body = read_error_body(response).await;
    let sanitized = sanitize_api_error(&body);
    let message = format!("{provider} API error ({status}): {sanitized}");
    match retry_after_ms {
        Some(millis) => anyhow::Error::new(rate_limit::RetryAfterHint { millis }).context(message),
        None => anyhow::anyhow!("{message}"),
    }
}

/// Build a [`StreamError`](traits::StreamError) from a failed streaming HTTP
/// response, reading the real `Retry-After` header before the body is consumed.
///
/// FIX-P0-33: for `429` (Too Many Requests) and `503` (Service Unavailable) the
/// header is parsed into a structured [`StreamError::RateLimited`] so the
/// reliability layer can honor the upstream backoff hint without relying on the
/// error's textual form (which previously dropped the header entirely). All
/// other non-2xx statuses map to [`StreamError::Provider`] as before.
///
/// The `Retry-After` header is captured *before* `response.text()` consumes the
/// response, since reading the body takes ownership.
pub async fn stream_api_error(provider: &str, response: reqwest::Response) -> traits::StreamError {
    use traits::StreamError;
    let status = response.status();
    let code = status.as_u16();
    let retry_after_ms = traits::retry_after_ms_from_headers(response.headers());
    let body = read_error_body(response).await;
    let sanitized = sanitize_api_error(&body);
    if code == 429 || code == 503 {
        StreamError::RateLimited {
            status: code,
            retry_after_ms,
            message: format!("{provider} streaming HTTP {status}: {sanitized}"),
        }
    } else {
        StreamError::Provider(format!("{provider} streaming HTTP {status}: {sanitized}"))
    }
}

/// A request refused before transport because the endpoint's declared
/// tool-schema dialect cannot carry some of the tools it offered.
///
/// The rendering keeps the `provider_capability_error … capability=tool_schema_dialect`
/// shape every capability refusal has, so a text-only consumer (a stream
/// error) still recognises it by its start ([`is_tool_schema_refusal_text`]);
/// typed consumers downcast to this type instead of matching text. It is
/// deterministic: the same request is refused the same way every time, so the
/// reliability layer never retries it against the same provider.
#[derive(Debug, Clone, thiserror::Error)]
#[error("provider_capability_error provider={provider} capability=tool_schema_dialect message={refusals}")]
pub struct ToolSchemaRefusalError {
    /// The provider implementation that refused (its display name).
    pub provider: String,
    /// Every refused tool and why.
    pub refusals: crate::tools::ToolSchemaRefusals,
}

/// How every rendered capability refusal starts (see [`ToolSchemaRefusalError`]).
pub(super) const CAPABILITY_REFUSAL_PREFIX: &str = "provider_capability_error provider=";

/// How a stream error renders a provider message (`StreamError::Provider`),
/// the one form in which a refusal crosses the streaming boundary.
pub(super) const STREAM_PROVIDER_ERROR_PREFIX: &str = "Provider error: ";

/// Whether `text` is a rendered [`ToolSchemaRefusalError`], as the streaming
/// boundary carries it: the text *starts with* the refusal's own header
/// (optionally inside a stream error's `Provider error: ` rendering), and that
/// header names the tool-schema capability.
///
/// Text that merely contains `capability=tool_schema_dialect` is not a refusal:
/// an upstream error body is rendered after `… API error (NNN): `, and a
/// hostile or confused upstream can quote the marker there (SP-INT2 a51).
#[must_use]
pub fn is_tool_schema_refusal_text(text: &str) -> bool {
    let text = text.strip_prefix(STREAM_PROVIDER_ERROR_PREFIX).unwrap_or(text);
    text.strip_prefix(CAPABILITY_REFUSAL_PREFIX)
        .and_then(|rest| rest.split_once(" capability="))
        .is_some_and(|(_, capability)| capability.starts_with("tool_schema_dialect message="))
}

/// The refused tools, if a tool-schema refusal ended the request `error`
/// reports.
///
/// Typed first: the leaf refusal, or a failover chain whose last candidate
/// was refused, anywhere in the cause chain. A boundary that carries errors
/// as text (the streaming path) loses the type, so a cause whose text *is* a
/// rendered refusal ([`is_tool_schema_refusal_text`]) is the fallback; text
/// that only quotes one is not.
#[must_use]
pub fn tool_schema_refused_tools(error: &anyhow::Error) -> Option<Vec<String>> {
    for cause in error.chain() {
        if let Some(refusal) = cause.downcast_ref::<ToolSchemaRefusalError>() {
            return Some(refusal.refusals.tool_names().map(str::to_string).collect());
        }
        if let Some(refusal) = cause
            .downcast_ref::<reliable::ProviderChainFailure>()
            .and_then(reliable::ProviderChainFailure::tool_schema_refusal)
        {
            return Some(refusal.refusals.tool_names().map(str::to_string).collect());
        }
    }
    error
        .chain()
        .map(ToString::to_string)
        .find(|text| is_tool_schema_refusal_text(text))
        .and_then(|text| crate::tools::ToolSchemaRefusals::tool_names_in(&text))
}
