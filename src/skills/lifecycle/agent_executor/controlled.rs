//! Native case data is disjoint from its runtime state, keys and lifecycle.
use super::{Config, EvaluationError};
use crate::runtime::experiment_stage::ExperimentStageRuntime;
use anyhow::{Result, ensure};
use std::path::Path;
use std::sync::{Arc, Weak};

pub(super) struct ControlledCasePolicy {
    stage: Weak<ExperimentStageRuntime>,
    stage_id: String,
}
impl ControlledCasePolicy {
    pub(super) fn capture(config: &Config, accounting: &Path) -> Result<Self, EvaluationError> {
        let result = (|| -> Result<Self> {
            crate::runtime::h2_entry::ControlledH2Profile::validate_trial_config(config)?;
            ensure!(
                config.workspace_dir.canonicalize()? == accounting.canonicalize()?,
                "controlled accounting owner differs from admitted workspace"
            );
            ensure!(
                crate::runtime::turn_cleanup::current_tracker().is_some(),
                "controlled case has no cleanup owner"
            );
            let stage = crate::runtime::experiment_stage::current()
                .ok_or_else(|| anyhow::anyhow!("controlled case has no admitted stage"))?;
            stage.verify_case_template(config)?;
            ensure!(
                matches!(
                    stage.kind(),
                    crate::skills::lifecycle::experiment::ExperimentStageKind::SolverTrial
                        | crate::skills::lifecycle::experiment::ExperimentStageKind::StudyTrial
                ),
                "controlled cases require the original admitted SolverTrial or StudyTrial owner"
            );
            Ok(Self {
                stage: Arc::downgrade(&stage),
                stage_id: stage.scope_id(),
            })
        })();
        result.map_err(|e| EvaluationError::Executor(e.to_string()))
    }
    pub(super) fn verify(&self) -> Result<(), EvaluationError> {
        let result = (|| -> Result<()> {
            let expected = self
                .stage
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("controlled stage owner ended"))?;
            let actual = crate::runtime::experiment_stage::current()
                .ok_or_else(|| anyhow::anyhow!("controlled case lost physical owner"))?;
            ensure!(
                Arc::ptr_eq(&expected, &actual) && actual.scope_id() == self.stage_id,
                "controlled case moved to another physical stage"
            );
            ensure!(
                crate::runtime::turn_cleanup::current_tracker().is_some(),
                "controlled case lost cleanup owner"
            );
            actual.ensure_active()
        })();
        result.map_err(|e| EvaluationError::Executor(e.to_string()))
    }
}
