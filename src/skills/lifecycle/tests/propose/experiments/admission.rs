//! Durable proposal admission and terminal-proof guards, using real receipts.
use super::holdout::{draft, evaluation, through_validation};
use super::*;
use crate::skills::lifecycle::publish::crash_hooks;
use crate::skills::lifecycle::{
    AccountingOwner, AgentCaseExecutor, CaseExecutor, CleanupState, EvaluationError, ExecutionRequest, ProposalStatus,
    RunObservation,
};

fn native(config: &Config) -> Result<(tempfile::TempDir, AgentCaseExecutor)> {
    let scratch = tempfile::tempdir_in(&config.workspace_dir)?;
    let executor = AgentCaseExecutor::new(
        config.clone(),
        scratch.path().into(),
        0.0,
        AccountingOwner::new(config.workspace_dir.clone()),
    );
    Ok((scratch, executor))
}

#[tokio::test]
async fn durable_reference_blocks_concurrent_regular_writers_before_first_handoff_then_terminal_allows_finish()
-> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = service.plan_instruction_cycle(
            draft(
                &f,
                &service,
                &config,
                pattern.pattern_id,
                InstructionSchedule::Single,
                true,
            )?,
            &config,
        )?;
        let journal = through_validation(&service, &config, &plan).await?;
        let validation = evaluation(&f, &journal, "final-evaluation")?;
        let binding = validation.receipt.proposal_evaluation.as_ref().unwrap().clone();
        let stale = f.store.read_proposal(&binding.proposal_id)?.unwrap();
        let sends = server.count.load(Ordering::SeqCst);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let writer_barrier = barrier.clone();
        let writer_service = service.clone();
        let store = f.store.clone();
        let counter = server.count.clone();
        let expected = binding.clone();
        let writer = std::thread::spawn(move || -> Result<()> {
            let _ = writer_barrier.wait();
            let result = (|| -> Result<()> {
                anyhow::ensure!(
                    counter.load(Ordering::SeqCst) == sends,
                    "no handoff may precede durable admission barrier"
                );
                anyhow::ensure!(
                    writer_service
                        .finish_evaluation(&expected.proposal_id, "concurrent finish")
                        .is_err(),
                    "pending admission must block ordinary finish"
                );
                anyhow::ensure!(
                    writer_service
                        .finish_evaluation_bound(&expected, "concurrent bound finish")
                        .is_err(),
                    "pending admission must block bound finish"
                );
                let lock = store.lock_proposals()?;
                let current = store.read_proposal(&expected.proposal_id)?.unwrap();
                anyhow::ensure!(
                    current.bound_evaluation_attempts.len() == 2,
                    "both references must already be durable"
                );
                anyhow::ensure!(
                    store.write_proposal(&lock, &stale).is_err(),
                    "stale clone cannot erase appended reference"
                );
                for field in 0..6 {
                    let mut changed = current.clone();
                    match field {
                        0 => changed.evaluation_epoch += 1,
                        1 => changed.evaluating_since = Some(expected.evaluating_since - chrono::Duration::seconds(1)),
                        2 => changed.candidate_revision = None,
                        3 => changed.baseline_revision = None,
                        4 => changed.required_suite = Some("f".repeat(64)),
                        _ => changed.expected_generation += 1,
                    }
                    anyhow::ensure!(
                        store.write_proposal(&lock, &changed).is_err(),
                        "pending attempt must protect subject field {field}"
                    );
                }
                for invalid in [
                    Vec::new(),
                    vec!["invalid-id".into()],
                    vec![current.bound_evaluation_attempts[0].clone(); 2],
                ] {
                    let mut changed = current.clone();
                    changed.bound_evaluation_attempts = invalid;
                    anyhow::ensure!(
                        store.write_proposal(&lock, &changed).is_err(),
                        "references cannot be removed, forged or duplicated"
                    );
                }
                Ok(())
            })();
            let _ = writer_barrier.wait();
            result
        });
        crash_hooks::run_at(
            "evaluation_before_first_executor",
            Box::new(move || {
                let _ = barrier.wait();
                let _ = barrier.wait();
            }),
        );
        let outcome = run_stage(
            &service,
            &config,
            &journal.plan,
            "final-holdout",
            &plan.operations["final-holdout"],
        )
        .await;
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("concurrent writer panicked"))??;
        let journal = outcome?;
        let holdout = evaluation(&f, &journal, "final-holdout")?;
        assert!(holdout.outcome.is_pass());
        assert!(
            server.count.load(Ordering::SeqCst) > sends,
            "correct admitted epoch must still run"
        );
        let finished = service.finish_evaluation_bound(&binding, "terminal paired observations")?;
        assert_eq!(finished.status, ProposalStatus::Evaluated);
        assert_eq!(finished.bound_evaluation_attempts.len(), 2);
        let repeated = service.finish_evaluation_bound(&binding, "idempotent same epoch")?;
        assert_eq!(serde_json::to_vec(&repeated)?, serde_json::to_vec(&finished)?);
        let mut wrong_epoch = binding.clone();
        wrong_epoch.evaluation_epoch += 1;
        assert!(service.finish_evaluation_bound(&wrong_epoch, "wrong epoch").is_err());
        let lock = f.store.lock_proposals()?;
        let mut wrong_subject = finished.clone();
        wrong_subject.expected_generation += 1;
        f.store.write_proposal(&lock, &wrong_subject)?;
        drop(lock);
        assert!(
            service
                .finish_evaluation_bound(&binding, "same epoch wrong subject")
                .is_err()
        );
        let lock = f.store.lock_proposals()?;
        f.store.write_proposal(&lock, &finished)?;
        drop(lock);
        f.store
            .verify_evaluation(&holdout.receipt.record_id, Some(&holdout.receipt_digest))?;
        Ok(())
    }
    .await;
    crash_hooks::disarm();
    server.finish().await?;
    result
}

