//! Opt-in logical time for one decision task.
//!
//! With no clock installed, [`now`] is `Instant::now()` and the timeout
//! helpers use tokio's clock. Acceptance tests install a clock and leave it
//! there: wall preemption does not spend `extra_latency_budget_ms` or the
//! rehearsal cap. Those budgets still elapse when the test advances the clock.
//!
//! The clock's epoch sits ahead of wall time, so a blocking read that still
//! compares `Instant::now()` with the decision deadline does not expire while
//! the test has not advanced logical time. Timeout helpers on this path wait
//! on logical time, so they do not sleep until that epoch.

use std::future::Future;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use tokio::sync::watch;

/// How far the logical epoch sits ahead of wall time while a clock is installed.
#[cfg(test)]
const EPOCH_LEAD: Duration = Duration::from_hours(1);

#[cfg(test)]
tokio::task_local! {
    static CLOCK: Arc<ManualClock>;
}

/// A deadline clock whose elapsed time changes only through [`ManualClock::advance`].
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ManualClock {
    origin: Instant,
    elapsed_ms: watch::Sender<u64>,
    stall_ms: AtomicU64,
    advance_ms: AtomicU64,
}

#[cfg(test)]
impl ManualClock {
    #[cfg(test)]
    pub(crate) fn new() -> Arc<Self> {
        let (elapsed_ms, _) = watch::channel(0);
        Arc::new(Self {
            origin: Instant::now() + EPOCH_LEAD,
            elapsed_ms,
            stall_ms: AtomicU64::new(0),
            advance_ms: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) fn instant(&self) -> Instant {
        self.origin + Duration::from_millis(self.elapsed())
    }

    #[cfg(test)]
    fn elapsed(&self) -> u64 {
        *self.elapsed_ms.borrow()
    }

    #[cfg(test)]
    pub(crate) fn advance(&self, by: Duration) {
        let add = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.elapsed_ms.send_modify(|slot| {
            *slot = slot.saturating_add(add);
        });
    }

    /// After the decision deadline is captured, sleep this long on the wall
    /// clock without spending logical time. Consumed once.
    #[cfg(test)]
    pub(crate) fn arm_wall_stall(&self, delay: Duration) {
        self.stall_ms
            .store(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX), Ordering::Release);
    }

    /// After the decision deadline is captured, spend this much logical time.
    /// Consumed once.
    #[cfg(test)]
    pub(crate) fn arm_logical_advance(&self, delay: Duration) {
        self.advance_ms
            .store(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX), Ordering::Release);
    }

    #[cfg(test)]
    async fn wait_until<T>(&self, deadline: Instant, call: impl Future<Output = T>) -> Result<T, ()> {
        let limit_ms = u64::try_from(deadline.saturating_duration_since(self.origin).as_millis()).unwrap_or(u64::MAX);
        tokio::pin!(call);
        let mut seen = self.elapsed_ms.subscribe();
        loop {
            if *seen.borrow() >= limit_ms {
                return Err(());
            }
            tokio::select! {
                biased;
                output = &mut call => return Ok(output),
                changed = seen.changed() => {
                    if changed.is_err() {
                        return Ok(call.await);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
fn current() -> Option<Arc<ManualClock>> {
    CLOCK.try_with(Arc::clone).ok()
}

/// Runs `fut` with `clock` visible to deadline checks on this task.
#[cfg(test)]
pub(crate) fn scope<F>(clock: Arc<ManualClock>, fut: F) -> impl Future<Output = F::Output> + Send
where
    F: Future + Send,
{
    CLOCK.scope(clock, fut)
}

/// `Instant::now()` when no clock is installed.
pub(crate) fn now() -> Instant {
    #[cfg(test)]
    if let Some(clock) = current() {
        return clock.instant();
    }
    Instant::now()
}

#[cfg(test)]
pub(crate) fn installed() -> bool {
    current().is_some()
}

#[cfg(not(test))]
pub(crate) const fn installed() -> bool {
    false
}

/// Applies a test stall or logical advance armed on the current clock.
/// Unarmed tests return immediately. Production does not call this.
#[cfg(test)]
pub(crate) async fn apply_test_stall() {
    let Some(clock) = current() else {
        return;
    };
    let stall = clock.stall_ms.swap(0, Ordering::AcqRel);
    if stall > 0 {
        tokio::time::sleep(Duration::from_millis(stall)).await;
    }
    let advance = clock.advance_ms.swap(0, Ordering::AcqRel);
    if advance > 0 {
        clock.advance(Duration::from_millis(advance));
    }
}

/// Waits for `call` until `deadline`. With a clock installed the wait follows
/// logical time. The error means the deadline elapsed.
pub(crate) async fn timeout_at_std<T>(deadline: Instant, call: impl Future<Output = T>) -> Result<T, ()> {
    #[cfg(test)]
    if let Some(clock) = current() {
        return clock.wait_until(deadline, call).await;
    }
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), call)
        .await
        .map_err(|_| ())
}

/// Waits for `call` for at most `limit`. With a clock installed, `limit` is
/// logical time from [`now`].
pub(crate) async fn timeout_for<T>(limit: Duration, call: impl Future<Output = T>) -> Result<T, ()> {
    #[cfg(test)]
    if let Some(clock) = current() {
        let deadline = clock.instant().checked_add(limit).unwrap_or_else(|| clock.instant());
        return clock.wait_until(deadline, call).await;
    }
    tokio::time::timeout(limit, call).await.map_err(|_| ())
}

/// Whether `deadline` (a tokio instant built from a decision deadline) has elapsed.
pub(crate) fn tokio_deadline_reached(deadline: tokio::time::Instant) -> bool {
    #[cfg(test)]
    if let Some(clock) = current() {
        return clock.instant() >= deadline.into_std();
    }
    tokio::time::Instant::now() >= deadline
}
