//! Provider-turn worker lifecycle model for main chat.
//!
//! The current TUI path still awaits one provider turn in the foreground, but
//! the worker boundary needs a per-turn lifecycle before the await can safely be
//! removed from `chat::run`. This registry is intentionally keyed by
//! `TurnTaskId`: terminal provider outcomes become "awaiting commit" until the
//! history commit coordinator releases the matching sequence.

use std::collections::HashMap;

use crate::chat::history_commit::{HistoryCommitDecision, HistoryCommitStatus};
use crate::chat::turn_scheduler::{TurnTask, TurnTaskId, TurnTaskState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTurnWorkerKind {
    ForegroundAwaited,
    Detached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTurnWorkerOutcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTurnWorkerState {
    Running,
    Cancelling,
    AwaitingCommit(ProviderTurnWorkerOutcome),
    Committed,
    Cancelled,
    Failed,
}

impl ProviderTurnWorkerState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Cancelled | Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderTurnFinalizedPayload {
    pub history_commit_len: usize,
    pub final_text_chars: usize,
    pub recorded_response_chars: usize,
    pub total_tokens: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTurnWorker {
    pub task_id: TurnTaskId,
    pub sequence: u64,
    pub kind: ProviderTurnWorkerKind,
    pub state: ProviderTurnWorkerState,
    pub history_base_len: usize,
    pub started_at_ms: i64,
    pub execution_started: bool,
    pub execution_exited: bool,
    pub execution_lease_id: Option<u64>,
    pub execution_lease_active: bool,
    pub completion_ready: bool,
    pub completion_lease_id: Option<u64>,
    pub finalized_payload: Option<ProviderTurnFinalizedPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTurnWorkerSnapshot {
    pub task_id: TurnTaskId,
    pub sequence: u64,
    pub kind: ProviderTurnWorkerKind,
    pub state: ProviderTurnWorkerState,
    pub history_base_len: usize,
    pub started_at_ms: i64,
    pub execution_started: bool,
    pub execution_exited: bool,
    pub execution_lease_id: Option<u64>,
    pub execution_lease_active: bool,
    pub execution_handle_attached: bool,
    pub completion_ready: bool,
    pub completion_lease_id: Option<u64>,
    pub finalized_payload_ready: bool,
    pub finalized_history_commit_len: Option<usize>,
    pub finalized_total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderTurnWorkerError {
    DuplicateWorker(TurnTaskId),
    UnknownWorker(TurnTaskId),
    InvalidTaskState {
        task_id: TurnTaskId,
        actual: TurnTaskState,
    },
    InvalidWorkerState {
        task_id: TurnTaskId,
        expected: &'static str,
        actual: ProviderTurnWorkerState,
    },
    SequenceMismatch {
        task_id: TurnTaskId,
        expected: u64,
        actual: u64,
    },
    ExecutionLeaseMismatch {
        task_id: TurnTaskId,
        expected: u64,
        actual: u64,
    },
}

#[derive(Debug, Default)]
pub struct ProviderTurnWorkerRegistry {
    workers: HashMap<TurnTaskId, ProviderTurnWorker>,
    execution_handles: HashMap<TurnTaskId, ProviderTurnExecutionHandle>,
}

#[derive(Debug)]
struct ProviderTurnExecutionHandle {
    lease_id: u64,
    abort_handle: tokio::task::AbortHandle,
}

impl ProviderTurnWorkerRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_from_task(
        &mut self,
        task: &TurnTask,
        kind: ProviderTurnWorkerKind,
    ) -> Result<(), ProviderTurnWorkerError> {
        if self.workers.contains_key(&task.id) {
            return Err(ProviderTurnWorkerError::DuplicateWorker(task.id));
        }
        if !matches!(task.state, TurnTaskState::Running | TurnTaskState::Cancelling) {
            return Err(ProviderTurnWorkerError::InvalidTaskState {
                task_id: task.id,
                actual: task.state,
            });
        }
        let state = if task.state == TurnTaskState::Cancelling {
            ProviderTurnWorkerState::Cancelling
        } else {
            ProviderTurnWorkerState::Running
        };
        self.workers.insert(
            task.id,
            ProviderTurnWorker {
                task_id: task.id,
                sequence: task.sequence,
                kind,
                state,
                history_base_len: task.history_base_len,
                started_at_ms: chrono::Utc::now().timestamp_millis(),
                execution_started: false,
                execution_exited: false,
                execution_lease_id: None,
                execution_lease_active: false,
                completion_ready: false,
                completion_lease_id: None,
                finalized_payload: None,
            },
        );
        Ok(())
    }

    pub fn attach_execution_handle(
        &mut self,
        task_id: TurnTaskId,
        lease_id: u64,
        abort_handle: tokio::task::AbortHandle,
    ) -> Result<(), ProviderTurnWorkerError> {
        let worker = self.worker_mut(task_id)?;
        if let Some(existing) = worker.execution_lease_id
            && existing != lease_id
        {
            return Err(ProviderTurnWorkerError::ExecutionLeaseMismatch {
                task_id,
                expected: existing,
                actual: lease_id,
            });
        }
        worker.execution_lease_id = Some(lease_id);
        self.execution_handles
            .insert(task_id, ProviderTurnExecutionHandle { lease_id, abort_handle });
        Ok(())
    }

    pub fn record_execution_started(
        &mut self,
        task_id: TurnTaskId,
        lease_id: u64,
    ) -> Result<(), ProviderTurnWorkerError> {
        let worker = self.worker_mut(task_id)?;
        if let Some(existing) = worker.execution_lease_id
            && existing != lease_id
        {
            return Err(ProviderTurnWorkerError::ExecutionLeaseMismatch {
                task_id,
                expected: existing,
                actual: lease_id,
            });
        }
        worker.execution_lease_id = Some(lease_id);
        worker.execution_lease_active = true;
        worker.execution_started = true;
        Ok(())
    }

    pub fn record_execution_exited(
        &mut self,
        task_id: TurnTaskId,
        lease_id: u64,
    ) -> Result<(), ProviderTurnWorkerError> {
        let worker = self.worker_mut(task_id)?;
        if let Some(existing) = worker.execution_lease_id
            && existing != lease_id
        {
            return Err(ProviderTurnWorkerError::ExecutionLeaseMismatch {
                task_id,
                expected: existing,
                actual: lease_id,
            });
        }
        worker.execution_lease_id = Some(lease_id);
        worker.execution_lease_active = false;
        worker.execution_exited = true;
        if let Some(handle) = self.execution_handles.get(&task_id)
            && handle.lease_id != lease_id
        {
            return Err(ProviderTurnWorkerError::ExecutionLeaseMismatch {
                task_id,
                expected: handle.lease_id,
                actual: lease_id,
            });
        }
        self.execution_handles.remove(&task_id);
        Ok(())
    }

    pub fn abort_execution(&self, task_id: TurnTaskId) -> Result<(), ProviderTurnWorkerError> {
        let _ = self
            .worker(task_id)
            .ok_or(ProviderTurnWorkerError::UnknownWorker(task_id))?;
        let handle =
            self.execution_handles
                .get(&task_id)
                .ok_or_else(|| ProviderTurnWorkerError::InvalidWorkerState {
                    task_id,
                    expected: "execution handle attached",
                    actual: self
                        .worker(task_id)
                        .map(|worker| worker.state)
                        .unwrap_or(ProviderTurnWorkerState::Failed),
                })?;
        handle.abort_handle.abort();
        Ok(())
    }

    #[must_use]
    pub fn execution_handle_attached(&self, task_id: TurnTaskId) -> bool {
        self.execution_handles.contains_key(&task_id)
    }

    pub fn record_completion_ready(&mut self, task_id: TurnTaskId) -> Result<(), ProviderTurnWorkerError> {
        let handle_attached = self.execution_handles.contains_key(&task_id);
        let worker = self.worker_mut(task_id)?;
        if worker.execution_lease_id.is_none() && !handle_attached {
            return Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "execution lease attached or started before completion",
                actual: worker.state,
            });
        }
        match worker.state {
            ProviderTurnWorkerState::Running
            | ProviderTurnWorkerState::Cancelling
            | ProviderTurnWorkerState::AwaitingCommit(_) => {
                worker.completion_ready = true;
                worker.completion_lease_id = worker.execution_lease_id;
                Ok(())
            }
            state => Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "non-terminal provider worker",
                actual: state,
            }),
        }
    }

    pub fn record_finalized_payload(
        &mut self,
        task_id: TurnTaskId,
        payload: ProviderTurnFinalizedPayload,
    ) -> Result<(), ProviderTurnWorkerError> {
        let worker = self.worker_mut(task_id)?;
        if !worker.completion_ready {
            return Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "completion-ready provider worker",
                actual: worker.state,
            });
        }
        match worker.state {
            ProviderTurnWorkerState::Running | ProviderTurnWorkerState::Cancelling => {
                worker.finalized_payload = Some(payload);
                Ok(())
            }
            state => Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "running or cancelling",
                actual: state,
            }),
        }
    }

    pub fn request_cancel(&mut self, task_id: TurnTaskId) -> Result<(), ProviderTurnWorkerError> {
        let worker = self.worker_mut(task_id)?;
        match worker.state {
            ProviderTurnWorkerState::Running => {
                worker.state = ProviderTurnWorkerState::Cancelling;
                Ok(())
            }
            ProviderTurnWorkerState::Cancelling => Ok(()),
            state => Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "running or cancelling",
                actual: state,
            }),
        }
    }

    pub fn record_completed(&mut self, task_id: TurnTaskId) -> Result<(), ProviderTurnWorkerError> {
        self.record_terminal_outcome(task_id, ProviderTurnWorkerOutcome::Completed)
    }

    pub fn record_cancelled(&mut self, task_id: TurnTaskId) -> Result<(), ProviderTurnWorkerError> {
        self.record_terminal_outcome(task_id, ProviderTurnWorkerOutcome::Cancelled)
    }

    pub fn record_failed(&mut self, task_id: TurnTaskId) -> Result<(), ProviderTurnWorkerError> {
        self.record_terminal_outcome(task_id, ProviderTurnWorkerOutcome::Failed)
    }

    pub fn apply_commit_decision(&mut self, decision: &HistoryCommitDecision) -> Result<(), ProviderTurnWorkerError> {
        let (task_id, sequence, target_state) = match decision {
            HistoryCommitDecision::Commit { task_id, sequence, .. } => {
                (*task_id, *sequence, ProviderTurnWorkerState::Committed)
            }
            HistoryCommitDecision::Skip {
                task_id,
                sequence,
                status,
                ..
            } => {
                let state = match status {
                    HistoryCommitStatus::Completed => ProviderTurnWorkerState::Committed,
                    HistoryCommitStatus::Cancelled => ProviderTurnWorkerState::Cancelled,
                    HistoryCommitStatus::Failed => ProviderTurnWorkerState::Failed,
                };
                (*task_id, *sequence, state)
            }
        };
        let worker = self.worker_mut(task_id)?;
        if worker.sequence != sequence {
            return Err(ProviderTurnWorkerError::SequenceMismatch {
                task_id,
                expected: worker.sequence,
                actual: sequence,
            });
        }
        match worker.state {
            ProviderTurnWorkerState::AwaitingCommit(_) => {
                worker.state = target_state;
                Ok(())
            }
            state => Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "awaiting commit",
                actual: state,
            }),
        }
    }

    #[must_use]
    pub fn worker(&self, task_id: TurnTaskId) -> Option<&ProviderTurnWorker> {
        self.workers.get(&task_id)
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<ProviderTurnWorkerSnapshot> {
        let mut rows: Vec<_> = self
            .workers
            .values()
            .map(|worker| ProviderTurnWorkerSnapshot {
                task_id: worker.task_id,
                sequence: worker.sequence,
                kind: worker.kind,
                state: worker.state,
                history_base_len: worker.history_base_len,
                started_at_ms: worker.started_at_ms,
                execution_started: worker.execution_started,
                execution_exited: worker.execution_exited,
                execution_lease_id: worker.execution_lease_id,
                execution_lease_active: worker.execution_lease_active,
                execution_handle_attached: self.execution_handles.contains_key(&worker.task_id),
                completion_ready: worker.completion_ready,
                completion_lease_id: worker.completion_lease_id,
                finalized_payload_ready: worker.finalized_payload.is_some(),
                finalized_history_commit_len: worker.finalized_payload.map(|payload| payload.history_commit_len),
                finalized_total_tokens: worker.finalized_payload.map(|payload| payload.total_tokens),
            })
            .collect();
        rows.sort_by_key(|row| row.sequence);
        rows
    }

    #[must_use]
    pub fn running_count(&self) -> usize {
        self.workers
            .values()
            .filter(|worker| {
                matches!(
                    worker.state,
                    ProviderTurnWorkerState::Running | ProviderTurnWorkerState::Cancelling
                )
            })
            .count()
    }

    #[must_use]
    pub fn awaiting_commit_count(&self) -> usize {
        self.workers
            .values()
            .filter(|worker| matches!(worker.state, ProviderTurnWorkerState::AwaitingCommit(_)))
            .count()
    }

    fn record_terminal_outcome(
        &mut self,
        task_id: TurnTaskId,
        outcome: ProviderTurnWorkerOutcome,
    ) -> Result<(), ProviderTurnWorkerError> {
        let worker = self.worker_mut(task_id)?;
        if !worker.completion_ready {
            return Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "completion-ready provider worker",
                actual: worker.state,
            });
        }
        if outcome == ProviderTurnWorkerOutcome::Completed && worker.finalized_payload.is_none() {
            return Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "finalized payload recorded",
                actual: worker.state,
            });
        }
        match worker.state {
            ProviderTurnWorkerState::Running | ProviderTurnWorkerState::Cancelling => {
                worker.state = ProviderTurnWorkerState::AwaitingCommit(outcome);
                Ok(())
            }
            state => Err(ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "running or cancelling",
                actual: state,
            }),
        }
    }

    fn worker_mut(&mut self, task_id: TurnTaskId) -> Result<&mut ProviderTurnWorker, ProviderTurnWorkerError> {
        self.workers
            .get_mut(&task_id)
            .ok_or(ProviderTurnWorkerError::UnknownWorker(task_id))
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::chat::history_commit::{HistoryCommitCoordinator, HistoryCommitOutcome};
    use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler};

    fn mark_completion_ready(registry: &mut ProviderTurnWorkerRegistry, id: TurnTaskId, lease_id: u64) {
        registry.record_execution_started(id, lease_id).unwrap();
        registry.record_completion_ready(id).unwrap();
    }

    fn sample_payload(history_commit_len: usize) -> ProviderTurnFinalizedPayload {
        ProviderTurnFinalizedPayload {
            history_commit_len,
            final_text_chars: 12,
            recorded_response_chars: 12,
            total_tokens: 34,
            prompt_tokens: 21,
            completion_tokens: 13,
        }
    }

    fn mark_completed_payload_ready(
        registry: &mut ProviderTurnWorkerRegistry,
        id: TurnTaskId,
        history_commit_len: usize,
    ) {
        registry
            .record_finalized_payload(id, sample_payload(history_commit_len))
            .unwrap();
    }

    #[test]
    fn worker_outcome_waits_for_commit_decision() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();
        assert_eq!(registry.running_count(), 1);

        mark_completion_ready(&mut registry, id, 101);
        mark_completed_payload_ready(&mut registry, id, 5);
        registry.record_completed(id).unwrap();
        assert_eq!(
            registry.worker(id).unwrap().state,
            ProviderTurnWorkerState::AwaitingCommit(ProviderTurnWorkerOutcome::Completed)
        );
        assert_eq!(registry.awaiting_commit_count(), 1);

        scheduler.mark_completed(id, 5, "done").unwrap();
        let mut commits = HistoryCommitCoordinator::new();
        commits.register_task(scheduler.task(id).unwrap()).unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(id).unwrap()).unwrap())
            .unwrap();
        let decisions = commits.drain_ready();
        assert_eq!(decisions.len(), 1);

        registry.apply_commit_decision(&decisions[0]).unwrap();
        assert_eq!(registry.worker(id).unwrap().state, ProviderTurnWorkerState::Committed);
    }

    #[test]
    fn later_worker_can_finish_before_earlier_worker_without_committing() {
        let mut scheduler = TurnScheduler::new();
        let first = scheduler.enqueue("first", TurnPriority::Normal, 0);
        let second = scheduler.enqueue("second", TurnPriority::Normal, 0);
        scheduler.start_task(first).unwrap();
        scheduler.start_task(second).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(first).unwrap(), ProviderTurnWorkerKind::Detached)
            .unwrap();
        registry
            .start_from_task(scheduler.task(second).unwrap(), ProviderTurnWorkerKind::Detached)
            .unwrap();
        mark_completion_ready(&mut registry, second, 202);
        mark_completed_payload_ready(&mut registry, second, 4);
        registry.record_completed(second).unwrap();

        let mut commits = HistoryCommitCoordinator::new();
        commits.register_task(scheduler.task(first).unwrap()).unwrap();
        commits.register_task(scheduler.task(second).unwrap()).unwrap();
        scheduler.mark_completed(second, 4, "second done").unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(second).unwrap()).unwrap())
            .unwrap();

        assert!(commits.drain_ready().is_empty());
        assert_eq!(
            registry.worker(second).unwrap().state,
            ProviderTurnWorkerState::AwaitingCommit(ProviderTurnWorkerOutcome::Completed)
        );

        mark_completion_ready(&mut registry, first, 201);
        registry.record_cancelled(first).unwrap();
        scheduler.request_cancel(first).unwrap();
        scheduler.mark_cancelled(first, "cancelled").unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(first).unwrap()).unwrap())
            .unwrap();
        let decisions = commits.drain_ready();
        assert_eq!(decisions.len(), 2);
        registry.apply_commit_decision(&decisions[0]).unwrap();
        registry.apply_commit_decision(&decisions[1]).unwrap();

        assert_eq!(
            registry.worker(first).unwrap().state,
            ProviderTurnWorkerState::Cancelled
        );
        assert_eq!(
            registry.worker(second).unwrap().state,
            ProviderTurnWorkerState::Committed
        );
    }

    #[test]
    fn cancelling_worker_records_failed_terminal_state_after_commit_skip() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 2);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::Detached)
            .unwrap();
        registry.request_cancel(id).unwrap();
        assert_eq!(registry.worker(id).unwrap().state, ProviderTurnWorkerState::Cancelling);
        mark_completion_ready(&mut registry, id, 303);
        registry.record_failed(id).unwrap();

        let mut commits = HistoryCommitCoordinator::new();
        commits.register_task(scheduler.task(id).unwrap()).unwrap();
        scheduler.mark_failed(id, 2, "failed after cancel").unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(id).unwrap()).unwrap())
            .unwrap();
        let decisions = commits.drain_ready();
        registry.apply_commit_decision(&decisions[0]).unwrap();

        assert_eq!(registry.worker(id).unwrap().state, ProviderTurnWorkerState::Failed);
    }

    #[test]
    fn snapshot_is_sequence_sorted_and_preserves_worker_state() {
        let mut scheduler = TurnScheduler::new();
        let first = scheduler.enqueue("first", TurnPriority::Normal, 9);
        let second = scheduler.enqueue("second", TurnPriority::Normal, 11);
        scheduler.start_task(second).unwrap();
        scheduler.start_task(first).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(second).unwrap(), ProviderTurnWorkerKind::Detached)
            .unwrap();
        registry
            .start_from_task(
                scheduler.task(first).unwrap(),
                ProviderTurnWorkerKind::ForegroundAwaited,
            )
            .unwrap();
        registry.request_cancel(first).unwrap();

        let rows = registry.snapshot();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].task_id, first);
        assert_eq!(rows[0].sequence, 1);
        assert_eq!(rows[0].kind, ProviderTurnWorkerKind::ForegroundAwaited);
        assert_eq!(rows[0].state, ProviderTurnWorkerState::Cancelling);
        assert_eq!(rows[0].history_base_len, 9);
        assert_eq!(rows[1].task_id, second);
        assert_eq!(rows[1].sequence, 2);
        assert_eq!(rows[1].kind, ProviderTurnWorkerKind::Detached);
        assert_eq!(rows[1].state, ProviderTurnWorkerState::Running);
        assert_eq!(rows[1].history_base_len, 11);
    }

    #[test]
    fn execution_lifecycle_marks_worker_and_snapshot() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        registry.record_execution_started(id, 42).unwrap();
        assert!(registry.worker(id).unwrap().execution_started);
        assert!(!registry.worker(id).unwrap().execution_exited);
        assert_eq!(registry.worker(id).unwrap().execution_lease_id, Some(42));
        assert!(registry.worker(id).unwrap().execution_lease_active);

        registry.record_execution_exited(id, 42).unwrap();
        let worker = registry.worker(id).unwrap();
        assert!(worker.execution_started);
        assert!(worker.execution_exited);
        assert_eq!(worker.execution_lease_id, Some(42));
        assert!(!worker.execution_lease_active);

        let rows = registry.snapshot();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].execution_started);
        assert!(rows[0].execution_exited);
        assert_eq!(rows[0].execution_lease_id, Some(42));
        assert!(!rows[0].execution_lease_active);
    }

    #[test]
    fn execution_lifecycle_rejects_mismatched_lease() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        registry.record_execution_started(id, 7).unwrap();
        let err = registry.record_execution_exited(id, 8).unwrap_err();
        assert!(matches!(
            err,
            ProviderTurnWorkerError::ExecutionLeaseMismatch {
                task_id,
                expected: 7,
                actual: 8,
            } if task_id == id
        ));
    }

    #[tokio::test]
    async fn execution_handle_is_attached_abortable_and_removed_on_exit() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::Detached)
            .unwrap();

        let task = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        registry.attach_execution_handle(id, 88, task.abort_handle()).unwrap();
        assert!(registry.execution_handle_attached(id));
        assert_eq!(registry.snapshot()[0].execution_lease_id, Some(88));
        assert!(registry.snapshot()[0].execution_handle_attached);

        registry.abort_execution(id).unwrap();
        let join = task.await;
        assert!(join.is_err());
        assert!(join.unwrap_err().is_cancelled());

        registry.record_execution_started(id, 88).unwrap();
        registry.record_execution_exited(id, 88).unwrap();
        assert!(!registry.execution_handle_attached(id));
        assert!(!registry.snapshot()[0].execution_handle_attached);
    }

    #[test]
    fn completion_ready_requires_execution_lease() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        let err = registry.record_completion_ready(id).unwrap_err();
        assert!(matches!(
            err,
            ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "execution lease attached or started before completion",
                actual: ProviderTurnWorkerState::Running,
            } if task_id == id
        ));
        assert!(!registry.worker(id).unwrap().completion_ready);
    }

    #[test]
    fn completion_ready_records_execution_lease_in_snapshot() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        registry.record_execution_started(id, 144).unwrap();
        registry.record_completion_ready(id).unwrap();

        let worker = registry.worker(id).unwrap();
        assert!(worker.completion_ready);
        assert_eq!(worker.completion_lease_id, Some(144));

        let rows = registry.snapshot();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].completion_ready);
        assert_eq!(rows[0].completion_lease_id, Some(144));
    }

    #[test]
    fn finalized_payload_records_commit_len_and_usage_projection() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        mark_completion_ready(&mut registry, id, 166);
        registry.record_finalized_payload(id, sample_payload(9)).unwrap();

        let worker = registry.worker(id).unwrap();
        assert_eq!(worker.finalized_payload.unwrap().history_commit_len, 9);
        assert_eq!(worker.finalized_payload.unwrap().total_tokens, 34);

        let rows = registry.snapshot();
        assert!(rows[0].finalized_payload_ready);
        assert_eq!(rows[0].finalized_history_commit_len, Some(9));
        assert_eq!(rows[0].finalized_total_tokens, Some(34));
    }

    #[test]
    fn completed_outcome_requires_finalized_payload() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        mark_completion_ready(&mut registry, id, 177);
        let err = registry.record_completed(id).unwrap_err();
        assert!(matches!(
            err,
            ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "finalized payload recorded",
                actual: ProviderTurnWorkerState::Running,
            } if task_id == id
        ));
    }

    #[test]
    fn finalized_payload_requires_completion_ready() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        registry.record_execution_started(id, 188).unwrap();
        let err = registry.record_finalized_payload(id, sample_payload(8)).unwrap_err();
        assert!(matches!(
            err,
            ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "completion-ready provider worker",
                actual: ProviderTurnWorkerState::Running,
            } if task_id == id
        ));
    }

    #[test]
    fn terminal_outcome_requires_completion_ready_gate() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        registry.record_execution_started(id, 155).unwrap();
        let err = registry.record_completed(id).unwrap_err();
        assert!(matches!(
            err,
            ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "completion-ready provider worker",
                actual: ProviderTurnWorkerState::Running,
            } if task_id == id
        ));
        assert_eq!(registry.worker(id).unwrap().state, ProviderTurnWorkerState::Running);
    }

    #[test]
    fn completion_ready_rejects_terminal_worker() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 3);
        scheduler.start_task(id).unwrap();

        let mut registry = ProviderTurnWorkerRegistry::new();
        registry
            .start_from_task(scheduler.task(id).unwrap(), ProviderTurnWorkerKind::ForegroundAwaited)
            .unwrap();

        registry.record_execution_started(id, 233).unwrap();
        registry.record_completion_ready(id).unwrap();
        mark_completed_payload_ready(&mut registry, id, 5);
        registry.record_completed(id).unwrap();
        scheduler.mark_completed(id, 5, "done").unwrap();

        let mut commits = HistoryCommitCoordinator::new();
        commits.register_task(scheduler.task(id).unwrap()).unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(id).unwrap()).unwrap())
            .unwrap();
        registry.apply_commit_decision(&commits.drain_ready()[0]).unwrap();

        let err = registry.record_completion_ready(id).unwrap_err();
        assert!(matches!(
            err,
            ProviderTurnWorkerError::InvalidWorkerState {
                task_id,
                expected: "non-terminal provider worker",
                actual: ProviderTurnWorkerState::Committed,
            } if task_id == id
        ));
    }
}
