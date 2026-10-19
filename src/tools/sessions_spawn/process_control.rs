use super::{
    HistoryEntry, PROCESS_TERMINATION_REQUEST_TIMEOUT_SECS, SpawnDelegationStatus, SpawnLineage, SubAgentRun,
    SubAgentStatus,
};
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::{RwLock, watch};

/// Everything the registry row for a process-mode run is built from.
pub(super) struct ProcessModeRunSeed<'a> {
    pub(super) id: String,
    pub(super) task: String,
    pub(super) lineage: &'a SpawnLineage,
    pub(super) recipient: Option<String>,
    pub(super) channel_name: Option<String>,
    pub(super) process_control: Arc<ProcessRunControl>,
    pub(super) history: Arc<RwLock<Vec<HistoryEntry>>>,
    pub(super) steer_tx: tokio::sync::mpsc::Sender<String>,
    pub(super) parent_run_id: Option<String>,
    pub(super) session_scope_key: String,
    pub(super) spawn_depth: usize,
    pub(super) delegation: Option<SpawnDelegationStatus>,
    pub(super) progress: Arc<crate::agent::idle::ProgressBeat>,
    pub(super) batch_id: Option<String>,
}

/// Build the registry row for a process-mode run.
///
/// Split out from the spawn path so the steering wiring is testable: the row a
/// process-mode spawn publishes must always carry a live `steer_tx`, otherwise
/// `sessions_send` rejects the run as a "legacy run without steer support".
pub(super) fn new_process_mode_run(seed: ProcessModeRunSeed<'_>) -> SubAgentRun {
    SubAgentRun {
        id: seed.id,
        task: seed.task,
        owner_id: seed.lineage.owner_id.clone(),
        topic_id: seed.lineage.topic_id.clone(),
        source_message_event_id: seed.lineage.source_message_event_id.clone(),
        started_at: Utc::now(),
        finished_at: None,
        status: SubAgentStatus::Running,
        recipient: seed.recipient,
        channel_name: seed.channel_name,
        abort_handle: None,
        process_control: Some(seed.process_control),
        task_control: None,
        history: seed.history,
        steer_tx: Some(seed.steer_tx),
        parent_run_id: seed.parent_run_id,
        session_scope_key: seed.session_scope_key,
        spawn_depth: seed.spawn_depth,
        delegation: seed.delegation,
        token_usage_records: Vec::new(),
        progress: seed.progress,
        batch_id: seed.batch_id,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessFinalization {
    Natural,
    Terminated,
    TerminationFailed,
    Unconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessTerminationRequestResult {
    Finalized(ProcessFinalization),
    Pending,
}

/// Per-run process lifecycle control. The first termination reason wins and
/// every requester waits for the sole process owner to publish finalization.
#[derive(Debug)]
pub(crate) struct ProcessRunControl {
    /// A recovery projection has no live process owner and cannot signal it.
    pub(super) recovery_only: bool,
    termination_tx: watch::Sender<Option<String>>,
    pub(super) finalized_tx: watch::Sender<Option<ProcessFinalization>>,
    request_timeout: std::time::Duration,
    #[cfg(test)]
    timeout_barrier: Option<Arc<tokio::sync::Barrier>>,
}

impl ProcessRunControl {
    pub(super) fn new() -> Arc<Self> {
        Self::new_with_request_timeout(std::time::Duration::from_secs(PROCESS_TERMINATION_REQUEST_TIMEOUT_SECS))
    }

    pub(super) fn new_with_request_timeout(request_timeout: std::time::Duration) -> Arc<Self> {
        let (termination_tx, _) = watch::channel(None);
        let (finalized_tx, _) = watch::channel(None);
        Arc::new(Self {
            recovery_only: false,
            termination_tx,
            finalized_tx,
            request_timeout,
            #[cfg(test)]
            timeout_barrier: None,
        })
    }

    pub(super) fn recovered_unconfirmed() -> Arc<Self> {
        let (termination_tx, _) = watch::channel(None);
        let (finalized_tx, _) = watch::channel(Some(ProcessFinalization::Unconfirmed));
        Arc::new(Self {
            recovery_only: true,
            termination_tx,
            finalized_tx,
            request_timeout: std::time::Duration::from_secs(PROCESS_TERMINATION_REQUEST_TIMEOUT_SECS),
            #[cfg(test)]
            timeout_barrier: None,
        })
    }

    pub(super) async fn termination_requested(&self) -> String {
        let mut receiver = self.termination_tx.subscribe();
        loop {
            let reason = receiver.borrow_and_update().clone();
            if let Some(reason) = reason {
                return reason;
            }
            if receiver.changed().await.is_err() {
                return "process owner stopped".to_string();
            }
        }
    }

    pub(crate) async fn request_termination(&self, reason: &str) -> ProcessTerminationRequestResult {
        self.request_termination_with_timeout(reason, self.request_timeout)
            .await
    }

    pub(super) async fn request_termination_with_timeout(
        &self,
        reason: &str,
        wait_timeout: std::time::Duration,
    ) -> ProcessTerminationRequestResult {
        let reason = reason.to_string();
        self.termination_tx.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(reason);
                true
            }
        });

        let wait_for_owner = async {
            let mut receiver = self.finalized_tx.subscribe();
            loop {
                let finalization = *receiver.borrow_and_update();
                if let Some(finalization) = finalization {
                    return ProcessTerminationRequestResult::Finalized(finalization);
                }
                if receiver.changed().await.is_err() {
                    return ProcessTerminationRequestResult::Pending;
                }
            }
        };
        // The test-only timeout barrier must be awaited before the final state
        // read, so this branch cannot use Clippy's synchronous map_or_else form.
        #[allow(clippy::option_if_let_else)]
        match tokio::time::timeout(wait_timeout, wait_for_owner).await {
            Ok(result) => result,
            Err(_) => {
                #[cfg(test)]
                if let Some(barrier) = &self.timeout_barrier {
                    barrier.wait().await;
                    barrier.wait().await;
                }
                self.finalization().map_or_else(
                    || ProcessTerminationRequestResult::Pending,
                    ProcessTerminationRequestResult::Finalized,
                )
            }
        }
    }

    pub(super) fn finalize(&self, finalization: ProcessFinalization) {
        self.finalized_tx.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(finalization);
                true
            }
        });
    }

    pub(crate) fn finalization(&self) -> Option<ProcessFinalization> {
        *self.finalized_tx.borrow()
    }

    /// Resolve once the sole owner has published a finalization — or once it is
    /// gone without publishing one, which is the same thing for a waiter that
    /// only needs to know the run is over.
    pub(super) async fn finalized(&self) {
        let mut receiver = self.finalized_tx.subscribe();
        while receiver.borrow_and_update().is_none() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    pub(super) fn termination_reason(&self) -> Option<String> {
        self.termination_tx.borrow().clone()
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Arc<Self> {
        Self::new()
    }

    #[cfg(test)]
    pub(crate) fn new_pending_for_test() -> Arc<Self> {
        Self::new_with_request_timeout(std::time::Duration::from_millis(20))
    }

    #[cfg(test)]
    pub(super) fn new_timeout_boundary_for_test(barrier: Arc<tokio::sync::Barrier>) -> Arc<Self> {
        let (termination_tx, _) = watch::channel(None);
        let (finalized_tx, _) = watch::channel(None);
        Arc::new(Self {
            recovery_only: false,
            termination_tx,
            finalized_tx,
            request_timeout: std::time::Duration::from_millis(20),
            timeout_barrier: Some(barrier),
        })
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_termination_for_test(&self) -> String {
        self.termination_requested().await
    }

    #[cfg(test)]
    pub(crate) fn finalize_for_test(&self, finalization: ProcessFinalization) {
        self.finalize(finalization);
    }
}
