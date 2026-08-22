//! Shared handling for upstream rate limiting (HTTP 429 / 503).
//!
//! Three concerns live here, all of them *reactive*: nothing in this module
//! caps concurrency or paces requests on its own. Everything is driven by an
//! explicit signal from the upstream provider.
//!
//! 1. [`RetryAfterHint`] — a structured `Retry-After` value attached to a
//!    non-streaming provider error, so the reliability layer can honor the real
//!    HTTP header instead of guessing from the error's textual form.
//! 2. [`RateLimitGate`] — a per-provider "earliest next attempt" deadline shared
//!    across every in-process request. A 429 on one request defers the others
//!    until the cool-down elapses. It stores a *time*, never a permit count, so
//!    an unlimited number of requests may pass through it simultaneously.
//! 3. Rate-limit telemetry ([`snapshot`]) — counters per provider/model so an
//!    operator can answer "how often are we being throttled, and did we recover".
//!
//! # Scope
//!
//! The gate is process-local. Sub-agents running in the default in-process
//! (`mode = "task"`) spawn mode share it with their parent. Sub-agents started
//! with `mode = "process"` are separate OS processes with their own address
//! space, so they do **not** observe this gate; covering them would require a
//! daemon-side service plus IPC and is deliberately out of scope.

use parking_lot::RwLock;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Longest upstream `Retry-After` we are willing to sleep out in place.
///
/// Anthropic and OpenAI routinely answer organization-level throttling with
/// `Retry-After: 60`; the previous 30s ceiling silently truncated those to half
/// the requested delay, which guarantees a second 429 on the retry. Two minutes
/// covers the observed upstream windows with headroom. A hint longer than this
/// is *not* truncated — the candidate is abandoned instead, so the caller sees
/// the real requested delay in the aggregated error rather than burning its
/// whole retry budget on waits that are known to be too short.
pub const MAX_HONORED_RETRY_AFTER_MS: u64 = 120_000;

/// Upper bound on how long the shared gate may defer a single request.
const MAX_GATE_DEFER_MS: u64 = MAX_HONORED_RETRY_AFTER_MS;

/// Extra spread applied when releasing requests parked behind the gate, so a
/// shared deadline does not release every waiter on the same millisecond.
const GATE_RELEASE_JITTER_MS: u64 = 250;

/// Cap on the additive jitter layered on top of an upstream `Retry-After`.
const MAX_HINT_JITTER_MS: u64 = 5_000;

/// Structured `Retry-After` hint carried as the source of a provider error.
///
/// Non-streaming provider calls return `anyhow::Error`, which cannot express a
/// typed rate-limit variant the way [`StreamError`](super::traits::StreamError)
/// can. Attaching this as the error's source keeps the hint machine-readable
/// (recovered via `downcast_ref`) while the user-visible `Display` output stays
/// exactly the sanitized provider message it always was.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("upstream asked to retry after {millis} ms")]
pub struct RetryAfterHint {
    /// Delay requested by the upstream, in milliseconds.
    pub millis: u64,
}

/// Recover a structured [`RetryAfterHint`] from anywhere in an error's source chain.
#[must_use]
pub fn retry_after_hint_ms(err: &anyhow::Error) -> Option<u64> {
    err.chain()
        .find_map(|source| source.downcast_ref::<RetryAfterHint>())
        .map(|hint| hint.millis)
}

/// Uniform random value in `[0, bound)`.
///
/// Jitter only needs decorrelation, not uniformity guarantees, so the modulo
/// bias of a plain reduction is irrelevant here.
fn random_below(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    rand::random::<u64>() % bound
}

/// Equal jitter for a locally computed exponential backoff.
///
/// Returns a value in `[nominal/2, nominal]`. Concurrent callers that were
/// throttled in the same instant would otherwise retry in lock-step forever
/// (1s / 2s / 4s on the dot), turning every recovery into a thundering herd.
/// The lower half-bound keeps the backoff schedule meaningful — full jitter
/// (`rand(0, nominal)`) can collapse a long backoff to almost nothing.
#[must_use]
pub fn jitter_backoff_ms(nominal_ms: u64) -> u64 {
    let half = nominal_ms / 2;
    half.saturating_add(random_below(nominal_ms.saturating_sub(half).saturating_add(1)))
}

