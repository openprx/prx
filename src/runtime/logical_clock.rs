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

#[cfg(test)]
use std::future::Future;
use std::time::Instant;

#[cfg(test)]
use std::sync::Arc;
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
    sites: parking_lot::Mutex<std::collections::HashMap<&'static str, SiteHook>>,
}

#[cfg(test)]
impl ManualClock {
    #[cfg(test)]
    pub(crate) fn new() -> Arc<Self> {
        let (elapsed_ms, _) = watch::channel(0);
        Arc::new(Self {
            origin: Instant::now() + EPOCH_LEAD,
            elapsed_ms,
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

/// Runs a fixture on its calling task without requiring its full future to be Send.
#[cfg(test)]
pub(crate) fn scope_local<F: Future>(clock: Arc<ManualClock>, fut: F) -> impl Future<Output = F::Output> {
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
