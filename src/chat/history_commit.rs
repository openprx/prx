//! Ordered history commit coordination for main-chat turns.
//!
//! This is the merge-lock model used by the chat scheduler as provider turns
//! move toward independent workers. A later turn may finish first, but its
//! history effects must not become commit-ready until every earlier registered
//! turn has either committed, failed, or been cancelled.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::chat::turn_scheduler::{TurnTask, TurnTaskId, TurnTaskState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryCommitStatus {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryCommitOutcome {
    pub task_id: TurnTaskId,
    pub sequence: u64,
    pub history_base_len: usize,
    pub history_commit_len: usize,
    pub status: HistoryCommitStatus,
    pub summary: String,
}

impl HistoryCommitOutcome {
    #[must_use]
    pub fn from_terminal_task(task: &TurnTask) -> Option<Self> {
        let status = match task.state {
            TurnTaskState::Completed => HistoryCommitStatus::Completed,
            TurnTaskState::Cancelled => HistoryCommitStatus::Cancelled,
            TurnTaskState::Failed => HistoryCommitStatus::Failed,
            _ => return None,
        };
        Some(Self {
            task_id: task.id,
            sequence: task.sequence,
            history_base_len: task.history_base_len,
            history_commit_len: task.history_commit_len.unwrap_or(task.history_base_len),
            status,
            summary: task.result_summary.clone().unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCommitDecision {
    Commit {
        task_id: TurnTaskId,
        sequence: u64,
        history_commit_len: usize,
        summary: String,
    },
    Skip {
        task_id: TurnTaskId,
        sequence: u64,
        rollback_to: usize,
        status: HistoryCommitStatus,
        summary: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCommitError {
    DuplicateTask(TurnTaskId),
    DuplicateSequence(u64),
    DuplicateOutcome(TurnTaskId),
    UnknownTask(TurnTaskId),
    SequenceMismatch {
        task_id: TurnTaskId,
        expected: u64,
        actual: u64,
    },
    BaseLenMismatch {
        task_id: TurnTaskId,
        expected: usize,
        actual: usize,
    },
}

#[derive(Debug, Default)]
pub struct HistoryCommitCoordinator {
    pending_order: BTreeSet<u64>,
    tasks_by_sequence: BTreeMap<u64, RegisteredHistoryTask>,
    sequence_by_task: HashMap<TurnTaskId, u64>,
    outcomes_by_sequence: BTreeMap<u64, HistoryCommitOutcome>,
}

#[derive(Debug, Clone, Copy)]
struct RegisteredHistoryTask {
    id: TurnTaskId,
    history_base_len: usize,
}

impl HistoryCommitCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_task(&mut self, task: &TurnTask) -> Result<(), HistoryCommitError> {
        if self.sequence_by_task.contains_key(&task.id) {
            return Err(HistoryCommitError::DuplicateTask(task.id));
        }
        if self.tasks_by_sequence.contains_key(&task.sequence) {
            return Err(HistoryCommitError::DuplicateSequence(task.sequence));
        }
        self.pending_order.insert(task.sequence);
        self.tasks_by_sequence.insert(
            task.sequence,
            RegisteredHistoryTask {
                id: task.id,
                history_base_len: task.history_base_len,
            },
        );
        self.sequence_by_task.insert(task.id, task.sequence);
        Ok(())
    }

    pub fn record_outcome(&mut self, outcome: HistoryCommitOutcome) -> Result<(), HistoryCommitError> {
        let Some(expected_sequence) = self.sequence_by_task.get(&outcome.task_id).copied() else {
            return Err(HistoryCommitError::UnknownTask(outcome.task_id));
        };
        if expected_sequence != outcome.sequence {
            return Err(HistoryCommitError::SequenceMismatch {
                task_id: outcome.task_id,
                expected: expected_sequence,
                actual: outcome.sequence,
            });
        }
        let Some(task) = self.tasks_by_sequence.get(&outcome.sequence).copied() else {
            return Err(HistoryCommitError::UnknownTask(outcome.task_id));
        };
        if task.history_base_len != outcome.history_base_len {
            return Err(HistoryCommitError::BaseLenMismatch {
                task_id: outcome.task_id,
                expected: task.history_base_len,
                actual: outcome.history_base_len,
            });
        }
        if self.outcomes_by_sequence.contains_key(&outcome.sequence) {
            return Err(HistoryCommitError::DuplicateOutcome(outcome.task_id));
        }
        self.outcomes_by_sequence.insert(outcome.sequence, outcome);
        Ok(())
    }

    #[must_use]
    pub fn drain_ready(&mut self) -> Vec<HistoryCommitDecision> {
        let mut ready = Vec::new();
        while let Some(sequence) = self.pending_order.iter().next().copied() {
            let Some(outcome) = self.outcomes_by_sequence.remove(&sequence) else {
                break;
            };
            self.pending_order.remove(&sequence);
            if let Some(task) = self.tasks_by_sequence.remove(&sequence) {
                self.sequence_by_task.remove(&task.id);
            }
            ready.push(match outcome.status {
                HistoryCommitStatus::Completed => HistoryCommitDecision::Commit {
                    task_id: outcome.task_id,
                    sequence: outcome.sequence,
                    history_commit_len: outcome.history_commit_len,
                    summary: outcome.summary,
                },
                HistoryCommitStatus::Cancelled | HistoryCommitStatus::Failed => HistoryCommitDecision::Skip {
                    task_id: outcome.task_id,
                    sequence: outcome.sequence,
                    rollback_to: outcome.history_base_len,
                    status: outcome.status,
                    summary: outcome.summary,
                },
            });
        }
        ready
    }

    #[must_use]
    pub fn pending_tasks(&self) -> usize {
        self.pending_order.len()
    }

    #[must_use]
    pub fn pending_outcomes(&self) -> usize {
        self.outcomes_by_sequence.len()
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler};

    #[test]
    fn later_completion_waits_for_earlier_turn() {
        let mut scheduler = TurnScheduler::new();
        let first = scheduler.enqueue("first", TurnPriority::Normal, 3);
        let second = scheduler.enqueue("second", TurnPriority::Normal, 5);
        scheduler.start_task(first).unwrap();
        scheduler.start_task(second).unwrap();

        let mut commits = HistoryCommitCoordinator::new();
        commits.register_task(scheduler.task(first).unwrap()).unwrap();
        commits.register_task(scheduler.task(second).unwrap()).unwrap();

        scheduler.mark_completed(second, 9, "second done").unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(second).unwrap()).unwrap())
            .unwrap();

        assert!(commits.drain_ready().is_empty());
        assert_eq!(commits.pending_outcomes(), 1);

        scheduler.mark_completed(first, 7, "first done").unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(first).unwrap()).unwrap())
            .unwrap();

        let decisions = commits.drain_ready();
        assert_eq!(decisions.len(), 2);
        assert!(matches!(
            decisions[0],
            HistoryCommitDecision::Commit {
                task_id,
                sequence: 1,
                history_commit_len: 7,
                ..
            } if task_id == first
        ));
        assert!(matches!(
            decisions[1],
            HistoryCommitDecision::Commit {
                task_id,
                sequence: 2,
                history_commit_len: 9,
                ..
            } if task_id == second
        ));
        assert_eq!(commits.pending_tasks(), 0);
    }

    #[test]
    fn cancellation_unblocks_later_commit_without_committing_history() {
        let mut scheduler = TurnScheduler::new();
        let first = scheduler.enqueue("cancel me", TurnPriority::Normal, 4);
        let second = scheduler.enqueue("commit me", TurnPriority::Normal, 4);
        scheduler.start_task(first).unwrap();
        scheduler.start_task(second).unwrap();

        let mut commits = HistoryCommitCoordinator::new();
        commits.register_task(scheduler.task(first).unwrap()).unwrap();
        commits.register_task(scheduler.task(second).unwrap()).unwrap();

        scheduler.mark_completed(second, 8, "second done").unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(second).unwrap()).unwrap())
            .unwrap();
        assert!(commits.drain_ready().is_empty());

