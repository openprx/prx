//! Real production-builder tests with scripted loopback model responses.
use super::*;

fn probe_plan(
    f: &Fixture,
    service: &SkillsLifecycleService,
    config: &Config,
    pattern: PatternId,
) -> Result<(FrozenExperiment, BTreeMap<String, ExperimentOperation>)> {
    let (original, mut inputs) = plan(f, service, config, vec![pattern])?;
    inputs.insert(
        "baseline-probe".into(),
        ExperimentOperation::DevelopmentProbe { proposal_stage: None },
    );
    inputs.insert(
        "candidate-probe".into(),
        ExperimentOperation::DevelopmentProbe {
            proposal_stage: Some("proposal".into()),
        },
    );
    let mut spec = original.spec().clone();
    spec.id = ExperimentId::parse("development-probe-integration")?;
    let allocation = spec
        .stages
        .iter()
        .find(|stage| stage.id == "evaluation")
        .unwrap()
        .allocation;
    spec.stages.insert(
        0,
        ExperimentStage {
            id: "baseline-probe".into(),
            kind: ExperimentStageKind::SolverTrial,
            dependencies: vec![],
            input_sha256: inputs["baseline-probe"].digest()?,
            allocation,
        },
    );
    spec.stages.insert(
        2,
        ExperimentStage {
            id: "candidate-probe".into(),
            kind: ExperimentStageKind::SolverTrial,
            dependencies: vec!["proposal".into()],
            input_sha256: inputs["candidate-probe"].digest()?,
            allocation,
        },
    );
    let frozen = service.plan_experiment(spec, config)?;
    service.admit_experiment(frozen.spec().clone(), config)?;
    Ok((frozen, inputs))
}

#[tokio::test]
async fn development_probes_run_one_arm_and_preserve_independent_attempts() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let (plan, inputs) = probe_plan(&f, &service, &config, pattern.pattern_id)?;
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        for stage in ["baseline-probe", "proposal", "candidate-probe"] {
            let journal = run_stage(&service, &config, &plan, stage, &inputs[stage]).await?;
            assert!(journal.state.stages[stage].successful());
        }
        let journal = service.inspect_experiment(&plan.spec().id, plan.digest())?;
        let probes = service.experiment_development_probes(&journal)?;
        for stage in ["baseline-probe", "candidate-probe"] {
            let receipt: DevelopmentProbeReceipt = serde_json::from_value(probes[stage].clone())?;
            assert_eq!(receipt.cases.len(), 3, "a single arm must not repeat a paired baseline");
            assert!(receipt.complete());
            assert!(receipt.cases.iter().all(|case| case.split == EvalSplit::Development));
            assert!(
                receipt
                    .cases
                    .iter()
                    .all(|case| case.run.cleanup == crate::skills::lifecycle::CleanupState::Removed)
            );
        }
        assert_eq!(
            probes["baseline-probe"]["revision"],
            serde_json::to_value(&f.pin.revision_id)?
        );
        assert_eq!(
            f.store.list_evaluation_ids()?.len(),
            0,
            "probes cannot consume independent evaluation attempts"
        );
        let sends = server.count.load(Ordering::SeqCst);
        assert!(
            run_stage(&service, &config, &plan, "candidate-probe", &inputs["candidate-probe"])
                .await
                .is_err()
        );
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            sends,
            "probe replay must send nothing"
        );
        let binding = f.store.active_binding(&f.pin.skill_id)?.unwrap();
        assert_eq!(binding.revision_id, f.pin.revision_id);
        assert_eq!(binding.generation, f.pin.generation);
        assert!(!service.experiment_qualified(&journal)?);
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn development_probe_dependency_and_input_tampering_refuse_before_send() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let (plan, inputs) = probe_plan(&f, &service, &config, pattern.pattern_id)?;
        assert!(
            run_stage(&service, &config, &plan, "candidate-probe", &inputs["candidate-probe"])
                .await
                .is_err()
        );
        assert!(
            run_stage(&service, &config, &plan, "baseline-probe", &inputs["candidate-probe"])
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
