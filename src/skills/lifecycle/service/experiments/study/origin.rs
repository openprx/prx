//! Resolve original historical instruction publication and optimizer ancestry
//! using the existing gate, publication and physical-accounting owners.
use super::{Config, Context, Result, SkillsLifecycleService, StageState, ensure};
use crate::runtime::experiment_stage::ExperimentStageRuntime;
use crate::skills::lifecycle::experiment::ExperimentResources;
use crate::skills::lifecycle::study::{
    StudyExperimentRef, StudyOptimizerPin, StudyOriginPin, StudyPhysicalObservation, StudyRevisionPin, digest,
};
use crate::skills::lifecycle::{ProposalStatus, SkillId, TransactionId, TransactionKind, TransactionState};
use std::collections::{BTreeMap, BTreeSet};

impl SkillsLifecycleService {
    pub(super) fn resolve_study_origin(
        &self,
        source: &StudyExperimentRef,
        gate_stage: &str,
        skill: &SkillId,
    ) -> Result<StudyOriginPin> {
        let journal = self.inspect_experiment(&source.id, &source.sha256)?;
        ensure!(
            journal.plan.spec().pins.skill_id == *skill && self.experiment_qualified(&journal)?,
            "study origin is not a qualified instruction experiment"
        );
        let history = self.verify_gate_review_history(&journal, gate_stage)?;
        let subject = history.subject();
        let proposal = self
            .store
            .read_proposal(&subject.proposal_id)?
            .context("study origin proposal is missing")?;
        ensure!(
            proposal.status == ProposalStatus::Committed && proposal.skill_id == *skill,
            "study treatment was not actually committed"
        );
        let publication = proposal
            .publication
            .as_ref()
            .context("study origin lacks actual publication")?;
        let authorization = proposal
            .authorization
            .as_ref()
            .context("study publication has no original authorization")?;
        let pin = history.pin();
        ensure!(
            authorization.gate_id == pin.gate_id
                && authorization.gate_digest == pin.gate_sha256
                && proposal.candidate_revision.as_ref() == Some(&subject.candidate_revision)
                && publication.revision_id == subject.candidate_revision
                && proposal.evaluation_epoch == subject.evaluation_epoch
                && proposal.expected_generation == subject.expected_generation,
            "study origin differs from its original exact gate/publication"
        );
        let tx = TransactionId::parse(&publication.transaction_id)?;
        ensure!(
            tx == crate::skills::lifecycle::authorize::publication_tx(&proposal.proposal_id, authorization.attempt)?,
            "study publication attempt mismatch"
        );
        let entry = self
            .store
            .journal_entry(&tx)?
            .context("study publication journal missing")?;
        let baseline_revision = subject
            .baseline_revision
            .as_ref()
            .context("initial study profile requires an instruction baseline")?;
        ensure!(
            entry.owner_scope == *self.owner_scope()
                && entry.kind == TransactionKind::Publish
                && entry.state == TransactionState::Committed
                && entry.skill_id == *skill
                && entry.candidate_revision_id == subject.candidate_revision
                && entry.candidate_record_digest == authorization.candidate_record_digest
                && entry.predecessor_revision_id.as_ref() == Some(baseline_revision)
                && entry.expected_generation == subject.expected_generation
                && entry.new_generation == publication.generation
                && entry.authorization_digest.as_deref()
                    == Some(
                        crate::skills::lifecycle::authorize::authorization_pin(&proposal.proposal_id, authorization)
                            .as_str()
                    ),
            "study treatment has no matching actual committed transaction"
        );
        let baseline_record = &journal.plan.spec().pins.baseline_record_digest;
        self.store
            .read_revision(skill, baseline_revision, Some(baseline_record))?;
        self.store
            .read_revision(skill, &subject.candidate_revision, Some(&entry.candidate_record_digest))?;
        Ok(StudyOriginPin {
            experiment: source.clone(),
            gate_stage: gate_stage.into(),
            gate_pin_sha256: digest(b"prx-v01-origin-gate-pin-v1\0", &pin)?,
            proposal_id: subject.proposal_id.clone(),
            publication_tx: tx,
            publication_sha256: digest(b"prx-v01-origin-publication-v1\0", &entry)?,
            baseline: StudyRevisionPin {
                revision: baseline_revision.clone(),
                record_sha256: baseline_record.clone(),
            },
            h2: StudyRevisionPin {
                revision: subject.candidate_revision.clone(),
                record_sha256: entry.candidate_record_digest,
            },
        })
    }

