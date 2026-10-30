//! Deterministic bounded development comparison. Parent choices carry exact
//! private probe evidence; ties retain the baseline or the earlier winning parent.
use super::operations::StageEvidence;
use super::{
    Context, ExperimentEvent, ExperimentJournal, ExperimentOperation, Result, SkillsLifecycleService, StageState,
    ensure,
};
use crate::skills::lifecycle::experiment::{DevelopmentProbeReceipt, ExperimentStageKind};
use crate::skills::lifecycle::{CaseVerdict, EvalSplit};
use sha2::{Digest, Sha256};

struct DevelopmentChoice {
    evidence: StageEvidence,
    selected_probe: Option<DevelopmentProbeReceipt>,
}
impl SkillsLifecycleService {
    pub(super) fn choose_development_candidate(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        probes: &[String],
        parents: &[String],
    ) -> Result<StageEvidence> {
        Ok(self.development_choice(journal, stage, probes, parents, 0)?.evidence)
    }

    fn development_choice(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        probes: &[String],
        parents: &[String],
        depth: usize,
    ) -> Result<DevelopmentChoice> {
        ensure!(
            depth <= 2 && parents.len() <= 2,
            "development selection exceeds bounded refinement depth"
        );
        ensure!(
            (1..=4).contains(&probes.len()) && probes.len() + parents.len() >= 2,
            "selection needs one baseline and bounded candidate evidence"
        );
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("selection stage missing")?;
        let dependencies: Vec<_> = probes.iter().chain(parents).cloned().collect();
        ensure!(
            declared.dependencies == dependencies,
            "selection dependencies differ from comparison order"
        );
        let operation = ExperimentOperation::DevelopmentSelection {
            probe_stages: probes.to_vec(),
            parent_selections: parents.to_vec(),
        };
        ensure!(
            declared.kind == ExperimentStageKind::Selection && declared.input_sha256 == operation.digest()?,
            "selection inputs changed"
        );
        let suite = self.store.read_suite(&journal.plan.spec().pins.suite_digest)?;
        let mut records = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut evidence = Vec::new();
        for name in probes {
            ensure!(
                seen.insert(name),
                "selection probes must be distinct direct dependencies"
            );
            ensure!(
                journal.state.stages.get(name).is_some_and(StageState::successful),
                "selection probe is not cleaned up"
            );
            let digest = journal
                .entries
                .iter()
                .find_map(|entry| match &entry.event {
                    ExperimentEvent::Settled {
                        stage,
                        reference: Some(reference),
                        evidence_sha256,
                        successful: true,
                        ..
                    } if stage == name && reference == &format!("probe:{name}") => Some(evidence_sha256),
                    _ => None,
                })
                .context("selection lacks pinned probe evidence")?;
            let record = self
                .store
                .read_development_probe(&journal.plan.spec().id, name, digest)?;
            let probe = journal
                .plan
                .spec()
                .stages
                .iter()
                .find(|value| value.id == *name)
                .context("probe stage missing")?;
            ensure!(
                record.binding.experiment_sha256 == journal.plan.digest()
                    && record.binding.input_sha256 == probe.input_sha256
                    && record.suite_digest == suite.digest()
                    && record.complete()
                    && crate::skills::lifecycle::experiment::probe_within_budget(&record, &suite),
                "selection probe identity, completeness or budget is invalid"
            );
            evidence.push((name, digest));
            records.push(record);
        }
        let baselines: Vec<_> = records
            .iter()
            .filter(|record| record.proposal_stage.is_none())
            .collect();
        ensure!(baselines.len() == 1, "selection requires exactly one baseline probe");
        let baseline = (*baselines.first().context("baseline missing")?).clone();
        ensure!(
            baseline.revision == journal.plan.spec().pins.baseline_revision,
            "selection baseline changed"
        );
        let mut candidates = Vec::new();
        let mut parent_evidence = Vec::new();
        for parent in parents {
            ensure!(seen.insert(parent), "selection parent must be distinct");
            let choice = self.verified_development_choice(journal, parent, depth + 1)?;
            parent_evidence.push((parent, choice.evidence.digest));
            if let Some(record) = choice.selected_probe {
                candidates.push(record);
            }
        }
        // Parent first: an equal new candidate cannot replace an earlier winner.
        candidates.extend(records.into_iter().filter(|record| record.proposal_stage.is_some()));
        let expected: Vec<_> = suite
            .cases_of(EvalSplit::Development)
            .flat_map(|case| (0..suite.suite().trials).map(move |trial| (&case.case_id, trial)))
            .collect();
        let case_passes = |record: &DevelopmentProbeReceipt, id: &str| {
            let runs: Vec<_> = record.cases.iter().filter(|case| case.case_id == id).collect();
            runs.len() == usize::try_from(suite.suite().trials).unwrap_or(usize::MAX)
                && runs.iter().all(|case| case.verdict == CaseVerdict::Pass)
        };
        let score = |record: &DevelopmentProbeReceipt| {
            suite
                .cases_of(EvalSplit::Development)
                .filter(|case| case_passes(record, &case.case_id))
                .count()
        };
        let mut best = score(&baseline);
        let mut selected_probe = None;
        for record in std::iter::once(&baseline).chain(candidates.iter()) {
            ensure!(
                record
                    .cases
                    .iter()
                    .map(|case| (&case.case_id, case.trial))
                    .collect::<Vec<_>>()
                    == expected,
                "selection samples differ"
            );
            if let Some(proposal_stage) = &record.proposal_stage {
                let proposal = self.experiment_proposal(journal, proposal_stage)?;
                ensure!(
                    proposal.candidate_revision.as_ref() == Some(&record.revision),
                    "probe candidate binding changed"
                );
                let mut gains = 0;
                let mut regressions = 0;
                let mut critical_failure = false;
                for case in suite.cases_of(EvalSplit::Development) {
                    let candidate = case_passes(record, &case.case_id);
                    let base = case_passes(&baseline, &case.case_id);
                    gains += usize::from(candidate && !base);
                    regressions += usize::from(!candidate && base);
                    critical_failure |= !candidate && case.critical;
                }
                if !critical_failure
                    && regressions <= usize::try_from(suite.suite().allowed_regressions)?
                    && gains >= usize::try_from(suite.suite().min_gain_cases)?
                    && score(record) > best
                {
                    best = score(record);
                    selected_probe = Some(record.clone());
                }
            }
        }
        let reference = selected_probe
            .as_ref()
            .and_then(|probe| probe.proposal_stage.as_ref())
            .map_or_else(|| "baseline".into(), |name| format!("selected:{name}"));
        let mut value = serde_json::json!({"experiment":journal.plan.digest(), "stage":stage, "probes":evidence, "choice":reference});
        // Keep historical single-cycle receipt hashes unchanged.
        if !parents.is_empty() {
            value
                .as_object_mut()
                .context("selection evidence is not an object")?
                .insert("parents".into(), serde_json::to_value(parent_evidence)?);
        }
        let mut hash = Sha256::new();
        hash.update(b"prx-development-selection-v1\0");
        hash.update(serde_json::to_vec(&value)?);
        Ok(DevelopmentChoice {
            evidence: StageEvidence {
                pending_gate: None,
                record_id: reference,
                digest: hex::encode(hash.finalize()),
                successful: true,
                scratch_bytes: 0,
            },
            selected_probe,
        })
    }

