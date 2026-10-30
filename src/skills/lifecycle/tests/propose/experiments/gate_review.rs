//! GateReview through real stage scheduling, managed ModelJudge and loopback HTTP.
//! Temporary fixtures cannot publish; physical requests use the original root.
use super::*;
use crate::skills::lifecycle::experiment::{ExperimentStageKind, GateReviewWitness};
use crate::skills::lifecycle::{GateDecision, GateError, GatePolicy, ModelJudge, ProposalStatus};

fn reviewed_service(
    f: &Fixture,
    suite: &crate::skills::lifecycle::FrozenSuite,
    config: &Config,
    timeout_ms: Option<u64>,
) -> Result<Arc<SkillsLifecycleService>> {
    let selected =
        crate::providers::selection::check_default(config, crate::providers::catalog::UnknownPolicy::Invocation)?;
    let binding = ModelBinding {
        provider_label: selected.provider_label().into(),
        model: selected.model().into(),
        temperature_milli: 100,
    };
    let mut settings = SkillsLifecycleSettings {
        enabled: true,
        mode: LifecycleMode::Shadow,
        suites: SuiteRegistry::new([(f.pin.skill_id.clone(), suite.digest().into())])?,
        ..Default::default()
    };
    settings.gate_policy.judge_required = true;
    settings.gate_policy.require_holdout = true;
    settings.gate_policy.judge_identity = Some(ModelJudge::identity_for(&binding));
    if let Some(timeout) = timeout_ms {
        settings.gate_policy.judge_timeout_ms = timeout;
    }
    let service = Arc::new(SkillsLifecycleService::new(f.store.clone(), settings)?);
    service.install_operator_suite(suite)?;
    Ok(service)
}
fn draft(
    f: &Fixture,
    service: &SkillsLifecycleService,
    config: &Config,
    pattern: PatternId,
) -> Result<InstructionCycleDraft> {
    let mut draft = holdout::draft(f, service, config, pattern, InstructionSchedule::Single, true)?;
    draft.gate_allocation = Some(draft.proposal_allocation);
    Ok(draft)
}
fn witness(f: &Fixture, journal: &ExperimentJournal) -> Result<GateReviewWitness> {
    let digest = journal
        .entries
        .iter()
        .find_map(|entry| match &entry.event {
            ExperimentEvent::Settled {
                stage, evidence_sha256, ..
            } if stage == "gate-review" => Some(evidence_sha256),
            _ => None,
        })
        .context("gate settlement is absent")?;
    Ok(f.store
        .read_gate_review(&journal.plan.spec().id, "gate-review", digest)?)
}
async fn through_holdout(
    service: &Arc<SkillsLifecycleService>,
    config: &Config,
    plan: &InstructionExperimentPlan,
) -> Result<ExperimentJournal> {
    let frozen = plan.spec.clone().freeze()?;
    let mut journal = service.admit_experiment(plan.spec.clone(), config)?;
    for stage in plan.spec.stages.iter().take_while(|stage| stage.id != "gate-review") {
        journal = run_stage(service, config, &frozen, &stage.id, &plan.operations[&stage.id]).await?;
        assert!(
            journal.state.stages[&stage.id].successful(),
            "prefix {} must pass",
            stage.id
        );
    }
    Ok(journal)
}

