//! Error classification for the reliable provider: which failures are
//! non-retryable, rate limits or overloads, and how a pre-content stream
//! failure is classified for retry and fallback.

use super::retry_policy::RetryKind;

// ── Error Classification ─────────────────────────────────────────────────
// Errors are split into retryable (transient server/network failures) and
// non-retryable (permanent client errors). This distinction drives whether
// the retry loop continues, falls back to the next provider, or aborts
// immediately — avoiding wasted latency on errors that cannot self-heal.

/// Whether `err` is a tool-schema refusal: the endpoint's declared dialect
/// cannot carry a tool the request offered, so nothing was sent.
///
/// Typed, never matched on text: the refusal is deterministic, and a text
/// heuristic both missed it (it was retried with backoff) and could
/// misclassify an unrelated error whose tool or model name happened to
/// contain a matching word.
pub(super) fn is_tool_schema_refusal(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<super::super::ToolSchemaRefusalError>().is_some())
}

/// Check if an error is non-retryable (client errors that won't resolve with retries).
pub(super) fn is_non_retryable(err: &anyhow::Error) -> bool {
    if crate::recovery::overflow::is_context_overflow_error(err) || is_tool_schema_refusal(err) {
        return true;
    }

    // 4xx errors are generally non-retryable (bad request, auth failure, etc.),
    // except 429 (rate-limit — transient) and 408 (timeout — worth retrying).
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>() {
        if let Some(status) = reqwest_err.status() {
            let code = status.as_u16();
            return status.is_client_error() && code != 429 && code != 408;
        }
    }
    // Fallback: parse status codes from stringified errors (some providers
    // embed codes in error messages rather than returning typed HTTP errors).
    let msg = err.to_string();
    for word in msg.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(code) = word.parse::<u16>() {
            if (400..500).contains(&code) {
                return code != 429 && code != 408;
            }
        }
    }

    // Heuristic: detect auth/model failures by keyword when no HTTP status
    // is available (e.g. gRPC or custom transport errors).
    let msg_lower = msg.to_lowercase();
    let auth_failure_hints = [
        "invalid api key",
        "incorrect api key",
        "missing api key",
        "api key not set",
        "authentication failed",
        "auth failed",
        "unauthorized",
        "forbidden",
        "permission denied",
        "access denied",
        "invalid token",
    ];

    if auth_failure_hints.iter().any(|hint| msg_lower.contains(hint)) {
        return true;
    }
    if msg_lower.contains("provider_response_parse_error") {
        return is_non_retryable_provider_response_parse_error(&msg_lower);
    }

    msg_lower.contains("model")
        && (msg_lower.contains("not found")
            || msg_lower.contains("unknown")
            || msg_lower.contains("unsupported")
            || msg_lower.contains("does not exist")
            || msg_lower.contains("invalid"))
}

pub(super) fn is_non_retryable_provider_response_parse_error(msg_lower: &str) -> bool {
    const NON_RETRYABLE_PARSE_ERROR_KINDS: [&str; 3] = [
        "kind=malformed_json",
        "kind=empty_or_unsupported_payload",
        "kind=payload_too_large",
    ];
    const RETRYABLE_PARSE_ERROR_KINDS: [&str; 1] = ["kind=body_read_failed"];
    const TRANSIENT_BODY_READ_HINTS: [&str; 9] = [
        "connection reset",
        "connection aborted",
        "connection closed",
        "connection refused",
        "broken pipe",
        "network unreachable",
        "temporary failure",
        "timed out",
        "timeout",
    ];

    if NON_RETRYABLE_PARSE_ERROR_KINDS
        .iter()
        .any(|kind| msg_lower.contains(kind))
    {
        return true;
    }

    if RETRYABLE_PARSE_ERROR_KINDS.iter().any(|kind| msg_lower.contains(kind)) {
        return false;
    }

    // Keep transient network read failures retryable even when kind tagging
    // differs by provider implementation details.
    if TRANSIENT_BODY_READ_HINTS.iter().any(|hint| msg_lower.contains(hint)) {
        return false;
    }

    // Unknown parse kinds remain non-retryable by default to preserve
    // fail-fast behavior on deterministic payload/protocol mismatches.
    true
}

