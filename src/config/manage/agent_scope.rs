//! The agent-management exemption from the high-risk confirmation.
//!
//! Creating, replacing, retiring and purging a named agent through the
//! management API never asks for `confirm_high_risk` (owner decision for the
//! rf integration). The exemption is only honoured for a plan whose every
//! change, every risk reason and every secret write or clear lies inside
//! `agents.<name>`; a plan that reaches anything else keeps the ordinary
//! confirmation.

use super::plan::{ConfigChangeSummary, RiskReason};

/// Whether `path` is `prefix` or lies below it (a key or an index).
fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.') || rest.starts_with('['))
}

/// The path a risk reason names; `None` for a reason that names none (the
/// reasons that concern the whole configuration).
fn reason_path(reason: &RiskReason) -> Option<&str> {
    match reason {
        RiskReason::CredentialChanged { path }
        | RiskReason::EndpointChanged { path }
        | RiskReason::CredentialReintroduced { path }
        | RiskReason::PrivilegeWidened { path }
        | RiskReason::CredentialRecipientChanged { path }
        | RiskReason::ProcessEnvironmentChanged { path }
        | RiskReason::CredentialOwnerChanged { path }
        | RiskReason::AccessGrantChanged { path }
        | RiskReason::RemoteDecisionEnabled { path } => Some(path),
        RiskReason::EncryptionDisabled | RiskReason::BaselineUnavailable => None,
    }
}

/// Whether the whole change described by `summary` stays inside the
/// configuration of the agent `agent`.
pub(super) fn confines(summary: &ConfigChangeSummary, agent: &str) -> bool {
    // A name that could be read as a longer path is never confined.
    if agent.is_empty() || agent.contains(['.', '[', ']']) {
        return false;
    }
    let prefix = format!("agents.{agent}");
    summary.changes.iter().all(|entry| under(&entry.path, &prefix))
        && summary
            .risk_reasons
            .iter()
            .all(|reason| reason_path(reason).is_some_and(|path| under(path, &prefix)))
        && summary
            .secret_writes
            .iter()
            .chain(&summary.secret_clears)
            .all(|path| under(path, &prefix))
}
