//! Approval selection after channel admission and decision-bound collection.
use super::{ToolApprovalDecision, ToolApprovalRequest, ToolExecutionService};
impl ToolExecutionService {
    pub(super) async fn resolve_ordinary_approval(&self, request: ToolApprovalRequest) -> (ToolApprovalDecision, bool) {
        let tool = request.descriptor.public_name.clone();
        let call_id = request.command.operation_id.clone();
        crate::hooks::notify(
            crate::hooks::HookEvent::ApprovalRequested,
            serde_json::json!({"tool": tool, "call_id": call_id}),
        );
        let resolved = self.resolve_ordinary_approval_inner(request).await;
        let (decision, cause) = match &resolved.0 {
            ToolApprovalDecision::Approved { .. } => ("approved", None),
            ToolApprovalDecision::Denied { cause, .. } => ("denied", Some(cause.as_str())),
            ToolApprovalDecision::Cancelled { .. } => ("cancelled", None),
        };
        crate::hooks::notify(
            crate::hooks::HookEvent::ApprovalDecided,
            serde_json::json!({"tool": tool, "call_id": call_id, "decision": decision, "cause": cause}),
        );
        resolved
    }

    async fn resolve_ordinary_approval_inner(&self, request: ToolApprovalRequest) -> (ToolApprovalDecision, bool) {
        if let Some(live) = request.context.live_turn.clone() {
            return (live.0.approve(request).await, false);
        }
        if let Some(ledger) = request.context.approval_ledger.clone()
            && (request.descriptor.backend_name == "mcp_call"
                || crate::security::approval_operation::grant_op_for_call(
                    &request.descriptor.backend_name,
                    &request.command.arguments,
                )
                .is_some())
        {
            return (ledger.resolve(request).await, true);
        }
        (self.approval.resolve(request).await, false)
    }

    /// Resolve an approval that a process worker relayed to its parent, along
    /// the chain an in-process child's request takes.
    pub(crate) async fn resolve_relayed_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        self.resolve_ordinary_approval(request).await.0
    }

    /// Whether the parent's own policy, at the call's sealed level, still asks
    /// for approval. A relayed request that would not ask is refused.
    pub(crate) fn relayed_call_asks(
        &self,
        descriptor: &super::ToolDescriptor,
        context: &super::ToolExecutionContext,
    ) -> bool {
        let level = context.child_scope.as_ref().map_or_else(
            || self.policy.autonomy_level(),
            |scope| scope.gate_inputs().0.authority.level,
        );
        let decision = super::admission::child_autonomy(
            context,
            descriptor,
            self.policy.decide_at(descriptor, context, level),
            level,
        );
        decision == super::ToolExecutionDecision::Ask
    }
}
