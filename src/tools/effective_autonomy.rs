//! Runtime-only authority for the three Full-mode native tool widenings.
use crate::security::{AutonomyLevel, SecurityPolicy};
use std::borrow::Cow;
use std::future::Future;

tokio::task_local! {
    static CURRENT: AutonomyLevel;
    static EVALUATION_PATH_POLICY: SecurityPolicy;
}

/// The call's level: the call context's when the turn was opened with one,
/// else the execution service's per-call carrier.
pub(crate) fn current() -> Option<AutonomyLevel> {
    crate::context::call::level().or_else(|| CURRENT.try_with(|level| *level).ok())
}

pub(crate) async fn scope<T>(level: AutonomyLevel, future: impl Future<Output = T>) -> T {
    CURRENT.scope(level, future).await
}

/// `held` at the call's level: the policy a tool-internal check reads. The
/// level comes from the execution service's per-call carrier and falls back
/// to the level `held` already carries when no call level is set.
pub(crate) fn call_policy(held: &SecurityPolicy) -> Cow<'_, SecurityPolicy> {
    let level = crate::security::agent_level::call_level(current(), held.autonomy);
    if level == held.autonomy {
        Cow::Borrowed(held)
    } else {
        Cow::Owned(SecurityPolicy {
            autonomy: level,
            ..held.clone()
        })
    }
}

/// Carry evaluator path restrictions through media preparation without changing
/// the media owner's public API or the call's Full network/tool behavior.
pub(crate) async fn scope_evaluation_paths<T>(policy: &SecurityPolicy, future: impl Future<Output = T>) -> T {
    if policy.evaluation_sandbox {
        EVALUATION_PATH_POLICY.scope(policy.clone(), future).await
    } else {
        future.await
    }
}

pub(crate) fn evaluation_path_policy() -> Option<SecurityPolicy> {
    EVALUATION_PATH_POLICY.try_with(Clone::clone).ok()
}