        scheduler.request_cancel(first).unwrap();
        scheduler
            .mark_cancelled(first, "priority turn cancelled earlier worker")
            .unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(first).unwrap()).unwrap())
            .unwrap();

        let decisions = commits.drain_ready();
        assert_eq!(decisions.len(), 2);
        assert!(matches!(
            decisions[0],
            HistoryCommitDecision::Skip {
                task_id,
                sequence: 1,
                rollback_to: 4,
                status: HistoryCommitStatus::Cancelled,
                ..
            } if task_id == first
        ));
        assert!(matches!(
            decisions[1],
            HistoryCommitDecision::Commit {
                task_id,
                sequence: 2,
                history_commit_len: 8,
                ..
            } if task_id == second
        ));
    }

    #[test]
    fn failed_turn_skips_and_unblocks_next_turn() {
        let mut scheduler = TurnScheduler::new();
        let first = scheduler.enqueue("fail", TurnPriority::Normal, 2);
        let second = scheduler.enqueue("succeed", TurnPriority::Normal, 2);
        scheduler.start_task(first).unwrap();
        scheduler.start_task(second).unwrap();

        let mut commits = HistoryCommitCoordinator::new();
        commits.register_task(scheduler.task(first).unwrap()).unwrap();
        commits.register_task(scheduler.task(second).unwrap()).unwrap();

        scheduler.mark_failed(first, 6, "provider failed").unwrap();
        scheduler.mark_completed(second, 10, "provider completed").unwrap();
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(second).unwrap()).unwrap())
            .unwrap();
        assert!(commits.drain_ready().is_empty());
        commits
            .record_outcome(HistoryCommitOutcome::from_terminal_task(scheduler.task(first).unwrap()).unwrap())
            .unwrap();

        let decisions = commits.drain_ready();
        assert_eq!(decisions.len(), 2);
        assert!(matches!(
            decisions[0],
            HistoryCommitDecision::Skip {
                task_id,
                sequence: 1,
                rollback_to: 2,
                status: HistoryCommitStatus::Failed,
                ..
            } if task_id == first
        ));
        assert!(matches!(
            decisions[1],
            HistoryCommitDecision::Commit {
                task_id,
                sequence: 2,
                history_commit_len: 10,
                ..
            } if task_id == second
        ));
    }

    #[test]
    fn base_len_mismatch_rejects_stale_outcome() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 12);
        scheduler.start_task(id).unwrap();

        let mut commits = HistoryCommitCoordinator::new();
        commits.register_task(scheduler.task(id).unwrap()).unwrap();

        scheduler.mark_completed(id, 14, "done").unwrap();
        let mut outcome = HistoryCommitOutcome::from_terminal_task(scheduler.task(id).unwrap()).unwrap();
        outcome.history_base_len = 11;

        assert_eq!(
            commits.record_outcome(outcome).unwrap_err(),
            HistoryCommitError::BaseLenMismatch {
                task_id: id,
                expected: 12,
                actual: 11,
            }
        );
    }
}
