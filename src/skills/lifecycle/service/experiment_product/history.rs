//! Read existing authorities; do not mint publication/adoption evidence.
use super::{Config, ProductResult, SkillsLifecycleService};
use crate::skills::lifecycle::experiment::{ExperimentJournal, ExperimentStageKind, LessonQueryIntent};
use crate::skills::lifecycle::*;

impl SkillsLifecycleService {
    pub(super) fn check_product_config(&self, config: &Config) -> ProductResult<()> {
        self.require_enabled()?;
        if !config.skills_lifecycle.enabled {
            return Err(ExperimentProductError::refused(
                ExperimentProductFailureKind::Disabled,
                "the current lifecycle configuration is disabled",
            ));
        }
        let owner = config.skills_lifecycle.owner().map_err(anyhow::Error::new)?;
        let workspace = config.workspace_dir.canonicalize().map_err(anyhow::Error::new)?;
        if owner.as_ref() != Some(self.owner_scope())
            || self.store.root().parent().and_then(std::path::Path::parent) != Some(workspace.as_path())
        {
            return Err(ExperimentProductError::refused(
                ExperimentProductFailureKind::WrongOwner,
                "the product configuration does not name this existing owner workspace",
            ));
        }
        let settings = config.skills_lifecycle.to_settings().map_err(anyhow::Error::new)?;
        if settings.mode != self.settings.mode
            || settings.suites != self.settings.suites
            || serde_json::to_value(&settings.gate_policy).map_err(anyhow::Error::new)?
                != serde_json::to_value(&self.settings.gate_policy).map_err(anyhow::Error::new)?
            || serde_json::to_value(&settings.limits).map_err(anyhow::Error::new)?
                != serde_json::to_value(&self.settings.limits).map_err(anyhow::Error::new)?
        {
            return Err(ExperimentProductError::refused(
                ExperimentProductFailureKind::Stale,
                "the current lifecycle settings changed after this service was built",
            ));
        }
        Ok(())
    }

    pub(super) fn resolve_product_final(
        &self,
        journal: &ExperimentJournal,
    ) -> ProductResult<Option<ExperimentFinalExpectation>> {
        let stages: Vec<_> = journal
            .plan
            .spec()
            .stages
            .iter()
            .filter(|stage| stage.kind == ExperimentStageKind::GateReview)
            .collect();
        if stages.is_empty() {
            return Ok(None);
        }
        if stages.len() != 1 {
            return Err(ExperimentProductError::refused(
                ExperimentProductFailureKind::Unsupported,
                "product publication requires one unique final gate",
            ));
        }
        let stage = stages.first().copied().ok_or_else(|| {
            ExperimentProductError::refused(
                ExperimentProductFailureKind::EvidenceUnavailable,
                "final gate stage is missing",
            )
        })?;
        if !journal
            .state
            .stages
            .get(&stage.id)
            .is_some_and(|state| state.successful())
        {
            return Ok(None);
        }
        let handle = ExperimentProductHandle {
            experiment_id: journal.plan.spec().id.clone(),
            plan_sha256: journal.plan.digest().into(),
        };
        let expected = self.store.resolve_experiment_final(&handle, &stage.id)?;
        self.verify_product_final(&expected)?;
        Ok(Some(expected))
    }

    pub(super) fn verify_product_final(&self, expected: &ExperimentFinalExpectation) -> ProductResult<()> {
        self.require_enabled()?;
        let actual = self
            .store
            .resolve_experiment_final(&expected.handle, &expected.gate_stage)?;
        if actual != *expected {
            return Err(ExperimentProductError::refused(
                ExperimentProductFailureKind::Stale,
                "caller final pins do not match the unique durable experiment result",
            ));
        }
        // Borrow the existing historical witness verifier, including original
        // physical/window evidence. This is not current gate authorization.
        self.verify_gate_review_witness(
            &expected.handle.experiment_id,
            &expected.handle.plan_sha256,
            &crate::skills::lifecycle::GateReviewPin {
                stage: expected.gate_stage.clone(),
                witness_sha256: expected.gate_witness_sha256.clone(),
                gate_id: expected.gate_id.clone(),
                gate_sha256: expected.gate_sha256.clone(),
            },
        )?;
        Ok(())
    }

    pub(super) fn verify_product_adoption_reference(
        &self,
        handle: &ExperimentProductHandle,
        reference: &LessonRef,
    ) -> ProductResult<()> {
        let journal = self.inspect_experiment(&handle.experiment_id, &handle.plan_sha256)?;
        let status = self.store.product_adoption_status(reference)?;
        if status.handle != *handle
            || status.skill_id != journal.plan.spec().pins.skill_id
            || status.owner_scope != self.owner_scope().as_str()
        {
            return Err(ExperimentProductError::refused(
                ExperimentProductFailureKind::WrongOwner,
                "adoption reference is not this exact owner/experiment/skill",
            ));
        }
        Ok(())
    }

    pub(super) fn product_lesson_query(
        &self,
        request: &ExperimentLessonQueryRequest,
        config: &Config,
    ) -> ProductResult<ExperimentLessonQueryResult> {
        let journal = self.inspect_experiment(&request.handle.experiment_id, &request.handle.plan_sha256)?;
        if request.lifetime_ms == 0 || request.lifetime_ms > journal.plan.spec().elapsed_ms {
            return Err(ExperimentProductError::refused(
                ExperimentProductFailureKind::InvalidInput,
                "query lifetime must fit the declared bounded experiment duration",
            ));
        }
        // Query against the ACTUAL current target baseline/model/policy, not
        // invented compatibility or the old experiment's now-stale baseline.
        let origin = &journal.plan.spec().pins;
        let pins = self.experiment_pins(
            &origin.skill_id,
            config,
            origin.verification_split,
            origin.holdout_split_digest.is_some(),
        )?;
        let now = chrono::Utc::now();
        let duration =
            chrono::Duration::try_milliseconds(i64::try_from(request.lifetime_ms).map_err(anyhow::Error::new)?)
                .ok_or_else(|| {
                    ExperimentProductError::refused(
                        ExperimentProductFailureKind::InvalidInput,
                        "query duration is out of range",
                    )
                })?;
        let expires = now.checked_add_signed(duration).ok_or_else(|| {
            ExperimentProductError::refused(ExperimentProductFailureKind::InvalidInput, "query expiry overflow")
        })?;
        let snapshot = self.resolve_lesson_query(
            &LessonQueryIntent {
                task_family: request.task_family.clone(),
            },
            &pins,
            now,
            expires,
        )?;
        Ok(ExperimentLessonQueryResult {
            owner_scope: self.owner_scope().to_string(),
            task_family: snapshot.task_family,
            query_sha256: snapshot.query_sha256,
            index_sha256: snapshot.index_sha256,
            references: snapshot.entries.into_iter().map(|entry| entry.reference).collect(),
            expires_at: snapshot.expires_at,
            guidance_delivery_or_utility_proved: false,
        })
    }
}
