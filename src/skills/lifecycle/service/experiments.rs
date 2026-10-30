//! Immutable experiment admission, bounded stage execution and k1 scheduling.
//! Existing accounting/cleanup owners remain authoritative; no publication grant.
mod development_proposals;
mod guarded_proposer;
mod operations;
mod pins;
mod probes;
mod reconciliation;
mod runner;
mod selection;
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
        draft: crate::skills::lifecycle::experiment::ExperimentDraft,
        config: &Config,
    ) -> Result<FrozenExperiment> {
        self.require_evaluation("planning an instruction experiment")?;
        let created_at = Utc::now();
        let duration = chrono::Duration::try_milliseconds(i64::try_from(draft.elapsed_ms)?)
            .context("experiment duration is out of range")?;
        let spec = ExperimentSpec {
            schema_version: crate::skills::lifecycle::experiment::EXPERIMENT_SCHEMA_VERSION,
            id: draft.id,
            owner_scope: self.owner_scope().clone(),
            pins: self.experiment_pins(&draft.skill_id, config, draft.verification_split)?,
            resources: draft.resources,
            elapsed_ms: draft.elapsed_ms,
            max_concurrent: draft.max_concurrent,
            created_at,
            expires_at: created_at
                .checked_add_signed(duration)
                .context("experiment expiry is out of range")?,
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
        Ok(spec.freeze()?)
    }
    /// Resolves actual owner/baseline/suite/model/runtime pins without a model
    /// send. The caller explicitly freezes the returned spec before admission.
    #[cfg(test)]
    pub(crate) fn plan_experiment(&self, mut spec: ExperimentSpec, config: &Config) -> Result<FrozenExperiment> {
        self.require_evaluation("planning an instruction experiment")?;
        spec.owner_scope = self.owner_scope().clone();
        spec.pins = self.experiment_pins(&spec.pins.skill_id, config, spec.pins.verification_split)?;
        Ok(spec.freeze()?)
    }
    pub(crate) fn admit_experiment(&self, spec: ExperimentSpec, config: &Config) -> Result<ExperimentJournal> {
        self.require_evaluation("admitting an instruction experiment")?;
        let plan = spec.freeze()?;
        self.check_experiment_pins(&plan, config)?;
        Ok(self.store.admit_experiment(&plan, Utc::now())?)
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
    fn experiment_append(
        &self,
        old: &ExperimentJournal,
        event_id: &str,
        event: ExperimentEvent,
    ) -> Result<ExperimentJournal> {
        let mut current = old.clone();
        // A reservation uses a unique writer nonce and may never be retried
        // as idempotent admission. Its sole owner may retry dispatch CAS when
        // only another stage changed the head; recovery/cancel still refuses it.
        let retryable = !matches!(event, ExperimentEvent::Reserved { .. });
        for _ in 0..4 {
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
                Err(crate::skills::lifecycle::experiment::ExperimentError::Conflict) if retryable => {
                    current = self.inspect_experiment(&old.plan.spec().id, old.plan.digest())?;
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
        ensure!(
            plan.spec().pins
                == self.experiment_pins(&plan.spec().pins.skill_id, config, plan.spec().pins.verification_split)?,
            "experiment runtime, baseline or policy pins changed"
        );
        Ok(())
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
        let owner = current_tracker().context("experiment stage has no cleanup owner")?;
        let child = cancel.child_token();
        let _cancel_on_drop = child.clone().drop_guard();
        let ticket = owner.register(child.clone())?;
        let service = Arc::clone(self);
        let config = config.clone();
        tokio::spawn(async move {
            let outcome = owner
                .scope(service.run_experiment_stage_owned(config, id, digest, stage, operation, child))
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
