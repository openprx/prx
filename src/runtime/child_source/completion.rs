//! Child completion settlement. Admission stays in the gated adapter.
use anyhow::Result;
use std::sync::Arc;

/// Completion uses the admitted segment's prebound terminal and usage owner.
/// It cannot admit a segment or reconstruct parent authority from a decision row.
pub(crate) struct ChildCompletionPort {
    pub(crate) usage: Arc<crate::runtime::turn_terminal::TerminalUsageCollector>,
}

impl ChildCompletionPort {
    pub(crate) async fn complete(
        &self,
        turn: &crate::runtime::child_turn::ChildSegmentTurn,
        outcome: &crate::agent::turn_contracts::ToolLoopOutcome,
        trace: &crate::agent::turn_contracts::ToolLoopTrace,
    ) -> Result<()> {
        turn.complete(outcome, trace).await.map(|_| ())
    }

    pub(crate) async fn fail(
        &self,
        turn: &crate::runtime::child_turn::ChildSegmentTurn,
        error: &anyhow::Error,
        cancelled: bool,
    ) -> Result<()> {
        turn.fail(error, cancelled).await
    }
}
