//! Scope optional verified guidance to one final development-editor call.
//! User text is already rendered, bounded and proposal-pinned by the existing
//! preparation owner. This adapter never appends or substitutes prompt data.
use crate::skills::lifecycle::SkillsLifecycleService;
use crate::skills::lifecycle::experiment::{ExperimentOperation, FrozenExperiment};
use crate::skills::lifecycle::{ModelBinding, ModelCallError, ModelReply, ProposerModel, TallyCounters};
use async_trait::async_trait;
use std::sync::Arc;

pub(super) struct LessonContextProposer<'a> {
    pub(super) service: &'a SkillsLifecycleService,
    pub(super) inner: &'a dyn ProposerModel,
    pub(super) plan: &'a FrozenExperiment,
    pub(super) stage: &'a str,
    pub(super) operation: &'a ExperimentOperation,
}

#[async_trait]
impl ProposerModel for LessonContextProposer<'_> {
    fn binding(&self) -> &ModelBinding {
        self.inner.binding()
    }

    async fn complete(
        &self,
        system: &str,
        user: &str,
        tally: Arc<TallyCounters>,
    ) -> Result<ModelReply, ModelCallError> {
        if !matches!(self.operation, ExperimentOperation::DevelopmentProposal { .. }) {
            return Err(ModelCallError::BudgetDenied);
        }
        // The factory resolves explicit frozen intent. Invalid Some is an
        // error, never None or replacement guidance. The legitimate None path
        // preserves the original complete call without a delivery scope.
        let prepared = self
            .service
            .prepare_lesson_context(self.plan, self.stage, self.operation, user)
            .await
            .map_err(|_| ModelCallError::BudgetDenied)?;
        match prepared {
            Some(prepared) => {
                crate::runtime::lesson_delivery::scope(prepared, self.inner.complete(system, user, tally)).await
            }
            None => self.inner.complete(system, user, tally).await,
        }
    }
}
