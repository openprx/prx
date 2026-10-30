//! Automatic ordering over admitted stages. This adds no execution, budget,
//! cleanup or publication owner. Unknown attempts always require reconciliation.
use super::{
    Arc, CancellationToken, Config, Context, ExperimentJournal, Result, SkillsLifecycleService, StageState, ensure,
};
use crate::skills::lifecycle::experiment::{InstructionCycleDraft, InstructionExperimentPlan};

impl SkillsLifecycleService {
    pub(crate) fn plan_instruction_cycle(
        &self,
        draft: InstructionCycleDraft,
        config: &Config,
    ) -> Result<InstructionExperimentPlan> {
        let draft = draft.compile()?;
        let operations = draft
            .stages
            .iter()
            .map(|stage| (stage.id.clone(), stage.operation.clone()))
            .collect();
        let spec = self.plan_experiment_draft(draft, config)?.spec().clone();
        let plan = InstructionExperimentPlan { spec, operations };
        plan.validate()?;
        Ok(plan)
    }

    pub(crate) async fn run_instruction_plan(
        self: &Arc<Self>,
        config: &Config,
        plan: &InstructionExperimentPlan,
        cancel: &CancellationToken,
    ) -> Result<ExperimentJournal> {
        plan.validate()?;
        let frozen = plan.spec.clone().freeze()?;
        let mut journal = self.inspect_experiment(&plan.spec.id, frozen.digest())?;
        for stage in &plan.spec.stages {
            ensure!(
                !cancel.is_cancelled() && !journal.state.cancelled,
                "instruction experiment cancelled"
            );
            match journal.state.stages.get(&stage.id).context("planned stage missing")? {
                state if state.successful() => continue,
                StageState::Settled {
                    cleanup_confirmed: true,
                    ..
                } => return Ok(journal),
                StageState::Planned => {}
                _ => anyhow::bail!(
                    "stage {} requires inspection/reconciliation; no effects repeated",
                    stage.id
                ),
            }
            let operation = plan.operations.get(&stage.id).context("stage input missing")?.clone();
            journal = self
                .run_experiment_stage(
                    config,
                    plan.spec.id.clone(),
                    frozen.digest().into(),
                    stage.id.clone(),
                    operation,
                    cancel,
                )
                .await?;
            if !journal.state.stages.get(&stage.id).is_some_and(StageState::successful) {
                return Ok(journal);
            }
        }
        Ok(journal)
    }
}
