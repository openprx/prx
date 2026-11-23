//! Explicit denial when the receiving root has no live approval adapter.
use crate::agent::live_frames::{LiveFrameSink, LiveTurn, LiveTurnRef};
use crate::agent::turn_contracts::ToolLoopEvent;
use std::sync::Arc;
struct NoSurface;
impl LiveFrameSink for NoSurface {
    fn emit(&self, _: &ToolLoopEvent) {}
    fn thinking(&self) {}
    fn awaiting_approval(&self, _: &str) {}
    fn child_denial(&self, _: &str, _: &str) {}
}
#[async_trait::async_trait]
impl LiveTurn for NoSurface {
    fn frames(
        &self,
        _: &crate::runtime::envelope::RuntimeEnvelope,
        _: &std::collections::HashSet<String>,
    ) -> Arc<dyn LiveFrameSink> {
        Arc::new(Self)
    }
    async fn approve(&self, _: crate::tools::ToolApprovalRequest) -> crate::tools::ToolApprovalDecision {
        crate::tools::ToolApprovalDecision::Denied {
            cause: crate::tools::ApprovalDenialCause::NoApprovalSurface,
            reason: crate::tools::execution::APPROVAL_DENIED_NO_APPROVAL_SURFACE.to_owned(),
        }
    }
}
pub(crate) fn no_surface() -> LiveTurnRef {
    LiveTurnRef(Arc::new(NoSurface))
}
