//! The decision backend trait and backend locality (CDP 4.2; CDP-PLAN 1.5;
//! SP-CDP-A 5.6).
//!
//! A backend answers a sealed [`DecisionRequest`] with a raw
//! [`DecisionResponse`]. It never validates: only the factory
//! (`factory.rs`) calls the validator, and only the factory builds a
//! backend (AG-04). The builtin backend is not behind this trait: the
//! factory answers with it directly as the always-available shadow.

use std::time::Instant;

use async_trait::async_trait;

use crate::config::schema::causal_tree::BackendLocality;
use crate::cost::decision_attempt::BackendIdentity;
use crate::cost::decision_attempt::error::DecisionError;
use crate::decision_engine::prediction::outbound::GuardedBody;
use crate::decision_engine::prediction::protocol::{DecisionRequest, DecisionResponse};
use crate::decision_engine::prediction::validate::ModelVersionPolicy;

/// A decision backend other than the builtin.
#[async_trait]
pub trait DecisionBackend: Send + Sync {
    /// The configured identity: the audit truth, whatever the response
    /// claims.
    fn identity(&self) -> &BackendIdentity;

    /// Where the backend runs.
    fn locality(&self) -> BackendLocality;

    /// Which model claims the validator accepts from this backend.
    fn version_policy(&self) -> ModelVersionPolicy;

    /// Whether the backend independently enforces its configured transport
    /// timeout. Such backends may use the manifest deadline for source-owned
    /// accounting while their network client retains the narrower I/O bound.
    fn bounds_transport_independently(&self) -> bool {
        false
    }

    /// Answer `request` before `deadline`, sending exactly `body`: the
    /// bytes the outbound guard checked (D-079 M-3). A backend never
    /// re-encodes the request; it may use the request to decode the answer.
    /// A backend that cannot finish in time returns
    /// [`DecisionError::Timeout`]; the factory also bounds the call, so a
    /// backend that ignores the deadline is cut off.
    async fn evaluate(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError>;

    /// Evaluate with an injected source-owned physical-attempt accountant.
    /// Custom backends must explicitly implement accounting before receiving a
    /// live budget scope; the default never silently bypasses it.
    async fn evaluate_with_accounting(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        accounting: Option<&dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, DecisionError> {
        if accounting.is_some() {
            return Err(DecisionError::BudgetExceeded);
        }
        self.evaluate(request, body, deadline).await
    }

    /// A health probe with its own operator-owned accounting scope.
    async fn health_with_accounting(
        &self,
        probe: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        accounting: Option<&dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
    ) -> Result<(), DecisionError> {
        if accounting.is_some() {
            return Err(DecisionError::BudgetExceeded);
        }
        self.health(probe, body, deadline).await
    }

    /// Whether the backend can serve (SP-CDP-A 5.6 "Health"). The factory
    /// hands a neutral `probe` request carrying no user state and its
    /// guarded `body` (D-082 N-2). A loopback backend answers it; a remote
    /// one reports configuration readiness only and performs no I/O, which
    /// is the default.
    async fn health(
        &self,
        probe: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
    ) -> Result<(), DecisionError> {
        let _ = (probe, body, deadline);
        Ok(())
    }
}