    pub(super) fn study_physical(
        &self,
        config: &Config,
        experiment: &str,
        stage: &str,
    ) -> Result<StudyPhysicalObservation> {
        let workspace = self
            .store
            .root()
            .parent()
            .and_then(std::path::Path::parent)
            .context("study workspace unavailable")?;
        ensure!(
            workspace == config.workspace_dir.canonicalize()?,
            "study accounting workspace differs from its actual owner"
        );
        let key = crate::decision::DecisionKey::load_existing(workspace)?;
        let scope_id = hex::encode(ExperimentStageRuntime::physical_identity(&key, experiment, stage));
        let usage = crate::cost::tracker::CostTracker::physical_scope_usage_read_only(workspace, &scope_id)?;
        Ok(StudyPhysicalObservation {
            scope_id,
            attempt_ids: usage.attempt_ids.into_iter().collect(),
            calls: usage.calls,
            settled_calls: usage.settled_calls,
            tokens: usage.tokens,
            micro_usd: usage.micro_usd,
            unresolved: usage.unresolved,
            models: usage.models,
        })
    }

    /// Traversal is bounded and follows actual MAC-verified pool ancestry, not
    /// only caller-disclosed roots. Repeated ancestors count once.
    pub(super) fn resolve_study_optimizers(
        &self,
        origin: &StudyExperimentRef,
        extras: &[StudyExperimentRef],
        config: &Config,
    ) -> Result<(Vec<StudyOptimizerPin>, Vec<String>)> {
        ensure!(extras.len() <= 3, "optimizer ancestry exceeds its supported bound");
        let mut pending = vec![origin.clone()];
        pending.extend_from_slice(extras);
        let mut seen = BTreeMap::new();
        let mut roots = BTreeSet::new();
        let mut all_attempts = BTreeSet::new();
        let mut examined = 0_usize;
        while let Some(reference) = pending.pop() {
            examined = examined.checked_add(1).context("optimizer ancestry overflow")?;
            ensure!(examined <= 16, "optimizer ancestry enumeration bound exhausted");
            if let Some(old) = seen.get(reference.id.as_str()) {
                let old: &StudyOptimizerPin = old;
                ensure!(
                    old.experiment == reference,
                    "optimizer identity has conflicting digest pins"
                );
                continue;
            }
            ensure!(
                seen.len() < 4,
                "complete optimizer ancestry exceeds four immutable roots"
            );
            let journal = self.inspect_experiment(&reference.id, &reference.sha256)?;
            ensure!(
                journal
                    .plan
                    .spec()
                    .stages
                    .iter()
                    .all(|stage| stage.kind != crate::skills::lifecycle::experiment::ExperimentStageKind::StudyTrial),
                "study/calibration cost must not be relabelled as optimizer cost"
            );
            let suite = self.store.read_suite(&journal.plan.spec().pins.suite_digest)?;
            roots.extend(suite.lesson_root_digests());
            if let Some(snapshot) = &journal.plan.spec().pins.lesson_snapshot {
                let (source_roots, ancestors) = self.store.study_snapshot_provenance(snapshot)?;
                roots.extend(source_roots);
                for (id, sha256) in ancestors {
                    pending.push(StudyExperimentRef {
                        id: crate::skills::lifecycle::experiment::ExperimentId::parse(&id)?,
                        sha256,
                    });
                }
            }
            let mut actual = ExperimentResources::default();
            let mut physical_rows = BTreeMap::new();
            let mut physical_ids = BTreeSet::new();
            for stage in &journal.plan.spec().stages {
                let Some(StageState::Settled {
                    usage,
                    cleanup_confirmed: true,
                    ..
                }) = journal.state.stages.get(&stage.id)
                else {
                    anyhow::bail!("optimizer ancestry is incomplete or has uncertain cleanup/accounting");
                };
                let physical = self.study_physical(config, journal.plan.digest(), &stage.id)?;
                ensure!(
                    !physical.unresolved
                        && physical.calls == physical.settled_calls
                        && physical.calls == usage.calls
                        && physical.tokens == usage.tokens
                        && physical.micro_usd == usage.micro_usd,
                    "optimizer settlement differs from its original physical owner"
                );
                for id in &physical.attempt_ids {
                    ensure!(
                        physical_ids.insert(id.clone()),
                        "optimizer physical attempt was counted twice"
                    );
                }
                actual = actual.checked_add(*usage)?;
                physical_rows.insert(stage.id.clone(), physical);
            }
            ensure!(
                !journal.state.cancelled && actual.within(journal.plan.spec().resources),
                "optimizer ancestry did not close within its admitted budget"
            );
            for attempt in &physical_ids {
                ensure!(
                    all_attempts.insert(attempt.clone()),
                    "physical cost appears in multiple optimizer roots"
                );
            }
            let pin = StudyOptimizerPin {
                experiment: reference,
                head_sha256: journal.head_sha256,
                actual,
                reserved_liability: ExperimentResources::default(),
                pool_snapshot_sha256: digest(b"prx-v01-frozen-pool-v1\0", &journal.plan.spec().pins.lesson_snapshot)?,
                physical_observations_sha256: digest(b"prx-v01-optimizer-physical-v1\0", &physical_rows)?,
                physical_attempt_ids: physical_ids.into_iter().collect(),
            };
            seen.insert(pin.experiment.id.as_str().to_string(), pin);
        }
        ensure!(seen.contains_key(origin.id.as_str()), "optimizer origin missing");
        Ok((seen.into_values().collect(), roots.into_iter().collect()))
    }
}
