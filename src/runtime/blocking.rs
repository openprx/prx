//! Blocking-pool sizing and saturation observability.
//!
//! PRX is an *unbounded* agent runtime: there is deliberately no global cap on
//! how many turns, sub-agents, tool calls or channel sessions run at once. That
//! makes the tokio **blocking pool** the last remaining implicit concurrency
//! gate in the process, and the most dangerous one, because it fails by
//! *deadlock* rather than by rejection:
//!
//! * every `spawn_blocking` task occupies one pool thread for its whole
//!   duration;
//! * once all `max_blocking_threads` slots are taken, further `spawn_blocking`
//!   calls queue *forever* (tokio never times them out);
//! * if any queued task is what an already-running task is waiting for
//!   (a lock, a channel ack, a database write), the pool is deadlocked.
//!
//! Tokio's built-in default is 512 threads regardless of the machine, which is
//! both arbitrary and invisible. This module replaces that with:
//!
//! 1. a hardware-derived, TOML-overridable cap
//!    ([`default_max_blocking_threads`], `[runtime] max_blocking_threads`);
//! 2. a drop-in [`spawn_blocking`] wrapper that tracks in-flight and peak
//!    occupancy so saturation is *observable* instead of being inferred from a
//!    hung process.
//!
//! Reducing the *demand* side matters just as much as raising the cap. Two
//! rules apply across the codebase:
//!
//! * **Never put an unbounded-duration loop in the blocking pool.** A watcher,
//!   a stdin reader or a render loop holds its slot until process exit, so it
//!   is a permanent capacity leak. Such loops use [`spawn_detached_thread`],
//!   which allocates a dedicated OS thread outside the pool.
//! * **Never put blocking network I/O in the blocking pool** when an async
//!   client exists; async I/O consumes no pool slot at all.
//!
//! Note that swapping `std::fs` inside `spawn_blocking` for `tokio::fs` does
//! *not* help: `tokio::fs` is itself implemented on top of `spawn_blocking`,
//! and doing so per-syscall rather than per-sequence typically increases pool
//! traffic. Those call sites are intentionally left as they are.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Lower bound for the blocking pool on very small machines.
///
/// Even a single-core container runs many concurrent agent sessions, each of
/// which may hold a slot for a SQLite write or a subprocess reap, so the floor
/// is set well above the core count.
const MIN_MAX_BLOCKING_THREADS: usize = 2_048;

/// Upper bound for the auto-derived default.
///
/// Blocking threads are cheap while idle (tokio reaps them after ~10s), but
/// each live one still costs a stack, so the automatic default stops here.
/// Operators who need more can raise `[runtime] max_blocking_threads` past
/// this ceiling explicitly.
const MAX_MAX_BLOCKING_THREADS: usize = 16_384;

/// Slots granted per available CPU when deriving the default.
const BLOCKING_THREADS_PER_CPU: usize = 512;

/// Fraction of the cap (in percent) at which occupancy is reported as
/// saturated. Chosen below 100% so the warning fires while the pool can still
/// make progress, not after it has already wedged.
const SATURATION_WARN_PERCENT: usize = 80;

/// Configured cap, published by [`configure`]. Zero means "not configured yet",
/// in which case reported saturation ratios fall back to the derived default.
static CONFIGURED_MAX: AtomicUsize = AtomicUsize::new(0);

/// Number of wrapped blocking tasks currently occupying (or queued for) a slot.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// High-water mark of [`IN_FLIGHT`] since process start.
static PEAK_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Number of times occupancy crossed the saturation threshold.
static SATURATION_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Total wrapped blocking tasks spawned since process start.
static SPAWNED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Blocking-pool cap derived from the machine the process is actually running
/// on.
///
/// `available_parallelism` is cgroup-aware on Linux (a container limited to
/// `CPUQuota=100%` reports 1), so the derived value tracks the real slice of
/// hardware rather than the host's core count. The result is clamped into
/// `[MIN_MAX_BLOCKING_THREADS, MAX_MAX_BLOCKING_THREADS]`.
#[must_use]
pub fn default_max_blocking_threads() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cpus.saturating_mul(BLOCKING_THREADS_PER_CPU)
        .clamp(MIN_MAX_BLOCKING_THREADS, MAX_MAX_BLOCKING_THREADS)
}

