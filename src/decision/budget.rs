//! Injected accounting for actual physical decision attempts.
//! This contract carries no source authority and never writes causal events.
use super::{
    error::DecisionError,
    protocol::{BackendIdentity, DecisionUsage},
};
use async_trait::async_trait;
use std::time::Instant;

pub(crate) mod sealed {
    pub trait AccountingOwner {}
    pub trait AttemptReceipt {}
}

/// Transport-created, borrowed identity for one physical send.
/// Secret material is represented only by a keyed publication binding.
pub struct AttemptBinding<'a> {
    pub(super) backend: &'a BackendIdentity,
    pub(super) provider: &'a str,
    pub(super) model: &'a str,
    pub(super) transport_binding: &'a [u8; 32],
    pub(super) guarded_body: &'a [u8],
    pub(super) request_id: &'a str,
    pub(super) deadline: Instant,
    pub(super) remaining_attempts: u8,
}
impl AttemptBinding<'_> {
    /// Remaining sends in the transport-owned bounded attempt plan.
    #[must_use]
    pub const fn remaining_attempts(&self) -> u8 {
        self.remaining_attempts
    }
    #[must_use]
    pub const fn backend(&self) -> &BackendIdentity {
        self.backend
    }
    #[must_use]
    pub const fn provider(&self) -> &str {
        self.provider
    }
    #[must_use]
    pub const fn model(&self) -> &str {
        self.model
    }
    #[must_use]
    pub const fn transport_binding(&self) -> &[u8; 32] {
        self.transport_binding
    }
    #[must_use]
    pub const fn guarded_body(&self) -> &[u8] {
        self.guarded_body
    }
    #[must_use]
    pub const fn request_id(&self) -> &str {
        self.request_id
    }
    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// Sealed runtime producer; borrowed on builtin/cache paths without an owner
/// allocation. The real implementation validates its source and lazily retains
/// one turn accounting owner across hops, fallbacks and retries.
#[async_trait]
pub trait DecisionAttemptAccounting: sealed::AccountingOwner + Send + Sync {
    /// Remaining token and micro-USD allowance after known usage and pending
    /// liabilities. This read performs no IO and must not lazily allocate an
    /// owner. Used to tighten the existing post-prediction G6 budget gate.
    fn remaining_budget(&self) -> (u64, u64);

    /// Reserve tokens and PMM-priced cost, then persist one-use Dispatched
    /// BEFORE returning. No receipt means no physical send. This method must
    /// never increase the original source deadline or any remaining allowance.
    async fn begin(&self, attempt: AttemptBinding<'_>) -> Result<Box<dyn DecisionAttemptReceipt>, DecisionError>;
}

/// A real dispatched accounting receipt, not permission for another call.
/// Drop retains durable token/cost liability. Missing or partial token usage
/// retains the full reservation; a missing value is never a known zero.
#[async_trait]
pub trait DecisionAttemptReceipt: sealed::AttemptReceipt + Send {
    /// Original absolute owner deadline, without clamping to the slot timeout.
    /// Transport clamps send/reads to the earlier of this and its slot deadline.
    /// Retaining provenance makes local deadline expiry distinct from a backend
    /// timeout, including the case where both deadlines are equal.
    fn deadline(&self) -> Instant;

    /// Record actual usage before semantic prediction validation. Both token
    /// fields must be known to release an estimate; reported vendor cost is a
    /// diagnostic and does not replace PMM pricing. Actual usage above estimate
    /// is recorded in full, never clamped. The implementation owns settlement
    /// work before awaiting so cancellation cannot orphan a persistence task.
    async fn settle(self: Box<Self>, usage: DecisionUsage) -> Result<(), DecisionError>;
}

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
    async fn settle(self: Box<Self>, usage: DecisionUsage) -> Result<(), DecisionError> {
        let Self { receipt, observation } = *self;
        observation.phase.store(2, Ordering::Release);
        let result = receipt.settle(usage).await;
        observation.phase.store(3, Ordering::Release);
        result
    }
}
