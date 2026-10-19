//! Runtime-owned lifecycle for terminal writes across immutable generations.
use crate::runtime::turn_decision::TurnDecisionService;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct CleanupTracker {
    state: parking_lot::Mutex<State>,
    changed: tokio::sync::Notify,
    #[cfg(test)]
    diagnostic: diagnostic::Binding,
}

#[derive(Default)]
struct State {
    closed: bool,
    failed: bool,
    next_id: u64,
    pending: BTreeMap<u64, CancellationToken>,
    services: Vec<Arc<TurnDecisionService>>,
    prepared_stores: Vec<PreparedStoreQueues>,
    ingress_monitors: Vec<IngressMonitor>,
    ingress_lifetimes: BTreeMap<String, Arc<dyn crate::memory::ingress_recovery::IngressLifetime>>,
}

#[derive(Clone)]
struct PreparedStoreQueues {
    store: Option<Arc<crate::causal_tree::store::CausalDecisionStore>>,
    events: Option<Arc<super::causal_events::DurableCausalEventSink>>,
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
        #[cfg(test)]
        self.diagnostic
            .record("ticket_registered", id, state.pending.len(), state.failed, None);
        Ok(CleanupTicket {
            tracker: Arc::clone(self),
            id,
            finished: false,
        })
    }

    pub(crate) fn retain_service(&self, service: Arc<TurnDecisionService>) {
        let mut state = self.state.lock();
        state
            .prepared_stores
            .retain(|prepared| !service.owns_prepared_queues(prepared.store.as_ref(), prepared.events.as_ref()));
        state.services.push(service);
    }

    /// Child publication must finish before its owner starts draining. Keeping
    /// this check and retention under one lock closes the drain snapshot race.
    pub(crate) fn retain_child_service(&self, service: Arc<TurnDecisionService>) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            !state.closed && !state.failed,
            "child decision publication owner is closed or has unconfirmed failures"
        );
        state
            .prepared_stores
            .retain(|prepared| !service.owns_prepared_queues(prepared.store.as_ref(), prepared.events.as_ref()));
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
            #[cfg(test)]
            self.diagnostic
                .record("drain_started", 0, state.pending.len(), state.failed, None);
            state.pending.values().cloned().collect::<Vec<_>>()
        };
        for cancel in cancellations {
            cancel.cancel();
        }
        let (services, prepared_stores, failed) = loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock();
                if state.pending.is_empty() {
                    #[cfg(test)]
                    self.diagnostic
                        .record("pending_empty", 0, 0, state.failed, Some(!state.failed));
                    break (state.services.clone(), state.prepared_stores.clone(), state.failed);
                }
            }
            tokio::time::timeout_at(deadline, changed).await.map_err(|_| {
                #[cfg(test)]
                {
                    let state = self.state.lock();
                    self.diagnostic
                        .record("pending_timeout", 0, state.pending.len(), state.failed, Some(false));
                }
                anyhow::anyhow!("runtime terminal cleanup drain timed out")
            })?;
        };
        let mut queues_drained = true;
        for service in services {
            #[cfg(test)]
            let drained = self.diagnostic.scope(service.drain_writes(deadline.into_std())).await;
            #[cfg(not(test))]
            let drained = service.drain_writes(deadline.into_std()).await;
            queues_drained &= drained;
            #[cfg(test)]
            self.diagnostic
                .record("service_queues_drained", 0, 0, failed, Some(drained));
        }
        for prepared in prepared_stores {
            if let Some(writer) = prepared.store.and_then(|store| store.writer()) {
                queues_drained &= writer.drain(deadline.into_std()).await;
            }
            if let Some(events) = prepared.events {
                queues_drained &= events.drain(deadline.into_std()).await;
            }
        }
        #[cfg(test)]
        self.diagnostic
            .record("drain_completed", 0, 0, failed, Some(!failed && queues_drained));
        anyhow::ensure!(!failed, "runtime terminal cleanup has unconfirmed failures");
        anyhow::ensure!(queues_drained, "runtime decision queues did not drain");
        Ok(())
    }
}

impl CleanupTicket {
    /// Keep construction queues owned even if the awaiting publisher was
    /// cancelled. This handoff precedes releasing its registered I/O ticket.
    pub(crate) fn retain_prepared_store(
        &self,
        store: Option<Arc<crate::causal_tree::store::CausalDecisionStore>>,
        events: Option<Arc<super::causal_events::DurableCausalEventSink>>,
    ) {
        self.tracker
            .state
            .lock()
            .prepared_stores
            .push(PreparedStoreQueues { store, events });
    }

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
                #[cfg(test)]
                owner
                    .diagnostic
                    .record("ingress_lifetime_lost", 0, state.pending.len(), true, Some(false));
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
        #[cfg(test)]
        self.tracker.diagnostic.record(
            "ticket_released",
            self.id,
            state.pending.len(),
            state.failed,
            Some(success),
        );
        self.finished = true;
        drop(state);
        self.tracker.changed.notify_waiters();
    }
}

