//! Runtime-owned lifecycle for terminal writes across immutable generations.
use crate::runtime::turn_decision::TurnDecisionService;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct CleanupTracker {
    state: parking_lot::Mutex<State>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct State {
    closed: bool,
    failed: bool,
    next_id: u64,
    pending: BTreeMap<u64, CancellationToken>,
    services: Vec<Arc<TurnDecisionService>>,
    ingress_monitors: Vec<IngressMonitor>,
    ingress_lifetimes: BTreeMap<String, Arc<dyn crate::memory::ingress_recovery::IngressLifetime>>,
}

struct IngressMonitor(tokio::task::JoinHandle<()>);
impl Drop for IngressMonitor {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct CleanupTicket {
    tracker: Arc<CleanupTracker>,
    id: u64,
    finished: bool,
}

impl CleanupTracker {
    pub(crate) async fn scope<F: std::future::Future>(self: &Arc<Self>, work: F) -> F::Output {
        CURRENT_CLEANUP.scope(Arc::clone(self), work).await
    }

    pub(crate) fn register(self: &Arc<Self>, cancel: CancellationToken) -> anyhow::Result<CleanupTicket> {
        let mut state = self.state.lock();
        // An unconfirmed write can still complete after its request ends.
        // Do not admit another turn under this owner until recovery establishes
        // the durable result; child admission already enforces this boundary.
        anyhow::ensure!(
            !state.closed && !state.failed,
            "runtime cleanup owner is closed or has unconfirmed failures"
        );
        self.register_locked(&mut state, cancel)
    }

    fn register_child(self: &Arc<Self>, cancel: CancellationToken) -> anyhow::Result<CleanupTicket> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            !state.closed && !state.failed,
            "runtime cleanup owner is closed or has unconfirmed failures"
        );
        self.register_locked(&mut state, cancel)
    }

    fn register_locked(
        self: &Arc<Self>,
        state: &mut State,
        cancel: CancellationToken,
    ) -> anyhow::Result<CleanupTicket> {
        let id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("runtime cleanup identity exhausted"))?;
        state.next_id = id;
        state.pending.insert(id, cancel);
        Ok(CleanupTicket {
            tracker: Arc::clone(self),
            id,
            finished: false,
        })
    }

    pub(crate) fn retain_service(&self, service: Arc<TurnDecisionService>) {
        self.state.lock().services.push(service);
    }

    /// Child publication must finish before its owner starts draining. Keeping
    /// this check and retention under one lock closes the drain snapshot race.
    pub(crate) fn retain_child_service(&self, service: Arc<TurnDecisionService>) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            !state.closed && !state.failed,
            "child decision publication owner is closed or has unconfirmed failures"
        );
        state.services.push(service);
        Ok(())
    }

    // Retiring a generation transfers its queue ownership into a tracked task
    // atomically, so shutdown cannot observe a gap between the two owners.
    pub(crate) fn retire(self: &Arc<Self>, service: &Arc<TurnDecisionService>) -> anyhow::Result<CleanupTicket> {
        let mut state = self.state.lock();
        let ticket = self.register_locked(&mut state, CancellationToken::new())?;
        state.services.retain(|retained| !Arc::ptr_eq(retained, service));
        Ok(ticket)
    }

    pub(crate) async fn drain(&self, deadline: tokio::time::Instant) -> anyhow::Result<()> {
        let cancellations = {
            let mut state = self.state.lock();
            state.closed = true;
            state.pending.values().cloned().collect::<Vec<_>>()
        };
        for cancel in cancellations {
            cancel.cancel();
        }
        let (services, failed) = loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock();
                if state.pending.is_empty() {
                    break (state.services.clone(), state.failed);
                }
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| anyhow::anyhow!("runtime terminal cleanup drain timed out"))?;
        };
        let mut queues_drained = true;
        for service in services {
            queues_drained &= service.drain_writes(deadline.into_std()).await;
        }
        anyhow::ensure!(!failed, "runtime terminal cleanup has unconfirmed failures");
        anyhow::ensure!(queues_drained, "runtime decision queues did not drain");
        Ok(())
    }
}

impl CleanupTicket {
    pub(crate) fn owner(&self) -> Arc<CleanupTracker> {
        Arc::clone(&self.tracker)
    }
    pub(crate) fn watch_ingress_lifetime(&self, lifetime: &Arc<dyn crate::memory::ingress_recovery::IngressLifetime>) {
        let Some(signal) = lifetime.loss_signal() else {
            return;
        };
        let owner = Arc::downgrade(&self.tracker);
        let task = tokio::spawn(async move {
            signal.cancelled().await;
            if let Some(owner) = owner.upgrade() {
                let mut state = owner.state.lock();
                state.failed = true;
                for cancellation in state.pending.values() {
                    cancellation.cancel();
                }
                drop(state);
                owner.changed.notify_waiters();
            }
        });
        self.tracker.state.lock().ingress_monitors.push(IngressMonitor(task));
    }