#[tokio::test]
async fn gate_none_is_omitted_and_every_resource_component_requires_original_root_capacity() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = reviewed_service(&f, &suite, &config, None)?;
        let mut legacy = holdout::draft(
            &f,
            &service,
            &config,
            pattern.pattern_id.clone(),
            InstructionSchedule::Single,
            false,
        )?;
        assert!(serde_json::to_value(&legacy)?.get("gate_allocation").is_none());
        let first = legacy.clone().compile()?;
        let mut explicit = serde_json::to_value(&legacy)?;
        explicit["gate_allocation"] = serde_json::Value::Null;
        let restored: InstructionCycleDraft = serde_json::from_value(explicit)?;
        assert_eq!(
            serde_json::to_vec(&first)?,
            serde_json::to_vec(&restored.compile()?)?,
            "absent and explicit None preserve exact old stages and operation hashes"
        );
        assert!(
            !first
                .stages
                .iter()
                .any(|stage| stage.operation.kind() == ExperimentStageKind::GateReview)
        );
        legacy.gate_allocation = Some(legacy.proposal_allocation);
        let mut full = legacy.clone();
        full.holdout_allocation = Some(full.evaluation_allocation);
        assert!(
            legacy.compile().is_err(),
            "gate requires explicit dual-independent allocation"
        );
        let compiled = full.clone().compile()?;
        let sum = compiled
            .stages
            .iter()
            .try_fold(ExperimentResources::default(), |total, stage| {
                total.checked_add(stage.allocation)
            })?;
        let mut exact = full.clone();
        exact.resources = sum;
        assert!(exact.compile().is_ok());
        for component in 0..6 {
            let mut reduced = full.clone();
            reduced.resources = sum;
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
                "component {component} must include the gate allocation"
            );
        }
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            0,
            "planning does not send a judge or solver request"
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn managed_gate_review_has_exact_actual_scope_sealed_history_and_zero_request_replay() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = reviewed_service(&f, &suite, &config, None)?;
        let plan = service.plan_instruction_cycle(draft(&f, &service, &config, pattern.pattern_id)?, &config)?;
        assert_eq!(plan.spec.stages.last().unwrap().kind, ExperimentStageKind::GateReview);
        service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert!(service.experiment_qualified(&journal)?);
        assert_eq!(f.store.list_evaluation_ids()?.len(), 2);
        assert_eq!(server.judge_count.load(Ordering::SeqCst), 1);
        let proof = witness(&f, &journal)?;
        let pin = crate::skills::lifecycle::GateReviewPin {
            stage: "gate-review".into(),
            witness_sha256: proof.digest()?,
            gate_id: proof.gate_id.clone(),
            gate_sha256: proof.gate_digest.clone(),
        };
        let verified = service.verify_gate_review_witness(&plan.spec.id, journal.plan.digest(), &pin)?;
        assert_eq!(verified.pin(), pin);
        assert_eq!(verified.subject(), &proof.subject);
        assert_eq!(verified.binding(), &proof.binding);
        assert_eq!(verified.physical().calls, 1);
        assert!(verified.verification_window().finished_at >= verified.verification_window().started_at);
        let mut wrong = pin.clone();
        wrong.gate_sha256 = "0".repeat(64);
        assert!(
            service
                .verify_gate_review_witness(&plan.spec.id, journal.plan.digest(), &wrong)
                .is_err()
        );
        let actual = service.experiment_physical_observations(&journal, &config)?;
        assert_eq!(
            proof.binding.physical_scope_id,
            actual["gate-review"]["scope_id"].as_str().unwrap()
        );
        assert_eq!(actual["gate-review"]["physical"]["calls"], 1);
        assert_eq!(actual["gate-review"]["physical"]["settled_calls"], 1);
        assert_eq!(proof.physical.as_ref().unwrap().calls, 1);
        assert_eq!(proof.physical.as_ref().unwrap().attempt_ids.len(), 1);
        assert!(!actual["gate-review"]["physical"]["unresolved"].as_bool().unwrap());
        let proposal = f.store.read_proposal(&proof.subject.proposal_id)?.unwrap();
        assert_eq!(proposal.status, ProposalStatus::Eligible);
        let gate = f.store.recorded_gate_for(&proposal)?.unwrap();
        assert_eq!(gate.decision.experiment.as_ref(), Some(&proof.binding));
        assert_eq!(gate.digest, proof.gate_digest);
        assert_eq!(verified.gate_decided_at(), gate.decision.decided_at);
        assert_eq!(verified.gate_expires_at(), gate.decision.expires_at);
        assert_eq!(gate.decision.decision, GateDecision::Accept);
        let judge_usage = gate.decision.judge.as_ref().unwrap().usage;
        assert_eq!((judge_usage.physical_requests, judge_usage.settled_requests), (1, 1));
        assert_eq!(
            proof.physical.as_ref().unwrap().calls,
            u64::from(judge_usage.physical_requests)
        );
        assert_eq!(
            proof.physical.as_ref().unwrap().settled_calls,
            u64::from(judge_usage.settled_requests)
        );
        let policy: GatePolicy = serde_json::from_value(proof.policy_snapshot["gate"].clone())?;
        assert!(policy.judge_required && policy.require_holdout && policy.min_cases_floor >= 3);
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
            1,
            "Shadow gate cannot publish"
        );
        assert!(proposal.publication.is_none() && proposal.authorization.is_none());
        let sends = server.count.load(Ordering::SeqCst);
        let costs = std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?;
        let replay = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        let recovered = service.recover_experiment(&plan.spec.id, journal.plan.digest())?;
        assert_eq!(replay.head_sha256, journal.head_sha256);
        assert_eq!(recovered.head_sha256, journal.head_sha256);
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(server.judge_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?,
            costs
        );
        // A later state change does not revoke a correctly sealed historical
        // completion; current publication authority still refuses it.
        f.store
            .cancel_proposal(&proposal.proposal_id, "later operator withdrawal after completed gate")?;
        assert!(service.experiment_qualified(&journal)?);
        assert!(
            f.store
                .verify_gate(&proof.gate_id, &proof.gate_digest, &policy, suite.digest())
                .is_err()
        );
        let checks = proof.current_verification.as_ref().unwrap();
        let validation = holdout::evaluation(&f, &journal, "final-evaluation")?;
        let holdout = holdout::evaluation(&f, &journal, "final-holdout")?;
        let pins = [
            crate::skills::lifecycle::EvaluationPin {
                evaluation_id: validation.receipt.record_id,
                receipt_digest: validation.receipt_digest,
            },
            crate::skills::lifecycle::EvaluationPin {
                evaluation_id: holdout.receipt.record_id,
                receipt_digest: holdout.receipt_digest,
            },
        ];
        let dispatched = journal
            .entries
            .iter()
            .find_map(|entry| match &entry.event {
                ExperimentEvent::Dispatched { stage } if stage == "gate-review" => Some(entry.recorded_at),
                _ => None,
            })
            .unwrap();
        let historical_times = f.store.verify_historical_gate_review(
            &proof.gate_id,
            &proof.gate_digest,
            &proof.binding,
            &proof.subject,
            &policy,
            suite.digest(),
            &pins,
            &ModelJudge::identity_for(&plan.spec.pins.model),
            proof.physical.as_ref().unwrap(),
            dispatched,
            checks.started_at,
            checks.finished_at,
            gate.decision.expires_at + chrono::Duration::seconds(1),
        )?;
        assert_eq!(historical_times, (gate.decision.decided_at, gate.decision.expires_at));
        assert!(matches!(
            f.store.verify_historical_gate_review(
                &proof.gate_id,
                &proof.gate_digest,
                &proof.binding,
                &proof.subject,
                &policy,
                suite.digest(),
                &pins,
                &ModelJudge::identity_for(&plan.spec.pins.model),
                proof.physical.as_ref().unwrap(),
                dispatched,
                checks.started_at,
                gate.decision.expires_at + chrono::Duration::seconds(1),
                gate.decision.expires_at + chrono::Duration::seconds(2)
            ),
            Err(GateError::InsufficientTemporalEvidence(_))
        ));
        let mut missing_owner = proof.physical.clone().unwrap();
        missing_owner.calls = 0;
        missing_owner.settled_calls = 0;
        missing_owner.attempt_ids.clear();
        assert!(
            matches!(
                f.store.verify_historical_gate_review(
                    &proof.gate_id,
                    &proof.gate_digest,
                    &proof.binding,
                    &proof.subject,
                    &policy,
                    suite.digest(),
                    &pins,
                    &ModelJudge::identity_for(&plan.spec.pins.model),
                    &missing_owner,
                    dispatched,
                    checks.started_at,
                    checks.finished_at,
                    gate.decision.expires_at + chrono::Duration::seconds(1),
                ),
                Err(GateError::Tampered(_))
            ),
            "zero owner ledger cannot explain a physical judge call"
        );
        let path = f
            .store
            .root()
            .join("experiment-evidence")
            .join(plan.spec.id.as_str())
            .join("gate-review.gate-review.json");
        let mut tampered: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        tampered["subject"]["evaluation_epoch"] = json!(proof.subject.evaluation_epoch + 1);
        std::fs::write(path, serde_json::to_vec(&tampered)?)?;
        assert!(service.experiment_qualified(&journal).is_err());
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            sends,
            "historical checks never re-judge"
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn judge_concern_and_malformed_reply_keep_real_root_cost_without_qualification() -> Result<()> {
    for mode in [1, 3] {
        let f = fixture();
        let pattern = usable_pattern(&f);
        let server = Server::start(false).await?;
        server.judge_mode.store(mode, Ordering::SeqCst);
        let result: Result<()> = async {
            let config = config(&f, &server);
            let suite = suite(&f);
            let service = reviewed_service(&f, &suite, &config, None)?;
            let plan = service.plan_instruction_cycle(draft(&f, &service, &config, pattern.pattern_id)?, &config)?;
            service.admit_experiment(plan.spec.clone(), &config)?;
            let cancel = CancellationToken::new();
            let journal = CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await?;
            assert!(!service.experiment_qualified(&journal)?);
            assert_eq!(server.judge_count.load(Ordering::SeqCst), 1);
            assert!(!journal.state.stages["gate-review"].successful());
            let proof = witness(&f, &journal)?;
            assert_eq!(proof.physical.as_ref().unwrap().calls, 1);
            assert_eq!(proof.physical.as_ref().unwrap().settled_calls, 1);
            assert!(proof.current_verification.is_none());
            let proposal = f.store.read_proposal(&proof.subject.proposal_id)?.unwrap();
            assert_eq!(
                proposal.status,
                if mode == 1 {
                    ProposalStatus::Rejected
                } else {
                    ProposalStatus::Inconclusive
                }
            );
            assert!(proposal.authorization.is_none() && proposal.publication.is_none());
            assert_eq!(f.store.active_binding(&f.pin.skill_id)?.unwrap().generation, 1);
            let sends = server.count.load(Ordering::SeqCst);
            let again = CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await?;
            assert_eq!(again.head_sha256, journal.head_sha256);
            assert_eq!(server.count.load(Ordering::SeqCst), sends);
            Ok(())
        }
        .await;
        server.finish().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn unknown_stage_after_bound_finish_is_not_automatically_judged_or_reopened() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = reviewed_service(&f, &suite, &config, None)?;
        let plan = service.plan_instruction_cycle(draft(&f, &service, &config, pattern.pattern_id)?, &config)?;
        let mut journal = through_holdout(&service, &config, &plan).await?;
        let validation = holdout::evaluation(&f, &journal, "final-evaluation")?;
        let epoch = validation.receipt.proposal_evaluation.unwrap();
        let finished = service.finish_evaluation_bound(&epoch, "fault boundary after exact bound finish")?;
        let repeated = service.finish_evaluation_bound(&epoch, "same bound finish replay")?;
        assert_eq!(finished.updated_at, repeated.updated_at);
        assert_eq!(finished.bound_evaluation_attempts, repeated.bound_evaluation_attempts);
        for (id, event) in [
            (
                "gate-reserve",
                ExperimentEvent::Reserved {
                    stage: "gate-review".into(),
                },
            ),
            (
                "gate-dispatch",
                ExperimentEvent::Dispatched {
                    stage: "gate-review".into(),
                },
            ),
            (
                "gate-lost",
                ExperimentEvent::DispatchUnknown {
                    stage: "gate-review".into(),
                },
            ),
        ] {
            journal = f.store.append_experiment(
                &plan.spec.id,
                journal.plan.digest(),
                journal.next_sequence(),
                &journal.head_sha256,
                id,
                event,
                Utc::now(),
            )?;
        }
        let sends = server.count.load(Ordering::SeqCst);
        let cost = std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?;
        let cancel = CancellationToken::new();
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        let recovered = service.recover_experiment(&plan.spec.id, journal.plan.digest())?;
        assert_eq!(recovered.head_sha256, journal.head_sha256);
        assert!(!service.experiment_qualified(&recovered)?);
        assert_eq!(server.judge_count.load(Ordering::SeqCst), 0);
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(
            std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?,
            cost
        );
        assert_eq!(
            f.store.read_proposal(&epoch.proposal_id)?.unwrap().evaluation_epoch,
            epoch.evaluation_epoch
        );
        let actual = service.experiment_physical_observations(&recovered, &config)?;
        assert!(actual["gate-review"]["needs_reconciliation"].as_bool().unwrap());
        assert!(f.store.recorded_gate_for(&finished)?.is_none());
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn judge_timeout_or_durable_cancel_keeps_actual_root_liability_and_never_rejudges() -> Result<()> {
    for durable_cancel in [false, true] {
        let f = fixture();
        let pattern = usable_pattern(&f);
        let server = Server::start(false).await?;
        server.judge_mode.store(2, Ordering::SeqCst);
        let result:Result<()>=async {
            let config=config(&f,&server); let suite=suite(&f);
            let service=reviewed_service(&f,&suite,&config,(!durable_cancel).then_some(50))?;
            let plan=service.plan_instruction_cycle(draft(&f,&service,&config,pattern.pattern_id)?,&config)?;
            let prefix=through_holdout(&service,&config,&plan).await?;
            let cancel=CancellationToken::new();
            let cleanup=CleanupScope::new(None,cancel.clone())?;
            let run=cleanup.run(service.run_instruction_plan(&config,&plan,&cancel));
            tokio::pin!(run);
            if durable_cancel {
                let wait=async {
                    while server.judge_count.load(Ordering::SeqCst)==0 {
                        tokio::select! { finished=&mut run=>anyhow::bail!("gate finished before cancellation: {finished:?}"), ()=tokio::time::sleep(Duration::from_millis(5))=>{} }
                    }
                    Ok::<_,anyhow::Error>(())
                };
                tokio::time::timeout(Duration::from_secs(10),wait).await??;
                service.cancel_experiment(&plan.spec.id,prefix.plan.digest())?;
                cancel.cancel();
            }
            assert!(tokio::time::timeout(Duration::from_secs(10),&mut run).await?.is_err());
            let journal=service.inspect_experiment(&plan.spec.id,prefix.plan.digest())?;
            assert!(!service.experiment_qualified(&journal)?);
            assert!(matches!(journal.state.stages["gate-review"],StageState::DispatchUnknown));
            let actual=service.experiment_physical_observations(&journal,&config)?;
            assert_eq!(server.judge_count.load(Ordering::SeqCst),1);
            assert_eq!(actual["gate-review"]["physical"]["calls"],1);
            assert!(actual["gate-review"]["needs_reconciliation"].as_bool().unwrap());
            assert!(!actual["gate-review"]["may_dispatch_again"].as_bool().unwrap());
            let sends=server.count.load(Ordering::SeqCst); let raw=std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?;
            let recovered=service.recover_experiment(&plan.spec.id,prefix.plan.digest())?;
            assert_eq!(recovered.head_sha256,journal.head_sha256);
            let replay_cancel=CancellationToken::new();
            assert!(CleanupScope::new(None,replay_cancel.clone())?.run(service.run_instruction_plan(&config,&plan,&replay_cancel)).await.is_err());
            assert_eq!(server.count.load(Ordering::SeqCst),sends);
            assert_eq!(std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?,raw);
            assert_eq!(f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,1);
            Ok(())
        }.await;
        server.finish().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn unknown_failed_judge_attempt_keeps_root_allowance_and_refuses_a_retry() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    server.judge_mode.store(4, Ordering::SeqCst);
    let result = async {
        let mut config = config(&f, &server);
        config.reliability.provider_retries = 1;
        let suite = suite(&f);
        let service = reviewed_service(&f, &suite, &config, None)?;
        let plan = service.plan_instruction_cycle(draft(&f, &service, &config, pattern.pattern_id)?, &config)?;
        let prefix = through_holdout(&service, &config, &plan).await?;
        let cancel = CancellationToken::new();
        let outcome = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await;
        let journal = service.inspect_experiment(&plan.spec.id, prefix.plan.digest())?;
        let actual = service.experiment_physical_observations(&journal, &config)?;
        assert_eq!(
            server.judge_count.load(Ordering::SeqCst),
            1,
            "unknown first 503 keeps its full allowance; retry cannot claim a refund"
        );
        assert_eq!(actual["gate-review"]["physical"]["calls"], 1);
        assert_eq!(actual["gate-review"]["physical"]["settled_calls"], 0);
        assert!(actual["gate-review"]["physical"]["unresolved"].as_bool().unwrap());
        assert!(outcome.is_err());
        assert!(!service.experiment_qualified(&journal)?);
        assert!(matches!(
            journal.state.stages["gate-review"],
            StageState::DispatchUnknown
        ));
        let root_held = journal.state.committed(&journal.plan)?;
        let prefix_used = prefix.state.committed(&prefix.plan)?;
        let allocation = plan
            .spec
            .stages
            .iter()
            .find(|stage| stage.id == "gate-review")
            .unwrap()
            .allocation;
        assert_eq!(
            root_held,
            prefix_used.checked_add(allocation)?,
            "unknown gate retains every original resource component"
        );
        let raw = std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?;
        let sends = server.count.load(Ordering::SeqCst);
        let recovered = service.recover_experiment(&plan.spec.id, prefix.plan.digest())?;
        assert_eq!(recovered.head_sha256, journal.head_sha256);
        let replay_cancel = CancellationToken::new();
        assert!(
            CleanupScope::new(None, replay_cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &replay_cancel))
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(
            std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?,
            raw
        );
        assert_eq!(f.store.active_binding(&f.pin.skill_id)?.unwrap().generation, 1);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn postcleanup_witness_write_failure_keeps_sealed_gate_and_prevents_a_second_judge() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = reviewed_service(&f, &suite, &config, None)?;
        let plan = service.plan_instruction_cycle(draft(&f, &service, &config, pattern.pattern_id)?, &config)?;
        let prefix = through_holdout(&service, &config, &plan).await?;
        // A directory at the future receipt path is a write fault, not a
        // mutation of existing immutable evidence. Gate and costs can persist.
        let blocked = f
            .store
            .root()
            .join("experiment-evidence")
            .join(plan.spec.id.as_str())
            .join("gate-review.gate-review.json");
        std::fs::create_dir(&blocked)?;
        let cancel = CancellationToken::new();
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        let journal = service.inspect_experiment(&plan.spec.id, prefix.plan.digest())?;
        assert!(matches!(
            journal.state.stages["gate-review"],
            StageState::DispatchUnknown
        ));
        assert!(!service.experiment_qualified(&journal)?);
        assert_eq!(server.judge_count.load(Ordering::SeqCst), 1);
        let actual = service.experiment_physical_observations(&journal, &config)?;
        assert_eq!(actual["gate-review"]["physical"]["calls"], 1);
        assert_eq!(actual["gate-review"]["physical"]["settled_calls"], 1);
        assert!(!actual["gate-review"]["physical"]["unresolved"].as_bool().unwrap());
        let validation = holdout::evaluation(&f, &journal, "final-evaluation")?;
        let proposal = f
            .store
            .read_proposal(&validation.receipt.proposal_evaluation.unwrap().proposal_id)?
            .unwrap();
        let gate = f.store.recorded_gate_for(&proposal)?.unwrap();
        assert!(gate.decision.experiment.is_some());
        assert_eq!(gate.decision.decision, GateDecision::Accept);
        assert!(blocked.is_dir());
        assert!(proposal.publication.is_none() && proposal.authorization.is_none());
        let sends = server.count.load(Ordering::SeqCst);
        let raw = std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?;
        let recovered = service.recover_experiment(&plan.spec.id, prefix.plan.digest())?;
        assert_eq!(recovered.head_sha256, journal.head_sha256);
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(server.judge_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(config.workspace_dir.join("state/costs.reservations.jsonl"))?,
            raw
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}
