//! Persist and read back the source-bound result of one bounded admission.
use super::{
    AgentDecisionError, Arc, CancellationToken, Context, DecisionRecordBuilder, Ordering, TurnDecisionContext,
    TurnDecisionService, TurnExecutionDirective, TurnSourceProof, accounting,
};
use crate::runtime::child_source::authority_journal;

impl TurnDecisionService {
    pub(super) async fn record_issued_decision(
        &self,
        proof: &dyn TurnSourceProof,
        admission: accounting::AdmissionDecision,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        let (context, _telemetry) = admission;
        // Admission, including a pause before any child publication, owns the
        // original run allowance. The context lease cleans up failed issuance.
        self.issue_recursive_allowance(proof, &context);
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
            // Replay evidence is durable before admission. Its physical write
            // runs on a blocking thread and is always awaited, so cancellation
            // cannot drop it midway.
            let store_workspace = self.store.workspace_id().ok_or_else(|| {
                AgentDecisionError::DecisionEvidence(anyhow::anyhow!("decision store has no workspace id"))
            })?;
            let evidence = authority_journal::decision_evidence_rows(&records, &store_workspace)
                .map_err(AgentDecisionError::DecisionEvidence)?;
            let workspace = self.generation.effective.workspace_dir.clone();
            let key = Arc::clone(&self.state_key);
            let ticket = crate::runtime::turn_cleanup::current_tracker()
                .map(|owner| owner.register(CancellationToken::new()))
                .transpose()
                .map_err(AgentDecisionError::DecisionEvidence)?;
            let written = crate::runtime::blocking::spawn_blocking(move || {
                let result = authority_journal::write_decision_evidence(&workspace, &key, &evidence);
                if let Some(ticket) = ticket {
                    ticket.finish(result.is_ok());
                }
                result
            })
            .await
            .map_err(|error| AgentDecisionError::DecisionEvidence(error.into()))?;
            written.map_err(AgentDecisionError::DecisionEvidence)?;
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
