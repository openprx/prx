//! Supervised approval on the line-mode tool loop (CR-2).
//!
//! A chat build without the `terminal-tui` feature runs every turn of
//! `prx chat --plain` and `prx chat --message` on the line-mode tool loop in
//! `chat::run`, not through the reducer driver. That loop gets the same
//! terminal prompt (`Approve <tool> <summary>? [y/N/a]`, see
//! [`super::terminal_approval`]) through the shared run approval strategy:
//! `y` approves this call, `a` approves it and later calls of the same tool
//! for the session (calls that need an explicit grant are still asked),
//! anything else denies. Ctrl-C cancels the turn and abandons the prompt.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::terminal_approval::TerminalApproval;
use crate::agent::turn_contracts::{ApprovalDecision, ApprovalResolver};
use crate::approval::{ApprovalManager, ApprovalRequest};
use crate::memory::Memory;
use crate::security::SecurityPolicy;
use crate::tools::{
    AdapterOwnedPreparation, SecurityEffectPolicy, Tool, ToolExecutionService, TracingToolExecutionAudit,
};

/// The channel name the line-mode loop records approvals under.
pub(super) const LINE_APPROVAL_CHANNEL: &str = "terminal";

/// The operator's terminal answer as an approval decision.
pub(super) const fn decision_for(answer: TerminalApproval) -> ApprovalDecision {
    match answer {
        TerminalApproval::Once => ApprovalDecision::Grant,
        TerminalApproval::Always => ApprovalDecision::GrantForSession,
        TerminalApproval::Deny => ApprovalDecision::Deny,
    }
}

/// Asks on the controlling terminal while the turn is not cancelled.
struct TerminalApprovalResolver {
    cancellation: CancellationToken,
}

#[async_trait]
impl ApprovalResolver for TerminalApprovalResolver {
    async fn resolve(&self, request: &ApprovalRequest, _channel: &str) -> ApprovalDecision {
        let arguments = request.arguments.to_string();
        let cancellation = self.cancellation.clone();
        let answer =
            super::terminal_approval::prompt(&request.tool_name, &arguments, move || !cancellation.is_cancelled())
                .await;
        decision_for(answer)
    }
}

/// The tool execution service of one line-mode turn: the run approval
/// strategy over the session's approval manager (its "always" allowlist
/// lasts for the session) and the terminal prompt.
pub(super) fn line_tool_execution_service(
    registry: Arc<Vec<Box<dyn Tool>>>,
    memory: &Arc<dyn Memory>,
    policy: &Arc<SecurityPolicy>,
    manager: Arc<ApprovalManager>,
    cancellation: CancellationToken,
) -> ToolExecutionService {
    let resolver: Arc<dyn ApprovalResolver> = Arc::new(TerminalApprovalResolver {
        cancellation: cancellation.clone(),
    });
    let strategy = crate::agent::execution_policy::run_approval_strategy(
        Some(manager),
        Some(resolver),
        LINE_APPROVAL_CHANNEL,
        Arc::clone(policy),
        Some(cancellation),
    );
    let service = ToolExecutionService::from_shared_boxed_registry(
        registry,
        Arc::new(SecurityEffectPolicy::new(Arc::clone(policy))),
        strategy,
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    );
    // As on the reducer path: the ledger comes from the configured memory
    // backend, or a dedicated store when that backend keeps no event log.
    match crate::memory::tool_execution_ledger(memory, &policy.workspace_dir) {
        Some(ledger) => service.with_idempotency_memory(ledger),
        None => service,
    }
}

#[cfg(test)]
mod tests {
    use super::{TerminalApproval, decision_for};
    use crate::agent::turn_contracts::ApprovalDecision;

    #[test]
    fn terminal_answers_map_to_approval_decisions() {
        assert!(
            matches!(decision_for(TerminalApproval::Once), ApprovalDecision::Grant),
            "terminal `once` answer must map to a single grant"
        );
        assert!(
            matches!(
                decision_for(TerminalApproval::Always),
                ApprovalDecision::GrantForSession
            ),
            "terminal `always` answer must map to a session grant"
        );
        assert!(
            matches!(decision_for(TerminalApproval::Deny), ApprovalDecision::Deny),
            "terminal `deny` answer must map to a denial"
        );
    }
}