#[tokio::test]
async fn uncertain_two_write_admission_never_sends_refunds_or_automatically_retries() -> Result<()> {
    let server = Server::start(false).await?;
    let result = async {
        for (point, indexed) in [
            ("evaluation_attempt_durable", false),
            ("evaluation_reference_durable", true),
        ] {
            let f = fixture();
            let pattern = usable_pattern(&f);
            let config = config(&f, &server);
            let suite = suite(&f);
            let service = service(&f, &suite);
            let plan = service.plan_instruction_cycle(
                draft(
                    &f,
                    &service,
                    &config,
                    pattern.pattern_id,
                    InstructionSchedule::Single,
                    true,
                )?,
                &config,
            )?;
            let journal = through_validation(&service, &config, &plan).await?;
            let validation = evaluation(&f, &journal, "final-evaluation")?;
            let binding = validation.receipt.proposal_evaluation.as_ref().unwrap();
            let sends = server.count.load(Ordering::SeqCst);
            crash_hooks::arm(point);
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
            crash_hooks::disarm();
            assert_eq!(server.count.load(Ordering::SeqCst), sends);
            assert_eq!(
                f.store.list_evaluation_ids()?.len(),
                2,
                "orphan reservation remains spent"
            );
            let proposal = f.store.read_proposal(&binding.proposal_id)?.unwrap();
            assert_eq!(proposal.bound_evaluation_attempts.len(), if indexed { 2 } else { 1 });
            let new_id = crate::skills::lifecycle::evaluate::new_evaluation_id();
            assert!(matches!(
                f.store.reserve_attempt(
                    &suite,
                    EvalSplit::Holdout,
                    &new_id,
                    &validation.receipt.candidate.revision_id,
                    Some(&f.pin.revision_id),
                    f.pin.generation
                ),
                Err(EvaluationError::AttemptsExhausted { .. })
            ));
            if indexed {
                let pending = proposal.bound_evaluation_attempts.last().unwrap();
                assert!(
                    service
                        .finish_evaluation_bound(binding, "no receipt is not terminal")
                        .is_err()
                );
                let attempt_path = f.store.evaluation_dir(pending).join("attempt.json");
                let bytes = std::fs::read(&attempt_path)?;
                std::fs::remove_file(&attempt_path)?;
                assert!(service.finish_evaluation_bound(binding, "missing attempt").is_err());
                std::fs::write(&attempt_path, &bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&attempt_path, std::fs::Permissions::from_mode(0o600))?;
                }
                std::fs::write(&attempt_path, b"invalid attempt")?;
                assert!(service.finish_evaluation_bound(binding, "corrupt attempt").is_err());
                let lock = f.store.lock_proposals()?;
                assert!(
                    f.store.write_proposal(&lock, &proposal).is_err(),
                    "even unchanged writes cannot hide a corrupt own reference"
                );
                drop(lock);
                std::fs::write(&attempt_path, bytes)?;
                let receipt_path = f.store.evaluation_dir(pending).join("receipt.json");
                std::fs::write(&receipt_path, b"invalid terminal receipt")?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&receipt_path, std::fs::Permissions::from_mode(0o600))?;
                }
                assert!(service.finish_evaluation_bound(binding, "corrupt receipt").is_err());
            }
            let recovered = service.recover_experiment(&plan.spec.id, journal.plan.digest())?;
            assert!(matches!(
                recovered.state.stages["final-holdout"],
                StageState::DispatchUnknown
            ));
            let cancel = CancellationToken::new();
            assert!(
                CleanupScope::new(None, cancel.clone())?
                    .run(service.run_instruction_plan(&config, &plan, &cancel))
                    .await
                    .is_err()
            );
            assert_eq!(server.count.load(Ordering::SeqCst), sends);
        }
        Ok(())
    }
    .await;
    crash_hooks::disarm();
    server.finish().await?;
    result
}

