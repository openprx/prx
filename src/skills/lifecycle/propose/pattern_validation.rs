//! Read-only pattern proof shared by initial planning and refreshed dispatch.
//! Planning never refreshes/consolidates knowledge or reserves a proposal.
use super::{
    KnowledgePattern, LifecycleError, MAX_PROPOSAL_PATTERNS, MIN_DISTINCT_TASKS_FOR_PROPOSAL, PatternId, ProposalBasis,
    Qualified, RevisionId, SkillId, SkillLifecycleStore, refuse,
};
use crate::skills::lifecycle::consolidate::EvidenceFacts;

impl SkillLifecycleStore {
    /// Reuse the exact proposer applicability, terminal-fact, root and
    /// contradiction checks against this real baseline. No caller claim is proof.
    pub(super) fn proposal_pattern_evidence(
        &self,
        pattern: &KnowledgePattern,
        skill: &SkillId,
        baseline: &RevisionId,
    ) -> Result<(Qualified, Qualified), LifecycleError> {
        self.check_pattern_is_for(pattern, skill)?;
        let support = self.qualified_evidence(&pattern.evidence_ids, skill, baseline, true)?;
        if support.ids.is_empty() {
            return Err(refuse(format!(
                "pattern {} has no stored failure of {skill} on revision {baseline} that can be held against its guidance ({})",
                pattern.pattern_id,
                support.excluded.describe()
            )));
        }
        let counter = self.qualified_evidence(&pattern.counterevidence_ids, skill, baseline, false)?;
        let pool: Vec<_> = support.facts.iter().chain(&counter.facts).cloned().collect();
        let own = self.distinct_roots(&support.facts, &pool)?;
        let against = self.distinct_roots(&counter.facts, &pool)?;
        if own < MIN_DISTINCT_TASKS_FOR_PROPOSAL || against >= own {
            return Err(refuse(format!(
                "pattern {} rests on {own} failed task(s) on revision {baseline} against {against} contradicting; it needs at least {MIN_DISTINCT_TASKS_FOR_PROPOSAL} and more than the contradiction",
                pattern.pattern_id
            )));
        }
        Ok((support, counter))
    }
    pub(super) fn proposal_evidence_basis(
        &self,
        support: &[EvidenceFacts],
        counter: &[EvidenceFacts],
    ) -> Result<ProposalBasis, LifecycleError> {
        let pool: Vec<_> = support.iter().chain(counter).cloned().collect();
        Ok(ProposalBasis {
            failed_tasks_on_baseline: self.distinct_roots(support, &pool)?,
            contradicting_tasks: self.distinct_roots(counter, &pool)?,
            // qualified_evidence only includes positively verified adherence
            // and absence of infrastructure/permission causes for support.
            adherence_verified: true,
        })
    }
    /// Initial planning only: metadata candidates do not grant eligibility.
    /// Actual dispatch still refreshes and reruns this same evidence proof.
    pub(in crate::skills::lifecycle) fn validate_proposal_patterns_for_plan(
        &self,
        skill: &SkillId,
        baseline: &RevisionId,
        ids: &[PatternId],
    ) -> Result<ProposalBasis, LifecycleError> {
        if ids.is_empty() || ids.len() > MAX_PROPOSAL_PATTERNS {
            return Err(refuse(format!("a proposal needs 1..={MAX_PROPOSAL_PATTERNS} patterns")));
        }
        let _owner_read = self.lock_store(true, std::time::Duration::from_secs(5))?;
        let index = self.pattern_index()?;
        let mut ids = ids.to_vec();
        ids.sort();
        ids.dedup();
        let mut support = Vec::new();
        let mut counter = Vec::new();
        for id in ids {
            if !index.entries.iter().any(|entry| entry.pattern_id == id) {
                return Err(refuse(format!(
                    "pattern {id} is not an active pattern (unknown, superseded or withdrawn)"
                )));
            }
            let pattern = self
                .read_pattern(&id)?
                .ok_or_else(|| refuse(format!("pattern {id} is missing")))?;
            let (positive, negative) = self.proposal_pattern_evidence(&pattern, skill, baseline)?;
            support.extend(positive.facts);
            counter.extend(negative.facts);
        }
        // The same global basis calculation used by the proposer; no new
        // independent threshold or qualification policy is introduced here.
        self.proposal_evidence_basis(&support, &counter)
    }
}
