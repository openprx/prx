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
use std::time::Instant;

#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use tokio::sync::watch;

/// How far the logical epoch sits ahead of wall time while a clock is installed.
#[cfg(test)]
const EPOCH_LEAD: Duration = Duration::from_hours(1);

#[cfg(test)]
tokio::task_local! {
    static CLOCK: Arc<ManualClock>;
}

#[cfg(test)]
#[derive(Debug)]
struct SiteHook {
    stall: Duration,
    advance: Duration,
    hits: usize,
    on_clock: bool,
}

/// A deadline clock whose elapsed time changes only through [`ManualClock::advance`].
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ManualClock {
    origin: Instant,
    elapsed_ms: watch::Sender<u64>,
    stall_ms: AtomicU64,
    advance_ms: AtomicU64,
    sites: parking_lot::Mutex<std::collections::HashMap<&'static str, SiteHook>>,
    checkpoint: parking_lot::Mutex<Option<(&'static str, usize, Duration)>>,
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
            checkpoint: parking_lot::Mutex::new(None),
            sites: parking_lot::Mutex::new(std::collections::HashMap::new()),
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

    /// Spend logical time on the Nth matching engine checkpoint.
    pub(crate) fn arm_checkpoint(&self, name: &'static str, occurrence: usize, by: Duration) {
        *self.checkpoint.lock() = Some((name, occurrence, by));
    }

    pub(crate) fn arm_site(&self, name: &'static str, stall: Duration, advance: Duration) {
        self.sites.lock().insert(
            name,
            SiteHook {
                stall,
                advance,
                hits: 0,
                on_clock: true,
            },
        );
    }

    pub(crate) fn site_hits(&self, name: &'static str) -> usize {
        self.sites.lock().get(name).map_or(0, |entry| entry.hits)
    }

    pub(crate) fn site_on_clock(&self, name: &'static str) -> bool {
        self.sites
            .lock()
            .get(name)
            .is_some_and(|entry| entry.hits > 0 && entry.on_clock)
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

/// The clock installed on this task, if any. Spawned work does not see it
/// until the caller passes this handle back in through [`scope`] or [`sync_scope`].
#[cfg(test)]
pub(crate) fn capture() -> Option<Arc<ManualClock>> {
    current()
}

/// Runs sync work, including a `spawn_blocking` body, on `clock`.
#[cfg(test)]
pub(crate) fn sync_scope<R>(clock: Arc<ManualClock>, body: impl FnOnce() -> R) -> R {
    CLOCK.sync_scope(clock, body)
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

/// Whether `deadline` (a tokio instant built from a decision deadline) has elapsed.
pub(crate) fn tokio_deadline_reached(deadline: tokio::time::Instant) -> bool {
    #[cfg(test)]
    if let Some(clock) = current() {
        return clock.instant() >= deadline.into_std();
    }
    tokio::time::Instant::now() >= deadline
}

/// Engine phase hook; only the installed logical test clock can advance time.
#[cfg(not(test))]
pub(crate) const fn checkpoint(_name: &'static str) {}

#[cfg(test)]
pub(crate) fn checkpoint(checkpoint_name: &'static str) {
    #[cfg(test)]
    if let Some(clock) = current() {
        let mut armed = clock.checkpoint.lock();
        if let Some((name, remaining, by)) = armed.as_mut()
            && *name == checkpoint_name
        {
            *remaining = remaining.saturating_sub(1);
            if *remaining == 0 {
                clock.advance(*by);
                *armed = None;
            }
        }
    }
}

/// Run the hook armed on this shared clock, independently of clock re-entry.
/// Worker regression hooks must still stall when clock propagation is removed.
#[cfg(test)]
impl ManualClock {
    pub(crate) fn visit_site(&self, name: &'static str) {
        let action = {
            let mut sites = self.sites.lock();
            sites.get_mut(name).and_then(|entry| {
                entry.hits += 1;
                entry.on_clock &= current().is_some_and(|clock| std::ptr::eq(self, Arc::as_ptr(&clock)));
                (entry.hits == 1).then_some((entry.stall, entry.advance))
            })
        };
        if let Some((stall, advance)) = action {
            std::thread::sleep(stall);
            self.advance(advance);
        }
    }
}

#[cfg(test)]
pub(crate) fn visit_site(name: &'static str) {
    if let Some(clock) = current() {
        clock.visit_site(name);
    }
}

/// Read the installed test clock only for a deadline minted in its epoch.
/// Other I/O keeps its independent wall deadline, even inside a decision scope.
pub(crate) fn deadline_now(deadline: Instant) -> Instant {
    #[cfg(test)]
    if let Some(clock) = current()
        && deadline >= clock.origin
    {
        return clock.instant();
    }
    let _ = deadline;
    Instant::now()
}

#[cfg(test)]
pub(crate) fn is_logical_deadline(deadline: Instant) -> bool {
    current().is_some_and(|clock| deadline >= clock.origin)
}

#[cfg(test)]
pub(crate) fn inherit_sync<F, R>(work: F) -> impl FnOnce() -> R + Send
where
    F: FnOnce() -> R + Send,
{
    let clock = capture();
    move || match clock {
        Some(clock) => sync_scope(clock, work),
        None => work(),
    }
}

#[cfg(test)]
pub(crate) fn inherit<F: Future + Send + 'static>(
    work: F,
) -> std::pin::Pin<Box<dyn Future<Output = F::Output> + Send>> {
    let clock = capture();
    Box::pin(async move {
        match clock {
            Some(clock) => scope(clock, work).await,
            None => work.await,
        }
    })
}
