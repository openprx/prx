//! Provider-turn worker supervisor.
//!
//! Every provider-turn worker task is awaited by one supervisor. After every
//! join result (normal return, panic, abort) the supervisor sends
//! [`ProviderTurnLifecycleEvent::Exited`], the worker's single exit path, so the
//! execution lease and abort handle are always retired. A panic is the only
//! failure it reports: it asks the registry owner to accept the failure for the
//! worker's lease and, only when accepted, sends a non-retryable
//! `StreamFailed` before `Exited`. The reducer then accepts that terminal only
//! while the turn's draft is visible and not yet ready for commit, so a panic
//! after the turn's own terminal never changes its outcome.

use tokio::sync::{mpsc, oneshot};

use crate::chat::action::Action;
use crate::chat::dispatcher::ProviderTurnLifecycleEvent;
use crate::chat::turn_scheduler::TurnTaskId;

/// The failure text of a panicked provider-turn worker.
pub(crate) const WORKER_FAILED_ERROR: &str = "provider turn worker failed";

/// The lifecycle identity of one supervised worker execution.
pub(crate) struct WorkerLease {
    pub(crate) task_id: TurnTaskId,
    pub(crate) lease_id: u64,
    pub(crate) lifecycle_tx: mpsc::Sender<ProviderTurnLifecycleEvent>,
}

/// What a supervisor needs besides the worker's join handle.
pub(crate) struct WorkerSupervision {
    /// `None` for workers without a scheduler task (tests and non-chat
    /// callers): no registry exists, so the reducer is the only failure gate.
    pub(crate) lease: Option<WorkerLease>,
    pub(crate) draft_id: String,
    pub(crate) action_tx: mpsc::Sender<Action>,
}

/// Await `worker`, report a panic once if the registry accepts it, then
/// retire the lease.
pub(crate) async fn supervise_provider_turn_worker(
    worker: tokio::task::JoinHandle<()>,
    supervision: WorkerSupervision,
) {
    let joined = worker.await;
    let panicked = match &joined {
        Ok(()) => false,
        Err(error) if error.is_cancelled() => {
            tracing::debug!(
                draft_id_len = supervision.draft_id.len(),
                "provider turn worker aborted"
            );
            false
        }
        Err(error) => {
            // The panic payload may carry turn content; only the class is logged.
            tracing::warn!(
                is_panic = error.is_panic(),
                draft_id_len = supervision.draft_id.len(),
                "provider turn worker panicked"
            );
            error.is_panic()
        }
    };
    if panicked && failure_accepted(supervision.lease.as_ref()).await {
        let failed = Action::StreamFailed {
            draft_id: supervision.draft_id.clone(),
            err: WORKER_FAILED_ERROR.to_owned(),
            retryable: false,
        };
        if supervision.action_tx.send(failed).await.is_err() {
            tracing::debug!("provider turn worker failure: action channel closed");
        }
    }
    if let Some(lease) = supervision.lease {
        let exited = ProviderTurnLifecycleEvent::Exited {
            task_id: lease.task_id,
            lease_id: lease.lease_id,
        };
        if lease.lifecycle_tx.send(exited).await.is_err() {
            tracing::debug!(
                task_id = lease.task_id.get(),
                lease_id = lease.lease_id,
                "provider turn worker exit: lifecycle channel closed"
            );
        }
    }
}

/// Ask the registry owner to accept the worker failure for this lease.
async fn failure_accepted(lease: Option<&WorkerLease>) -> bool {
    let Some(lease) = lease else {
        return true;
    };
    let (accepted_tx, accepted_rx) = oneshot::channel();
    let request = ProviderTurnLifecycleEvent::FailureRequested {
        task_id: lease.task_id,
        lease_id: lease.lease_id,
        accepted: accepted_tx,
    };
    if lease.lifecycle_tx.send(request).await.is_err() {
        tracing::debug!(
            task_id = lease.task_id.get(),
            lease_id = lease.lease_id,
            "provider turn worker failure: lifecycle channel closed"
        );
        return false;
    }
    accepted_rx.await.unwrap_or(false)
}
