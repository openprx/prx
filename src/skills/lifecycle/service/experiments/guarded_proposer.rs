//! Check the prepared immutable inputs at the last proposer seam before send.
use super::{Arc, Context, FrozenExperiment, Result, Utc, ensure};
use crate::skills::lifecycle::proposer_model::{ModelCallError, ModelReply, TallyCounters};
use crate::skills::lifecycle::{ModelBinding, SkillLifecycleStore};

pub(super) struct GuardedProposer {
    pub store: Arc<SkillLifecycleStore>,
    pub model: Arc<dyn crate::skills::lifecycle::ProposerModel>,
    pub plan: FrozenExperiment,
    pub proposal_id: String,
}
#[async_trait::async_trait]
impl crate::skills::lifecycle::ProposerModel for GuardedProposer {
    fn binding(&self) -> &ModelBinding {
        self.model.binding()
    }
    async fn complete(
        &self,
        system: &str,
        user: &str,
        tally: Arc<TallyCounters>,
    ) -> Result<ModelReply, ModelCallError> {
        self.check(system, user).map_err(|_| ModelCallError::BudgetDenied)?;
        self.model.complete(system, user, tally).await
    }
}
impl GuardedProposer {
    fn check(&self, system: &str, user: &str) -> Result<()> {
        self.plan.ensure_live(Utc::now())?;
        let pins = &self.plan.spec().pins;
        let active = self.store.active_binding(&pins.skill_id)?.context("baseline missing")?;
        ensure!(
            active.enabled
                && active.revision_id == pins.baseline_revision
                && active.record_digest == pins.baseline_record_digest
                && active.generation == pins.baseline_generation,
            "prepared baseline changed"
        );
        self.store.read_revision(
            &pins.skill_id,
            &pins.baseline_revision,
            Some(&pins.baseline_record_digest),
        )?;
        let prepared = self
            .store
            .read_proposal(&self.proposal_id)?
            .context("prepared proposal missing")?;
        let attempt = prepared.attempts.last().context("prepared attempt missing")?;
        ensure!(
            prepared.owner_scope == self.plan.spec().owner_scope
                && prepared.skill_id == pins.skill_id
                && prepared.baseline_revision.as_ref() == Some(&pins.baseline_revision)
                && prepared.expected_generation == pins.baseline_generation
                && prepared.author.model == pins.model
                && attempt.prompt_sha256 == crate::skills::lifecycle::propose::sha256_hex(&[system, user])
                && prepared.author.system_prompt_sha256 == crate::skills::lifecycle::propose::sha256_hex(&[system]),
            "prepared proposal input differs from admitted pins"
        );
        crate::runtime::experiment_stage::current()
            .context("prepared stage missing")?
            .ensure_active()?;
        Ok(())
    }
}
