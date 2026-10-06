//! One circuit breaker per decision backend slot (SP-CDP-A 5.6; CDP 17;
//! SP-CDP-C V2.07).
//!
//! - Counted classes: timeout, transport, unavailable and invalid response.
//!   `failure_threshold` consecutive ones open the breaker for
//!   `cooldown_secs`.
//! - A rate limit opens it at once, for the longer of the cooldown and the
//!   wait the backend asked for.
//! - Authentication and model-version mismatch are persistent
//!   misconfiguration or drift: they open it until the next configuration
//!   generation builds a new factory.
//! - Never counted: invalid request (a PRX bug), policy rejection, budget,
//!   unsupported operation, cancellation and an already open circuit.
//! - After the cooldown one probe is let through (half-open): success
//!   closes, failure opens again. The probe holds an [`Admission`]; if it is
//!   dropped without a verdict (the decision was cancelled), the probe slot
//!   is released, so a cancelled probe never wedges the breaker (D-079 M-1).
//! - A backend's requested wait is capped at [`MAX_REQUESTED_WAIT`]
//!   (D-079 L-7).
//!
//! The builtin has no breaker.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::error::DecisionError;

/// The longest wait a backend may ask for with a rate limit.
pub const MAX_REQUESTED_WAIT: Duration = Duration::from_hours(1);

/// Where a breaker is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Calls pass; `consecutive` counted failures so far.
    Closed { consecutive: u32 },
    /// Calls are refused until `until`.
    Open { until: Instant },
    /// One probe may pass; `probe_in_flight` while it runs.
    HalfOpen { probe_in_flight: bool },
    /// Refused until the next configuration generation.
    OpenForGeneration,
}

impl BreakerState {
    /// The state's name, for status output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed { .. } => "closed",
            Self::Open { .. } => "open",
            Self::HalfOpen { .. } => "half_open",
            Self::OpenForGeneration => "open_for_generation",
        }
    }
}

/// A state change worth reporting (`causal.backend.circuit_opened`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerTransition {
    /// The breaker opened, until the given instant.
    Opened { until: Instant },
    /// The breaker opened until the next configuration generation.
    OpenedForGeneration,
    /// A half-open probe succeeded; the breaker closed.
    Closed,
}

/// How a failure class affects a breaker.
enum Effect {
    Ignore,
    Count,
    OpenFor(Duration),
    OpenForGeneration,
}

fn effect(error: &DecisionError, cooldown: Duration) -> Effect {
    match error {
        DecisionError::Timeout
        | DecisionError::Transport
        | DecisionError::Unavailable
        | DecisionError::InvalidResponse(_) => Effect::Count,
        DecisionError::RateLimited { retry_after_ms } => Effect::OpenFor(retry_after_ms.map_or(cooldown, |wait| {
            cooldown.max(Duration::from_millis(wait).min(MAX_REQUESTED_WAIT))
        })),
        DecisionError::Authentication | DecisionError::ModelVersionMismatch => Effect::OpenForGeneration,
        DecisionError::InvalidRequest(_)
        | DecisionError::PolicyRejection(_)
        | DecisionError::BudgetExceeded
        | DecisionError::UnsupportedOperation
        | DecisionError::Cancelled
        | DecisionError::CircuitOpen => Effect::Ignore,
    }
}

/// The breaker of one backend slot.
#[derive(Debug)]
pub struct CircuitBreaker {
    inner: Mutex<Inner>,
    threshold: u32,
    cooldown: Duration,
}

/// The state and the epoch of the newest half-open probe: a verdict or a
/// release acts on the half-open state only when it comes from that probe
/// (D-082 N-1).
#[derive(Debug)]
struct Inner {
    state: BreakerState,
    probe_epoch: u64,
}

