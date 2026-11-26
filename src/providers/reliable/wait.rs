//! Retry wait planning for the reliable provider: the wait before an overload
//! retry, the `Retry-After` hint parsers, and the jittered wait planned for
//! a failed attempt on the same candidate.

use super::super::rate_limit::{
    MAX_HONORED_RETRY_AFTER_MS, jitter_after_hint_ms, jitter_backoff_ms, retry_after_hint_ms,
};
use std::time::Duration;

/// Wait before each overload retry when the upstream sent no `Retry-After`.
const OVERLOAD_RETRY_WAITS_MS: [u64; 2] = [2_000, 6_000];

/// Longest wait an overload retry honors from a `Retry-After` hint.
const OVERLOAD_MAX_WAIT_MS: u64 = 30_000;

/// The wait before overload retry `retry` (1-based): the upstream's
/// `Retry-After` capped at [`OVERLOAD_MAX_WAIT_MS`], else the fixed schedule.
pub(super) fn overload_wait_ms(retry: u32, hint_ms: Option<u64>) -> u64 {
    hint_ms.map_or_else(
        || {
            usize::try_from(retry.saturating_sub(1))
                .ok()
                .and_then(|index| OVERLOAD_RETRY_WAITS_MS.get(index).copied())
                .unwrap_or(OVERLOAD_MAX_WAIT_MS)
        },
        |hint| hint.min(OVERLOAD_MAX_WAIT_MS),
    )
}

/// Extract a `Retry-After` value (in milliseconds) from a non-streaming error.
///
/// Prefers the **structured** hint attached by
/// [`api_error`](super::super::api_error), which reads the real HTTP `Retry-After`
/// response header. Before that hint existed this function could only inspect
/// the error's text, and no provider ever put the header into the message — so
/// the whole honor path was unreachable on the non-streaming side and every
/// 429 fell back to a locally guessed exponential backoff. The textual scan is
/// kept as a fallback for upstreams that spell the delay into the body.
pub(super) fn parse_retry_after_ms(err: &anyhow::Error) -> Option<u64> {
    if let Some(millis) = retry_after_hint_ms(err) {
        return Some(millis);
    }
    let msg = err.to_string();
    let lower = msg.to_lowercase();

    // Look for "retry-after: <number>" or "retry_after: <number>"
    for prefix in &["retry-after:", "retry_after:", "retry-after ", "retry_after "] {
        if let Some(pos) = lower.find(prefix) {
            let after = &msg[pos + prefix.len()..];
            let num_str: String = after
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(secs) = num_str.parse::<f64>() {
                if secs.is_finite() && secs >= 0.0 {
                    let millis = Duration::from_secs_f64(secs).as_millis();
                    if let Ok(value) = u64::try_from(millis) {
                        return Some(value);
                    }
                }
            }
        }
    }
    None
}

/// How long to wait before the next attempt on the same candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryWait {
    /// Sleep this many milliseconds (jitter already applied), then retry.
    Sleep(u64),
    /// The upstream asked for a longer pause than we honor in place; the
    /// payload is the delay it actually requested, for diagnostics.
    HintExceedsCap(u64),
}

/// Compute the (jittered) wait before retrying the same provider/model.
///
/// * With a `Retry-After` hint: honor it, floored at the local backoff so a
///   `0` hint still pauses, and add a small positive spread. The wait is never
///   shortened below what the server asked for — truncating a server-declared
///   cool-down guarantees an immediate second 429.
/// * Without a hint: equal-jitter the local exponential backoff so concurrent
///   callers throttled in the same instant stop retrying in lock-step.
pub(super) fn plan_retry_wait(base: u64, err: &anyhow::Error) -> RetryWait {
    plan_wait_from_hint(base, parse_retry_after_ms(err))
}

/// [`plan_retry_wait`] for callers that already extracted the hint (the
/// streaming driver reads it off the structured `StreamError` variant).
pub(super) fn plan_wait_from_hint(base: u64, hint_ms: Option<u64>) -> RetryWait {
    match hint_ms {
        Some(hint) if hint > MAX_HONORED_RETRY_AFTER_MS => RetryWait::HintExceedsCap(hint),
        Some(hint) => RetryWait::Sleep(jitter_after_hint_ms(hint.max(base))),
        None => RetryWait::Sleep(jitter_backoff_ms(base)),
    }
}

/// Extract a Retry-After value (in milliseconds) from a streaming error.
///
/// Prefers the **structured** `retry_after_ms` field carried by
/// [`StreamError::RateLimited`] — read directly off the real HTTP `Retry-After`
/// header by the provider — rather than errors whose textual form happens to
/// embed the value. Falls back to parsing the error's `Display` form (mirroring
/// [`parse_retry_after_ms`]) for any other error shape.
///
/// The non-streaming side reaches the same header through
/// [`RetryAfterHint`](super::super::rate_limit::RetryAfterHint); neither path depends
/// on an upstream spelling the delay into the response body any more.
pub(super) fn parse_stream_retry_after_ms(err: &super::super::traits::StreamError) -> Option<u64> {
    if let super::super::traits::StreamError::RateLimited {
        retry_after_ms: Some(ms),
        ..
    } = err
    {
        return Some(*ms);
    }
    let anyhow_err = anyhow::anyhow!("{err}");
    parse_retry_after_ms(&anyhow_err)
}
