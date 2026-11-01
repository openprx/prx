//! Resolve optional guidance before freezing and bind the final editor slot.
//! No publication, new budget, model call or alternative lesson authority.
use super::{Context, Result, SkillsLifecycleService, ensure};
use crate::skills::lifecycle::adoption::{LessonLimits, LessonQuery, PreparedLessonContext};
use crate::skills::lifecycle::experiment::{ExperimentOperation, ExperimentPins, FrozenExperiment, LessonQueryIntent};
use chrono::{DateTime, Utc};
use sha2::Digest;
use std::sync::Arc;

impl SkillsLifecycleService {
    pub(super) fn pin_lesson_operation(
        snapshot: Option<&crate::skills::lifecycle::adoption::LessonSnapshot>,
        stage: &str,
        operation: &mut ExperimentOperation,
    ) -> Result<()> {
        if let ExperimentOperation::DevelopmentProposal {
            lesson_context_sha256, ..
        } = operation
        {
            *lesson_context_sha256 = match snapshot {
                Some(snapshot) if !snapshot.entries.is_empty() => {
                    let block = crate::skills::lifecycle::adoption::format_lesson_block(snapshot, stage)?;
                    Some(hex::encode(sha2::Sha256::digest(block.as_bytes())))
                }
                _ => None,
            };
        }
        Ok(())
    }

    pub(in crate::skills::lifecycle) fn resolve_lesson_query(
        &self,
        intent: &LessonQueryIntent,
        pins: &ExperimentPins,
        planned_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<crate::skills::lifecycle::adoption::LessonSnapshot> {
        let suite = self.store.read_suite(&pins.suite_digest)?;
        ensure!(
            !suite.suite().cases.is_empty()
                && suite
                    .suite()
                    .cases
                    .iter()
                    .all(|case| case.task_family == intent.task_family),
            "lesson intent must match the registered task family"
        );
        let compatibility =
            self.store
                .lesson_profile_compatibility(&pins.model, &pins.policy_sha256, &pins.binary_sha256)?;
        Ok(self.store.query_adopted_lessons(&LessonQuery {
            owner_scope: self.owner_scope().clone(),
            task_family: intent.task_family.clone(),
            skill_id: pins.skill_id.clone(),
            baseline_revision: pins.baseline_revision.clone(),
            baseline_record_sha256: pins.baseline_record_digest.clone(),
            compatibility,
            excluded_roots: suite.lesson_root_digests(),
            limits: LessonLimits::default(),
            planned_at,
            expires_at,
        })?)
    }

    pub(super) fn check_lesson_pins(&self, plan: &FrozenExperiment) -> Result<()> {
        let Some(snapshot) = &plan.spec().pins.lesson_snapshot else {
            return Ok(());
        };
        let suite = self.store.read_suite(&plan.spec().pins.suite_digest)?;
        ensure!(
            suite
                .suite()
                .cases
                .iter()
                .all(|case| case.task_family == snapshot.task_family),
            "lesson family differs from the registered suite"
        );
        let compatibility = self.store.lesson_profile_compatibility(
            &plan.spec().pins.model,
            &plan.spec().pins.policy_sha256,
            &plan.spec().pins.binary_sha256,
        )?;
        let _guard = self
            .store
            .lock_store(true, std::time::Duration::from_millis(snapshot.limits.elapsed_ms))?;
        self.store.validate_lesson_snapshot_locked(
            snapshot,
            &plan.spec().pins.skill_id,
            &plan.spec().pins.baseline_revision,
            &plan.spec().pins.baseline_record_digest,
            &compatibility,
            &suite.lesson_root_digests(),
            Utc::now(),
        )?;
        Ok(())
    }

    /// Supported service surface used only by the final development adapter.
    /// None exits before profile inspection, hashing or store I/O.
    pub(in crate::skills::lifecycle) async fn prepare_lesson_context(
        &self,
        plan: &FrozenExperiment,
        stage: &str,
        operation: &ExperimentOperation,
        final_user: &str,
    ) -> Result<Option<Arc<PreparedLessonContext>>> {
        if plan.spec().pins.lesson_snapshot.is_none() {
            return Ok(None);
        }
        ensure!(
            matches!(operation, ExperimentOperation::DevelopmentProposal { .. }),
            "lessons are restricted to development editors"
        );
        let mut expected = operation.clone();
        Self::pin_lesson_operation(plan.spec().pins.lesson_snapshot.as_ref(), stage, &mut expected)?;
        ensure!(
            expected == *operation,
            "lesson block differs from its admitted operation pin"
        );
        let declared = plan
            .spec()
            .stages
            .iter()
            .find(|item| item.id == stage)
            .context("unknown lesson stage")?;
        ensure!(
            declared.input_sha256 == operation.digest()?,
            "lesson editor operation changed"
        );
        let runtime = crate::runtime::experiment_stage::current().context("lesson editor has no actual runtime")?;
        runtime.ensure_active()?;
        let binding = runtime.binding();
        ensure!(
            binding.experiment_sha256 == plan.digest()
                && binding.stage == stage
                && binding.input_sha256 == declared.input_sha256,
            "lesson runtime does not match the frozen editor"
        );
        let store = Arc::clone(&self.store);
        let plan = plan.clone();
        let user = final_user.to_owned();
        let cleanup =
            crate::runtime::turn_cleanup::current_tracker().context("lesson preparation has no cleanup owner")?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let ticket = cleanup.register(cancellation.clone())?;
        // Verification runs on the blocking pool. Re-enter the test clock there
        // so the snapshot budget does not fall back to wall time.
        #[cfg(test)]
        let inherited_clock = crate::causal_tree::logical_clock::capture();
        crate::runtime::blocking::spawn_blocking(move || {
            let run = || -> Result<_> {
                ensure!(!cancellation.is_cancelled(), "lesson preparation cancelled");
                runtime.ensure_active()?;
                let prepared = store.prepare_verified_lesson_context(&plan, binding, &user, Utc::now())?;
                ensure!(!cancellation.is_cancelled(), "lesson preparation cancelled");
                runtime.ensure_active()?;
                Ok(prepared)
            };
            let result = {
                #[cfg(test)]
                {
                    if let Some(clock) = inherited_clock {
                        crate::causal_tree::logical_clock::sync_scope(clock, run)
                    } else {
                        run()
                    }
                }
                #[cfg(not(test))]
                {
                    run()
                }
            };
            // Read-only I/O has joined even when business admission refuses.
            // A panic keeps the existing cleanup owner unconfirmed.
            ticket.finish(true);
            result
        })
        .await?
    }
}
