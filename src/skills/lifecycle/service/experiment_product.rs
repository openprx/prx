//! Explicit local product flow over the existing lifecycle authorities.
//! No frontend origin, approval, provider, publication or accounting owner is created here.
mod history;
mod status;

use super::SkillsLifecycleService;
use crate::config::Config;
use crate::skills::lifecycle::experiment::{InstructionCycleDraft, InstructionExperimentPlan};
use crate::skills::lifecycle::*;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

type ProductResult<T> = Result<T, ExperimentProductError>;

impl SkillsLifecycleService {
    pub fn plan_product(
        &self,
        draft: InstructionCycleDraft,
        config: &Config,
    ) -> ProductResult<InstructionExperimentPlan> {
        self.check_product_config(config)?;
        Ok(self.plan_instruction_cycle(draft, config)?)
    }

    pub fn admit_product(
        &self,
        plan: &InstructionExperimentPlan,
        config: &Config,
    ) -> ProductResult<ExperimentProductSnapshot> {
        self.check_product_config(config)?;
        plan.validate()?;
        let journal = self.admit_experiment(plan.spec.clone(), config)?;
        self.product_snapshot(&journal, config)
    }

    pub async fn run_product(
        self: &Arc<Self>,
        plan: &InstructionExperimentPlan,
        config: &Config,
        cancel: &CancellationToken,
    ) -> ProductResult<ExperimentProductSnapshot> {
        self.check_product_config(config)?;
        let handle = ExperimentProductHandle::of(plan)?;
        // The runtime, not a dropped CLI waiter, retains owned stage work.
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(
            crate::runtime::turn_cleanup::current_tracker().as_ref(),
            cancel.clone(),
        )?;
        match cleanup.run(self.run_instruction_plan(config, plan, cancel)).await {
            Ok(journal) => self.product_snapshot(&journal, config),
            Err(error) => Err(self.product_failure_with_snapshot(error.into(), &handle, config)),
        }
    }

    pub fn inspect_product(
        &self,
        handle: &ExperimentProductHandle,
        config: &Config,
    ) -> ProductResult<ExperimentProductSnapshot> {
        self.check_product_config(config)?;
        let journal = self.inspect_experiment(&handle.experiment_id, &handle.plan_sha256)?;
        self.product_snapshot(&journal, config)
    }

    pub fn cancel_product(
        &self,
        handle: &ExperimentProductHandle,
        config: &Config,
    ) -> ProductResult<ExperimentProductSnapshot> {
        self.check_product_config(config)?;
        let journal = self.cancel_product_experiment(&handle.experiment_id, &handle.plan_sha256)?;
        self.product_snapshot(&journal, config)
    }

    pub fn recover_product(
        &self,
        handle: &ExperimentProductHandle,
        config: &Config,
    ) -> ProductResult<ExperimentProductSnapshot> {
        self.check_product_config(config)?;
        let journal = self.recover_experiment(&handle.experiment_id, &handle.plan_sha256)?;
        self.product_snapshot(&journal, config)
    }

    pub fn review_final_product(&self, expected: &ExperimentFinalExpectation) -> ProductResult<SourceReviewOutcome> {
        self.require_enabled()?;
        self.verify_product_final(expected)?;
        Ok(self.store.review_experiment_final(expected, "PRXOperator")?)
    }

    pub fn publish_final_product(
        &self,
        request: &ExperimentPublishRequest,
    ) -> ProductResult<ExperimentProductPublication> {
        self.require_enabled()?;
        let expected = &request.expected;
        // This resolves the durable final before the original owners repeat
        // exact comparisons under their existing proposal locks.
        self.verify_product_final(expected)?;
        let proposal = self.proposal_of(&expected.proposal_id)?;
        // Historical exact replay makes no active effect and does not borrow
        // today's gate expiry, active generation or publication mode.
        let suite = if crate::skills::lifecycle::product::recorded_publication_status(proposal.status)
            && proposal.publication.is_some()
        {
            expected.suite_sha256.clone()
        } else {
            self.require_publication("publishing an exact experiment result")?;
            self.registered_suite(&expected.skill_id)?.to_string()
        };
        if proposal.status == ProposalStatus::Eligible {
            self.store
                .authorize_experiment_publication(expected, &self.settings.gate_policy, &suite)?;
        }
        let outcome = self
            .store
            .publish_experiment_authorized(expected, &self.settings.gate_policy, &suite)?;
        Ok(ExperimentProductPublication {
            expected: expected.clone(),
            transaction: outcome.transaction.entry,
            replayed: outcome.transaction.replayed,
            proposal_committed: outcome.proposal.status == ProposalStatus::Committed,
        })
    }

    pub fn annotate_product(&self, request: &ExperimentAnnotationRequest, config: &Config) -> ProductResult<LessonRef> {
        self.check_product_config(config)?;
        self.verify_product_final(&request.expected)?;
        let expected = &request.expected;
        // record_adoption alone owns historical proof, the publication join and
        // annotation. No publication/recovery is performed on its behalf.
        self.record_adoption(
            &RecordAdoptionRequest {
                proposal_id: expected.proposal_id.clone(),
                publication_tx: request.publication_tx.clone(),
                experiment_id: expected.handle.experiment_id.to_string(),
                experiment_sha256: expected.handle.plan_sha256.clone(),
                gate_stage: expected.gate_stage.clone(),
                gate_witness_sha256: expected.gate_witness_sha256.clone(),
                gate_id: expected.gate_id.clone(),
                gate_sha256: expected.gate_sha256.clone(),
                task_family: request.task_family.clone(),
                expires_at: request.expires_at,
            },
            config,
        )
        .map_err(|error| {
            let mut failure: ExperimentProductError = error.into();
            failure.kind = ExperimentProductFailureKind::AnnotationFailed;
            failure
        })
    }

    pub fn rollback_product(&self, request: &ExperimentRollbackRequest) -> ProductResult<TransactionOutcome> {
        self.require_publication("rolling back an experiment skill")?;
        let journal = self.inspect_experiment(&request.handle.experiment_id, &request.handle.plan_sha256)?;
        // This is the previous-publication/generation-CAS owner. It never
        // annotates, retires a lesson or republishes the experiment candidate.
        Ok(self.rollback(
            &journal.plan.spec().pins.skill_id,
            &request.prior_revision,
            request.observed_generation,
        )?)
    }

    pub fn set_product_adoption_status(
        &self,
        request: &ExperimentAdoptionStatusRequest,
    ) -> ProductResult<ExperimentAdoptionStatusObservation> {
        self.require_enabled()?;
        // Verification returns and releases its shared/proposal owners before
        // the existing exclusive-store -> skill status writer is entered.
        self.verify_product_adoption_reference(&request.handle, &request.reference)?;
        self.set_adoption_status(&request.reference, request.retired, request.invalidated)?;
        Ok(self.store.product_adoption_status(&request.reference)?)
    }

    pub fn query_product_lessons(
        &self,
        request: &ExperimentLessonQueryRequest,
        config: &Config,
    ) -> ProductResult<ExperimentLessonQueryResult> {
        self.check_product_config(config)?;
        self.product_lesson_query(request, config)
    }
}