    fn verified_development_choice(
        &self,
        journal: &ExperimentJournal,
        selection: &str,
        depth: usize,
    ) -> Result<DevelopmentChoice> {
        ensure!(depth <= 2, "selection ancestry exceeds bounded refinement depth");
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == selection)
            .context("selection missing")?;
        ensure!(
            declared.kind == ExperimentStageKind::Selection
                && journal.state.stages.get(selection).is_some_and(StageState::successful),
            "selection is not settled and cleaned up"
        );
        let (reference, digest) = journal
            .entries
            .iter()
            .find_map(|entry| match &entry.event {
                ExperimentEvent::Settled {
                    stage,
                    reference: Some(reference),
                    evidence_sha256,
                    successful: true,
                    ..
                } if stage == selection => Some((reference, evidence_sha256)),
                _ => None,
            })
            .context("selection evidence missing")?;
        let mut probes = Vec::new();
        let mut parents = Vec::new();
        for dependency in &declared.dependencies {
            let source = journal
                .plan
                .spec()
                .stages
                .iter()
                .find(|value| value.id == *dependency)
                .context("selection dependency missing")?;
            if source.kind == ExperimentStageKind::Selection {
                parents.push(dependency.clone());
            } else {
                probes.push(dependency.clone());
            }
        }
        let computed = self.development_choice(journal, selection, &probes, &parents, depth)?;
        ensure!(
            computed.evidence.record_id == *reference && computed.evidence.digest == *digest,
            "selection evidence changed"
        );
        Ok(computed)
    }

    pub(super) fn selected_development_context(
        &self,
        journal: &ExperimentJournal,
        selection: &str,
    ) -> Result<serde_json::Value> {
        let choice = self.verified_development_choice(journal, selection, 0)?;
        let probe = choice.selected_probe.context("selection retained baseline")?;
        Ok(serde_json::json!({
            "selection_sha256":choice.evidence.digest, "probe_stage":probe.binding.stage,
            "probe_sha256":probe.digest()?, "cases":probe.cases.iter().map(|case| serde_json::json!({
                "case_id":case.case_id, "trial":case.trial, "verdict":case.verdict,
                "run_receipt_sha256":case.run_receipt_digest,
            })).collect::<Vec<_>>()
        }))
    }

    pub(super) fn selected_development_proposal(
        &self,
        journal: &ExperimentJournal,
        selection: &str,
    ) -> Result<Option<String>> {
        Ok(self
            .verified_development_choice(journal, selection, 0)?
            .selected_probe
            .and_then(|probe| probe.proposal_stage))
    }
}
