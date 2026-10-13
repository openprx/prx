//! Task-mode termination is acknowledged by the observer outside the run.
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskFinalization {
    Natural,
    Terminated,
    Unconfirmed,
}

#[derive(Debug, Default)]
struct State {
    abort: Option<tokio::task::AbortHandle>,
    reason: Option<String>,
    operator_lifetime: Option<crate::runtime::operator_spawn::task::OperatorTaskLifetime>,
    ordinary_lifetime: Option<crate::runtime::child_source::ordinary_task::OrdinaryTaskLifetime>,
}

#[derive(Debug)]
pub(crate) struct TaskRunControl {
    cancellation: tokio_util::sync::CancellationToken,
    state: parking_lot::Mutex<State>,
    finalized: watch::Sender<Option<TaskFinalization>>,
}

impl TaskRunControl {
    pub(super) fn new() -> Arc<Self> {
        let (finalized, _) = watch::channel(None);
        Arc::new(Self {
            cancellation: tokio_util::sync::CancellationToken::new(),
            state: parking_lot::Mutex::new(State::default()),
            finalized,
        })
    }

    pub(super) const fn cancellation(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancellation
    }

    pub(super) fn retain_operator_lifetime(
        &self,
        lifetime: crate::runtime::operator_spawn::task::OperatorTaskLifetime,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            state.operator_lifetime.is_none()
                && state.ordinary_lifetime.is_none()
                && state.reason.is_none()
                && self.finalization().is_none()
                && !self.cancellation.is_cancelled(),
            "task authority owner is already closed or assigned"
        );
        state.operator_lifetime = Some(lifetime);
        Ok(())
    }

    pub(super) fn retain_ordinary_lifetime(
        &self,
        lifetime: crate::runtime::child_source::ordinary_task::OrdinaryTaskLifetime,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            state.operator_lifetime.is_none()
                && state.ordinary_lifetime.is_none()
                && state.reason.is_none()
                && self.finalization().is_none()
                && !self.cancellation.is_cancelled(),
            "task authority owner is already closed or assigned"
        );
        state.ordinary_lifetime = Some(lifetime);
        Ok(())
    }

    pub(super) fn attach(&self, handle: tokio::task::AbortHandle) {
        let mut state = self.state.lock();
        state.abort = Some(handle.clone());
        if state.reason.is_some() {
            handle.abort();
        }
    }

    pub(super) fn reason(&self) -> Option<String> {
        self.state.lock().reason.clone()
    }

    pub(crate) fn finalization(&self) -> Option<TaskFinalization> {
        *self.finalized.borrow()
    }

    pub(super) fn revoke_authority(&self) {
        self.cancellation.cancel();
    }

    pub(super) fn publish(&self, result: TaskFinalization) {
        self.cancellation.cancel();
        let lifetime = {
            let mut state = self.state.lock();
            (state.ordinary_lifetime.take(), state.operator_lifetime.take())
        };
        drop(lifetime);
        self.finalized.send_if_modified(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(result);
            true
        });
    }

    /// None means pending, not a successful kill. Requests made before the
    /// driver publishes its AbortHandle are applied by attach().
    pub(crate) async fn request_termination(&self, reason: &str) -> Option<TaskFinalization> {
        {
            let mut state = self.state.lock();
            if let Some(result) = self.finalization() {
                return Some(result);
            }
            self.cancellation.cancel();
            if state.reason.is_none() {
                state.reason = Some(reason.to_owned());
            }
            if let Some(handle) = &state.abort {
                handle.abort();
            }
        }
        let mut receiver = self.finalized.subscribe();
        let wait = async {
            loop {
                let observed = *receiver.borrow_and_update();
                if let Some(result) = observed {
                    return result;
                }
                if receiver.changed().await.is_err() {
                    return TaskFinalization::Unconfirmed;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(15), wait)
            .await
            .ok()
    }
}

// The fixture observer owns the real task join; no terminal-state injection API.
#[cfg(test)]
impl TaskRunControl {
    pub(crate) fn observe_fixture_task(task: tokio::task::JoinHandle<()>) -> OwnedFixtureTask {
        let control = Self::new();
        let abort = task.abort_handle();
        control.attach(abort.clone());
        let observed = Arc::clone(&control);
        let observer = tokio::spawn(async move {
            let result = match task.await {
                Ok(()) => TaskFinalization::Natural,
                Err(error) if error.is_cancelled() => TaskFinalization::Terminated,
                Err(_) => TaskFinalization::Unconfirmed,
            };
            observed.publish(result);
        });
        OwnedFixtureTask {
            control,
            abort,
            observer,
        }
    }
}

#[cfg(test)]
pub(crate) struct OwnedFixtureTask {
    control: Arc<TaskRunControl>,
    abort: tokio::task::AbortHandle,
    observer: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
impl OwnedFixtureTask {
    pub(crate) fn control(&self) -> Arc<TaskRunControl> {
        Arc::clone(&self.control)
    }

    pub(crate) fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.abort.clone()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.abort.is_finished()
    }

    pub(crate) async fn finish(mut self, outcome: std::thread::Result<()>) {
        self.abort.abort();
        let cleanup = tokio::time::timeout(std::time::Duration::from_secs(15), &mut self.observer).await;
        // On timeout the observer retains the original task JoinHandle and
        // remains responsible for its real join. This is UNKNOWN, never success.
        let joined = matches!(cleanup, Ok(Ok(())));
        if !joined {
            tracing::error!("PRX fixture task observer cleanup UNKNOWN");
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        assert!(joined, "fixture observer did not join within owner deadline");
    }
}

#[cfg(test)]
impl Drop for OwnedFixtureTask {
    fn drop(&mut self) {
        // External cancellation requests abort; the observer still owns join.
        // Neither this request nor dropping its handle claims finalization.
        self.abort.abort();
    }
}
