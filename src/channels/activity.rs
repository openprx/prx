//! Real-activity tracking for channel listeners.
//!
//! # Why this module exists
//!
//! The channel supervisor used to report health on a bare timer: as long as the
//! heartbeat interval kept ticking, the channel was published as `Ready`. That
//! signal was independent of the listener — a listener wedged forever inside its
//! receive future (a long poll that never returns, a websocket that never errors
//! out, a subprocess that stopped writing) kept publishing perfect health while
//! being completely deaf. Operators had no way to see the difference.
//!
//! This module records what a listener actually *did*: inbound messages handed
//! to the pipeline, outbound sends that succeeded, and completed upstream
//! round-trips. The supervisor then reports the observed staleness instead of
//! the liveness of its own timer.
//!
//! # The three signals
//!
//! * **inbound** — a message was received and delivered. Strongest proof of life
//!   but useless on its own: a quiet chat produces none for hours.
//! * **outbound** — a send to the remote side succeeded. Proves the credentials
//!   and the network still work, but says nothing about the receive path.
//! * **upstream** — one receive round-trip completed: a poll returned (*even with
//!   zero messages*), a websocket frame or keepalive arrived, one subprocess read
//!   cycle finished. This is the signal that separates "idle" from "wedged",
//!   because it keeps arriving while nobody is talking.
//!
//! A channel that knows the maximum normal gap between two consecutive upstream
//! round-trips declares it through [`crate::channels::traits::Channel::liveness_expectation`].
//! Channels that cannot bound that gap (pure server-push, local stdin) declare
//! nothing and are reported as `Passive`: their idle time is still published, but
//! no stall verdict is claimed, because none can honestly be made.
//!
//! # Deliberately report-only — do not turn this into a timeout
//!
//! [`ChannelActivityStatus::stalled`] and the stall threshold behind it exist to
//! *describe* a channel to a human. They must never be wired to a restart, an
//! abort, a cancellation, a reconnect or any other corrective action. prx is an
//! LLM agent runtime that does not impose execution timeouts, and a listener
//! blocking for a long time is a legitimate state, not an error: only an operator
//! can tell a wedged channel from a deliberately quiet one. Making a stall
//! trigger recovery would re-introduce, by the back door, exactly the timeout
//! this design removes.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Multiplier applied to a channel's declared expectation to obtain the stall
/// threshold.
///
/// A poll cadence is never exact — scheduling, retries and network latency all
/// stretch it — so a single missed round-trip must not be reported as a stall.
/// Three consecutive missed round-trips is short enough to be noticed quickly
/// and long enough to stay quiet during ordinary jitter.
///
/// Reporting only: see the module docs. This value must never gate a restart.
const STALL_TOLERANCE_FACTOR: u32 = 3;

/// How a channel proves that its receive path is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessModel {
    /// The listener completes an upstream round-trip at a bounded cadence, so a
    /// missing round-trip is evidence of a wedged listener. Carries the longest
    /// gap between two consecutive round-trips that is still considered normal.
    Bounded(Duration),
    /// The listener has no bounded cadence (server push without keepalive, local
    /// stdin, TUI input). Silence is indistinguishable from a wedge, so no stall
    /// verdict is claimed.
    Passive,
}

impl LivenessModel {
    /// Longest silence that is still reported as normal, if one can be derived.
    #[must_use]
    pub const fn stall_threshold(self) -> Option<Duration> {
        match self {
            Self::Bounded(expected) => Some(expected.saturating_mul(STALL_TOLERANCE_FACTOR)),
            Self::Passive => None,
        }
    }

    /// Human-readable tag published alongside the channel status.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bounded(_) => "bounded",
            Self::Passive => "passive",
        }
    }
}

/// Raw activity record for one channel listener.
#[derive(Debug, Clone, Copy)]
struct ChannelActivity {
    model: LivenessModel,
    /// When the current listener incarnation started; used as the baseline so a
    /// channel that has not seen a single round-trip yet is still measured.
    started_at: Instant,
    last_inbound: Option<Instant>,
    last_outbound: Option<Instant>,
    last_upstream: Option<Instant>,
}

impl ChannelActivity {
    const fn new(model: LivenessModel, now: Instant) -> Self {
        Self {
            model,
            started_at: now,
            last_inbound: None,
            last_outbound: None,
            last_upstream: None,
        }
    }

