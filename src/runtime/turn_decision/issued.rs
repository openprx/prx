//! Admit an opened turn: the run allowance and the delegation scope.
use super::{
    AgentDecisionError, Arc, CancellationToken, Context, TurnDecisionContext, TurnDecisionService, TurnSourceProof,
};

impl TurnDecisionService {
    pub(crate) async fn record_opened_context(
        &self,
        proof: &dyn TurnSourceProof,
        context: Arc<TurnDecisionContext>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        self.record_issued_decision(proof, context, cancellation).await
    }

    pub(crate) async fn open_issued_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        kind: super::AgentTurnKind,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        let opened = self.open_turn_context(proof, kind, cancellation).await?;
        self.record_opened_context(proof, opened, cancellation).await
    }

    pub(in crate::runtime) async fn open_issued_segment_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        observation: Option<&crate::runtime::child_turn::segment_observation::SegmentObservation>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        let opened = self.open_segment_turn_context(proof, observation, cancellation).await?;
        self.record_opened_context(proof, opened, cancellation).await
    }

    pub(super) async fn record_issued_decision(
        &self,
        proof: &dyn TurnSourceProof,
        context: Arc<TurnDecisionContext>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        // Admission, including a pause before any child publication, owns the
        // original run allowance. The context lease cleans up failed issuance.
        context.runtime().issue_recursive_allowance(proof);
        if cancellation.is_cancelled() {
            return Err(AgentDecisionError::Cancelled);
        }
        // A published delegator makes delegation storage reachable (R28).
        let admits_delegation = context.execution_refs().iter().any(|reference| {
            self.catalog_view().entry_any(&reference.id).is_some_and(|entry| {
                entry.fingerprint == reference.fingerprint
                    && entry.generation == reference.generation
                    && crate::tools::surface_owner::publication::is_delegator(&entry.public_name)
            })
        });
        if admits_delegation {
            self.loop_runtime
                .as_ref()
                .context("ordinary delegation runtime missing")
                .map_err(AgentDecisionError::DelegationScope)?;
            self.verified_delegation_scope(&context)
                .map_err(AgentDecisionError::DelegationScope)?;
            if cancellation.is_cancelled() {
                return Err(AgentDecisionError::Cancelled);
            }
        }

        Ok(context)
    }
}
