//! End-to-end automatic k1 schedule using actual runtime builders and loopback
//! transport. These are deterministic mechanism tests, not live Splash results.
use super::*;

pub(super) fn cycle(
    f: &Fixture,
    service: &SkillsLifecycleService,
    config: &Config,
    pattern: PatternId,
) -> Result<InstructionExperimentPlan> {
    cycle_with_schedule(f, service, config, pattern, InstructionSchedule::Single)
}

pub(super) fn cycle_with_schedule(
    f: &Fixture,
    service: &SkillsLifecycleService,
    config: &Config,
    pattern: PatternId,
    schedule: InstructionSchedule,
) -> Result<InstructionExperimentPlan> {
    let (_, inputs) = plan(f, service, config, vec![pattern.clone()])?;
    let proposal_allocation = ExperimentResources {
        calls: 4,
        tokens: 131_072,
        micro_usd: 100_000,
        tool_calls: 0,
        output_bytes: 131_072,
        scratch_bytes: 8 * 1024 * 1024,
    };
    let probe_allocation = ExperimentResources {
        calls: 64,
        tokens: 500_000,
        micro_usd: 500_000,
        tool_calls: 64,
        output_bytes: 500_000,
        scratch_bytes: 32 * 1024 * 1024,
    };
    assert!(inputs.contains_key("proposal"));
    let resources = if schedule == InstructionSchedule::Single {
        ExperimentResources {
            calls: 512,
            tokens: 4_000_000,
            micro_usd: 4_000_000,
            tool_calls: 512,
            output_bytes: 4_000_000,
            scratch_bytes: 256 * 1024 * 1024,
        }
    } else {
        ExperimentResources {
            calls: 1024,
            tokens: 8_000_000,
            micro_usd: 8_000_000,
            tool_calls: 1024,
            output_bytes: 8_000_000,
            scratch_bytes: 512 * 1024 * 1024,
        }
    };
    service.plan_instruction_cycle(
        InstructionCycleDraft {
            id: ExperimentId::parse("automatic-instruction-cycle")?,
            skill_id: f.pin.skill_id.clone(),
            schedule,
            patterns: vec![pattern],
            verification_split: EvalSplit::Validation,
            resources,
            proposal_allocation,
            probe_allocation,
            evaluation_allocation: probe_allocation,
            holdout_allocation: None,
            elapsed_ms: 300_000,
            max_input_bytes: 64 * 1024,
            max_output_bytes: 16 * 1024,
            proposal_timeout_ms: 120_000,
        },
        config,
    )
}