/// Check if an error is a rate-limit (429) error.
pub(super) fn is_rate_limited(err: &anyhow::Error) -> bool {
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>() {
        if let Some(status) = reqwest_err.status() {
            return status.as_u16() == 429;
        }
    }
    let msg = err.to_string();
    msg.contains("429") && (msg.contains("Too Many") || msg.contains("rate") || msg.contains("limit"))
}

/// Whether a provider error text names an overload: HTTP 503, or an error
/// body whose type/code says overloaded or rate limited (`engine_overloaded`,
/// `overloaded_error`, `rate_limit_exceeded`).
pub(super) fn names_overload(lower: &str) -> bool {
    lower.contains("(503") || lower.contains("http 503") || lower.contains("overloaded") || lower.contains("rate_limit")
}

/// Whether a retryable buffered failure is an overload response (HTTP 429 or
/// 503, or an overloaded / rate-limited error body). Callers have already
/// ruled out non-retryable errors, business 429s included.
pub(super) fn is_overload(err: &anyhow::Error) -> bool {
    if let Some(status) = err.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status) {
        return matches!(status.as_u16(), 429 | 503);
    }
    is_rate_limited(err) || names_overload(&err.to_string().to_lowercase())
}

/// Classify a retryable buffered failure. Callers have already ruled out
/// non-retryable errors, business 429s included.
pub(super) fn retry_kind(err: &anyhow::Error) -> RetryKind {
    if is_rate_limited(err) {
        RetryKind::RateLimited
    } else if is_overload(err) {
        RetryKind::Overload
    } else {
        RetryKind::Ordinary
    }
}

/// Business error codes observed on 429 responses where retrying is futile
/// (Z.AI / GLM: plan does not cover the model, insufficient balance).
pub(super) const NON_RETRYABLE_RATE_LIMIT_CODES: [u16; 2] = [1113, 1311];

/// Match a provider business code only where it appears as the value of a
/// `code`-like key (`"code":1311`, `error_code = 1113`, ...).
///
/// A bare digit scan over the whole message is not safe: an ordinary 429 body
/// carries a `request_id`, token counters and timestamps, and any digit run
/// that happened to read `1113` or `1311` used to classify that response as
/// permanently failed — silently skipping the entire retry budget. Requiring
/// the key context keeps the real business codes matched while making an
/// accidental hit impossible. Default is "retryable".
pub(super) fn has_non_retryable_business_code(lower: &str) -> bool {
    lower.match_indices("code").any(|(idx, keyword)| {
        let Some(rest) = lower.get(idx + keyword.len()..) else {
            return false;
        };
        let value = rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ':' | '='));
        let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse::<u16>()
            .is_ok_and(|code| NON_RETRYABLE_RATE_LIMIT_CODES.contains(&code))
    })
}

/// Check if a 429 is a business/quota-plan error that retries cannot fix.
///
/// Examples:
/// - plan does not include requested model
/// - insufficient balance / package not active
/// - known provider business codes (e.g. Z.AI: 1311, 1113)
pub(super) fn is_non_retryable_rate_limit(err: &anyhow::Error) -> bool {
    if !is_rate_limited(err) {
        return false;
    }
    is_business_rate_limit_message(&err.to_string().to_lowercase())
}

/// Whether an already-confirmed 429 body describes a billing/entitlement
/// condition rather than transient throttling.
///
/// Split out from [`is_non_retryable_rate_limit`] so the streaming path — which
/// learns the `429` from the structured status code, not from the text — can
/// apply the same verdict without the message having to spell out "429".
pub(super) fn is_business_rate_limit_message(lower: &str) -> bool {
    // Every hint below names a *billing or entitlement* condition. The bare
    // `"not include"` fragment used to live here too and was far too broad —
    // any upstream sentence containing those two words (for example "this
    // error does not include a request id") turned a transient 429 into a
    // permanent failure.
    let business_hints = [
        "plan does not include",
        "does not include",
        "doesn't include",
        "insufficient balance",
        "insufficient_balance",
        "insufficient quota",
        "insufficient_quota",
        "quota exhausted",
        "out of credits",
        "no available package",
        "package not active",
        "purchase package",
        "model not available for your plan",
    ];

    if business_hints.iter().any(|hint| lower.contains(hint)) {
        return true;
    }

    has_non_retryable_business_code(lower)
}