#[tokio::test]
async fn controlled_pre_handoff_cancel_records_terminal_zero_cases_without_refunding_attempt() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = service.plan_instruction_cycle(
            draft(
                &f,
                &service,
                &config,
                pattern.pattern_id,
                InstructionSchedule::Single,
                true,
            )?,
            &config,
        )?;
        let journal = through_validation(&service, &config, &plan).await?;
        let validation = evaluation(&f, &journal, "final-evaluation")?;
        let binding = validation.receipt.proposal_evaluation.as_ref().unwrap();
        let sends = server.count.load(Ordering::SeqCst);
        let (_scratch, executor) = native(&config)?;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        crash_hooks::run_at("evaluation_before_first_executor", Box::new(move || stop.cancel()));
        let recorded = CleanupScope::new(None, CancellationToken::new())?
            .run(async {
                Ok(service
                    .evaluate_in_epoch(
                        &binding.proposal_id,
                        EvalSplit::Holdout,
                        Some(binding),
                        &executor,
                        &cancel,
                    )
                    .await?)
            })
            .await?;
        let verified = f
            .store
            .verify_evaluation(&recorded.record_id, Some(&recorded.receipt_digest))?;
        assert!(verified.receipt.cancelled);
        assert_eq!(verified.receipt.recorded_runs, 0);
        assert_eq!(verified.cases, Vec::<crate::skills::lifecycle::CaseReceipt>::new());
        assert!(!verified.outcome.is_pass());
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        service.finish_evaluation_bound(binding, "controlled cancellation before any case")?;
        let next = crate::skills::lifecycle::evaluate::new_evaluation_id();
        assert!(matches!(
            f.store.reserve_attempt(
                &suite,
                EvalSplit::Holdout,
                &next,
                &validation.receipt.candidate.revision_id,
                Some(&f.pin.revision_id),
                f.pin.generation
            ),
            Err(EvaluationError::AttemptsExhausted { .. })
        ));
        Ok(())
    }
    .await;
    crash_hooks::disarm();
    server.finish().await?;
    result
}

struct FailedCleanup(AgentCaseExecutor);
#[async_trait::async_trait]
impl CaseExecutor for FailedCleanup {
    fn identity(&self) -> String {
        "native-cleanup-negative".into()
    }
    fn seed_control(&self) -> String {
        self.0.seed_control()
    }
    async fn run_case(
        &self,
        request: &ExecutionRequest<'_>,
        cancel: &CancellationToken,
    ) -> std::result::Result<RunObservation, EvaluationError> {
        let mut observed = self.0.run_case(request, cancel).await?;
        observed.cleanup = CleanupState::Failed;
        Ok(observed)
    }
}