#[tokio::test]
async fn instruction_cycle_runs_actual_four_probes_three_edits_then_independent_validation() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle(&f, &service, &config, pattern.pattern_id)?;
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        let admitted = service.admit_experiment(plan.spec.clone(), &config)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert!(
            service.experiment_qualified(&journal)?,
            "cycle must independently qualify in the improving fixture"
        );
        let probes = service.experiment_development_probes(&journal)?;
        assert_eq!(probes.as_object().unwrap().len(), 4);
        let case_runs: usize = probes
            .as_object()
            .unwrap()
            .values()
            .map(|probe| probe["cases"].as_array().unwrap().len())
            .sum();
        assert_eq!(case_runs, 12, "four logical slots with three development cases each");
        assert_eq!(f.store.list_evaluation_ids()?.len(), 1);
        let bodies = server.bodies.lock().clone();
        let editors: Vec<_> = bodies
            .iter()
            .filter(|body| {
                body["messages"].as_array().unwrap().iter().any(|message| {
                    message["role"] == "system"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("You revise one instruction skill"))
                })
            })
            .collect();
        assert_eq!(editors.len(), 3);
        let compose = editors
            .iter()
            .find(|body| body["messages"].to_string().contains("H2 role: compose."))
            .unwrap();
        let content = compose["messages"].to_string();
        assert!(content.contains("observed_development_contrasts"));
        assert!(content.contains("general-probe") && content.contains("domain-probe"));
        assert!(
            !content.contains("validation-0") && !content.contains("holdout-0"),
            "independent case identities cannot enter editor context"
        );
        drop(bodies);
        let sends = server.count.load(Ordering::SeqCst);
        let repeated = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&config, &plan, &cancel))
            .await?;
        assert_eq!(repeated.head_sha256, journal.head_sha256);
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            sends,
            "whole-plan replay must send nothing"
        );
        assert_eq!(admitted.plan.digest(), journal.plan.digest());
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
            f.pin.generation,
            "Shadow cannot publish"
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn instruction_cycle_checks_all_operations_before_first_send() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let mut plan = cycle(&f, &service, &config, pattern.pattern_id)?;
        service.admit_experiment(plan.spec.clone(), &config)?;
        plan.operations.insert(
            "verify".into(),
            ExperimentOperation::DevelopmentProbe { proposal_stage: None },
        );
        let cancel = CancellationToken::new();
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await
                .is_err()
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn instruction_cycle_retains_baseline_on_ties_and_rejects_harmful_composition() -> Result<()> {
    for mode in [1, 2] {
        let f = fixture();
        let pattern = usable_pattern(&f);
        let server = Server::start(false).await?;
        server.h2_mode.store(mode, Ordering::SeqCst);
        let result: Result<()> = async {
            let config = config(&f, &server);
            let suite = suite(&f);
            let service = service(&f, &suite);
            let plan = cycle(&f, &service, &config, pattern.pattern_id)?;
            let admitted = service.admit_experiment(plan.spec.clone(), &config)?;
            let cancel = CancellationToken::new();
            let journal = CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await?;
            let choice = journal
                .entries
                .iter()
                .find_map(|entry| match &entry.event {
                    ExperimentEvent::Settled { stage, reference, .. } if stage == "select" => reference.as_deref(),
                    _ => None,
                })
                .unwrap();
            if mode == 1 {
                assert_eq!(choice, "baseline");
                assert_eq!(
                    f.store.list_evaluation_ids()?.len(),
                    0,
                    "neutral search must spend no independent evaluation allowance"
                );
                assert!(!service.experiment_qualified(&journal)?);
                assert!(matches!(journal.state.stages["verify"], StageState::Planned));
            } else {
                assert_eq!(
                    choice, "selected:general",
                    "harmful composition cannot displace an eligible raw probe"
                );
                let probes = service.experiment_development_probes(&journal)?;
                let composed: DevelopmentProbeReceipt = serde_json::from_value(probes["compose-probe"].clone())?;
                assert!(composed.complete());
                assert_eq!(composed.passed(), 0);
                assert!(service.experiment_qualified(&journal)?);
            }
            let sends = server.count.load(Ordering::SeqCst);
            let repeated = CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&config, &plan, &cancel))
                .await?;
            assert_eq!(repeated.head_sha256, journal.head_sha256);
            assert_eq!(server.count.load(Ordering::SeqCst), sends);
            assert_eq!(admitted.plan.digest(), journal.plan.digest());
            assert_eq!(
                f.store.active_binding(&f.pin.skill_id)?.unwrap().generation,
                f.pin.generation
            );
            Ok(())
        }
        .await;
        server.finish().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn instruction_cycle_cannot_compose_from_changed_probe_evidence() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let plan = cycle(&f, &service, &config, pattern.pattern_id)?;
        let admitted = service.admit_experiment(plan.spec.clone(), &config)?;
        for stage in ["baseline", "general", "domain", "general-probe", "domain-probe"] {
            run_stage(&service, &config, &admitted.plan, stage, &plan.operations[stage]).await?;
        }
        let evidence = f
            .store
            .root()
            .join("experiment-evidence")
            .join(plan.spec.id.as_str())
            .join("general-probe.json");
        let mut altered: serde_json::Value = serde_json::from_slice(&std::fs::read(&evidence)?)?;
        altered["executor"] = json!("forged-executor");
        std::fs::write(evidence, serde_json::to_vec(&altered)?)?;
        let sends = server.count.load(Ordering::SeqCst);
        assert!(
            run_stage(
                &service,
                &config,
                &admitted.plan,
                "compose",
                &plan.operations["compose"]
            )
            .await
            .is_err()
        );
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            sends,
            "tampered contrasts must be refused before editor dispatch"
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}