impl CircuitBreaker {
    /// A closed breaker that opens after `threshold` counted failures (at
    /// least one) for `cooldown`.
    #[must_use]
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: BreakerState::Closed { consecutive: 0 },
                probe_epoch: 0,
            }),
            threshold: threshold.max(1),
            cooldown,
        }
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> BreakerState {
        self.inner.lock().state
    }

    /// Let a call start at `now`, or refuse it. An expired open breaker
    /// becomes half-open and lets exactly this call through as the probe.
    /// The call reports its verdict through the returned [`Admission`].
    pub fn admit(&self, now: Instant) -> Option<Admission<'_>> {
        let mut inner = self.inner.lock();
        let probe = match inner.state {
            BreakerState::Closed { .. } => false,
            BreakerState::Open { until } if now >= until => true,
            BreakerState::HalfOpen { probe_in_flight: false } => true,
            BreakerState::Open { .. }
            | BreakerState::HalfOpen { probe_in_flight: true }
            | BreakerState::OpenForGeneration => {
                return None;
            }
        };
        let probe = probe.then(|| {
            inner.probe_epoch = inner.probe_epoch.wrapping_add(1);
            inner.state = BreakerState::HalfOpen { probe_in_flight: true };
            inner.probe_epoch
        });
        Some(Admission {
            breaker: self,
            probe,
            settled: false,
        })
    }

    /// Release the probe `epoch` when it ended without a verdict; a newer
    /// probe is left alone.
    fn release_probe(&self, epoch: u64) {
        let mut inner = self.inner.lock();
        if inner.state == (BreakerState::HalfOpen { probe_in_flight: true }) && inner.probe_epoch == epoch {
            inner.state = BreakerState::HalfOpen { probe_in_flight: false };
        }
    }

    /// Whether calls are refused at `now` (for status and the order).
    #[must_use]
    pub fn is_open(&self, now: Instant) -> bool {
        let state = self.inner.lock().state;
        match state {
            BreakerState::Closed { .. } | BreakerState::HalfOpen { .. } => false,
            BreakerState::Open { until } => now < until,
            BreakerState::OpenForGeneration => true,
        }
    }

    /// Whether `probe` is the probe the half-open state waits for.
    const fn is_current_probe(inner: &Inner, probe: Option<u64>) -> bool {
        matches!(probe, Some(epoch) if epoch == inner.probe_epoch)
    }

    /// Record a successful call. Only the current probe closes a half-open
    /// breaker; a call admitted before the breaker opened changes nothing.
    fn record_success(&self, probe: Option<u64>) -> Option<BreakerTransition> {
        let mut inner = self.inner.lock();
        match inner.state {
            BreakerState::OpenForGeneration | BreakerState::Open { .. } => None,
            BreakerState::HalfOpen { .. } if !Self::is_current_probe(&inner, probe) => None,
            BreakerState::HalfOpen { .. } => {
                inner.state = BreakerState::Closed { consecutive: 0 };
                Some(BreakerTransition::Closed)
            }
            BreakerState::Closed { .. } => {
                inner.state = BreakerState::Closed { consecutive: 0 };
                None
            }
        }
    }

    /// Record a failed call of class `error` at `now`. While half-open only
    /// the current probe's verdict counts; persistent misconfiguration opens
    /// the breaker for the generation whoever reports it.
    fn record_failure(&self, error: &DecisionError, now: Instant, probe: Option<u64>) -> Option<BreakerTransition> {
        let mut inner = self.inner.lock();
        let current_probe = Self::is_current_probe(&inner, probe);
        let open_for = |inner: &mut Inner, duration: Duration| match now.checked_add(duration) {
            Some(until) => {
                inner.state = BreakerState::Open { until };
                Some(BreakerTransition::Opened { until })
            }
            None => {
                inner.state = BreakerState::OpenForGeneration;
                Some(BreakerTransition::OpenedForGeneration)
            }
        };
        match (effect(error, self.cooldown), inner.state) {
            (_, BreakerState::OpenForGeneration) => None,
            (Effect::OpenForGeneration, _) => {
                inner.state = BreakerState::OpenForGeneration;
                Some(BreakerTransition::OpenedForGeneration)
            }
            // A verdict from anything but the current probe leaves the
            // half-open state to that probe.
            (_, BreakerState::HalfOpen { .. }) if !current_probe => None,
            (Effect::OpenFor(duration), _) => open_for(&mut inner, duration),
            // A failed probe reopens; an ignored class releases the probe.
            (Effect::Count, BreakerState::HalfOpen { .. }) => open_for(&mut inner, self.cooldown),
            (Effect::Ignore, BreakerState::HalfOpen { .. }) => {
                inner.state = BreakerState::HalfOpen { probe_in_flight: false };
                None
            }
            (Effect::Count, BreakerState::Closed { consecutive }) => {
                let consecutive = consecutive.saturating_add(1);
                if consecutive >= self.threshold {
                    open_for(&mut inner, self.cooldown)
                } else {
                    inner.state = BreakerState::Closed { consecutive };
                    None
                }
            }
            (Effect::Count | Effect::Ignore, BreakerState::Open { .. })
            | (Effect::Ignore, BreakerState::Closed { .. }) => None,
        }
    }
}

/// A call the breaker let start. Report its verdict with
/// [`Admission::succeeded`] or [`Admission::failed`]; dropping it without one
/// (a cancelled call) releases its half-open probe and counts nothing.
#[derive(Debug)]
#[must_use = "report the call's verdict, or drop it to release the probe"]
pub struct Admission<'a> {
    breaker: &'a CircuitBreaker,
    /// The epoch of the probe this call is, if it is one.
    probe: Option<u64>,
    settled: bool,
}

