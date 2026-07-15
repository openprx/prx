//! Main-chat turn scheduler model.
//!
//! P6A is deliberately a state model first. It does not run providers yet. The
//! chat loop can keep using its active-turn backlog while this module proves the
//! lifecycle rules needed for the later provider-worker migration:
//!
//! - each user input becomes a `TurnTask`;
//! - priority tasks preempt queued normal tasks without reordering peers;
//! - running tasks can enter a cancellable state;
//! - terminal tasks preserve enough history metadata to merge or roll back
//!   without leaking cancelled turns into later prompts.

use std::collections::VecDeque;

use crate::chat::action::MainQueueStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnTaskId(u64);

impl TurnTaskId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TurnPriority {
    Normal,
    Priority,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnTaskState {
    Queued,
    Dispatched,
    Running,
    Cancelling,
    Cancelled,
    Completed,
    Failed,
}

impl TurnTaskState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTask {
    pub id: TurnTaskId,
    pub sequence: u64,
    pub priority: TurnPriority,
    pub state: TurnTaskState,
    pub input: String,
    pub history_base_len: usize,
    pub history_commit_len: Option<usize>,
    pub result_summary: Option<String>,
    pub usage: TurnTaskUsageLedger,
}

impl TurnTask {
    fn queued(id: TurnTaskId, sequence: u64, priority: TurnPriority, input: String, history_base_len: usize) -> Self {
        Self {
            id,
            sequence,
            priority,
            state: TurnTaskState::Queued,
            input,
            history_base_len,
            history_commit_len: None,
            result_summary: None,
            usage: TurnTaskUsageLedger::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnTaskUsageLedger {
    pub request_count: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub reported_tokens: u64,
    pub estimated_tokens: u64,
    pub known_cost_microusd: u64,
    pub unknown_cost_requests: u64,
}

impl TurnTaskUsageLedger {
    pub fn record(&mut self, record: &crate::chat::session::MainSessionTokenUsageRecord) {
        self.request_count = self.request_count.saturating_add(1);
        self.prompt_tokens = self.prompt_tokens.saturating_add(record.prompt_tokens);
        self.completion_tokens = self.completion_tokens.saturating_add(record.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(record.total_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(record.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(record.cache_read_input_tokens);
        match record.source {
            crate::llm::route_decision::TokenUsageSource::Reported => {
                self.reported_tokens = self.reported_tokens.saturating_add(record.total_tokens);
            }
            crate::llm::route_decision::TokenUsageSource::Estimated => {
                self.estimated_tokens = self.estimated_tokens.saturating_add(record.total_tokens);
            }
        }
        if let Some(cost) = record.cost_usd.filter(|cost| cost.is_finite() && *cost >= 0.0) {
            self.known_cost_microusd = self
                .known_cost_microusd
                .saturating_add((cost * 1_000_000.0).round() as u64);
        } else {
            self.unknown_cost_requests = self.unknown_cost_requests.saturating_add(1);
        }
    }

    #[must_use]
    pub const fn has_usage(self) -> bool {
        self.total_tokens > 0 || self.prompt_tokens > 0 || self.completion_tokens > 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnSchedulerStatus {
    pub queued: usize,
    pub priority_queued: usize,
    pub running: usize,
    pub cancelling: usize,
    pub terminal: usize,
}

impl TurnSchedulerStatus {
    #[must_use]
    pub const fn main_queue_status(self) -> MainQueueStatus {
        MainQueueStatus {
            queued: self.queued,
            priority: self.priority_queued,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnSchedulerError {
    UnknownTask(TurnTaskId),
    InvalidState {
        id: TurnTaskId,
        expected: &'static str,
        actual: TurnTaskState,
    },
}

#[derive(Debug, Default)]
pub struct TurnScheduler {
    next_id: u64,
    next_sequence: u64,
    tasks: VecDeque<TurnTask>,
}

impl TurnScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(&mut self, input: impl Into<String>, priority: TurnPriority, history_base_len: usize) -> TurnTaskId {
        self.next_id = self.next_id.saturating_add(1);
        self.next_sequence = self.next_sequence.saturating_add(1);
        let id = TurnTaskId(self.next_id);
        let task = TurnTask::queued(id, self.next_sequence, priority, input.into(), history_base_len);
        self.tasks.push_back(task);
        id
    }

    pub fn start_next(&mut self) -> Option<TurnTaskId> {
        let idx = self.next_queued_index()?;
        let task = self.tasks.get_mut(idx)?;
        task.state = TurnTaskState::Running;
        Some(task.id)
    }

    pub fn start_task(&mut self, id: TurnTaskId) -> Result<(), TurnSchedulerError> {
        let task = self.task_mut(id)?;
        match task.state {
            TurnTaskState::Queued | TurnTaskState::Dispatched => {
                task.state = TurnTaskState::Running;
                Ok(())
            }
            state => Err(TurnSchedulerError::InvalidState {
                id,
                expected: "queued or dispatched",
                actual: state,
            }),
        }
    }

    pub fn mark_dispatched_to_chat_loop(&mut self, id: TurnTaskId) -> Result<(), TurnSchedulerError> {
        let task = self.task_mut(id)?;
        match task.state {
            TurnTaskState::Queued => {
                task.state = TurnTaskState::Dispatched;
                task.result_summary = Some("dequeued to chat loop".to_string());
                Ok(())
            }
            state => Err(TurnSchedulerError::InvalidState {
                id,
                expected: "queued",
                actual: state,
            }),
        }
    }

    pub fn mark_legacy_dispatched(&mut self, id: TurnTaskId) -> Result<(), TurnSchedulerError> {
        let task = self.task_mut(id)?;
        match task.state {
            TurnTaskState::Queued => {
                task.state = TurnTaskState::Completed;
                task.history_commit_len = Some(task.history_base_len);
                task.result_summary = Some("dispatched to legacy chat loop".to_string());
                Ok(())
            }
            state => Err(TurnSchedulerError::InvalidState {
                id,
                expected: "queued",
                actual: state,
            }),
        }
    }

    pub fn request_cancel(&mut self, id: TurnTaskId) -> Result<(), TurnSchedulerError> {
        let task = self.task_mut(id)?;
        match task.state {
            TurnTaskState::Queued => {
                task.state = TurnTaskState::Cancelled;
                task.history_commit_len = Some(task.history_base_len);
                task.result_summary = Some("cancelled before start".to_string());
                Ok(())
            }
            TurnTaskState::Dispatched => {
                task.state = TurnTaskState::Cancelled;
                task.history_commit_len = Some(task.history_base_len);
                task.result_summary = Some("cancelled after dequeue before provider start".to_string());
                Ok(())
            }
            TurnTaskState::Running => {
                task.state = TurnTaskState::Cancelling;
                Ok(())
            }
            TurnTaskState::Cancelling => Ok(()),
            state if state.is_terminal() => Err(TurnSchedulerError::InvalidState {
                id,
                expected: "queued, running, or cancelling",
                actual: state,
            }),
            state => Err(TurnSchedulerError::InvalidState {
                id,
                expected: "queued, running, or cancelling",
                actual: state,
            }),
        }
    }

    pub fn mark_cancelled(&mut self, id: TurnTaskId, summary: impl Into<String>) -> Result<(), TurnSchedulerError> {
        let task = self.task_mut(id)?;
        match task.state {
            TurnTaskState::Dispatched | TurnTaskState::Running | TurnTaskState::Cancelling => {
                task.state = TurnTaskState::Cancelled;
                task.history_commit_len = Some(task.history_base_len);
                task.result_summary = Some(summary.into());
                Ok(())
            }
            state => Err(TurnSchedulerError::InvalidState {
                id,
                expected: "running or cancelling",
                actual: state,
            }),
        }
    }

    pub fn mark_completed(
        &mut self,
        id: TurnTaskId,
        history_commit_len: usize,
        summary: impl Into<String>,
    ) -> Result<(), TurnSchedulerError> {
        let task = self.task_mut(id)?;
        match task.state {
            TurnTaskState::Dispatched | TurnTaskState::Running | TurnTaskState::Cancelling => {
                task.state = TurnTaskState::Completed;
                task.history_commit_len = Some(history_commit_len);
                task.result_summary = Some(summary.into());
                Ok(())
            }
            state => Err(TurnSchedulerError::InvalidState {
                id,
                expected: "dispatched, running, or cancelling",
                actual: state,
            }),
        }
    }

    pub fn mark_failed(
        &mut self,
        id: TurnTaskId,
        history_commit_len: usize,
        summary: impl Into<String>,
    ) -> Result<(), TurnSchedulerError> {
        let task = self.task_mut(id)?;
        match task.state {
            TurnTaskState::Dispatched | TurnTaskState::Running | TurnTaskState::Cancelling => {
                task.state = TurnTaskState::Failed;
                task.history_commit_len = Some(history_commit_len);
                task.result_summary = Some(summary.into());
                Ok(())
            }
            state => Err(TurnSchedulerError::InvalidState {
                id,
                expected: "dispatched, running, or cancelling",
                actual: state,
            }),
        }
    }

    pub fn record_usage(
        &mut self,
        id: TurnTaskId,
        record: &crate::chat::session::MainSessionTokenUsageRecord,
    ) -> Result<(), TurnSchedulerError> {
        let task = self.task_mut(id)?;
        task.usage.record(record);
        Ok(())
    }

    #[must_use]
    pub fn status(&self) -> TurnSchedulerStatus {
        let mut status = TurnSchedulerStatus::default();
        for task in &self.tasks {
            match task.state {
                TurnTaskState::Queued => {
                    status.queued += 1;
                    if task.priority == TurnPriority::Priority {
                        status.priority_queued += 1;
                    }
                }
                TurnTaskState::Dispatched => {}
                TurnTaskState::Running => status.running += 1,
                TurnTaskState::Cancelling => status.cancelling += 1,
                TurnTaskState::Cancelled | TurnTaskState::Completed | TurnTaskState::Failed => status.terminal += 1,
            }
        }
        status
    }

    #[must_use]
    pub fn task(&self, id: TurnTaskId) -> Option<&TurnTask> {
        self.tasks.iter().find(|task| task.id == id)
    }

    #[must_use]
    pub fn queued_preview(&self, max: usize) -> Vec<&TurnTask> {
        self.tasks
            .iter()
            .filter(|task| task.state == TurnTaskState::Queued)
            .take(max)
            .collect()
    }

    fn task_mut(&mut self, id: TurnTaskId) -> Result<&mut TurnTask, TurnSchedulerError> {
        self.tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or(TurnSchedulerError::UnknownTask(id))
    }

    fn next_queued_index(&self) -> Option<usize> {
        let mut best_idx = None;
        let mut best_priority = TurnPriority::Normal;
        let mut best_sequence = u64::MAX;
        for (idx, task) in self.tasks.iter().enumerate() {
            if task.state != TurnTaskState::Queued {
                continue;
            }
            if best_idx.is_none()
                || task.priority > best_priority
                || (task.priority == best_priority && task.sequence < best_sequence)
            {
                best_idx = Some(idx);
                best_priority = task.priority;
                best_sequence = task.sequence;
            }
        }
        best_idx
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_runs_normal_tasks_fifo() {
        let mut scheduler = TurnScheduler::new();
        let first = scheduler.enqueue("first", TurnPriority::Normal, 3);
        let second = scheduler.enqueue("second", TurnPriority::Normal, 3);

        assert_eq!(scheduler.start_next(), Some(first));
        scheduler.mark_completed(first, 5, "done").unwrap();
        assert_eq!(scheduler.start_next(), Some(second));
    }

    #[test]
    fn scheduler_prefers_priority_without_reordering_priority_peers() {
        let mut scheduler = TurnScheduler::new();
        let normal = scheduler.enqueue("normal", TurnPriority::Normal, 0);
        let urgent_one = scheduler.enqueue("urgent one", TurnPriority::Priority, 0);
        let urgent_two = scheduler.enqueue("urgent two", TurnPriority::Priority, 0);

        assert_eq!(scheduler.start_next(), Some(urgent_one));
        scheduler.mark_completed(urgent_one, 2, "done").unwrap();
        assert_eq!(scheduler.start_next(), Some(urgent_two));
        scheduler.mark_completed(urgent_two, 4, "done").unwrap();
        assert_eq!(scheduler.start_next(), Some(normal));
    }

    #[test]
    fn start_task_runs_exact_task_without_priority_selection() {
        let mut scheduler = TurnScheduler::new();
        let normal = scheduler.enqueue("normal provider turn", TurnPriority::Normal, 0);
        let urgent = scheduler.enqueue("urgent queued turn", TurnPriority::Priority, 0);

        scheduler.start_task(normal).unwrap();

        assert_eq!(scheduler.task(normal).unwrap().state, TurnTaskState::Running);
        assert_eq!(scheduler.task(urgent).unwrap().state, TurnTaskState::Queued);
        assert_eq!(scheduler.status().running, 1);
        assert_eq!(scheduler.status().priority_queued, 1);
    }

    #[test]
    fn dispatched_task_can_become_running_provider_task() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("queued provider turn", TurnPriority::Normal, 0);

        scheduler.mark_dispatched_to_chat_loop(id).unwrap();
        assert_eq!(scheduler.task(id).unwrap().state, TurnTaskState::Dispatched);
        assert_eq!(scheduler.status().main_queue_status(), MainQueueStatus::default());

        scheduler.start_task(id).unwrap();
        assert_eq!(scheduler.task(id).unwrap().state, TurnTaskState::Running);
    }

    #[test]
    fn cancelling_running_task_preserves_rollback_boundary() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("long turn", TurnPriority::Normal, 7);

        assert_eq!(scheduler.start_next(), Some(id));
        scheduler.request_cancel(id).unwrap();
        assert_eq!(scheduler.task(id).unwrap().state, TurnTaskState::Cancelling);

        scheduler.mark_cancelled(id, "interrupted").unwrap();
        let task = scheduler.task(id).unwrap();
        assert_eq!(task.state, TurnTaskState::Cancelled);
        assert_eq!(task.history_commit_len, Some(7));
        assert_eq!(task.result_summary.as_deref(), Some("interrupted"));
    }

    #[test]
    fn queued_cancel_does_not_start_or_commit_new_history() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("queued turn", TurnPriority::Normal, 11);

        scheduler.request_cancel(id).unwrap();
        let task = scheduler.task(id).unwrap();
        assert_eq!(task.state, TurnTaskState::Cancelled);
        assert_eq!(task.history_commit_len, Some(11));
        assert_eq!(scheduler.start_next(), None);
    }

    #[test]
    fn legacy_dispatch_removes_task_from_visible_queue_without_running_worker() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("legacy", TurnPriority::Normal, 4);

        scheduler.mark_legacy_dispatched(id).unwrap();

        let task = scheduler.task(id).unwrap();
        assert_eq!(task.state, TurnTaskState::Completed);
        assert_eq!(task.history_commit_len, Some(4));
        assert_eq!(task.result_summary.as_deref(), Some("dispatched to legacy chat loop"));
        assert_eq!(scheduler.status().main_queue_status(), MainQueueStatus::default());
    }

    #[test]
    fn status_projects_queue_and_running_counts() {
        let mut scheduler = TurnScheduler::new();
        scheduler.enqueue("normal", TurnPriority::Normal, 0);
        scheduler.enqueue("urgent", TurnPriority::Priority, 0);
        scheduler.enqueue("control", TurnPriority::Control, 0);
        let running = scheduler.start_next().unwrap();

        let status = scheduler.status();
        assert_eq!(scheduler.task(running).unwrap().priority, TurnPriority::Control);
        assert_eq!(status.queued, 2);
        assert_eq!(status.priority_queued, 1);
        assert_eq!(status.running, 1);
        assert_eq!(status.main_queue_status(), MainQueueStatus { queued: 2, priority: 1 });
    }

    #[test]
    fn terminal_tasks_reject_duplicate_completion() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("turn", TurnPriority::Normal, 0);
        scheduler.start_next();
        scheduler.mark_completed(id, 2, "done").unwrap();

        let err = scheduler.mark_completed(id, 3, "again").unwrap_err();
        assert_eq!(
            err,
            TurnSchedulerError::InvalidState {
                id,
                expected: "dispatched, running, or cancelling",
                actual: TurnTaskState::Completed,
            }
        );
    }

    #[test]
    fn queued_preview_excludes_running_and_terminal_tasks() {
        let mut scheduler = TurnScheduler::new();
        let first = scheduler.enqueue("first", TurnPriority::Normal, 0);
        scheduler.enqueue("second", TurnPriority::Normal, 0);
        scheduler.enqueue("third", TurnPriority::Normal, 0);
        assert_eq!(scheduler.start_next(), Some(first));

        let preview = scheduler.queued_preview(1);
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0].input, "second");
    }

    #[test]
    fn scheduler_records_per_task_usage_ledger() {
        let mut scheduler = TurnScheduler::new();
        let id = scheduler.enqueue("metered", TurnPriority::Normal, 0);
        scheduler.start_task(id).unwrap();
        let record = crate::chat::session::MainSessionTokenUsageRecord {
            settlement_id: None,
            provider: "kimi-code".to_string(),
            model: "kimi-k2.7-code".to_string(),
            prompt_tokens: 100,
            completion_tokens: 40,
            total_tokens: 140,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 20,
            source: crate::llm::route_decision::TokenUsageSource::Reported,
            cost_usd: Some(0.001_234),
        };

        scheduler.record_usage(id, &record).unwrap();

        let usage = scheduler.task(id).unwrap().usage;
        assert!(usage.has_usage());
        assert_eq!(usage.request_count, 1);
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 40);
        assert_eq!(usage.total_tokens, 140);
        assert_eq!(usage.cache_creation_input_tokens, 10);
        assert_eq!(usage.cache_read_input_tokens, 20);
        assert_eq!(usage.reported_tokens, 140);
        assert_eq!(usage.estimated_tokens, 0);
        assert_eq!(usage.known_cost_microusd, 1_234);
        assert_eq!(usage.unknown_cost_requests, 0);
    }
}