/// Jitter layered on top of an upstream `Retry-After` hint.
///
/// The result is never shorter than what the server asked for — undershooting a
/// server-declared cool-down guarantees another 429 — so the spread is additive
/// and capped at a quarter of the hint (at most [`MAX_HINT_JITTER_MS`]).
#[must_use]
pub fn jitter_after_hint_ms(hint_ms: u64) -> u64 {
    let span = (hint_ms / 4).min(MAX_HINT_JITTER_MS);
    hint_ms.saturating_add(random_below(span.saturating_add(1)))
}

/// Per-provider cool-down shared by every request in this process.
///
/// Holds a single `Instant` per provider: the earliest moment a new attempt may
/// start. There is no permit count, no queue and no concurrency ceiling — any
/// number of requests may run at once, they simply all observe the same
/// "not before" deadline once an upstream has told us to slow down.
#[derive(Debug, Default)]
pub struct RateLimitGate {
    deadlines: RwLock<BTreeMap<Arc<str>, Instant>>,
}

impl RateLimitGate {
    /// Create an empty gate (no provider is throttled).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a cool-down for `provider` learned from an upstream throttle.
    ///
    /// Deadlines only ever move forward, so a long `Retry-After` is not
    /// shortened by a concurrent request that got a smaller backoff.
    pub fn note_rate_limited(&self, provider: &str, wait_ms: u64) {
        let capped = wait_ms.min(MAX_GATE_DEFER_MS);
        if capped == 0 {
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(capped);
        let mut deadlines = self.deadlines.write();
        match deadlines.get_mut(provider) {
            Some(existing) => {
                if deadline > *existing {
                    *existing = deadline;
                }
            }
            None => {
                deadlines.insert(Arc::from(provider), deadline);
            }
        }
    }

    /// Remaining cool-down for `provider`, or `None` when it is not throttled.
    #[must_use]
    pub fn cooldown_remaining(&self, provider: &str) -> Option<Duration> {
        let deadlines = self.deadlines.read();
        let deadline = *deadlines.get(provider)?;
        deadline.checked_duration_since(Instant::now())
    }

    /// Park until `provider`'s shared cool-down has elapsed.
    ///
    /// Returns the number of milliseconds actually spent waiting, or `None`
    /// when the provider was not throttled (the overwhelmingly common case,
    /// which costs one uncontended read lock). The total wait is bounded by
    /// [`MAX_GATE_DEFER_MS`] so a pathological upstream cannot park a request
    /// forever.
    pub async fn wait_until_clear(&self, provider: &str) -> Option<u64> {
        let started = Instant::now();
        let budget = Duration::from_millis(MAX_GATE_DEFER_MS);
        let mut deferred = false;

        while let Some(remaining) = self.cooldown_remaining(provider) {
            let elapsed = started.elapsed();
            if elapsed >= budget {
                break;
            }
            let capped = remaining.min(budget.saturating_sub(elapsed));
            let spread = Duration::from_millis(random_below(GATE_RELEASE_JITTER_MS + 1));
            deferred = true;
            tokio::time::sleep(capped + spread).await;
        }

        if !deferred {
            return None;
        }
        let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        record_gate_deferral(provider, waited);
        Some(waited)
    }
}

/// Process-wide gate shared by every provider chain built through
/// [`create_resilient_provider_with_options`](super::create_resilient_provider_with_options).
///
/// Chains constructed directly via `ReliableProvider::new` get a private gate
/// instead, so unit tests never leak cool-downs into each other.
pub fn shared_gate() -> Arc<RateLimitGate> {
    static GATE: OnceLock<Arc<RateLimitGate>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| Arc::new(RateLimitGate::new())))
}

// ── Telemetry ────────────────────────────────────────────────────────────

