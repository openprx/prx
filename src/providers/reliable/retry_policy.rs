//! Retry budgets and fixed waits per failure kind for the reliable provider.
//!
//! A failed attempt is retried on the same candidate by its [`RetryKind`]:
//! ordinary errors get the chain's `max_retries`, overloads (HTTP 502, 503 or
//! 504, or an overloaded body) at most [`OVERLOAD_MAX_RETRIES`], and rate
//! limits (HTTP 429) at least [`RATE_LIMIT_MAX_RETRIES`], so the retries span
//! a one-minute provider window.

use crate::security::redaction::redact_provider_text;

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
    /// HTTP 502, 503 or 504, or an overloaded error body: at most [`OVERLOAD_MAX_RETRIES`].
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

/// The most attempts one candidate can make: the first attempt plus the retry
/// budget of every kind. The retry loop also stops on [`RetryCounts::may_retry`],
/// so this is only the bound that keeps it finite.
pub(super) fn max_attempts(max_retries: u32, is_last: bool) -> u32 {
    1u32.saturating_add(retry_budget(max_retries, RetryKind::Ordinary, is_last))
        .saturating_add(retry_budget(max_retries, RetryKind::Overload, is_last))
        .saturating_add(retry_budget(max_retries, RetryKind::RateLimited, is_last))
}

/// Retries one candidate has taken, counted per [`RetryKind`]. Each kind spends
/// only its own budget, so a 502 after several 429s still gets its overload
/// retries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RetryCounts {
    ordinary: u32,
    overload: u32,
    rate_limited: u32,
}

impl RetryCounts {
    /// Retries already taken for failures of `kind`.
    pub(super) fn taken(&self, kind: RetryKind) -> u32 {
        match kind {
            RetryKind::Ordinary => self.ordinary,
            RetryKind::Overload => self.overload,
            RetryKind::RateLimited => self.rate_limited,
        }
    }

    /// Whether a failure of `kind` may be followed by another attempt on the
    /// same candidate, given the retries of that kind already taken.
    pub(super) fn may_retry(&self, kind: RetryKind, max_retries: u32, is_last: bool) -> bool {
        self.taken(kind) < retry_budget(max_retries, kind, is_last)
    }

    /// Count one retry taken for a failure of `kind`.
    pub(super) fn record(&mut self, kind: RetryKind) {
        let count = match kind {
            RetryKind::Ordinary => &mut self.ordinary,
            RetryKind::Overload => &mut self.overload,
            RetryKind::RateLimited => &mut self.rate_limited,
        };
        *count = count.saturating_add(1);
    }

    /// Retries taken for failures of any kind.
    pub(super) fn total(&self) -> u32 {
        self.ordinary
            .saturating_add(self.overload)
            .saturating_add(self.rate_limited)
    }
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

/// Report a candidate whose retry budget ran out on its last failure. `exhausted`
/// is the kind of that failure and the attempts made up to it. A rate limit is
/// an error that also feeds the rate-limit counters; an overload is a warning.
pub(super) fn log_budget_exhausted(provider_name: &str, model: &str, exhausted: Option<(RetryKind, u32)>) {
    match exhausted {
        Some((RetryKind::RateLimited, made)) => {
            crate::providers::rate_limit::record_exhausted(provider_name);
            tracing::error!(
                provider = %redact_provider_text(provider_name),
                model,
                attempts = made,
                "Provider rate limit outlasted the retry budget; abandoning this candidate"
            );
        }
        Some((RetryKind::Overload, made)) => {
            tracing::warn!(
                provider = %redact_provider_text(provider_name),
                model,
                attempts = made,
                "Provider overload outlasted the retry budget; abandoning this candidate"
            );
        }
        Some((RetryKind::Ordinary, _)) | None => {}
    }
}
