//! Injected accounting for actual physical decision attempts.
//! This contract carries no source authority and never writes causal events.
//!
//! Neutral home (R17) of the attempt contract shared by the decision engine
//! and the runtime accounting owner: the attempt binding, the sealed
//! accounting and receipt traits, the usage report, the backend identity and
//! the decision error classes. Nothing here imports the decision engine.

pub mod error;

use std::sync::Arc;

pub use error::DecisionError;

use crate::config::schema::causal_tree::BackendKind;
use crate::providers::ProviderId;

/// Token and cost accounting reported by a backend. Accounting only: a
/// missing value is never a validation failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecisionUsage {
    /// Input tokens.
    pub input_tokens: Option<u32>,
    /// Output tokens.
    pub output_tokens: Option<u32>,
    /// Cost in millionths of a US dollar, when the backend reports one.
    pub cost_micro_usd: Option<u64>,
}

/// The locally configured identity of a backend: the audit truth, as opposed
/// to the claims in a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendIdentity {
    /// The backend kind.
    pub kind: BackendKind,
    /// The provider the backend belongs to, for hosted kinds.
    pub provider: Option<ProviderId>,
    /// The configured model.
    pub configured_model: Arc<str>,
}

use async_trait::async_trait;
use std::time::Instant;

pub(crate) mod sealed {
    pub trait AccountingOwner {}
    pub trait AttemptReceipt {}
}

/// Transport-created, borrowed identity for one physical send.
/// Secret material is represented only by a keyed publication binding.
pub struct AttemptBinding<'a> {
    pub(crate) backend: &'a BackendIdentity,
    pub(crate) provider: &'a str,
    pub(crate) model: &'a str,
    pub(crate) transport_binding: &'a [u8; 32],
    pub(crate) guarded_body: &'a [u8],
    pub(crate) request_id: &'a str,
    pub(crate) deadline: Instant,
    pub(crate) remaining_attempts: u8,
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
    /// Reserve retained response bytes when the parent has an output ceiling.
    fn retain_response_chunk(&self, _bytes: usize) -> Result<(), DecisionError> {
        Ok(())
    }

    /// Record actual usage before semantic prediction validation. Both token
    /// fields must be known to release an estimate; reported vendor cost is a
    /// diagnostic and does not replace PMM pricing. Actual usage above estimate
    /// is recorded in full, never clamped. The implementation transfers durable
    /// settlement to its cleanup owner before returning; the dispatched hold
    /// remains until persistence completes, and further physical sends wait for
    /// that completion. Cancellation cannot orphan the persistence task.
    async fn settle(self: Box<Self>, usage: DecisionUsage) -> Result<(), DecisionError>;
}
