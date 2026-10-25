//! Evolution scheduling enters the same owner-scoped proposal store as manual
//! lifecycle work. The general evolution judge cannot publish these drafts.

use super::{ServiceError, SkillProposal, SkillsLifecycleService};
use crate::skills::lifecycle::{
    MAX_PROPOSAL_PATTERNS, ProposeOutcome, ProposeRequest, ProposerBudget, ProposerModel, propose_revision,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl SkillsLifecycleService {
    /// Stage a revision of a managed instruction document using verified
    /// lifecycle evidence. The supplied projection must still match the
    /// authoritative baseline before any model call is made.
    pub(crate) async fn propose_prompt_revision(
        &self,
        name: &str,
        expected_content: &str,
        request_id: String,
        budget: ProposerBudget,
        model: &dyn ProposerModel,
    ) -> Result<(SkillProposal, String), ServiceError> {
        self.require_enabled()?;
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
        let outcome = propose_revision(Arc::clone(&self.store), request, model, &CancellationToken::new()).await?;
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