    /// Most recent moment at which this channel proved its receive path works.
    ///
    /// Outbound sends are deliberately excluded: a channel can keep sending
    /// successfully while its receive path is wedged, which is precisely the
    /// failure this module exists to expose.
    fn last_receive_proof(&self) -> Instant {
        let mut latest = self.started_at;
        for candidate in [self.last_inbound, self.last_upstream].into_iter().flatten() {
            if candidate > latest {
                latest = candidate;
            }
        }
        latest
    }

    fn status_at(&self, now: Instant) -> ChannelActivityStatus {
        let idle = now.saturating_duration_since(self.last_receive_proof());
        let stall_threshold = self.model.stall_threshold();
        ChannelActivityStatus {
            model: self.model,
            idle,
            stall_threshold,
            stalled: stall_threshold.is_some_and(|threshold| idle > threshold),
            last_inbound_age: self.last_inbound.map(|at| now.saturating_duration_since(at)),
            last_outbound_age: self.last_outbound.map(|at| now.saturating_duration_since(at)),
            last_upstream_age: self.last_upstream.map(|at| now.saturating_duration_since(at)),
        }
    }
}

/// Observed state of one channel listener at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelActivityStatus {
    /// How this channel's liveness can be judged at all.
    pub model: LivenessModel,
    /// Time since the receive path last proved itself.
    pub idle: Duration,
    /// Silence beyond which the channel is *reported* stalled. `None` for
    /// passive channels, where no honest verdict exists.
    pub stall_threshold: Option<Duration>,
    /// Whether the channel is reported as stalled. Report-only: never act on it.
    pub stalled: bool,
    pub last_inbound_age: Option<Duration>,
    pub last_outbound_age: Option<Duration>,
    pub last_upstream_age: Option<Duration>,
}

impl ChannelActivityStatus {
    /// One-line operator summary, e.g. `stalled for 91s (no upstream round-trip
    /// within 30s)`.
    #[must_use]
    pub fn stall_summary(&self) -> String {
        self.stall_threshold.map_or_else(
            || format!("listener idle for {}s", self.idle.as_secs()),
            |threshold| {
                format!(
                    "listener stalled: no receive activity for {}s (expected at least one every {}s)",
                    self.idle.as_secs(),
                    threshold.as_secs()
                )
            },
        )
    }
}

type Registry = Mutex<BTreeMap<String, ChannelActivity>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Declare (or re-declare, on listener restart) how a channel proves liveness.
///
/// Resets the activity baseline so a freshly restarted listener is not reported
/// as stalled for the silence of its previous incarnation.
pub fn register(channel: &str, model: LivenessModel) {
    let mut guard = registry().lock();
    guard.insert(channel.to_string(), ChannelActivity::new(model, Instant::now()));
}

fn record<F>(channel: &str, update: F)
where
    F: FnOnce(&mut ChannelActivity, Instant),
{
    let now = Instant::now();
    let mut guard = registry().lock();
    // A channel that records activity before registering is still tracked, but
    // without a declared cadence no stall verdict is claimed for it.
    let entry = guard
        .entry(channel.to_string())
        .or_insert_with(|| ChannelActivity::new(LivenessModel::Passive, now));
    update(entry, now);
}

/// Record that one upstream receive round-trip completed, including an empty one.
///
/// This is the signal that keeps a healthy but quiet channel out of the stalled
/// bucket, so it belongs immediately after the remote side answers — not after a
/// message is found.
pub fn record_upstream(channel: &str) {
    record(channel, |entry, now| entry.last_upstream = Some(now));
}

/// Record that an inbound message was delivered to the pipeline.
pub fn record_inbound(channel: &str) {
    record(channel, |entry, now| {
        entry.last_inbound = Some(now);
        // Receiving a message is also proof that the upstream round-trip that
        // carried it completed, which matters for push-only channels that have
        // no separate round-trip hook.
        entry.last_upstream = Some(now);
    });
}

/// Record that an outbound send succeeded.
pub fn record_outbound(channel: &str) {
    record(channel, |entry, now| entry.last_outbound = Some(now));
}

/// Current status of one channel, or `None` if nothing was ever recorded for it.
#[must_use]
pub fn status(channel: &str) -> Option<ChannelActivityStatus> {
    let now = Instant::now();
    let guard = registry().lock();
    guard.get(channel).map(|entry| entry.status_at(now))
}

