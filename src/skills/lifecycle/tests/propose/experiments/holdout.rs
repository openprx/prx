//! Explicit paired holdout through the actual service, evaluator and HTTP adapter.
//! Loopback proves mechanisms and accounting; it is not live-model acceptance.
use super::*;
use crate::skills::lifecycle::{
    AccountingOwner, AgentCaseExecutor, EvaluationReceipt, EvaluationRequest, ProposalStatus,
};

pub(super) fn draft(
    f: &Fixture,
    service: &SkillsLifecycleService,
    config: &Config,
    pattern: PatternId,
    schedule: InstructionSchedule,
    additional: bool,
) -> Result<InstructionCycleDraft> {
    let base = instruction_cycle::cycle_with_schedule(f, service, config, pattern.clone(), schedule)?;
    let evaluation = base
        .spec
        .stages
        .iter()
        .find(|stage| stage.id == "final-evaluation")
        .unwrap()
        .allocation;
    Ok(InstructionCycleDraft {
        id: base.spec.id,
        skill_id: f.pin.skill_id.clone(),
        patterns: vec![pattern],
        schedule,
        verification_split: EvalSplit::Validation,
        resources: base.spec.resources,
        proposal_allocation: base.spec.stages[1].allocation,
        probe_allocation: base.spec.stages[0].allocation,
        evaluation_allocation: evaluation,
        holdout_allocation: additional.then_some(evaluation),
        lesson_query: None,
        gate_allocation: None,
        elapsed_ms: 300_000,
        max_input_bytes: 65536,
        max_output_bytes: 16384,
        proposal_timeout_ms: 120_000,
    })
}

pub(super) fn evaluation(
    f: &Fixture,
    journal: &ExperimentJournal,
    stage: &str,
) -> Result<crate::skills::lifecycle::VerifiedEvaluation> {
    let (id, digest) = journal
        .entries
        .iter()
        .find_map(|entry| match &entry.event {
            ExperimentEvent::Settled {
                stage: name,
                reference: Some(id),
                evidence_sha256,
                ..
            } if name == stage => Some((id, evidence_sha256)),
            _ => None,
        })
        .context("evaluation has no settled pin")?;
    Ok(f.store.verify_evaluation(id, Some(digest))?)
}

pub(super) async fn through_validation(
    service: &Arc<SkillsLifecycleService>,
    config: &Config,
    plan: &InstructionExperimentPlan,
) -> Result<ExperimentJournal> {
    let frozen = plan.spec.clone().freeze()?;
    let mut journal = service.admit_experiment(plan.spec.clone(), config)?;
    for stage in plan.spec.stages.iter().take_while(|stage| stage.id != "final-holdout") {
        journal = run_stage(service, config, &frozen, &stage.id, &plan.operations[&stage.id]).await?;
        assert!(
            journal.state.stages[&stage.id].successful(),
            "prefix stage {} must pass",
            stage.id
        );
    }
    Ok(journal)
}