#[tokio::test]
async fn valid_failed_cleanup_receipt_retains_own_lease_without_global_history_block() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = service.plan_instruction_cycle(
            draft(
                &f,
                &service,
                &config,
                pattern.pattern_id,
                InstructionSchedule::Single,
                true,
            )?,
            &config,
        )?;
        let journal = through_validation(&service, &config, &plan).await?;
        let validation = evaluation(&f, &journal, "final-evaluation")?;
        let binding = validation.receipt.proposal_evaluation.as_ref().unwrap();
        let (_scratch, executor) = native(&config)?;
        let executor = FailedCleanup(executor);
        let cancel = CancellationToken::new();
        let recorded = CleanupScope::new(None, cancel.clone())?
            .run(async {
                Ok(service
                    .evaluate_in_epoch(
                        &binding.proposal_id,
                        EvalSplit::Holdout,
                        Some(binding),
                        &executor,
                        &cancel,
                    )
                    .await?)
            })
            .await?;
        let verified = f
            .store
            .verify_evaluation(&recorded.record_id, Some(&recorded.receipt_digest))?;
        assert!(!verified.outcome.is_pass());
        assert!(
            verified
                .cases
                .iter()
                .all(|case| case.run.cleanup == CleanupState::Failed)
        );
        assert!(
            service
                .finish_evaluation_bound(binding, "uncertain cleanup cannot unlock")
                .is_err()
        );
        // Old None attempts are not retroactively attributed; another proposal
        // is not disabled by this proposal's known pending cleanup.
        let other = f
            .store
            .list_proposals()?
            .into_iter()
            .find(|proposal| proposal.proposal_id != binding.proposal_id)
            .unwrap();
        let lock = f.store.lock_proposals()?;
        let mut changed = other.clone();
        changed.status = ProposalStatus::Cancelled;
        f.store.write_proposal(&lock, &changed)?;
        drop(lock);
        let sends = server.count.load(Ordering::SeqCst);
        assert!(service.finish_evaluation_bound(binding, "still quarantined").is_err());
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        let mut legacy = other;
        legacy.bound_evaluation_attempts.clear();
        let value = serde_json::to_value(&legacy)?;
        assert!(value.get("bound_evaluation_attempts").is_none());
        let restored: crate::skills::lifecycle::SkillProposal = serde_json::from_value(value)?;
        assert_eq!(restored.bound_evaluation_attempts, Vec::<String>::new());
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn holdout_refuses_a_successful_verifier_with_the_wrong_persisted_pin() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = service.plan_instruction_cycle(
            draft(
                &f,
                &service,
                &config,
                pattern.pattern_id,
                InstructionSchedule::Single,
                true,
            )?,
            &config,
        )?;
        let frozen = plan.spec.clone().freeze()?;
        let mut journal = service.admit_experiment(plan.spec.clone(), &config)?;
        for stage in plan.spec.stages.iter().take_while(|stage| stage.id != "verify") {
            journal = run_stage(&service, &config, &frozen, &stage.id, &plan.operations[&stage.id]).await?;
        }
        let validation = evaluation(&f, &journal, "final-evaluation")?;
        for (event_id, event) in [
            ("negative-reserve", ExperimentEvent::Reserved { stage: "verify".into() }),
            (
                "negative-dispatch",
                ExperimentEvent::Dispatched { stage: "verify".into() },
            ),
            (
                "negative-pin",
                ExperimentEvent::Settled {
                    stage: "verify".into(),
                    usage: ExperimentResources::default(),
                    evidence_sha256: "f".repeat(64),
                    reference: Some(validation.receipt.record_id),
                    successful: true,
                },
            ),
            (
                "negative-cleanup",
                ExperimentEvent::CleanupConfirmed { stage: "verify".into() },
            ),
        ] {
            journal = f.store.append_experiment(
                &plan.spec.id,
                frozen.digest(),
                journal.next_sequence(),
                &journal.head_sha256,
                event_id,
                event,
                Utc::now(),
            )?;
        }
        assert!(journal.state.stages["verify"].successful());
        let sends = server.count.load(Ordering::SeqCst);
        assert!(
            run_stage(
                &service,
                &config,
                &frozen,
                "final-holdout",
                &plan.operations["final-holdout"]
            )
            .await
            .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(f.store.list_evaluation_ids()?.len(), 1);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}