/// Classification of a streaming error, mirroring the non-streaming
/// `is_*` classifiers so the streaming fallback driver can make the same
/// retry/fallback/abort decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamFailureClass {
    /// Rate-limited (429) and recoverable by trying another provider/model.
    RateLimited,
    /// Input exceeds the model context window — fall back to a larger model if
    /// one remains in the chain, otherwise abort (retrying the same model is futile).
    ContextOverflow,
    /// Permanent client error (auth/model-not-found/business 429) — do not retry,
    /// but a different compatible provider/model may still succeed.
    NonRetryable,
    /// Transient server/network failure — worth trying the next candidate.
    Retryable,
}

impl StreamFailureClass {
    /// Whether falling back to the next candidate is worthwhile for this class.
    /// Context overflow only benefits from fallback to a *different* (larger)
    /// model; that gating is applied by the driver using the candidate list.
    pub(super) const fn allows_fallback(self) -> bool {
        matches!(self, Self::RateLimited | Self::Retryable | Self::ContextOverflow)
    }
}

/// Start of the error PRX builds when a stream reports a content-filter stop.
/// Providers build it only from structured fields (`finish_reason:
/// content_filter`, or an in-stream error whose `code`/`type` names the
/// filter), never from upstream prose.
pub(crate) const CONTENT_FILTER_STOP_PREFIX: &str = "content filter stop";

/// Start of the error PRX builds when a streamed tool-call index exceeds the
/// supported maximum.
pub(crate) const TOOL_CALL_INDEX_LIMIT_PREFIX: &str = "streamed tool-call index over the limit";

/// Deterministic pre-content failures that PRX itself detected from
/// structured stream fields. Retrying the same provider in place cannot change
/// the outcome, so these skip only the in-place retry; another candidate may
/// still succeed (filters and parsers differ by provider), so fallback stays.
///
/// Matched by the PRX-built prefix at the start of the message, never by a
/// phrase anywhere in upstream text.
pub(super) fn skips_in_place_retry(err: &super::super::traits::StreamError) -> bool {
    use super::super::traits::StreamError;
    match err {
        StreamError::Provider(message) => message.starts_with(CONTENT_FILTER_STOP_PREFIX),
        StreamError::InvalidSse(message) => message.starts_with(TOOL_CALL_INDEX_LIMIT_PREFIX),
        _ => false,
    }
}

/// Whether a stream error is PRX's own tool-schema refusal: a provider
/// message that is a rendered refusal ([`super::super::is_tool_schema_refusal_text`]).
pub(super) fn stream_error_is_tool_schema_refusal(err: &super::super::traits::StreamError) -> bool {
    matches!(err, super::super::traits::StreamError::Provider(message) if super::super::is_tool_schema_refusal_text(message))
}

