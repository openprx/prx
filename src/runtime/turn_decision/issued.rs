//! Persist and read back the source-bound result of one bounded admission.
use super::{
    AgentDecisionError, Arc, CancellationToken, Context, DecisionRecordBuilder, Ordering, TurnDecisionContext,
    TurnDecisionService, TurnExecutionDirective, TurnSourceProof, accounting,
};

impl TurnDecisionService {
    pub(super) async fn record_issued_decision(
        &self,
        proof: &dyn TurnSourceProof,
        admission: accounting::AdmissionDecision,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        let (context, _telemetry) = admission;
        if cancellation.is_cancelled() && !context.router_cleanup_close() {
            return Err(AgentDecisionError::Cancelled);
        }
        if self.durable_key && self.recordable && self.generation.effective.causal_tree.enabled {
            let records = DecisionRecordBuilder::build_enabled_chain(
                &context,
                &self.state_key,
                true,
                self.generation.effective.causal_tree.store.record_redacted_projection,
            )?;
            if cancellation.is_cancelled() && !context.router_cleanup_close() {
                return Err(AgentDecisionError::Cancelled);
            }
            let writer = self.store.writer().ok_or(AgentDecisionError::RecordWriterUnavailable)?;
            // Admission to the bounded queue is not a durability receipt.
            // The writer reports a dropped chain through its incomplete event.
            if writer.record_chain(records) == crate::causal_tree::store::Enqueue::Queued {
                context.record_admitted.store(true, Ordering::Release);
            }
        }
        let admits_delegation = matches!(context.directive, TurnExecutionDirective::Delegate { .. })
            || context.execution_refs.iter().any(|reference| {
                self.catalog_view().entry_any(&reference.id).is_some_and(|entry| {
                    entry.fingerprint == reference.fingerprint
                        && entry.generation == reference.generation
                        && matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn")
                })
            });
        if admits_delegation {
            if self.generation.effective.causal_tree.enabled {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(AgentDecisionError::Cancelled),
                    verified = self.validate_delegation_storage(Arc::clone(&context)) => {
                        verified.map_err(AgentDecisionError::DelegationStorage)?;
                    }
                }
            } else {
                self.loop_runtime
                    .as_ref()
                    .context("ordinary delegation runtime missing")
                    .map_err(AgentDecisionError::DelegationScope)?;
                self.verified_delegation_scope(&context)
                    .map_err(AgentDecisionError::DelegationScope)?;
            }
            if cancellation.is_cancelled() && !context.router_cleanup_close() {
                return Err(AgentDecisionError::Cancelled);
            }
        }
        if matches!(context.directive, TurnExecutionDirective::RequestApproval { .. }) {
            self.record_pending_decision_approval(proof, &context)
                .await
                .map_err(AgentDecisionError::PendingApproval)?;
        }
        Ok(context)
    }
}