#[tokio::test]
async fn explicit_holdout_runs_each_split_once_on_same_proposal_and_replay_sends_nothing() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let draft = draft(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Single,
            true,
        )?;
        let plan = service.plan_instruction_cycle(draft, &config)?;
        assert_eq!(
            plan.spec.pins.holdout_split_digest,
            Some(suite.split_digest(EvalSplit::Holdout))
        );
        let prefix = through_validation(&service, &config, &plan).await?;
        assert_eq!(f.store.list_evaluation_ids()?.len(), 1);
        assert!(!service.experiment_qualified(&prefix)?);
        let sends_before_holdout = server.count.load(Ordering::SeqCst);
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert!(service.experiment_qualified(&journal)?);
        assert_eq!(f.store.list_evaluation_ids()?.len(), 2);
        let validation = evaluation(&f, &journal, "final-evaluation")?;
        let holdout = evaluation(&f, &journal, "final-holdout")?;
        for verified in [&validation, &holdout] {
            assert_eq!(verified.receipt.planned_runs, 6);
            assert_eq!(verified.receipt.recorded_runs, 6);
            assert_eq!(verified.cases.len(), 6);
            assert!(verified.outcome.is_pass());
        }
        assert_eq!(validation.receipt.split, EvalSplit::Validation);
        assert_eq!(holdout.receipt.split, EvalSplit::Holdout);
        assert_eq!(validation.receipt.baseline, holdout.receipt.baseline);
        assert_eq!(validation.receipt.candidate, holdout.receipt.candidate);
        assert_eq!(
            validation.receipt.proposal_evaluation,
            holdout.receipt.proposal_evaluation
        );
        assert_eq!(validation.receipt.observed_generation, f.pin.generation);
        let binding = validation.receipt.proposal_evaluation.as_ref().unwrap();
        assert_eq!(binding.evaluation_epoch, 1);
        let physical: u64 = journal
            .entries
            .iter()
            .filter_map(|entry| match &entry.event {
                ExperimentEvent::Settled { stage, usage, .. } if stage == "final-holdout" => Some(usage.calls),
                _ => None,
            })
            .sum();
        assert!(physical > 0);
        assert_eq!(
            u64::try_from(server.count.load(Ordering::SeqCst) - sends_before_holdout)?,
            physical,
            "every holdout wire request must join the existing physical stage tally"
        );
        let sends = server.count.load(Ordering::SeqCst);
        let recovered = service.recover_experiment(&plan.spec.id, journal.plan.digest())?;
        assert_eq!(recovered.head_sha256, journal.head_sha256);
        let repeated = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert_eq!(repeated.head_sha256, journal.head_sha256);
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(f.store.list_evaluation_ids()?.len(), 2);
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
            f.pin.generation
        );
        let finished = service.finish_evaluation(&binding.proposal_id, "paired observations complete")?;
        assert_eq!(finished.evaluating_since, Some(binding.evaluating_since));
        for status in [ProposalStatus::Evaluated, ProposalStatus::Committed] {
            // Mutate only current state to prove static receipts never consult it.
            // This is not a claim that the fixture actually published anything.
            let mut later = finished.clone();
            later.status = status;
            later.evaluation_epoch += 1;
            later.evaluating_since = Some(binding.evaluating_since + chrono::Duration::seconds(1));
            let lock = f.store.lock_proposals()?;
            f.store.write_proposal(&lock, &later)?;
            drop(lock);
            f.store
                .verify_evaluation(&validation.receipt.record_id, Some(&validation.receipt_digest))?;
            f.store
                .verify_evaluation(&holdout.receipt.record_id, Some(&holdout.receipt_digest))?;
            assert!(
                service.experiment_qualified(&journal)?,
                "historical qualification cannot depend on current epoch/status"
            );
        }
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn failed_validation_never_reserves_or_sends_holdout() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let mut spec = suite(&f).suite().clone();
        for case in spec.cases.iter_mut().filter(|case| case.split == EvalSplit::Validation) {
            case.validators = vec![Validator::ResponseContains { text: "VALIDATION-REQUIRED-NEGATIVE".into() }];
        }
        let suite = spec.freeze()?;
        let service = service(&f, &suite);
        let draft = draft(&f, &service, &config, pattern.pattern_id, InstructionSchedule::Single, true)?;
        let plan = service.plan_instruction_cycle(draft, &config)?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?.run(service.run_instruction_plan(&config, &plan, &cancel)).await?;
        assert!(!service.experiment_qualified(&journal)?);
        assert_eq!(f.store.list_evaluation_ids()?.len(), 1);
        assert!(!evaluation(&f, &journal, "final-evaluation")?.outcome.is_pass());
        assert!(matches!(journal.state.stages["final-holdout"], StageState::Planned));
        assert!(!journal.entries.iter().any(|entry| matches!(&entry.event, ExperimentEvent::Reserved { stage } | ExperimentEvent::Dispatched { stage } if stage == "final-holdout")));
        let sends = server.count.load(Ordering::SeqCst);
        assert!(run_stage(&service, &config, &journal.plan, "final-holdout", &plan.operations["final-holdout"]).await.is_err());
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        Ok(())
    }.await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn holdout_refuses_changed_epoch_window_status_and_same_candidate_other_proposal_before_send() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let draft = draft(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Sequential2,
            true,
        )?;
        let plan = service.plan_instruction_cycle(draft, &config)?;
        let journal = through_validation(&service, &config, &plan).await?;
        let validation = evaluation(&f, &journal, "final-evaluation")?;
        let binding = validation.receipt.proposal_evaluation.as_ref().unwrap();
        let original = f.store.read_proposal(&binding.proposal_id)?.unwrap();
        let sends = server.count.load(Ordering::SeqCst);
        for change in 0..3 {
            let mut changed = original.clone();
            match change {
                0 => changed.evaluation_epoch += 1, // since deliberately unchanged
                1 => changed.evaluating_since = Some(binding.evaluating_since - chrono::Duration::seconds(1)),
                _ => changed.status = ProposalStatus::Evaluated,
            }
            let lock = f.store.lock_proposals()?;
            f.store.write_proposal(&lock, &changed)?;
            drop(lock);
            assert!(
                run_stage(
                    &service,
                    &config,
                    &journal.plan,
                    "final-holdout",
                    &plan.operations["final-holdout"]
                )
                .await
                .is_err()
            );
            assert_eq!(server.count.load(Ordering::SeqCst), sends);
            assert_eq!(f.store.list_evaluation_ids()?.len(), 1);
            let current = service.inspect_experiment(&plan.spec.id, journal.plan.digest())?;
            assert!(matches!(current.state.stages["final-holdout"], StageState::Planned));
            let lock = f.store.lock_proposals()?;
            f.store.write_proposal(&lock, &original)?;
        }
        // k2 produced a distinct real proposal with the same revision bundle.
        let other = f
            .store
            .list_proposals()?
            .into_iter()
            .find(|proposal| {
                proposal.proposal_id != original.proposal_id
                    && proposal.candidate_revision == original.candidate_revision
            })
            .context("fixture needs same-candidate distinct proposals")?;
        service.review_source(&other.proposal_id, "PRXHoldoutTest")?;
        let mut other = f.store.read_proposal(&other.proposal_id)?.unwrap();
        // Ordinary begin also refuses a revision installed by another proposal.
        // Fault-inject an Evaluating state with the same window to isolate this
        // identity guard even if that earlier provenance guard were bypassed.
        other.status = ProposalStatus::Evaluating;
        other.required_suite = original.required_suite.clone();
        other.evaluation_epoch = binding.evaluation_epoch;
        other.evaluating_since = Some(binding.evaluating_since);
        let lock = f.store.lock_proposals()?;
        f.store.write_proposal(&lock, &other)?;
        drop(lock);
        assert_eq!(other.evaluation_epoch, binding.evaluation_epoch);
        assert!(
            service
                .require_validation_epoch(&other.proposal_id, &validation)
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        f.store
            .verify_evaluation(&validation.receipt.record_id, Some(&validation.receipt_digest))?;
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn legacy_unbound_evaluation_keeps_seal_and_history_but_cannot_authorize_new_holdout() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let draft = draft(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Single,
            true,
        )?;
        let plan = service.plan_instruction_cycle(draft, &config)?;
        let journal = through_validation(&service, &config, &plan).await?;
        let first = evaluation(&f, &journal, "final-evaluation")?;
        let binding = first.receipt.proposal_evaluation.as_ref().unwrap();
        let scratch = tempfile::tempdir_in(&config.workspace_dir)?;
        let executor = AgentCaseExecutor::new(
            config.clone(),
            scratch.path().into(),
            0.0,
            AccountingOwner::new(config.workspace_dir.clone()),
        );
        let request = EvaluationRequest {
            suite: &suite,
            split: EvalSplit::Validation,
            baseline: Some(f.pin.revision_id.clone()),
            candidate: first.receipt.candidate.revision_id.clone(),
            observed_generation: f.pin.generation,
        };
        let cancel = CancellationToken::new();
        // The ordinary store API genuinely writes a legal legacy-style None record.
        let legacy = CleanupScope::new(None, cancel.clone())?
            .run(async { Ok(f.store.evaluate(&request, &executor, &cancel).await?) })
            .await?;
        assert!(legacy.proposal_evaluation.is_none());
        let verified = f
            .store
            .verify_evaluation(&legacy.record_id, Some(&legacy.receipt_digest))?;
        assert!(verified.outcome.is_pass());
        let bytes = serde_json::to_vec(&legacy.receipt)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert!(value.get("proposal_evaluation").is_none());
        let restored: EvaluationReceipt = serde_json::from_value(value)?;
        assert_eq!(serde_json::to_vec(&restored)?, bytes);
        assert_eq!(
            crate::skills::lifecycle::evaluate::seal_digest(&restored)?,
            legacy.receipt_digest
        );
        let sends = server.count.load(Ordering::SeqCst);
        assert!(
            service
                .require_validation_epoch(&binding.proposal_id, &verified)
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert!(matches!(
            service
                .inspect_experiment(&plan.spec.id, journal.plan.digest())?
                .state
                .stages["final-holdout"],
            StageState::Planned
        ));
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn requested_holdout_missing_from_manual_plan_never_qualifies() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let draft = draft(
            &f,
            &service,
            &config,
            pattern.pattern_id,
            InstructionSchedule::Single,
            true,
        )?;
        let mut plan = service.plan_instruction_cycle(draft, &config)?;
        plan.spec
            .stages
            .retain(|stage| !matches!(stage.id.as_str(), "final-holdout" | "verify-holdout"));
        plan.operations
            .retain(|stage, _| !matches!(stage.as_str(), "final-holdout" | "verify-holdout"));
        assert!(plan.validate().is_err());
        let frozen = plan.spec.clone().freeze()?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        // Direct stage execution cannot turn validation-only into dual-split proof.
        let mut journal = service.inspect_experiment(&plan.spec.id, frozen.digest())?;
        for stage in &plan.spec.stages {
            journal = run_stage(&service, &config, &frozen, &stage.id, &plan.operations[&stage.id]).await?;
        }
        assert!(journal.state.qualified(&frozen));
        assert!(!service.experiment_qualified(&journal)?);
        assert_eq!(f.store.list_evaluation_ids()?.len(), 1);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn explicit_holdout_planning_checks_authoritative_suite_all_resources_and_default_identity() -> Result<()> {
    let server = Server::start(false).await?;
    let result = async {
        for (schedule, old_count) in [
            (InstructionSchedule::Single, 10),
            (InstructionSchedule::Sequential2, 17),
            (InstructionSchedule::Parallel2, 18),
        ] {
            let f = fixture();
            let pattern = usable_pattern(&f);
            let config = config(&f, &server);
            let suite = suite(&f);
            let service = service(&f, &suite);
            let plain = draft(&f, &service, &config, pattern.pattern_id, schedule, false)?;
            let value = serde_json::to_value(&plain)?;
            assert!(value.get("holdout_allocation").is_none());
            let old: InstructionCycleDraft = serde_json::from_value(value)?;
            let old = old.compile()?;
            assert!(!old.verify_holdout);
            assert!(serde_json::to_value(&old)?.get("verify_holdout").is_none());
            assert_eq!(old.stages.len(), old_count);
            assert!(!old.stages.iter().any(|stage| stage.id.contains("holdout")));
            let mut extra = plain;
            extra.holdout_allocation = Some(extra.evaluation_allocation);
            let compiled = extra.clone().compile()?;
            assert_eq!(compiled.stages.len(), old_count + 2);
            let total = compiled
                .stages
                .iter()
                .try_fold(ExperimentResources::default(), |sum, stage| {
                    sum.checked_add(stage.allocation)
                })?;
            for component in 0..6 {
                let mut reduced = extra.clone();
                reduced.resources = total;
                match component {
                    0 => reduced.resources.calls -= 1,
                    1 => reduced.resources.tokens -= 1,
                    2 => reduced.resources.micro_usd -= 1,
                    3 => reduced.resources.tool_calls -= 1,
                    4 => reduced.resources.output_bytes -= 1,
                    _ => reduced.resources.scratch_bytes -= 1,
                }
                assert!(
                    matches!(reduced.compile(), Err(ExperimentError::BudgetExceeded)),
                    "resource {component}"
                );
            }
            let plan = service.plan_instruction_cycle(extra.clone(), &config)?;
            assert!(plan.spec.pins.holdout_split_digest.is_some());
            let mut forged = plan.spec.clone();
            forged.pins.holdout_split_digest = Some("f".repeat(64));
            assert!(service.admit_experiment(forged, &config).is_err());
            extra.verification_split = EvalSplit::Holdout;
            assert!(extra.compile().is_err());
            let mut no_cases = suite.suite().clone();
            no_cases.cases.retain(|case| case.split != EvalSplit::Holdout);
            let no_cases = no_cases.freeze()?;
            let no_holdout_service = super::service(&f, &no_cases);
            assert!(no_holdout_service.plan_experiment_draft(compiled, &config).is_err());
        }
        let legacy = r#"{"operation":"selected_evaluation","selection_stage":"select","split":"validation"}"#;
        let operation: ExperimentOperation = serde_json::from_str(legacy)?;
        assert_eq!(serde_json::to_string(&operation)?, legacy);
        use sha2::{Digest, Sha256};
        let mut old = Sha256::new();
        old.update(b"prx-instruction-experiment-input-v1\0");
        old.update(legacy.as_bytes());
        assert_eq!(operation.digest()?, hex::encode(old.finalize()));
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}
