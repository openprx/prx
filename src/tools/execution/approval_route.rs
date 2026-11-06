//! Approval selection after channel admission and decision-bound collection.
use super::{ToolApprovalDecision, ToolApprovalRequest, ToolExecutionService};
impl ToolExecutionService {
    pub(super) async fn resolve_ordinary_approval(&self, request: ToolApprovalRequest) -> (ToolApprovalDecision, bool) {
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
}
