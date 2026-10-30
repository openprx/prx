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
        // Refuse any active/uncertain stage before starting unrelated work.
        for state in journal.state.stages.values() {
            match state {
                StageState::Planned => {}
                state if state.successful() => {}
                StageState::Settled {
                    cleanup_confirmed: true,
                    ..
                } => return Ok(journal),
                _ => anyhow::bail!("instruction experiment requires inspection/reconciliation; no effects repeated"),
            }
        }
        use futures_util::{StreamExt, stream::FuturesUnordered};
        let workers_cancel = cancel.child_token();
        let _cancel_on_drop = workers_cancel.clone().drop_guard();
        let mut active = FuturesUnordered::new();
        let mut running = std::collections::BTreeSet::new();
        loop {
            journal = self.inspect_experiment(&plan.spec.id, frozen.digest())?;
            if cancel.is_cancelled() || journal.state.cancelled {
                workers_cancel.cancel();
                while active.next().await.is_some() {}
                anyhow::bail!("instruction experiment cancelled");
            }
            for stage in &plan.spec.stages {
                if active.len() >= usize::try_from(plan.spec.max_concurrent)? {
                    break;
                }
                if running.contains(&stage.id)
                    || !matches!(journal.state.stages.get(&stage.id), Some(StageState::Planned))
                    || !stage
                        .dependencies
                        .iter()
                        .all(|id| journal.state.stages.get(id).is_some_and(StageState::successful))
                {
                    continue;
                }
                let operation = plan.operations.get(&stage.id).context("stage input missing")?.clone();
                if let crate::skills::lifecycle::experiment::ExperimentOperation::DevelopmentProposal {
                    parent_selection: Some(parent),
                    ..
                } = &operation
                {
                    if self.selected_development_proposal(&journal, parent)?.is_none() {
                        workers_cancel.cancel();
                        while active.next().await.is_some() {}
                        return self.inspect_experiment(&plan.spec.id, frozen.digest());
                    }
                }
                let stage_id = stage.id.clone();
                let digest = frozen.digest().to_owned();
                let child = workers_cancel.clone();
                running.insert(stage_id.clone());
                active.push(async move {
                    let outcome = self
                        .run_experiment_stage(
                            config,
                            plan.spec.id.clone(),
                            digest,
                            stage_id.clone(),
                            operation,
                            &child,
                        )
                        .await;
                    (stage_id, outcome)
                });
            }
            let Some((stage, outcome)) = active.next().await else {
                ensure!(
                    journal.state.stages.values().all(StageState::successful),
                    "instruction DAG cannot progress"
                );
                return Ok(journal);
            };
            running.remove(&stage);
            match outcome {
                Ok(result) if result.state.stages.get(&stage).is_some_and(StageState::successful) => {}
                Ok(_) => {
                    workers_cancel.cancel();
                    while active.next().await.is_some() {}
                    return self.inspect_experiment(&plan.spec.id, frozen.digest());
                }
                Err(error) => {
                    workers_cancel.cancel();
                    while active.next().await.is_some() {}
                    return Err(error);
                }
            }
        }
    }
}
