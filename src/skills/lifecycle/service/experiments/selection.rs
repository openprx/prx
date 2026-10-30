//! Deterministic development comparison. Baseline is always eligible and ties
//! retain it. Independent evaluation and publication remain separate owners.
use super::operations::StageEvidence;
use super::{Context, ExperimentEvent, ExperimentJournal, Result, SkillsLifecycleService, StageState, ensure};
use crate::skills::lifecycle::{CaseVerdict, EvalSplit};
use sha2::{Digest, Sha256};

impl SkillsLifecycleService {
    pub(super) fn choose_development_candidate(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        probes: &[String],
    ) -> Result<StageEvidence> {
        ensure!(
            (2..=4).contains(&probes.len()),
            "selection needs baseline and one to three candidates"
        );
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("selection stage missing")?;
        let suite = self.store.read_suite(&journal.plan.spec().pins.suite_digest)?;
        let mut records = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut evidence = Vec::new();
        for name in probes {
            ensure!(
                seen.insert(name) && declared.dependencies.contains(name),
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
            ensure!(
                record.binding.experiment_sha256 == journal.plan.digest()
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
        let baseline = baselines.first().context("baseline missing")?;
        ensure!(
            baseline.revision == journal.plan.spec().pins.baseline_revision,
            "selection baseline changed"
        );
        let expected: Vec<_> = suite
            .cases_of(EvalSplit::Development)
            .flat_map(|case| (0..suite.suite().trials).map(move |trial| (&case.case_id, trial)))
            .collect();
        // Match the existing evaluator: a root case passes only when every
        // declared repeat passes. Repeats cannot inflate gain/regression counts.
        let case_passes = |record: &crate::skills::lifecycle::experiment::DevelopmentProbeReceipt, id: &str| {
            let runs: Vec<_> = record.cases.iter().filter(|case| case.case_id == id).collect();
            runs.len() == usize::try_from(suite.suite().trials).unwrap_or(usize::MAX)
                && runs.iter().all(|case| case.verdict == CaseVerdict::Pass)
        };
        let score = |record: &crate::skills::lifecycle::experiment::DevelopmentProbeReceipt| {
            suite
                .cases_of(EvalSplit::Development)
                .filter(|case| case_passes(record, &case.case_id))
                .count()
        };
        let mut best = score(baseline);
        let mut selected = None;
        // Input order is immutable. Among equal improving candidates use the
        // first declared candidate; an equal baseline score never replaces it.
        for record in &records {
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
                    let base = case_passes(baseline, &case.case_id);
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
                    selected = Some(proposal_stage.clone());
                }
            }
        }
        let reference = selected.map_or_else(|| "baseline".into(), |name| format!("selected:{name}"));
        let bytes = serde_json::to_vec(
            &serde_json::json!({"experiment":journal.plan.digest(), "stage":stage, "probes":evidence, "choice":reference}),
        )?;
        let mut hash = Sha256::new();
        hash.update(b"prx-development-selection-v1\0");
        hash.update(bytes);
        Ok(StageEvidence {
            record_id: reference,
            digest: hex::encode(hash.finalize()),
            successful: true,
            scratch_bytes: 0,
        })
    }

    pub(super) fn selected_development_proposal(
        &self,
        journal: &ExperimentJournal,
        selection: &str,
    ) -> Result<Option<String>> {
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == selection)
            .context("selection missing")?;
        ensure!(
            declared.kind == crate::skills::lifecycle::experiment::ExperimentStageKind::Selection
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
        // Recompute from its exact frozen direct probe dependencies. The
        // runner generates selection dependencies in their operation order.
        let computed = self.choose_development_candidate(journal, selection, &declared.dependencies)?;
        ensure!(
            computed.record_id == *reference && computed.digest == *digest,
            "selection evidence changed"
        );
        if reference == "baseline" {
            return Ok(None);
        }
        let proposal = reference
            .strip_prefix("selected:")
            .context("invalid selection reference")?;
        self.experiment_proposal(journal, proposal)?;
        Ok(Some(proposal.into()))
    }
}