impl Drop for CleanupTicket {
    fn drop(&mut self) {
        #[cfg(test)]
        if !self.finished {
            let state = self.tracker.state.lock();
            self.tracker.diagnostic.record(
                "unfinished_ticket_dropped",
                self.id,
                state.pending.len(),
                state.failed,
                Some(false),
            );
        }
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
    #[cfg(test)]
    diagnostic_completion: Option<diagnostic::Completion>,
}

impl CleanupScope {
    pub(crate) fn new(parent: Option<&Arc<CleanupTracker>>, cancellation: CancellationToken) -> anyhow::Result<Self> {
        let parent_ticket = parent
            .map(|parent| parent.register_child(cancellation.clone()))
            .transpose()?;
        #[cfg(test)]
        let tracker = Arc::new(CleanupTracker {
            diagnostic: diagnostic::Binding::for_parent(parent.map(|p| &p.diagnostic)),
            ..Default::default()
        });
        #[cfg(not(test))]
        let tracker = Arc::new(CleanupTracker::default());
        #[cfg(test)]
        tracker.diagnostic.record(
            "scope_created_parent_owner",
            parent.map_or(0, |p| p.diagnostic.owner()),
            0,
            false,
            None,
        );
        #[cfg(test)]
        let diagnostic_completion = tracker.diagnostic.start_completion();
        Ok(Self {
            tracker,
            parent_ticket,
            cancellation,
            retirement_guard: None,
            cleanup_ack: None,
            finished: false,
            #[cfg(test)]
            diagnostic_completion,
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
            let sent = ack.send(
                cleanup
                    .as_ref()
                    .map(|_| ())
                    .map_err(|_| "child terminal cleanup is unconfirmed".to_owned()),
            );
            #[cfg(not(test))]
            drop(sent);
            #[cfg(test)]
            self.tracker
                .diagnostic
                .record("ack_normal_cleanup", 0, 0, cleanup.is_err(), Some(cleanup.is_ok()));
            #[cfg(test)]
            self.tracker
                .diagnostic
                .record("ack_normal_sender", 0, 0, cleanup.is_err(), Some(sent.is_ok()));
        }
        self.finished = true;
        #[cfg(test)]
        if let Some(completion) = &mut self.diagnostic_completion {
            completion.mark_finished();
        }
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
        #[cfg(test)]
        let diagnostic_completion = self.diagnostic_completion.take();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    #[cfg(test)]
                    let mut diagnostic_completion = diagnostic_completion;
                    #[cfg(not(test))]
                    let _retirement_guard = retirement_guard;
                    #[cfg(test)]
                    let retirement_guard = retirement_guard;
                    let result = tracker.drain(cleanup_deadline()).await;
                    if result.is_err() {
                        tracing::error!("Interrupted runtime cleanup remains unconfirmed");
                    }
                    if let Some(ticket) = parent_ticket {
                        ticket.finish(result.is_ok());
                    }
                    if let Some(ack) = cleanup_ack {
                        #[cfg(test)]
                        tracker
                            .diagnostic
                            .record("ack_drop_cleanup", 0, 0, result.is_err(), Some(result.is_ok()));
                        let sent = ack.send(result.map_err(|_| "child terminal cleanup is unconfirmed".to_owned()));
                        #[cfg(not(test))]
                        drop(sent);
                        #[cfg(test)]
                        tracker
                            .diagnostic
                            .record("ack_drop_sender", 0, 0, false, Some(sent.is_ok()));
                    }
                    #[cfg(test)]
                    {
                        // Release task-owned resources before signalling its end to the fixture.
                        drop(retirement_guard);
                        drop(tracker);
                        if let Some(completion) = &mut diagnostic_completion {
                            completion.mark_finished();
                        }
                    }
                });
            }
            Err(_) => {
                #[cfg(test)]
                let _diagnostic_completion = diagnostic_completion;
                // Dropping the parent ticket records a failure, not success.
                tracing::error!("Interrupted runtime cleanup has no runtime to acknowledge writes");
                if let Some(ack) = cleanup_ack {
                    #[cfg(test)]
                    self.tracker
                        .diagnostic
                        .record("ack_no_runtime_cleanup", 0, 0, true, Some(false));
                    let sent = ack.send(Err("child terminal cleanup runtime is unavailable".to_owned()));
                    #[cfg(not(test))]
                    drop(sent);
                    #[cfg(test)]
                    self.tracker
                        .diagnostic
                        .record("ack_no_runtime_sender", 0, 0, true, Some(sent.is_ok()));
                }
                drop(parent_ticket);
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod diagnostic;
