//! Immutable experiment admission, bounded stage execution and instruction scheduling.
//! Existing accounting/cleanup owners remain authoritative; no publication grant.
mod development_proposals;
mod gate_review;
pub(in crate::skills::lifecycle) use gate_review::{GateReviewPin, VerifiedGateReviewHistory};
mod guarded_proposer;
mod lesson_context;
mod lesson_preparation;
mod operations;
mod pins;
mod probes;
mod reconciliation;
mod runner;
mod selection;
mod study;
use super::SkillsLifecycleService;
use crate::config::Config;
use crate::runtime::{
    experiment_stage::ExperimentStageRuntime,
    turn_cleanup::{CleanupScope, current_tracker},
};
use crate::skills::lifecycle::experiment::{
    ExperimentEvent, ExperimentId, ExperimentJournal, ExperimentOperation, ExperimentSpec, FrozenExperiment, StageState,
};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl SkillsLifecycleService {
    /// Operator drafts contain intent and limits; they never invent pin values.
    pub(crate) fn plan_experiment_draft(
        &self,
        mut draft: crate::skills::lifecycle::experiment::ExperimentDraft,
        config: &Config,
    ) -> Result<FrozenExperiment> {
        self.require_evaluation("planning an instruction experiment")?;
        let created_at = Utc::now();
        let duration = chrono::Duration::try_milliseconds(i64::try_from(draft.elapsed_ms)?)
            .context("experiment duration is out of range")?;
        let expires_at = created_at
            .checked_add_signed(duration)
            .context("experiment expiry is out of range")?;
        let mut pins = self.experiment_pins(&draft.skill_id, config, draft.verification_split, draft.verify_holdout)?;
        if let Some(intent) = &draft.lesson_query {
            pins.lesson_snapshot = Some(self.resolve_lesson_query(intent, &pins, created_at, expires_at)?);
        }
        for stage in &mut draft.stages {
            Self::pin_lesson_operation(pins.lesson_snapshot.as_ref(), &stage.id, &mut stage.operation)?;
        }
        let operations = draft
            .stages
            .iter()
            .map(|stage| (stage.id.clone(), stage.operation.clone()))
            .collect();
        let spec = ExperimentSpec {
            controlled_admission: None,
            schema_version: crate::skills::lifecycle::experiment::EXPERIMENT_SCHEMA_VERSION,
            id: draft.id,
            owner_scope: self.owner_scope().clone(),
            pins,
            resources: draft.resources,
            elapsed_ms: draft.elapsed_ms,
            max_concurrent: draft.max_concurrent,
            created_at,
            expires_at,
            stages: draft
                .stages
                .into_iter()
                .map(|stage| {
                    Ok(crate::skills::lifecycle::experiment::ExperimentStage {
                        id: stage.id,
                        dependencies: stage.dependencies,
                        allocation: stage.allocation,
                        kind: stage.operation.kind(),
                        input_sha256: stage.operation.digest()?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        };
        let spec = self.store.freeze_experiment_admission(spec, config)?.spec().clone();
        let plan = crate::skills::lifecycle::experiment::InstructionExperimentPlan { spec, operations };
        plan.validate()?;
        self.validate_study_instruction_plan(&plan, config)?;
        Ok(plan.spec.freeze()?)
    }
    /// Resolves actual owner/baseline/suite/model/runtime pins without a model
    /// send. The caller explicitly freezes the returned spec before admission.
    #[cfg(test)]
    pub(crate) fn plan_experiment(&self, mut spec: ExperimentSpec, config: &Config) -> Result<FrozenExperiment> {
        self.require_evaluation("planning an instruction experiment")?;
        spec.owner_scope = self.owner_scope().clone();
        spec.pins = self.experiment_pins(
            &spec.pins.skill_id,
            config,
            spec.pins.verification_split,
            spec.pins.holdout_split_digest.is_some(),
        )?;
        Ok(spec.freeze()?)
    }
    pub(crate) fn admit_experiment(&self, spec: ExperimentSpec, config: &Config) -> Result<ExperimentJournal> {
        self.require_evaluation("admitting an instruction experiment")?;
        let plan = spec.freeze()?;
        self.validate_study_spec(plan.spec(), config)?;
        self.check_experiment_pins(&plan, config)?;
        let continuation = self.controlled_experiment_continuation(&plan, config)?;
        let admit = || self.store.admit_experiment(&plan, Utc::now());
        Ok(match continuation {
            Some(continuation) => continuation.sync_scope(admit),
            None => admit(),
        }?)
    }
    pub(crate) fn inspect_experiment(&self, id: &ExperimentId, digest: &str) -> Result<ExperimentJournal> {
        self.require_enabled()?;
        Ok(self.store.read_experiment(id, digest)?)
    }
    /// Cancellation is durable and cannot be used to refresh an allowance.
    /// Each active stage also observes this record before subsequent sends.
    pub(crate) fn cancel_experiment(&self, id: &ExperimentId, digest: &str) -> Result<ExperimentJournal> {
        let journal = self.inspect_experiment(id, digest)?;
        if journal.state.cancelled {
            return Ok(journal);
        }
        self.experiment_append(&journal, "cancel", ExperimentEvent::Cancelled)
    }
    pub(in crate::skills::lifecycle) fn cancel_product_experiment(
        &self,
        id: &ExperimentId,
        digest: &str,
    ) -> Result<ExperimentJournal> {
        let journal = self.inspect_experiment(id, digest)?;
        self.experiment_append(&journal, "product-cancel", ExperimentEvent::Cancelled)
    }
    fn experiment_append(
        &self,
        old: &ExperimentJournal,
        event_id: &str,
        event: ExperimentEvent,
    ) -> Result<ExperimentJournal> {
        let mut current = old.clone();
        // A new reservation can retry a stale head only while its exact stage
        // remains Planned. The unique nonce is never treated as ownership of an
        // already reserved stage. I/O uncertainty is never retried here.
        for _ in 0..16 {
            // Product cancellation observes the same CAS head on every retry.
            // A terminal completed root is historical evidence, not live work.
            if event_id == "product-cancel"
                && matches!(event, ExperimentEvent::Cancelled)
                && (current.state.cancelled
                    || current.state.stages.values().all(|state| {
                        matches!(
                            state,
                            StageState::Settled {
                                cleanup_confirmed: true,
                                ..
                            }
                        )
                    }))
            {
                return Ok(current);
            }
            match self.store.append_experiment(
                &current.plan.spec().id,
                current.plan.digest(),
                current.next_sequence(),
                &current.head_sha256,
                event_id,
                event.clone(),
                Utc::now(),
            ) {
                Ok(journal) => return Ok(journal),
                Err(crate::skills::lifecycle::experiment::ExperimentError::Conflict) => {
                    current = self.inspect_experiment(&old.plan.spec().id, old.plan.digest())?;
                    if let ExperimentEvent::Reserved { stage } = &event {
                        ensure!(
                            !current.state.cancelled
                                && matches!(current.state.stages.get(stage), Some(StageState::Planned)),
                            "experiment reservation ownership was superseded"
                        );
                    }
                    if let ExperimentEvent::Dispatched { stage } = &event {
                        ensure!(
                            !current.state.cancelled
                                && matches!(current.state.stages.get(stage), Some(StageState::Reserved)),
                            "experiment dispatch ownership was superseded"
                        );
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("concurrent experiment journal updates exceeded retry limit")
    }
    fn check_experiment_pins(&self, plan: &FrozenExperiment, config: &Config) -> Result<()> {
        self.require_evaluation("running an instruction experiment")?;
        ensure!(
            plan.spec().owner_scope == *self.owner_scope(),
            "experiment belongs to another owner"
        );
        let mut expected_pins = self.experiment_pins(
            &plan.spec().pins.skill_id,
            config,
            plan.spec().pins.verification_split,
            plan.spec().pins.holdout_split_digest.is_some(),
        )?;
        expected_pins.lesson_snapshot = plan.spec().pins.lesson_snapshot.clone();
        ensure!(
            plan.spec().pins == expected_pins,
            "experiment runtime, baseline or policy pins changed"
        );
        self.check_lesson_pins(plan)?;
        Ok(())
    }
    fn controlled_experiment_continuation(
        &self,
        plan: &FrozenExperiment,
        config: &Config,
    ) -> Result<Option<crate::runtime::h2_entry::RunContinuation>> {
        self.store.verify_experiment_admission(plan)?;
        match (
            plan.spec().controlled_admission.as_ref(),
            crate::runtime::h2_entry::current_run_continuation(),
        ) {
            (None, None) => Ok(None),
            (Some(envelope), Some(current)) => Ok(Some(current.for_admitted_plan(plan, &envelope.pin, config)?)),
            _ => anyhow::bail!("controlled experiment requires its original live source; no legacy fallback"),
        }
    }
    /// Recovery never dispatches work. Dispatched stages become unknown, and
    /// never-dispatched reservations can be cleaned up without consuming spend.
    pub(crate) fn recover_experiment(&self, id: &ExperimentId, digest: &str) -> Result<ExperimentJournal> {
        let mut journal = self.inspect_experiment(id, digest)?;
        let stages: Vec<_> = journal
            .state
            .stages
            .iter()
            .map(|(id, state)| (id.clone(), state.clone()))
            .collect();
        for (stage, state) in stages {
            let event = match state {
                StageState::Reserved => Some(ExperimentEvent::CleanupConfirmed { stage: stage.clone() }),
                StageState::Dispatched => Some(ExperimentEvent::DispatchUnknown { stage: stage.clone() }),
                StageState::Settled {
                    cleanup_confirmed: false,
                    ..
                } => Some(ExperimentEvent::CleanupConfirmed { stage: stage.clone() }),
                _ => None,
            };
            if let Some(event) = event {
                journal = self.experiment_append(&journal, &format!("{stage}-recover"), event)?;
            }
        }
        Ok(journal)
    }
    /// The waiter can be dropped: cancellation reaches the tracked worker,
    /// which retains the final durable stage record and all physical owners.
    pub(crate) async fn run_experiment_stage(
        self: &Arc<Self>,
        config: &Config,
        id: ExperimentId,
        digest: String,
        stage: String,
        operation: ExperimentOperation,
        cancel: &CancellationToken,
    ) -> Result<ExperimentJournal> {
        let admitted = self.inspect_experiment(&id, &digest)?;
        let continuation = self.controlled_experiment_continuation(&admitted.plan, config)?;
        let owner = current_tracker().context("experiment stage has no cleanup owner")?;
        let child = cancel.child_token();
        let _cancel_on_drop = child.clone().drop_guard();
        let ticket = owner.register(child.clone())?;
        let service = Arc::clone(self);
        let config = config.clone();
        tokio::spawn(async move {
            let outcome = crate::runtime::h2_entry::retain_run_continuation(
                continuation,
                owner.scope(service.run_experiment_stage_owned(config, id, digest, stage, operation, child)),
            )
            .await;
            ticket.finish(
                !outcome
                    .as_ref()
                    .is_err_and(|error| error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>()),
            );
            outcome
        })
        .await
        .context("experiment stage worker failed")?
    }
    async fn run_experiment_stage_owned(
        self: Arc<Self>,
        config: Config,
        id: ExperimentId,
        digest: String,
        stage: String,
        operation: ExperimentOperation,
        cancel: CancellationToken,
    ) -> Result<ExperimentJournal> {
        let mut journal = self.inspect_experiment(&id, &digest)?;
        self.controlled_experiment_continuation(&journal.plan, &config)?;
        self.validate_study_spec(journal.plan.spec(), &config)?;
        self.check_experiment_pins(&journal.plan, &config)?;
        ensure!(
            matches!(journal.state.stages.get(&stage), Some(StageState::Planned)),
            "stage was already admitted; inspect or recover it without repeating effects"
        );
        ensure!(!cancel.is_cancelled(), "experiment cancelled before reservation");
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("stage not in frozen plan")?;
        ensure!(
            declared.kind == operation.kind() && declared.input_sha256 == operation.digest()?,
            "stage input or operation does not match frozen plan"
        );
        self.validate_stage_operation(&journal, &stage, &operation)?;
        let cleanup = CleanupScope::new(current_tracker().as_ref(), cancel.clone())?;
        let runtime = cleanup
            .scope_admission(async {
                ExperimentStageRuntime::new(&config, &journal.plan, &stage, cancel.clone(), Arc::clone(&self.store))
            })
            .await?;
        journal = self.experiment_append(
            &journal,
            &format!("reserve-{}", uuid::Uuid::new_v4().simple()),
            ExperimentEvent::Reserved { stage: stage.clone() },
        )?;
        let dispatch = self.experiment_append(
            &journal,
            &format!("{stage}-dispatch"),
            ExperimentEvent::Dispatched { stage: stage.clone() },
        );
        journal = match dispatch {
            Ok(journal) => journal,
            Err(error) => {
                // No operation is started until dispatch returns. A reserved
                // stage is therefore proven unsent. A possibly durable
                // dispatch is retained as unknown by recovery instead.
                let current = self.inspect_experiment(&id, &digest)?;
                if matches!(current.state.stages.get(&stage), Some(StageState::Reserved)) {
                    self.experiment_append(
                        &current,
                        &format!("{stage}-unsent"),
                        ExperimentEvent::CleanupConfirmed { stage: stage.clone() },
                    )?;
                }
                return Err(error);
            }
        };
        let operation_result = cleanup
            .run(runtime.scope(self.execute_stage_operation(&config, &journal, &stage, &operation, &cancel)))
            .await;
        // The nested cleanup is drained before these receipts are re-read.
        let tracker = crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), &config.workspace_dir)?;
        let usage = tracker.physical_scope_usage(&runtime.scope_id())?;
        // Cancellation can race a foreground result. Reload before append;
        // neither late results nor cancellation authorize another reservation.
        journal = self.inspect_experiment(&id, &digest)?;
        let operation_result = match operation_result {
            Ok(evidence) if evidence.pending_gate.is_some() && !usage.unresolved => {
                self.finalize_gate_review_after_cleanup(&journal, runtime.as_ref(), &usage, evidence)
            }
            result => result,
        };
        match operation_result {
            Ok(evidence) if !usage.unresolved => {
                let resources = crate::skills::lifecycle::experiment::ExperimentResources {
                    calls: usage.calls,
                    tokens: usage.tokens,
                    micro_usd: usage.micro_usd,
                    tool_calls: runtime.tool_count(),
                    output_bytes: runtime.output_bytes(),
                    scratch_bytes: runtime.scratch_bytes().max(evidence.scratch_bytes),
                };
                journal = self.experiment_append(
                    &journal,
                    &format!("{stage}-settle"),
                    ExperimentEvent::Settled {
                        stage: stage.clone(),
                        usage: resources,
                        evidence_sha256: evidence.digest,
                        reference: Some(evidence.record_id),
                        successful: evidence.successful,
                    },
                )?;
                self.experiment_append(
                    &journal,
                    &format!("{stage}-cleanup"),
                    ExperimentEvent::CleanupConfirmed { stage },
                )
            }
            result => {
                let error = result
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "physical usage is unresolved".into());
                if !matches!(journal.state.stages.get(&stage), Some(StageState::DispatchUnknown)) {
                    self.experiment_append(
                        &journal,
                        &format!("{stage}-unknown"),
                        ExperimentEvent::DispatchUnknown { stage },
                    )?;
                }
                anyhow::bail!("experiment stage is nonqualifying; durable unknown record retained: {error}")
            }
        }
    }
}