/// Rate-limit counters for a single provider.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ProviderRateLimitStats {
    /// Rate-limited attempts observed (streaming and non-streaming combined).
    pub rate_limited_total: u64,
    /// Subset of `rate_limited_total` that came from a streaming request.
    pub streaming_total: u64,
    /// Subset of `rate_limited_total` where the upstream supplied a usable
    /// `Retry-After` hint that we honored.
    pub retry_after_honored_total: u64,
    /// Candidates that were rate-limited and then succeeded on a later attempt.
    pub recovered_total: u64,
    /// Candidates abandoned with the last failure still being a rate limit.
    pub exhausted_total: u64,
    /// Requests deferred behind the shared cool-down gate.
    pub gate_deferrals_total: u64,
    /// Cumulative time spent parked behind the gate.
    pub gate_deferred_ms_total: u64,
    /// Rate-limited attempts broken down by model.
    pub by_model: BTreeMap<String, u64>,
    /// RFC 3339 timestamp of the most recent rate-limit event.
    pub last_event_at: Option<String>,
}

/// Aggregated rate-limit telemetry for the whole process.
#[derive(Debug, Default, Clone, Serialize)]
pub struct RateLimitStatsSnapshot {
    /// Rate-limited attempts across all providers.
    pub rate_limited_total: u64,
    /// Per-provider breakdown.
    pub providers: BTreeMap<String, ProviderRateLimitStats>,
}

fn stats() -> &'static RwLock<BTreeMap<String, ProviderRateLimitStats>> {
    static STATS: OnceLock<RwLock<BTreeMap<String, ProviderRateLimitStats>>> = OnceLock::new();
    STATS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

fn with_provider_stats<F: FnOnce(&mut ProviderRateLimitStats)>(provider: &str, update: F) {
    let mut guard = stats().write();
    let entry = guard.entry(provider.to_string()).or_default();
    update(entry);
}

/// Record one rate-limited provider attempt.
pub fn record_rate_limited(provider: &str, model: &str, streaming: bool, retry_after_honored: bool) {
    let now = chrono::Utc::now().to_rfc3339();
    with_provider_stats(provider, |entry| {
        entry.rate_limited_total = entry.rate_limited_total.saturating_add(1);
        if streaming {
            entry.streaming_total = entry.streaming_total.saturating_add(1);
        }
        if retry_after_honored {
            entry.retry_after_honored_total = entry.retry_after_honored_total.saturating_add(1);
        }
        let per_model = entry.by_model.entry(model.to_string()).or_default();
        *per_model = per_model.saturating_add(1);
        entry.last_event_at = Some(now);
    });
}

/// Record that a previously rate-limited candidate went on to succeed.
pub fn record_recovered(provider: &str) {
    with_provider_stats(provider, |entry| {
        entry.recovered_total = entry.recovered_total.saturating_add(1);
    });
}

/// Record that a candidate was abandoned while still rate-limited.
pub fn record_exhausted(provider: &str) {
    with_provider_stats(provider, |entry| {
        entry.exhausted_total = entry.exhausted_total.saturating_add(1);
    });
}

fn record_gate_deferral(provider: &str, waited_ms: u64) {
    with_provider_stats(provider, |entry| {
        entry.gate_deferrals_total = entry.gate_deferrals_total.saturating_add(1);
        entry.gate_deferred_ms_total = entry.gate_deferred_ms_total.saturating_add(waited_ms);
    });
}