/// Classify a [`StreamError`] for failover decisions. Reuses the same
/// string/`reqwest` heuristics as the non-streaming path by routing through the
/// shared `anyhow` classifiers where possible.
pub(super) fn classify_stream_error(err: &super::super::traits::StreamError) -> StreamFailureClass {
    use super::super::traits::StreamError;

    // A failure PRX detected from structured stream fields (a content-filter
    // stop, a tool-call index over the limit) is classified by what it is:
    // deterministic for this candidate, so no in-place retry, and worth a
    // fallback. Its message quotes upstream text (the filter's own words, the
    // index), which the prose heuristics below would misread: `(403)` or
    // "invalid model" in a filter message ended the whole chain, and a
    // tool-call index of 404 or 429 read as an HTTP status (COMPATf F-1).
    if skips_in_place_retry(err) {
        return StreamFailureClass::Retryable;
    }

    // Structured rate-limit (FIX-P0-33): a 429 is rate-limited; a 503 is a
    // transient server failure worth retrying/falling back. Both already carry
    // the parsed Retry-After hint, so the transport classification needs no
    // textual heuristics.
    //
    // The message still has to be inspected for *business* 429s (plan does not
    // cover the model, insufficient balance, ...): those never clear on their
    // own, and since the driver now retries a `RateLimited` class even without
    // a Retry-After hint, misclassifying one would burn the whole retry budget
    // waiting for a quota that is not coming back. This keeps the streaming and
    // non-streaming verdicts identical for the same upstream body.
    if let StreamError::RateLimited { status, message, .. } = err {
        if *status == 429 && is_business_rate_limit_message(&message.to_lowercase()) {
            return StreamFailureClass::NonRetryable;
        }
        return if *status == 429 {
            StreamFailureClass::RateLimited
        } else {
            StreamFailureClass::Retryable
        };
    }

    // HTTP transport errors carry a status we can map directly.
    if let StreamError::Http(http_err) = err {
        if let Some(status) = http_err.status() {
            let code = status.as_u16();
            if code == 429 {
                return StreamFailureClass::RateLimited;
            }
            if code == 413 {
                return StreamFailureClass::ContextOverflow;
            }
            if status.is_client_error() && code != 408 {
                return StreamFailureClass::NonRetryable;
            }
        }
        // Timeouts / connects / unknown HTTP failures are transient.
        return StreamFailureClass::Retryable;
    }

    // For message-bearing variants, reuse the anyhow-based classifiers so the
    // streaming path stays in lock-step with the non-streaming path.
    let message = match err {
        StreamError::Provider(msg) | StreamError::InvalidSse(msg) => msg.clone(),
        StreamError::Json(e) => e.to_string(),
        StreamError::Io(_) => return StreamFailureClass::Retryable,
        // `StreamError::Http` and `StreamError::RateLimited` are fully classified
        // by the blocks above, which always return before reaching this match.
        // Should that invariant ever change, treat the failure as transient
        // rather than panicking.
        StreamError::Http(_) | StreamError::RateLimited { .. } => return StreamFailureClass::Retryable,
    };
    // The stream boundary carries errors as text, so a tool-schema refusal is
    // recognised by the refusal's own rendering at the start of a provider
    // message, never by the marker anywhere in upstream text (SP-INT2 a51).
    // It is deterministic: retrying the same provider cannot help.
    if stream_error_is_tool_schema_refusal(err) {
        return StreamFailureClass::NonRetryable;
    }
    let anyhow_err = anyhow::anyhow!("{message}");

    if crate::recovery::overflow::is_context_overflow_error(&anyhow_err) {
        return StreamFailureClass::ContextOverflow;
    }
    if is_non_retryable_rate_limit(&anyhow_err) {
        return StreamFailureClass::NonRetryable;
    }
    if is_rate_limited(&anyhow_err) {
        return StreamFailureClass::RateLimited;
    }
    if is_non_retryable(&anyhow_err) {
        return StreamFailureClass::NonRetryable;
    }
    StreamFailureClass::Retryable
}

/// The [`RetryKind`] of a pre-content stream failure: a rate limit (429) is
/// [`RetryKind::RateLimited`], otherwise overload or ordinary per
/// [`stream_error_is_overload`].
pub(super) fn stream_retry_kind(err: &super::super::traits::StreamError, class: StreamFailureClass) -> RetryKind {
    if class == StreamFailureClass::RateLimited {
        RetryKind::RateLimited
    } else if stream_error_is_overload(err, class) {
        RetryKind::Overload
    } else {
        RetryKind::Ordinary
    }
}

/// Whether a retryable pre-content stream failure is an overload response:
/// HTTP 503, or an in-stream error whose type/code says overloaded or rate
/// limited.
pub(super) fn stream_error_is_overload(err: &super::super::traits::StreamError, class: StreamFailureClass) -> bool {
    use super::super::traits::StreamError;
    match (class, err) {
        (StreamFailureClass::Retryable, StreamError::RateLimited { status, .. }) => *status == 503,
        (StreamFailureClass::Retryable, StreamError::Http(http)) => http.status().is_some_and(|s| s.as_u16() == 503),
        (StreamFailureClass::Retryable, StreamError::Provider(message) | StreamError::InvalidSse(message)) => {
            names_overload(&message.to_lowercase())
        }
        _ => false,
    }
}
