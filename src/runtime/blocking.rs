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
    pub const fn utilization_percent(&self) -> usize {
        if self.max_threads == 0 {
            return 0;
        }
        self.in_flight.saturating_mul(100) / self.max_threads
    }

    /// Whether current occupancy is at or above the saturation threshold.
    #[must_use]
    pub const fn is_saturated(&self) -> bool {
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

/// Synchronous blocking sections currently executing through
/// [`run_sync_blocking`].
static SYNC_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// High-water mark of [`SYNC_IN_FLIGHT`] since process start.
static SYNC_PEAK_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Total synchronous blocking sections run since process start.
static SYNC_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Sections that had to be pushed onto a private thread because the caller was
/// on a runtime whose scheduler cannot hand its work to a replacement thread.
static SYNC_OFFLOADED: AtomicU64 = AtomicU64::new(0);

/// Times concurrent synchronous blocking exceeded the machine's parallelism.
static SYNC_SATURATION_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Point-in-time view of synchronous blocking demand.
///
/// Distinct from [`BlockingPoolStats`]: these callers do not queue for a pool
/// slot, they occupy the *calling* thread for the duration of the work, so the
/// pressure they create is on the async scheduler rather than on the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncBlockingStats {
    /// Sections currently executing.
    pub in_flight: usize,
    /// Highest `in_flight` observed since process start.
    pub peak_in_flight: usize,
    /// Sections run since process start.
    pub total: u64,
    /// Sections that ran on a private thread instead of in place.
    pub offloaded: u64,
    /// Times `in_flight` exceeded the machine's available parallelism.
    pub saturation_events: u64,
}

/// Read the current synchronous-blocking counters.
#[must_use]
pub fn sync_snapshot() -> SyncBlockingStats {
    SyncBlockingStats {
        in_flight: SYNC_IN_FLIGHT.load(Ordering::Relaxed),
        peak_in_flight: SYNC_PEAK_IN_FLIGHT.load(Ordering::Relaxed),
        total: SYNC_TOTAL.load(Ordering::Relaxed),
        offloaded: SYNC_OFFLOADED.load(Ordering::Relaxed),
        saturation_events: SYNC_SATURATION_EVENTS.load(Ordering::Relaxed),
    }
}

/// RAII counter for one in-flight synchronous blocking section.
struct SyncBlockingGuard;

impl SyncBlockingGuard {
    fn enter() -> Self {
        let now = SYNC_IN_FLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
        SYNC_TOTAL.fetch_add(1, Ordering::Relaxed);
        SYNC_PEAK_IN_FLIGHT.fetch_max(now, Ordering::Relaxed);

        let parallelism = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        if now > parallelism {
            SYNC_SATURATION_EVENTS.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                in_flight = now,
                parallelism,
                "more synchronous blocking sections than CPUs; async progress depends on replacement workers"
            );
        }
        Self
    }
}

impl Drop for SyncBlockingGuard {
    fn drop(&mut self) {
        SYNC_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Run synchronous blocking work from a *synchronous* caller that may be
/// executing on an async worker thread.
///
/// This exists for library boundaries that are not async and cannot become
/// async without changing every caller — most notably the blocking `postgres`
/// client, which drives its own private Tokio runtime and therefore panics with
/// "cannot start a runtime from within a runtime" if it is invoked while the
/// current thread has a runtime entered.
///
/// `spawn_blocking` is deliberately *not* the mechanism here. It requires a
/// `'static` closure, whereas these call sites pass closures that borrow their
/// caller's stack (`&self`, a job snapshot, a lease), and its handle can only
/// be awaited, which a synchronous function cannot do.
///
/// The three cases:
///
/// * **No runtime** — nothing to protect; the closure runs in place.
/// * **Multi-threaded runtime** — `block_in_place` moves this worker's queue to
///   a replacement thread before the closure runs, so the scheduler keeps
///   making progress instead of losing a worker, and the closure observes a
///   thread with no runtime entered.
/// * **Any other flavour** (current-thread, used by unit tests) —
///   `block_in_place` is unavailable there, so the closure is offloaded to a
///   scoped thread and the caller waits for it. That does block the caller,
///   which is why it is the fallback rather than the default.
///
/// The closure returns a `Result` so that a refused thread is reported as an
/// error rather than as a panic. Panics raised *inside* the closure propagate
/// unchanged in every case.
///
/// # Errors
///
/// Returns the closure's error, or an error if the OS refuses the offload
/// thread on the fallback path.
pub fn run_sync_blocking<F, T>(name: &str, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send,
    T: Send,
{
    let _guard = SyncBlockingGuard::enter();
    match tokio::runtime::Handle::try_current() {
        Err(_) => f(),
        Ok(handle) => match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
            _ => {
                SYNC_OFFLOADED.fetch_add(1, Ordering::Relaxed);
                run_on_scoped_thread(name, f)
            }
        },
    }
}

/// Run `f` on a scoped thread and wait for it, preserving panic semantics.
fn run_on_scoped_thread<F, T>(name: &str, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name(name.to_string())
            .spawn_scoped(scope, f)
            .map_err(|error| anyhow::anyhow!("failed to spawn `{name}` blocking offload thread: {error}"))?;
        match handle.join() {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
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
    use anyhow::Context;

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

    /// Drive a nested current-thread runtime to completion, mirroring what the
    /// blocking `postgres` client does internally. It panics with "cannot start
    /// a runtime from within a runtime" unless the caller has really left the
    /// outer runtime's entered state, so this is the property under test rather
    /// than the presence of a `Handle` (which `block_in_place` keeps).
    fn drive_nested_runtime() -> anyhow::Result<u32> {
        let nested = tokio::runtime::Builder::new_current_thread()
            .build()
            .context("test: build nested runtime")?;
        Ok(nested.block_on(async { 5_u32 }))
    }

    #[test]
    fn sync_blocking_runs_in_place_without_a_runtime() {
        let before = sync_snapshot();
        let value = run_sync_blocking("prx-test-sync", || Ok(7_u32)).expect("test: closure succeeds");
        assert_eq!(value, 7, "the wrapper must not alter the closure's result");
        // Counters are process-wide and tests run in parallel, so only monotonic
        // growth can be asserted here.
        assert!(sync_snapshot().total > before.total);
    }

    #[test]
    fn sync_blocking_propagates_the_closure_error() {
        let error = run_sync_blocking("prx-test-sync", || anyhow::bail!("test: closure fails"))
            .map(|(): ()| ())
            .expect_err("test: error surfaces");
        assert!(error.to_string().contains("test: closure fails"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_blocking_allows_a_nested_runtime_on_the_multi_thread_runtime() {
        let value = run_sync_blocking("prx-test-sync", drive_nested_runtime).expect("test: nested runtime runs");
        assert_eq!(value, 5);
    }

    #[tokio::test]
    async fn sync_blocking_offloads_on_the_current_thread_runtime() {
        let before = sync_snapshot();
        let value = run_sync_blocking("prx-test-sync", drive_nested_runtime).expect("test: nested runtime runs");
        assert_eq!(value, 5);
        assert!(
            sync_snapshot().offloaded > before.offloaded,
            "the current-thread runtime must take the scoped-thread fallback"
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