    pub(crate) fn retain_ingress_lifetime(
        &self,
        namespace: String,
        lifetime: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
    ) -> anyhow::Result<()> {
        let mut state = self.tracker.state.lock();
        if let Some(existing) = state.ingress_lifetimes.get(&namespace) {
            anyhow::ensure!(
                Arc::ptr_eq(existing, &lifetime),
                "cleanup owner backend lifetime changed"
            );
            return Ok(());
        }
        anyhow::ensure!(
            state.ingress_lifetimes.len() < 16,
            "cleanup ingress lifetime capacity exhausted"
        );
        state.ingress_lifetimes.insert(namespace, lifetime.clone());
        drop(state);
        self.watch_ingress_lifetime(&lifetime);
        Ok(())
    }
    pub(crate) fn finish(mut self, success: bool) {
        self.release(success);
    }
    fn release(&mut self, success: bool) {
        if self.finished {
            return;
        }
        let mut state = self.tracker.state.lock();
        state.pending.remove(&self.id);
        state.failed |= !success;
        self.finished = true;
        drop(state);
        self.tracker.changed.notify_waiters();
    }
}

impl Drop for CleanupTicket {
    fn drop(&mut self) {
        self.release(false);
    }
}

// Task-local ownership is explicit across spawn. It is not an authority token.
tokio::task_local! {
    static CURRENT_CLEANUP: Arc<CleanupTracker>;
}

pub(crate) fn current_tracker() -> Option<Arc<CleanupTracker>> {
    CURRENT_CLEANUP.try_with(Arc::clone).ok()
}

#[derive(Debug, thiserror::Error)]
#[error("runtime terminal cleanup was not confirmed")]
pub(crate) struct CleanupIncomplete;

/// A generation, job or manual invocation owns its children until their writes
/// are acknowledged. On forced drop the parent's ticket follows the drain task,
/// so the parent cannot mistake an aborted runner for completed cleanup.
pub(crate) struct CleanupScope {
    tracker: Arc<CleanupTracker>,
    parent_ticket: Option<CleanupTicket>,
    cancellation: CancellationToken,
    retirement_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    cleanup_ack: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    finished: bool,
}

impl CleanupScope {
    pub(crate) fn new(parent: Option<&Arc<CleanupTracker>>, cancellation: CancellationToken) -> anyhow::Result<Self> {
        let parent_ticket = parent
            .map(|parent| parent.register_child(cancellation.clone()))
            .transpose()?;
        Ok(Self {
            tracker: Arc::new(CleanupTracker::default()),
            parent_ticket,
            cancellation,
            retirement_guard: None,
            cleanup_ack: None,
            finished: false,
        })
    }

    /// Keep a component's replacement generation behind the previous cleanup,
    /// including when its supervisor stops waiting and aborts the runner.
    pub(crate) fn with_retirement_guard(mut self, guard: tokio::sync::OwnedMutexGuard<()>) -> Self {
        self.retirement_guard = Some(guard);
        self
    }

    /// The caller owns the receiver outside its abortable work task.
    pub(crate) fn with_acknowledgement(
        mut self,
        acknowledgement: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    ) -> Self {
        self.cleanup_ack = acknowledgement;
        self
    }

    /// Poll admission under this same owned tracker without closing it.
    /// Successful admission can retain accounting receipts for later work;
    /// the caller must still consume this owner with `run`, or drop it so the
    /// existing owned drain retains its parent ticket and acknowledgement.
    /// Cancellation drops the admission future before the caller starts drain.
    pub(crate) async fn scope_admission<T>(
        &self,
        work: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let work = CURRENT_CLEANUP.scope(Arc::clone(&self.tracker), work);
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(anyhow::anyhow!("runtime admission cancelled")),
            result = work => result,
        }
    }

    pub(crate) async fn run<T>(
        mut self,
        work: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let result = {
            let work = CURRENT_CLEANUP.scope(Arc::clone(&self.tracker), work);
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => Err(anyhow::anyhow!("runtime work cancelled")),
                result = work => result,
            }
        };
        // The work future has been dropped before closing admission and waiting
        // for its terminal guards. Never drain while still owning such a guard.
        let cleanup = self.tracker.drain(cleanup_deadline()).await;
        if let Some(ticket) = self.parent_ticket.take() {
            ticket.finish(cleanup.is_ok());
        }
        if let Some(ack) = self.cleanup_ack.take() {
            let _ = ack.send(
                cleanup
                    .as_ref()
                    .map(|_| ())
                    .map_err(|_| "child terminal cleanup is unconfirmed".to_owned()),
            );
        }
        self.finished = true;
        cleanup.map_err(|error| error.context(CleanupIncomplete))?;
        result
    }
}

fn cleanup_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(15)
}

impl Drop for CleanupScope {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.cancellation.cancel();
        let tracker = Arc::clone(&self.tracker);
        let parent_ticket = self.parent_ticket.take();
        let retirement_guard = self.retirement_guard.take();
        let cleanup_ack = self.cleanup_ack.take();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _retirement_guard = retirement_guard;
                    let result = tracker.drain(cleanup_deadline()).await;
                    if result.is_err() {
                        tracing::error!("Interrupted runtime cleanup remains unconfirmed");
                    }
                    if let Some(ticket) = parent_ticket {
                        ticket.finish(result.is_ok());
                    }
                    if let Some(ack) = cleanup_ack {
                        let _ = ack.send(result.map_err(|_| "child terminal cleanup is unconfirmed".to_owned()));
                    }
                });
            }
            Err(_) => {
                // Dropping the parent ticket records a failure, not success.
                tracing::error!("Interrupted runtime cleanup has no runtime to acknowledge writes");
                if let Some(ack) = cleanup_ack {
                    let _ = ack.send(Err("child terminal cleanup runtime is unavailable".to_owned()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
