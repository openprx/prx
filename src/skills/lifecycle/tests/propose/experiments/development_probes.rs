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
async fn r4_builtin_skill_solver_cannot_read_its_instruction_file() -> Result<()> {
    // pinned until P-DECISION-SCORER-01 recalibrates the builtin scorer
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let config = config(&f, &server);
        let suite = suite(&f);
        let service = service(&f, &suite);
        let (plan, inputs) = probe_plan(&f, &service, &config, pattern.pattern_id)?;
        let error = run_stage(&service, &config, &plan, "baseline-probe", &inputs["baseline-probe"])
            .await
            .expect_err("a solver without its required read must not qualify");
        assert!(error.to_string().contains("physical usage is unresolved"), "{error:#}");
        let journal = service.inspect_experiment(&plan.spec().id, plan.digest())?;
        assert!(!journal.state.stages["baseline-probe"].successful());
        // Unsettled physical usage retains the evidence without a Settled journal entry.
        let receipt: DevelopmentProbeReceipt = serde_json::from_slice(&std::fs::read(
            f.store
                .root()
                .join("experiment-evidence")
                .join(plan.spec().id.as_str())
                .join("baseline-probe.json"),
        )?)?;
        assert!(!receipt.complete());
        assert!(receipt.cases.iter().all(|case| case.run.hydrations.is_empty()));
        let bodies = server.bodies.lock();
        let solver = bodies
            .iter()
            .find(|body| {
                body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "user"
                            && message["content"]
                                .as_str()
                                .is_some_and(|text| text.contains("Use skill_read to read Deploy helper"))
                    })
                })
            })
            .expect("the real builtin fallback Answer must reach the serving model");
        assert!(
            !solver["tools"].as_array().is_some_and(|tools| tools
                .iter()
                .any(|tool| tool["function"]["parameters"]["properties"]["name"]["const"] == "Deploy helper")),
            "the fallback Answer cannot offer the instruction-read alias: {solver}"
        );
        assert!(
            receipt.cases.iter().any(|case| !case.run.failure_detail.is_empty()),
            "the scripted solver cannot complete its required instruction read"
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn development_probes_run_one_arm_and_preserve_independent_attempts() -> Result<()> {
    crate::runtime::turn_decision::with_scored_publication("skill", async {
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
                if !journal.state.stages[stage].successful() {
                    let evidence = json!({
                        "stage":stage, "entries":journal.entries,
                        "probes":service.experiment_development_probes(&journal)?,
                        "proposals":f.store.list_proposals()?,
                        "physical":service.experiment_physical_observations(&journal,&config)?,
                        "requests":server.bodies.lock().clone(),
                    });
                    let path = std::env::temp_dir().join(format!("prx-m2-probe-failure-{}.json", uuid::Uuid::new_v4()));
                    std::fs::write(&path, serde_json::to_vec_pretty(&evidence)?)?;
                    panic!(
                        "development stage {stage} is nonqualifying; retained exact evidence at {}",
                        path.display()
                    );
                }
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
    })
    .await
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
