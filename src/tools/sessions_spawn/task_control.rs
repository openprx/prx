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
