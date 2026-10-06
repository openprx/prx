//! In-process authority for one durably claimed decision call. Wire grants
//! alone cannot install this scope, and task spawning does not inherit it.

use crate::acl::approval_grant::{ApprovalGrantV2, RiskLevel};
use std::future::Future;

/// Implemented by the decision service's private successful claim. The
/// implementation must retain the exact call and recheck its live authority.
/// Ordinary approval adapters and serialized grants do not implement this.
pub(crate) trait DecisionGrantAuthority: Send + Sync {
    fn verify(
        &self,
        grant: &ApprovalGrantV2,
        tool: &str,
        operation: &str,
        risk: RiskLevel,
        caller_principal: Option<&str>,
    ) -> Result<(), String>;
}

tokio::task_local! {
    static CLAIMED_DECISION: Box<dyn DecisionGrantAuthority>;
}

/// Install the nonserializable claim only around the exact backend future.
/// Ownership is moved into this future; dropping it cannot reopen the durable
/// resolution. Backend child tasks must obtain their own authority.
pub(crate) async fn with_claimed_decision<F: Future>(
    authority: Box<dyn DecisionGrantAuthority>,
    future: F,
) -> F::Output {
    CLAIMED_DECISION.scope(authority, future).await
}

pub(crate) fn verify_claimed_decision(
    grant: &ApprovalGrantV2,
    tool: &str,
    operation: &str,
    risk: RiskLevel,
    caller_principal: Option<&str>,
) -> Result<(), String> {
    CLAIMED_DECISION
        .try_with(|authority| authority.verify(grant, tool, operation, risk, caller_principal))
        .map_err(|_| "decision grant requires a durable claimed execution scope".to_owned())?
}