/// Snapshot the rate-limit telemetry (exposed through the daemon health report).
#[must_use]
pub fn snapshot() -> RateLimitStatsSnapshot {
    let guard = stats().read();
    let providers: BTreeMap<String, ProviderRateLimitStats> = guard
        .iter()
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect();
    let rate_limited_total = providers
        .values()
        .fold(0_u64, |acc, entry| acc.saturating_add(entry.rate_limited_total));
    RateLimitStatsSnapshot {
        rate_limited_total,
        providers,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn retry_after_hint_survives_context_wrapping() {
        let err = anyhow::Error::new(RetryAfterHint { millis: 60_000 }).context("Anthropic API error (429): nope");
        assert_eq!(retry_after_hint_ms(&err), Some(60_000));
        assert_eq!(err.to_string(), "Anthropic API error (429): nope");
    }

    #[test]
    fn retry_after_hint_absent_on_plain_error() {
        assert_eq!(retry_after_hint_ms(&anyhow::anyhow!("500 boom")), None);
    }

    #[test]
    fn backoff_jitter_stays_within_the_equal_jitter_band() {
        for _ in 0..2_000 {
            let value = jitter_backoff_ms(1_000);
            assert!((500..=1_000).contains(&value), "out of band: {value}");
        }
    }

    #[test]
    fn backoff_jitter_actually_spreads() {
        let samples: std::collections::BTreeSet<u64> = (0..200).map(|_| jitter_backoff_ms(4_000)).collect();
        assert!(samples.len() > 50, "jitter collapsed to {} values", samples.len());
    }

    #[test]
    fn backoff_jitter_handles_degenerate_inputs() {
        assert_eq!(jitter_backoff_ms(0), 0);
        assert!((0..=1).contains(&jitter_backoff_ms(1)));
    }

    #[test]
    fn hint_jitter_never_undershoots_the_server_request() {
        for _ in 0..2_000 {
            let value = jitter_after_hint_ms(60_000);
            assert!((60_000..=65_000).contains(&value), "out of band: {value}");
        }
    }

    #[test]
    fn hint_jitter_caps_additive_span() {
        for _ in 0..500 {
            let value = jitter_after_hint_ms(1_000_000);
            assert!((1_000_000..=1_005_000).contains(&value), "out of band: {value}");
        }
    }

    #[tokio::test]
    async fn gate_is_transparent_when_no_rate_limit_was_observed() {
        let gate = RateLimitGate::new();
        assert!(gate.wait_until_clear("quiet-provider").await.is_none());
        assert!(gate.cooldown_remaining("quiet-provider").is_none());
    }

    #[tokio::test]
    async fn gate_defers_other_requests_after_a_rate_limit() {
        let gate = Arc::new(RateLimitGate::new());
        gate.note_rate_limited("shared", 200);

        let started = Instant::now();
        let waited = gate.wait_until_clear("shared").await;
        let elapsed = started.elapsed();

        assert!(waited.is_some(), "a throttled provider must defer the next request");
        assert!(
            elapsed >= Duration::from_millis(180),
            "gate released too early: {elapsed:?}"
        );
    }

    #[test]
    fn gate_deadlines_only_move_forward() {
        let gate = RateLimitGate::new();
        gate.note_rate_limited("p", 5_000);
        let long = gate.cooldown_remaining("p").unwrap();
        gate.note_rate_limited("p", 10);
        let after_short = gate.cooldown_remaining("p").unwrap();
        assert!(
            after_short + Duration::from_millis(200) >= long,
            "a short backoff must not shorten a long cool-down"
        );
    }

    #[test]
    fn gate_caps_absurd_cooldowns() {
        let gate = RateLimitGate::new();
        gate.note_rate_limited("p", u64::MAX);
        let remaining = gate.cooldown_remaining("p").unwrap();
        assert!(remaining <= Duration::from_millis(MAX_GATE_DEFER_MS));
    }

    #[test]
    fn gate_ignores_zero_length_cooldowns() {
        let gate = RateLimitGate::new();
        gate.note_rate_limited("p", 0);
        assert!(gate.cooldown_remaining("p").is_none());
    }

    #[test]
    fn stats_accumulate_per_provider_and_model() {
        record_rate_limited("stats-test-provider", "m1", false, true);
        record_rate_limited("stats-test-provider", "m1", true, false);
        record_rate_limited("stats-test-provider", "m2", false, false);
        record_recovered("stats-test-provider");
        record_exhausted("stats-test-provider");

        let snap = snapshot();
        let entry = snap.providers.get("stats-test-provider").unwrap();
        assert_eq!(entry.rate_limited_total, 3);
        assert_eq!(entry.streaming_total, 1);
        assert_eq!(entry.retry_after_honored_total, 1);
        assert_eq!(entry.recovered_total, 1);
        assert_eq!(entry.exhausted_total, 1);
        assert_eq!(entry.by_model.get("m1").copied(), Some(2));
        assert_eq!(entry.by_model.get("m2").copied(), Some(1));
        assert!(entry.last_event_at.is_some());
        assert!(snap.rate_limited_total >= 3);
    }
}
