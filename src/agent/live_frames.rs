//! Optional, infallible ingress adapters inherited through trusted child contexts.
use crate::agent::turn_contracts::ToolLoopEvent;
use crate::runtime::envelope::RuntimeEnvelope;
use std::sync::Arc;

pub(crate) trait LiveFrameSink: Send + Sync {
    fn emit(&self, event: &ToolLoopEvent);
    fn thinking(&self);
    fn awaiting_approval(&self, tool: &str);
    fn child_denial(&self, tool: &str, child_run_id: &str);
}

#[async_trait::async_trait]
pub(crate) trait LiveTurn: Send + Sync {
    fn frames(&self, envelope: &RuntimeEnvelope, tools: &std::collections::HashSet<String>) -> Arc<dyn LiveFrameSink>;
    async fn approve(&self, request: crate::tools::ToolApprovalRequest) -> crate::tools::ToolApprovalDecision;
}

/// Runtime-owned hooks are inherited only through trusted child contexts.
#[derive(Clone)]
pub(crate) struct LiveTurnRef(pub(crate) Arc<dyn LiveTurn>);
impl std::fmt::Debug for LiveTurnRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveTurnRef")
    }
}
impl PartialEq for LiveTurnRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for LiveTurnRef {}
