//! Approval selection after channel admission and decision-bound collection.
use super::{ToolApprovalDecision, ToolApprovalRequest, ToolExecutionService};
impl ToolExecutionService {
    pub(super) async fn resolve_ordinary_approval(&self, request: ToolApprovalRequest) -> (ToolApprovalDecision, bool) {
        if let Some(ledger) = request.context.approval_ledger.clone() {
            return (ledger.resolve(request).await, true);
        }
        (self.approval.resolve(request).await, false)
    }
}