/// Publish the cap the tokio runtime was actually built with.
///
/// Called once from `main` immediately after the runtime is constructed, so the
/// saturation ratio reported by [`snapshot`] is measured against the real cap
/// rather than a guess. A zero value is ignored (the derived default stands).
pub fn configure(max_blocking_threads: usize) {
    if max_blocking_threads > 0 {
        CONFIGURED_MAX.store(max_blocking_threads, Ordering::Relaxed);
    }
}

/// The cap currently in force: the configured value, or the derived default
/// when `configure` has not run (unit tests, embedded uses of the library).
#[must_use]
pub fn max_blocking_threads() -> usize {
    let configured = CONFIGURED_MAX.load(Ordering::Relaxed);
    if configured > 0 {
        configured
    } else {
        default_max_blocking_threads()
    }
}

/// Point-in-time view of blocking-pool demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockingPoolStats {
    /// Cap the runtime was built with.
    pub max_threads: usize,
    /// Wrapped tasks currently holding or queued for a slot.
    pub in_flight: usize,
    /// Highest `in_flight` observed since process start.
    pub peak_in_flight: usize,
    /// Times occupancy crossed [`SATURATION_WARN_PERCENT`] of the cap.
    pub saturation_events: u64,
    /// Wrapped tasks spawned since process start.
    pub spawned_total: u64,
}

impl BlockingPoolStats {
    /// Occupancy as a percentage of the cap, saturating at `usize::MAX` guards.
    #[must_use]
    pub fn utilization_percent(&self) -> usize {
        if self.max_threads == 0 {
            return 0;
        }
        self.in_flight.saturating_mul(100) / self.max_threads
    }

    /// Whether current occupancy is at or above the saturation threshold.
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        self.utilization_percent() >= SATURATION_WARN_PERCENT
    }
}

/// Read the current blocking-pool demand counters.
///
/// Counters cover tasks spawned through [`spawn_blocking`]; direct
/// `tokio::task::spawn_blocking` calls are invisible here, which is why the
/// codebase routes blocking work through this module.
#[must_use]
pub fn snapshot() -> BlockingPoolStats {
    BlockingPoolStats {
        max_threads: max_blocking_threads(),
        in_flight: IN_FLIGHT.load(Ordering::Relaxed),
        peak_in_flight: PEAK_IN_FLIGHT.load(Ordering::Relaxed),
        saturation_events: SATURATION_EVENTS.load(Ordering::Relaxed),
        spawned_total: SPAWNED_TOTAL.load(Ordering::Relaxed),
    }
}

/// RAII counter for one in-flight blocking task.
///
/// Decrements on drop so the count stays correct when the closure panics or
/// unwinds; tokio catches the panic and drops the closure's locals either way.
struct InFlightGuard;

impl InFlightGuard {
    fn enter() -> Self {
        let now = IN_FLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
        SPAWNED_TOTAL.fetch_add(1, Ordering::Relaxed);
        PEAK_IN_FLIGHT.fetch_max(now, Ordering::Relaxed);

        let max = max_blocking_threads();
        // `>= max * pct / 100` without intermediate overflow on absurd caps.
        if max > 0 && now.saturating_mul(100) >= max.saturating_mul(SATURATION_WARN_PERCENT) {
            SATURATION_EVENTS.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                in_flight = now,
                max_blocking_threads = max,
                "tokio blocking pool is saturated; further blocking work will queue without a timeout. \
                 Raise `[runtime] max_blocking_threads` or move long-lived blocking loops onto dedicated threads."
            );
        }
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Drop-in replacement for `tokio::task::spawn_blocking` that accounts for
/// blocking-pool occupancy.
///
/// Semantics are identical to the tokio original — same bounds, same
/// `JoinHandle`, same panic propagation — so call sites only swap the path.
///
/// # Panics
///
/// Same as `tokio::task::spawn_blocking`: panics if called from outside a tokio
/// runtime context.
pub fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _guard = InFlightGuard::enter();
        f()
    })
}

