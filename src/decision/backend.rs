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

use super::error::DecisionError;
use super::loopback::is_literal_loopback;
use super::outbound::GuardedBody;
use super::protocol::{BackendIdentity, BackendKind, BackendLocality, DecisionRequest, DecisionResponse};
use super::validate::ModelVersionPolicy;

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
        accounting: Option<&dyn super::budget::DecisionAttemptAccounting>,
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
        accounting: Option<&dyn super::budget::DecisionAttemptAccounting>,
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

/// Where a backend of `kind` configured with `endpoint` runs (SP-CDP-A 5.6),
/// derived from the kind and the literal-loopback predicate:
///
/// - `builtin` runs in process;
/// - `compatible_http` is on loopback when its endpoint is written as a
///   literal loopback address, and remote otherwise;
/// - a hosted kind is always remote, even with an endpoint override that
///   reads as loopback (COORD-6).
#[must_use]
pub fn locality_of(kind: BackendKind, endpoint: &str) -> BackendLocality {
    match kind {
        BackendKind::Builtin => BackendLocality::Builtin,
        BackendKind::CompatibleHttp if is_literal_loopback(endpoint) => BackendLocality::Loopback,
        BackendKind::CompatibleHttp | BackendKind::TypeSafe | BackendKind::OpenRouter => BackendLocality::Remote,
        #[cfg(any(test, feature = "test-mock"))]
        BackendKind::Mock => BackendLocality::Builtin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locality_follows_the_kind_and_the_written_endpoint() {
        assert_eq!(locality_of(BackendKind::Builtin, ""), BackendLocality::Builtin);
        assert_eq!(
            locality_of(BackendKind::CompatibleHttp, "http://127.0.0.1:8900/v1"),
            BackendLocality::Loopback
        );
        assert_eq!(
            locality_of(BackendKind::CompatibleHttp, "http://127.1:8900/v1"),
            BackendLocality::Remote,
            "only a literal loopback spelling is loopback"
        );
        assert_eq!(
            locality_of(BackendKind::CompatibleHttp, "https://decisions.example.com/v1"),
            BackendLocality::Remote
        );
        for hosted in [BackendKind::TypeSafe, BackendKind::OpenRouter] {
            assert_eq!(
                locality_of(hosted, "http://127.0.0.1:8900/v1"),
                BackendLocality::Remote,
                "COORD-6: a hosted override that reads as loopback stays remote"
            );
        }
    }
}
