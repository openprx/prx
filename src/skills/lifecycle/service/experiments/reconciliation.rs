//! Read-only physical observations never turn missing evidence into success.
use super::{Config, ExperimentJournal, ExperimentStageRuntime, Result, SkillsLifecycleService, StageState, ensure};
impl SkillsLifecycleService {
    pub(crate) fn experiment_physical_observations(
        &self,
        journal: &ExperimentJournal,
        config: &Config,
    ) -> Result<serde_json::Value> {
        ensure!(
            journal.plan.spec().owner_scope == *self.owner_scope(),
            "experiment owner mismatch"
        );
        let key = crate::decision::DecisionKey::load_existing(&config.workspace_dir)?;
        let tracker = crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), &config.workspace_dir)?;
        let mut stages = serde_json::Map::new();
        for stage in &journal.plan.spec().stages {
            let scope = hex::encode(ExperimentStageRuntime::physical_identity(
                &key,
                journal.plan.digest(),
                &stage.id,
            ));
            let physical = tracker.physical_scope_usage(&scope)?;
            let needs_reconciliation = matches!(
                journal.state.stages.get(&stage.id),
                Some(StageState::Dispatched | StageState::DispatchUnknown)
            ) || physical.unresolved;
            stages.insert(
                stage.id.clone(),
                serde_json::json!({
                    "scope_id":scope, "physical":physical,
                    "needs_reconciliation":needs_reconciliation,
                    "missing_rows_prove_unsent":false,
                    "may_dispatch_again":false,
                }),
            );
        }
        Ok(serde_json::Value::Object(stages))
    }
}