impl Admission<'_> {
    /// Whether this call is the half-open probe (tests read it; the factory
    /// hands the admission back without asking).
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn is_probe(&self) -> bool {
        self.probe.is_some()
    }

    /// The call succeeded.
    pub fn succeeded(mut self) -> Option<BreakerTransition> {
        self.settled = true;
        self.breaker.record_success(self.probe)
    }

    /// The call failed with `error` at `now`.
    pub fn failed(mut self, error: &DecisionError, now: Instant) -> Option<BreakerTransition> {
        self.settled = true;
        self.breaker.record_failure(error, now, self.probe)
    }
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if let (Some(epoch), false) = (self.probe, self.settled) {
            self.breaker.release_probe(epoch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{PolicyRule, RequestRule, ValidationRule};

    const COOLDOWN: Duration = Duration::from_mins(1);

    #[test]
    fn counted_failures_open_after_the_threshold_and_success_resets() {
        let breaker = CircuitBreaker::new(3, COOLDOWN);
        let now = Instant::now();
        for error in [DecisionError::Timeout, DecisionError::Transport] {
            let admission = breaker.admit(now).expect("test: closed admits");
            assert!(!admission.is_probe());
            assert_eq!(admission.failed(&error, now), None);
        }
        assert_eq!(
            breaker.record_success(None),
            None,
            "a closed breaker reports no transition"
        );
        assert_eq!(breaker.state(), BreakerState::Closed { consecutive: 0 });
        for error in [
            DecisionError::Unavailable,
            DecisionError::InvalidResponse(ValidationRule::Malformed),
        ] {
            assert_eq!(breaker.record_failure(&error, now, None), None);
        }
        assert_eq!(
            breaker.record_failure(&DecisionError::Timeout, now, None),
            Some(BreakerTransition::Opened { until: now + COOLDOWN })
        );
        assert!(breaker.is_open(now));
        assert!(breaker.admit(now + Duration::from_secs(59)).is_none());
    }

    #[test]
    fn a_half_open_probe_closes_on_success_and_reopens_on_failure() {
        let breaker = CircuitBreaker::new(1, COOLDOWN);
        let now = Instant::now();
        breaker.record_failure(&DecisionError::Timeout, now, None);
        let later = now + COOLDOWN;
        let probe = breaker.admit(later).expect("test: the cooldown passed: one probe");
        assert!(probe.is_probe());
        assert!(breaker.admit(later).is_none(), "only one probe at a time");
        assert_eq!(probe.succeeded(), Some(BreakerTransition::Closed));
        assert!(breaker.admit(later).is_some());

        breaker.record_failure(&DecisionError::Timeout, later, None);
        let at = later + COOLDOWN;
        let probe = breaker.admit(at).expect("test: probe");
        assert_eq!(
            probe.failed(&DecisionError::Transport, at),
            Some(BreakerTransition::Opened { until: at + COOLDOWN })
        );
        let probe = breaker
            .admit(at + COOLDOWN)
            .expect("test: a new probe after the next cooldown");
        assert_eq!(
            probe.failed(&DecisionError::Cancelled, at + COOLDOWN),
            None,
            "a cancelled probe is no verdict"
        );
        assert!(breaker.admit(at + COOLDOWN).is_some(), "the probe slot is released");
    }

    /// D-082 N-1: a call admitted before the breaker opened neither frees
    /// nor settles a later probe; an old probe's release leaves a newer probe
    /// alone.
    #[test]
    fn only_the_current_probe_settles_the_half_open_breaker() {
        let breaker = CircuitBreaker::new(1, COOLDOWN);
        let now = Instant::now();
        let early = breaker.admit(now).expect("test: closed admits");
        let early_success = breaker.admit(now).expect("test: closed admits");
        let opener = breaker.admit(now).expect("test: closed admits");
        assert!(opener.failed(&DecisionError::Timeout, now).is_some());
        let later = now + COOLDOWN;
        let probe = breaker.admit(later).expect("test: probe");
        assert_eq!(early.failed(&DecisionError::Cancelled, later), None);
        assert_eq!(early_success.succeeded(), None, "an old success does not close");
        assert!(breaker.admit(later).is_none(), "the probe is still in flight");
        assert_eq!(breaker.state(), BreakerState::HalfOpen { probe_in_flight: true });
        drop(probe);
        let newer = breaker.admit(later).expect("test: a new probe");
        assert!(newer.is_probe());
        assert_eq!(newer.succeeded(), Some(BreakerTransition::Closed));
        let stale_success = breaker.admit(later).expect("test: closed admits");
        let opener = breaker.admit(later).expect("test: closed admits");
        assert!(opener.failed(&DecisionError::Timeout, later).is_some());
        assert_eq!(
            stale_success.succeeded(),
            None,
            "an old success does not close an open breaker"
        );
        assert!(breaker.is_open(later));
    }

    /// D-082 N-1: releasing an old probe after a newer probe was admitted
    /// leaves the newer one in flight. `Admission` settles before a new probe
    /// can start, so the old verdict is reported through the recorder here.
    #[test]
    fn an_old_probe_release_leaves_a_newer_probe_in_flight() {
        let breaker = CircuitBreaker::new(1, COOLDOWN);
        let now = Instant::now();
        breaker.record_failure(&DecisionError::Timeout, now, None);
        let later = now + COOLDOWN;
        let old = breaker.admit(later).expect("test: probe");
        let old_epoch = old.probe.expect("test: a probe has an epoch");
        assert!(
            breaker
                .record_failure(&DecisionError::Timeout, later, Some(old_epoch))
                .is_some()
        );
        let latest = later + COOLDOWN;
        let newer = breaker.admit(latest).expect("test: a newer probe");
        assert!(newer.is_probe());
        drop(old);
        assert_eq!(
            breaker.state(),
            BreakerState::HalfOpen { probe_in_flight: true },
            "the newer probe is still in flight"
        );
        assert!(breaker.admit(latest).is_none());
        assert_eq!(newer.succeeded(), Some(BreakerTransition::Closed));
    }

    /// D-079 M-1: a probe dropped without a verdict (a cancelled decision)
    /// releases the probe slot; the next call probes again.
    #[test]
    fn a_dropped_probe_releases_the_breaker() {
        let breaker = CircuitBreaker::new(1, COOLDOWN);
        let now = Instant::now();
        breaker.record_failure(&DecisionError::Timeout, now, None);
        let later = now + COOLDOWN;
        let probe = breaker.admit(later).expect("test: probe");
        drop(probe);
        assert_eq!(breaker.state(), BreakerState::HalfOpen { probe_in_flight: false });
        let again = breaker.admit(later).expect("test: probed again");
        assert!(again.is_probe());
        let closed = CircuitBreaker::new(1, COOLDOWN);
        drop(closed.admit(now).expect("test: closed admits"));
        assert_eq!(
            closed.state(),
            BreakerState::Closed { consecutive: 0 },
            "a dropped call counts nothing"
        );
    }

    /// D-079 L-7: a backend's requested wait is capped.
    #[test]
    fn a_requested_wait_is_capped() {
        let breaker = CircuitBreaker::new(5, COOLDOWN);
        let now = Instant::now();
        assert_eq!(
            breaker.record_failure(
                &DecisionError::RateLimited {
                    retry_after_ms: Some(u64::MAX)
                },
                now,
                None
            ),
            Some(BreakerTransition::Opened {
                until: now + MAX_REQUESTED_WAIT
            })
        );
    }

    #[test]
    fn a_rate_limit_opens_for_the_longer_wait() {
        let breaker = CircuitBreaker::new(5, COOLDOWN);
        let now = Instant::now();
        assert_eq!(
            breaker.record_failure(
                &DecisionError::RateLimited {
                    retry_after_ms: Some(120_000)
                },
                now,
                None
            ),
            Some(BreakerTransition::Opened {
                until: now + Duration::from_mins(2)
            })
        );
        let breaker = CircuitBreaker::new(5, COOLDOWN);
        assert_eq!(
            breaker.record_failure(
                &DecisionError::RateLimited {
                    retry_after_ms: Some(10)
                },
                now,
                None
            ),
            Some(BreakerTransition::Opened { until: now + COOLDOWN })
        );
    }

    #[test]
    fn persistent_misconfiguration_opens_until_the_next_generation() {
        for error in [DecisionError::Authentication, DecisionError::ModelVersionMismatch] {
            let breaker = CircuitBreaker::new(5, COOLDOWN);
            let now = Instant::now();
            assert_eq!(
                breaker.record_failure(&error, now, None),
                Some(BreakerTransition::OpenedForGeneration)
            );
            assert!(breaker.admit(now + Duration::from_hours(24)).is_none(), "{error}");
            assert_eq!(breaker.record_success(None), None);
            assert!(breaker.is_open(now + Duration::from_hours(24)));
        }
    }

    #[test]
    fn prx_and_policy_failures_are_never_counted() {
        let breaker = CircuitBreaker::new(1, COOLDOWN);
        let now = Instant::now();
        for error in [
            DecisionError::InvalidRequest(RequestRule::NoQuestions),
            DecisionError::PolicyRejection(PolicyRule::SecretInRequest),
            DecisionError::BudgetExceeded,
            DecisionError::UnsupportedOperation,
            DecisionError::Cancelled,
            DecisionError::CircuitOpen,
        ] {
            assert_eq!(breaker.record_failure(&error, now, None), None, "{error}");
            assert_eq!(breaker.state(), BreakerState::Closed { consecutive: 0 }, "{error}");
        }
    }
}
