//! Engine-internal observation of injected attempt accounting. The contract
//! lives in `cost::decision_attempt` (R17).
use super::{error::DecisionError, protocol::DecisionUsage};
// Forwarding paths until Part 2: moved to `cost::decision_attempt` (R17).
pub(crate) use crate::cost::decision_attempt::sealed;
pub use crate::cost::decision_attempt::{AttemptBinding, DecisionAttemptAccounting, DecisionAttemptReceipt};
use async_trait::async_trait;
use std::time::Instant;

// Created only after a configured cache miss and outbound guard. Observation
// describes ownership stages, never proof that a socket was physically sent.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

struct Observation {
    phase: AtomicU8,
    owner_controls_deadline: AtomicBool,
}

pub(super) struct ObservedAccounting<'a> {
    owner: &'a dyn DecisionAttemptAccounting,
    observation: Arc<Observation>,
}
impl<'a> ObservedAccounting<'a> {
    pub(super) fn new(owner: &'a dyn DecisionAttemptAccounting) -> Self {
        Self {
            owner,
            observation: Arc::new(Observation {
                phase: AtomicU8::new(0),
                owner_controls_deadline: AtomicBool::new(false),
            }),
        }
    }
    pub(super) fn timeout_error(&self) -> DecisionError {
        if self.observation.phase.load(Ordering::Acquire) == 1
            && !self.observation.owner_controls_deadline.load(Ordering::Acquire)
        {
            DecisionError::Timeout
        } else {
            DecisionError::BudgetExceeded
        }
    }
}
impl sealed::AccountingOwner for ObservedAccounting<'_> {}
#[async_trait]
impl DecisionAttemptAccounting for ObservedAccounting<'_> {
    fn remaining_budget(&self) -> (u64, u64) {
        self.owner.remaining_budget()
    }
    async fn begin(&self, attempt: AttemptBinding<'_>) -> Result<Box<dyn DecisionAttemptReceipt>, DecisionError> {
        let slot_deadline = attempt.deadline;
        self.observation.phase.store(0, Ordering::Release);
        let receipt = self.owner.begin(attempt).await?;
        self.observation
            .owner_controls_deadline
            .store(receipt.deadline() <= slot_deadline, Ordering::Relaxed);
        self.observation.phase.store(1, Ordering::Release);
        Ok(Box::new(ObservedReceipt {
            receipt,
            observation: Arc::clone(&self.observation),
        }))
    }
}
struct ObservedReceipt {
    receipt: Box<dyn DecisionAttemptReceipt>,
    observation: Arc<Observation>,
}
impl sealed::AttemptReceipt for ObservedReceipt {}
#[async_trait]
impl DecisionAttemptReceipt for ObservedReceipt {
    fn deadline(&self) -> Instant {
        self.receipt.deadline()
    }
    fn retain_response_chunk(&self, bytes: usize) -> Result<(), DecisionError> {
        self.receipt.retain_response_chunk(bytes)
    }
    async fn settle(self: Box<Self>, usage: DecisionUsage) -> Result<(), DecisionError> {
        let Self { receipt, observation } = *self;
        observation.phase.store(2, Ordering::Release);
        let result = receipt.settle(usage).await;
        observation.phase.store(3, Ordering::Release);
        result
    }
}
