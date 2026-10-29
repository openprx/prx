//! Evolution scheduling enters the same owner-scoped proposal store as manual
//! lifecycle work. The general evolution judge cannot publish these drafts.

use super::{ServiceError, SkillProposal, SkillsLifecycleService};
use crate::skills::lifecycle::{
    MAX_PROPOSAL_PATTERNS, ProposeOutcome, ProposeRequest, ProposerBudget, ProposerModel, propose_revision,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl SkillsLifecycleService {
    /// Retain terminal recording and the physical/cleanup owners across a
    /// dropped foreground call. Both ordinary evolution and M1 use this seam.
    pub(super) async fn owned_propose_revision(
        &self,
        request: ProposeRequest,
        model: Arc<dyn ProposerModel>,
        cancel: &CancellationToken,
    ) -> Result<ProposeOutcome, ServiceError> {
        Ok(if let Some(owner) = crate::runtime::turn_cleanup::current_tracker() {
            // Retain the proposal until its cancelled or completed record is
            // durable, even if the foreground caller is dropped.
            let child_cancel = cancel.child_token();
            let _cancel_on_drop = child_cancel.clone().drop_guard();
            let ticket = owner
                .register(child_cancel.clone())
                .map_err(|error| ServiceError::Worker(error.to_string()))?;
            let store = Arc::clone(&self.store);
            let envelope = crate::runtime::evolution_model::current_envelope();
            let experiment_stage = crate::runtime::experiment_stage::current();
            tokio::spawn(async move {
                let outcome = crate::runtime::evolution_model::retain_envelope(
                    envelope,
                    crate::runtime::experiment_stage::retain(
                        experiment_stage,
                        owner.scope(propose_revision(store, request, model.as_ref(), &child_cancel)),
                    ),
                )
                .await;
                ticket.finish(outcome.is_ok());
                outcome
            })
            .await
            .map_err(|error| ServiceError::Worker(error.to_string()))??
        } else {
            propose_revision(Arc::clone(&self.store), request, model.as_ref(), cancel).await?
        })
    }

    /// Stage a revision of a managed instruction document using verified
    /// lifecycle evidence. The supplied projection must still match the
    /// authoritative baseline before any model call is made.
    pub(crate) async fn propose_prompt_revision(
        &self,
        name: &str,
        expected_content: &str,
        request_id: String,
        budget: ProposerBudget,
        model: Arc<dyn ProposerModel>,
        cancel: &CancellationToken,
    ) -> Result<(SkillProposal, String), ServiceError> {
        self.require_enabled()?;
        if cancel.is_cancelled() {
            return Err(ServiceError::Settings(
                "evolution proposal cancelled before preparation".into(),
            ));
        }
        let store = Arc::clone(&self.store);
        let name = name.to_string();
        let expected = expected_content.to_string();
        let request = crate::runtime::blocking::spawn_blocking(move || {
            let (binding, _) = store
                .find_binding_by_name(&name)?
                .ok_or_else(|| super::LifecycleError::ProposalRefused("the prompt is not a managed skill".into()))?;
            let baseline =
                store.read_revision(&binding.skill_id, &binding.revision_id, Some(&binding.record_digest))?;
            if baseline.bundle.file("SKILL.md").map(|file| file.bytes()) != Some(expected.as_bytes()) {
                return Err(super::LifecycleError::ProposalRefused(
                    "the prompt projection differs from its active revision; repair it before proposing".into(),
                ));
            }
            let pattern_ids = store
                .active_patterns()?
                .into_iter()
                .filter(|pattern| pattern.skill_id.as_ref() == Some(&binding.skill_id) && pattern.usable_for_proposal())
                .take(MAX_PROPOSAL_PATTERNS)
                .map(|pattern| pattern.pattern_id)
                .collect();
            Ok(ProposeRequest {
                request_id,
                skill_id: binding.skill_id,
                pattern_ids,
                budget,
            })
        })
        .await
        .map_err(|error| ServiceError::Worker(error.to_string()))??;
        let outcome = self.owned_propose_revision(request, model, cancel).await?;
        let proposal = match outcome {
            ProposeOutcome::Staged(proposal) | ProposeOutcome::Reused(proposal)
                if proposal.candidate_revision.is_some() =>
            {
                proposal
            }
            other => {
                return Err(ServiceError::Settings(match other {
                    ProposeOutcome::Suppressed { .. } => "equivalent lifecycle proposal suppressed".into(),
                    ProposeOutcome::Failed(proposal)
                    | ProposeOutcome::Cancelled(proposal)
                    | ProposeOutcome::Stale(proposal)
                    | ProposeOutcome::Staged(proposal)
                    | ProposeOutcome::Reused(proposal) => format!(
                        "lifecycle proposal {} ended {}; inspect its durable record",
                        proposal.proposal_id,
                        proposal.status.as_str()
                    ),
                }));
            }
        };
        let store = Arc::clone(&self.store);
        let id = proposal.proposal_id.clone();
        let content = crate::runtime::blocking::spawn_blocking(move || {
            let bundle = store.read_candidate_bundle(&id)?;
            let file = bundle.file("SKILL.md").ok_or_else(|| {
                super::LifecycleError::ProposalRefused("candidate has no instruction document".into())
            })?;
            String::from_utf8(file.bytes().to_vec())
                .map_err(|_| super::LifecycleError::ProposalRefused("candidate instructions are not UTF-8".into()))
        })
        .await
        .map_err(|error| ServiceError::Worker(error.to_string()))??;
        Ok((proposal, content))
    }
}