/// Run an unbounded-duration blocking loop on a dedicated OS thread instead of
/// in the tokio blocking pool.
///
/// Use this for work that only ends at process shutdown — filesystem watchers,
/// stdin readers, terminal render loops. Putting such a loop in the blocking
/// pool permanently removes a slot from every other blocking caller, which is
/// exactly the starvation this module exists to prevent.
///
/// The returned handle can be joined, but callers usually detach: these loops
/// terminate by observing a cancellation token, not by being joined.
///
/// # Errors
///
/// Returns an error if the OS refuses to create the thread.
pub fn spawn_detached_thread<F>(name: &str, f: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new().name(name.to_string()).spawn(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cap_respects_bounds() {
        let derived = default_max_blocking_threads();
        assert!(derived >= MIN_MAX_BLOCKING_THREADS);
        assert!(derived <= MAX_MAX_BLOCKING_THREADS);
    }

    #[test]
    fn default_cap_exceeds_tokio_builtin() {
        // The whole point of overriding is to leave tokio's hardware-agnostic
        // 512 behind.
        assert!(default_max_blocking_threads() > 512);
    }

    #[test]
    fn configure_ignores_zero() {
        let before = max_blocking_threads();
        configure(0);
        assert_eq!(max_blocking_threads(), before);
    }

    #[test]
    fn utilization_is_zero_without_capacity() {
        let stats = BlockingPoolStats {
            max_threads: 0,
            in_flight: 10,
            peak_in_flight: 10,
            saturation_events: 0,
            spawned_total: 10,
        };
        assert_eq!(stats.utilization_percent(), 0);
        assert!(!stats.is_saturated());
    }

    #[test]
    fn utilization_flags_saturation() {
        let stats = BlockingPoolStats {
            max_threads: 100,
            in_flight: 80,
            peak_in_flight: 80,
            saturation_events: 1,
            spawned_total: 200,
        };
        assert_eq!(stats.utilization_percent(), 80);
        assert!(stats.is_saturated());
    }

    /// Number of sequential spawns used by the slot-release tests. Large enough
    /// that a leaked slot per spawn would be unmistakable against the noise of
    /// other tests sharing these process-wide counters.
    const RELEASE_PROBE_SPAWNS: usize = 64;

    #[tokio::test]
    async fn spawn_blocking_is_transparent_and_counts_tasks() {
        let before = snapshot();
        let value = spawn_blocking(|| 21_u32 * 2).await.expect("blocking task joins");
        assert_eq!(value, 42, "the wrapper must not alter the closure's result");

        let after = snapshot();
        assert!(after.spawned_total > before.spawned_total);
        assert!(after.peak_in_flight >= 1);
    }

    #[tokio::test]
    async fn spawn_blocking_releases_its_slot_when_the_closure_returns() {
        // Counters are process-wide and the test harness runs tests in
        // parallel, so assert on the peak's *growth* rather than an absolute
        // in-flight count. These spawns are strictly sequential: a healthy
        // guard contributes at most 1 to the peak, while a guard that never
        // released would push it up by RELEASE_PROBE_SPAWNS.
        let before = snapshot();
        for _ in 0..RELEASE_PROBE_SPAWNS {
            spawn_blocking(|| ()).await.expect("blocking task joins");
        }
        let after = snapshot();
        assert!(
            after.peak_in_flight < before.peak_in_flight + RELEASE_PROBE_SPAWNS,
            "sequential spawns leaked in-flight slots: {before:?} -> {after:?}"
        );
    }

    #[tokio::test]
    async fn spawn_blocking_releases_its_slot_when_the_closure_panics() {
        let before = snapshot();
        for _ in 0..RELEASE_PROBE_SPAWNS {
            let handle = spawn_blocking(|| panic!("test: blocking task panics"));
            assert!(handle.await.is_err(), "panic must surface as a join error");
        }
        let after = snapshot();
        assert!(
            after.peak_in_flight < before.peak_in_flight + RELEASE_PROBE_SPAWNS,
            "panicking tasks leaked in-flight slots: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn detached_thread_runs_outside_the_pool() {
        // Returning a `std::thread::JoinHandle` rather than a
        // `tokio::task::JoinHandle` is the structural guarantee that this work
        // never occupies a blocking-pool slot. The explicit annotation below is
        // the assertion: it stops compiling the moment the implementation
        // reaches for `spawn_blocking`, whose handle has no `join()`.
        //
        // Deliberately *no* assertion on `snapshot().spawned_total`. That is a
        // process-wide counter which every other test bumps concurrently, so a
        // before/after comparison here races the rest of the suite rather than
        // proving anything about this call.
        let (tx, rx) = std::sync::mpsc::channel();
        let handle: std::thread::JoinHandle<()> = spawn_detached_thread("prx-test-detached", move || {
            let _ = tx.send(());
        })
        .expect("test: thread spawns");
        rx.recv().expect("test: detached thread ran");
        handle.join().expect("test: detached thread joins");
    }
}
