//! Tracks evolution verdict tasks (judge, the write an accepted verdict
//! allows, and the evolution log row) so the daemon can cancel them and wait
//! for them before it returns and the runtime is shut down (D-046,
//! R-PMMCi F-1).
//!
//! `tokio_util::task::TaskTracker` would do this, but it needs the
//! `tokio-util/rt` feature, which this build does not enable; this is the
//! same idea on `tokio::sync::Notify` and a counter.

use parking_lot::Mutex;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A set of tracked tasks that can be cancelled and awaited together.
#[derive(Clone, Default)]
pub struct CommitTracker {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    active: AtomicUsize,
    idle: Notify,
    shutdown: Mutex<CancellationToken>,
}

/// Decrements the active count when a tracked task finishes or is dropped.
struct ActiveGuard(Arc<Inner>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

static DAEMON_COMMITS: LazyLock<CommitTracker> = LazyLock::new(CommitTracker::default);

/// The tracker the daemon's evolution pipelines use and `daemon::run` drains.
pub fn daemon_commits() -> &'static CommitTracker {
    &DAEMON_COMMITS
}

impl CommitTracker {
    /// A token for one tracked task. [`Self::drain`] cancels it, and after a
    /// drain has begun every new token is born cancelled.
    pub fn task_token(&self) -> CancellationToken {
        self.inner.shutdown.lock().child_token()
    }

    /// Run `future` as a tracked task.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.inner.active.fetch_add(1, Ordering::AcqRel);
        let guard = ActiveGuard(Arc::clone(&self.inner));
        tokio::spawn(async move {
            let _guard = guard;
            future.await
        })
    }

    /// Tracked tasks that have not finished.
    pub fn active(&self) -> usize {
        self.inner.active.load(Ordering::Acquire)
    }

    /// Close the tracker: cancel every tracked task's token, and every
    /// token handed out from now on (R-PMMCj G-1), then wait up to `bound`
    /// for the tasks to finish. A task that starts during or after the drain
    /// therefore writes nothing and records its cycle as deferred. Returns
    /// `false` when some were still running at the bound. The daemon's
    /// tracker is drained once, as the process exits; there is no reopen.
    pub async fn drain(&self, bound: Duration) -> bool {
        self.inner.shutdown.lock().cancel();
        tokio::time::timeout(bound, self.wait_idle()).await.is_ok()
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            // Register before checking, so a task finishing in between is
            // not missed.
            notified.as_mut().enable();
            if self.active() == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drain_cancels_and_waits_for_tracked_tasks() {
        let tracker = CommitTracker::default();
        let token = tracker.task_token();
        let finished = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&finished);
        tracker.spawn(async move {
            token.cancelled().await;
            // Work that outlives the cancellation, like writing the row.
            tokio::time::sleep(Duration::from_millis(50)).await;
            seen.fetch_add(1, Ordering::AcqRel);
        });
        assert_eq!(tracker.active(), 1);
        assert!(tracker.drain(Duration::from_secs(5)).await);
        assert_eq!(finished.load(Ordering::Acquire), 1);
        assert_eq!(tracker.active(), 0);
    }

    #[tokio::test]
    async fn drain_reports_a_task_that_outlives_the_bound() {
        let tracker = CommitTracker::default();
        let handle = tracker.spawn(std::future::pending::<()>());
        assert!(!tracker.drain(Duration::from_millis(20)).await);
        handle.abort();
        let _ = handle.await;
        assert_eq!(tracker.active(), 0, "an aborted task is no longer counted");
    }

    /// R-PMMCj G-1: the drain closes the tracker for good.
    #[tokio::test]
    async fn tokens_handed_out_after_a_drain_are_born_cancelled() {
        let tracker = CommitTracker::default();
        assert!(tracker.drain(Duration::from_millis(10)).await);
        assert!(tracker.task_token().is_cancelled());
        assert!(
            tracker.drain(Duration::from_millis(10)).await,
            "a second drain is a no-op"
        );
        assert!(tracker.task_token().is_cancelled());
    }
}
