//! Retry budgets and fixed waits per failure kind for the reliable provider.
//!
//! A failed attempt is retried on the same candidate by its [`RetryKind`]:
//! ordinary errors get the chain's `max_retries`, overloads (HTTP 503 or an
//! overloaded body) at most [`OVERLOAD_MAX_RETRIES`], and rate limits (HTTP 429)
//! at least [`RATE_LIMIT_MAX_RETRIES`], so the retries span a one-minute
//! provider window.

/// Most in-place retries of one candidate after an overload response.
const OVERLOAD_MAX_RETRIES: u32 = 2;

/// Most in-place retries of one candidate after a rate-limit (429) response.
/// The waits in [`RATE_LIMIT_RETRY_WAITS_MS`] add up to 83 s, enough to outlast
/// a one-minute provider window before the last attempt.
const RATE_LIMIT_MAX_RETRIES: u32 = 5;

/// Wait before each rate-limit retry when the upstream sent no `Retry-After`.
/// Its length is [`RATE_LIMIT_MAX_RETRIES`].
const RATE_LIMIT_RETRY_WAITS_MS: [u64; 5] = [2_000, 6_000, 15_000, 30_000, 30_000];

/// How a failed attempt is retried on the same candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryKind {
    /// Any other retryable failure: the chain's `max_retries`.
    Ordinary,
    /// HTTP 503 or an overloaded error body: at most [`OVERLOAD_MAX_RETRIES`].
    Overload,
    /// HTTP 429 already judged retryable: at least [`RATE_LIMIT_MAX_RETRIES`].
    RateLimited,
}

/// In-place retries one candidate may take after a failure of `kind`, given the
/// chain's `max_retries`. A rate limit gets at least [`RATE_LIMIT_MAX_RETRIES`]
/// only on the last candidate of the chain, unless the chain disables retries
/// with `0`. On any earlier candidate a rate limit gets the overload budget, so
/// the chain fails over instead of waiting out the long schedule.
pub(super) fn retry_budget(max_retries: u32, kind: RetryKind, is_last: bool) -> u32 {
    match kind {
        RetryKind::Ordinary => max_retries,
        RetryKind::Overload => max_retries.min(OVERLOAD_MAX_RETRIES),
        RetryKind::RateLimited if !is_last => max_retries.min(OVERLOAD_MAX_RETRIES),
        RetryKind::RateLimited if max_retries == 0 => 0,
        RetryKind::RateLimited => max_retries.max(RATE_LIMIT_MAX_RETRIES),
    }
}

/// Whether the failed attempt number `attempt` (0-based) of a candidate may be
/// followed by another attempt on the same candidate.
pub(super) fn may_retry(attempt: u32, max_retries: u32, kind: RetryKind, is_last: bool) -> bool {
    attempt < retry_budget(max_retries, kind, is_last)
}

/// The fixed wait before rate-limit retry `retry` (1-based), used when the 429
/// carried no `Retry-After`. Past the end of the table the last wait repeats.
pub(super) fn rate_limit_wait_ms(retry: u32) -> u64 {
    let index = usize::try_from(retry.saturating_sub(1)).unwrap_or(usize::MAX);
    // The table is never empty, so the final fallback is unreachable.
    RATE_LIMIT_RETRY_WAITS_MS
        .get(index)
        .or_else(|| RATE_LIMIT_RETRY_WAITS_MS.last())
        .copied()
        .unwrap_or(0)
}