impl ChannelActivityStatus {
    /// Project into the health-registry shape published on `/health`, the daemon
    /// state file and `prx doctor`.
    #[must_use]
    pub fn to_component_activity(&self) -> crate::health::ComponentActivity {
        crate::health::ComponentActivity {
            liveness: self.model.label(),
            idle_seconds: self.idle.as_secs(),
            stall_threshold_seconds: self.stall_threshold.map(|threshold| threshold.as_secs()),
            stalled: self.stalled,
            last_inbound_seconds_ago: self.last_inbound_age.map(|age| age.as_secs()),
            last_outbound_seconds_ago: self.last_outbound_age.map(|age| age.as_secs()),
            last_upstream_seconds_ago: self.last_upstream_age.map(|age| age.as_secs()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn bounded_model_derives_threshold_from_expectation() {
        let model = LivenessModel::Bounded(Duration::from_secs(30));
        assert_eq!(model.stall_threshold(), Some(Duration::from_secs(90)));
        assert_eq!(LivenessModel::Passive.stall_threshold(), None);
    }

    #[test]
    fn fresh_registration_is_not_stalled() {
        let name = unique("activity-fresh");
        register(&name, LivenessModel::Bounded(Duration::from_secs(30)));

        let status = status(&name).expect("registered channel should have a status");
        assert!(!status.stalled);
        assert_eq!(status.stall_threshold, Some(Duration::from_secs(90)));
    }

    /// Moves forward from a fixed base instead of subtracting from
    /// `Instant::now()`, which can underflow the platform's monotonic clock base
    /// on a freshly booted machine.
    fn later(base: Instant, secs: u64) -> Instant {
        base.checked_add(Duration::from_secs(secs)).unwrap_or(base)
    }

    #[test]
    fn silence_beyond_threshold_is_reported_stalled() {
        let base = Instant::now();
        let activity = ChannelActivity {
            model: LivenessModel::Bounded(Duration::from_secs(10)),
            started_at: base,
            last_inbound: None,
            last_outbound: Some(later(base, 60)),
            last_upstream: Some(later(base, 15)),
        };

        let status = activity.status_at(later(base, 60));

        // Outbound traffic must not mask a dead receive path.
        assert!(status.stalled);
        assert_eq!(status.idle, Duration::from_secs(45));
        assert_eq!(status.last_outbound_age, Some(Duration::ZERO));
        assert!(status.stall_summary().contains("stalled"));
    }

    #[test]
    fn recent_upstream_round_trip_clears_stall() {
        let base = Instant::now();
        let activity = ChannelActivity {
            model: LivenessModel::Bounded(Duration::from_secs(10)),
            started_at: base,
            last_inbound: None,
            last_outbound: None,
            last_upstream: Some(later(base, 595)),
        };

        let status = activity.status_at(later(base, 600));

        assert!(!status.stalled);
        assert_eq!(status.idle, Duration::from_secs(5));
    }

    #[test]
    fn passive_channel_never_claims_a_stall_verdict() {
        let base = Instant::now();
        let activity = ChannelActivity {
            model: LivenessModel::Passive,
            started_at: base,
            last_inbound: None,
            last_outbound: None,
            last_upstream: None,
        };

        let status = activity.status_at(later(base, 86_400));

        assert!(!status.stalled);
        assert_eq!(status.stall_threshold, None);
        assert!(status.stall_summary().contains("idle"));
    }

    #[test]
    fn recording_activity_updates_the_registry() {
        let name = unique("activity-record");
        register(&name, LivenessModel::Bounded(Duration::from_secs(30)));

        record_inbound(&name);
        record_outbound(&name);
        record_upstream(&name);

        let status = status(&name).expect("recorded channel should have a status");
        assert!(status.last_inbound_age.is_some());
        assert!(status.last_outbound_age.is_some());
        assert!(status.last_upstream_age.is_some());
    }

    #[test]
    fn re_registration_resets_the_baseline() {
        let name = unique("activity-restart");
        register(&name, LivenessModel::Bounded(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(20));
        assert!(status(&name).is_some_and(|status| status.stalled));

        register(&name, LivenessModel::Bounded(Duration::from_secs(30)));

        assert!(status(&name).is_some_and(|status| !status.stalled));
    }

    #[test]
    fn unknown_channel_has_no_status() {
        assert!(status(&unique("activity-unknown")).is_none());
    }
}
