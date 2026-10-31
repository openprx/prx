//! Full owner-chain regression with the existing loopback HTTP provider and
//! native filesystem. No constructor below mints a receipt, grant or budget.
use super::*;
use crate::skills::lifecycle::study::*;
use crate::skills::lifecycle::{ExperimentProductHandle, ExperimentPublishRequest};
use sha2::{Digest, Sha256};

fn effect_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn study_draft(f: &Fixture, origin: &ExperimentJournal, id: &str, phase: StudyPhase) -> Result<StudyDraft> {
    let tools: Vec<String> = vec!["file_write".into(), "file_edit".into(), "file_read".into()];
    let mut case_tools = BTreeMap::new();
    let roots = (0..2).map(|number| {
        let case_id = format!("{id}-case-{number}");
        case_tools.insert(case_id.clone(), tools.clone());
        StudyRoot { root_task_id: format!("{id}-root-{number}"), cases: vec![EvalCase {
            case_id, split: if phase == StudyPhase::Calibration { EvalSplit::Development } else { EvalSplit::Holdout },
            task_family: "deploy".into(),
            prompt: format!("Independent {id} release task {number}: Use skill_read to read Deploy helper. Use file_write to create artifact.txt, file_edit to correct its migration marker, then file_read to verify artifact.txt. Follow the skill and report the migration result."),
            fixtures: vec![], validators: vec![Validator::ArtifactSha256 { path: "artifact.txt".into(), sha256: effect_hash(b"VERIFIED\n") }],
            expect_skill_read: true, critical: true, max_elapsed_ms: 30_000,
        }] }
    }).collect();
    Ok(StudyDraft {
        schema_version: STUDY_SCHEMA_VERSION,
        id: ExperimentId::parse(id)?,
        skill_id: f.pin.skill_id.clone(),
        origin: StudyExperimentRef {
            id: origin.plan.spec().id.clone(),
            sha256: origin.plan.digest().into(),
        },
        gate_stage: "gate-review".into(),
        additional_optimizer_roots: vec![],
        protocol: StudyProtocol {
            protocol_id: format!("{id}-protocol"),
            phase,
            primary_metric: StudyMetric::Success,
            interval: StudyInterval::HoeffdingIndependentRootMeans,
            stop_rule: StudyStopRule::CompleteFrozenPlanOrSafetyAbort,
            alpha_ppm: 50_000,
            practical_gain_ppm: 100_000,
            min_independent_roots: 2,
            max_interval_half_width_ppm: 2_000_000,
            max_arm_calls: 128,
            max_arm_tokens: 1_000_000,
            max_arm_micro_usd: 1_000_000,
            max_arm_elapsed_ms: 300_000,
            calibration: None,
        },
        roots,
        repetitions: 1,
        order_seed: 19,
        required_tools: tools,
        case_tools,
        side_effects: SideEffects::WorkspaceWrite,
        resources: ExperimentResources {
            calls: 128,
            tokens: 1_000_000,
            micro_usd: 1_000_000,
            tool_calls: 128,
            output_bytes: 4 * 1024 * 1024,
            scratch_bytes: 128 * 1024 * 1024,
        },
        per_trial: ExperimentResources {
            calls: 16,
            tokens: 64_000,
            micro_usd: 100_000,
            tool_calls: 16,
            output_bytes: 256 * 1024,
            scratch_bytes: 16 * 1024 * 1024,
        },
        elapsed_ms: 300_000,
    })
}
async fn run_study(service: &Arc<SkillsLifecycleService>, cfg: &Config, plan: &StudyPlan) -> Result<StudyStatus> {
    let cancel = CancellationToken::new();
    CleanupScope::new(None, cancel.clone())?
        .run(service.run_v01_study(
            cfg,
            plan.manifest.id.clone(),
            plan.experiment.spec.clone().freeze()?.digest().into(),
            plan.manifest.sha256()?,
            &cancel,
        ))
        .await
}
fn closed_report(service: &SkillsLifecycleService, cfg: &Config, plan: &StudyPlan) -> Result<StudyReport> {
    service.report_v01_study(
        cfg,
        &plan.manifest.id,
        plan.experiment.spec.clone().freeze()?.digest(),
        &plan.manifest.sha256()?,
    )
}
fn no_oracle_feedback<T: serde::Serialize>(value: &T, plan: &StudyPlan) -> Result<()> {
    let json = serde_json::to_value(value)?;
    let text = serde_json::to_string(&json)?;
    for forbidden in [
        "quality_ppm",
        "validators",
        "expected",
        "artifact_sha256",
        "invalid_reasons",
    ] {
        assert!(
            !text.contains(forbidden),
            "open owner read exposed private grading field {forbidden}"
        );
    }
    assert!(!text.contains(&effect_hash(b"VERIFIED\n")));
    for root in &plan.manifest.roots {
        for case in &root.cases {
            assert!(!text.contains(&case.prompt));
        }
    }
    Ok(())
}

#[tokio::test]
async fn v01_loopback_keeps_original_owner_denominator_private_oracles_and_sealed_physical_evidence() -> Result<()> {
    let f = fixture();
    let server = Server::start_mode(false, Some(false)).await?;
    let result = async {
        let cfg = config(&f, &server);
        let suite = suite_mode(&f, true);
        let reviewed = gate_review::reviewed_service(&f, &suite, &cfg, None)?;
        let mut settings = reviewed.settings().clone();
        settings.mode = LifecycleMode::Auto;
        let service = Arc::new(SkillsLifecycleService::new(f.store.clone(), settings)?);
        service.install_operator_suite(&suite)?;
        let draft = gate_review::draft(&f, &service, &cfg, usable_pattern(&f).pattern_id)?;
        let origin = service.plan_instruction_cycle(draft, &cfg)?;
        service.admit_experiment(origin.spec.clone(), &cfg)?;
        let cancel = CancellationToken::new();
        let journal = CleanupScope::new(None, cancel.clone())?
            .run(service.run_instruction_plan(&cfg, &origin, &cancel))
            .await?;
        assert!(
            service.experiment_qualified(&journal)?,
            "the original exact-paired gate must genuinely pass before a study exists"
        );
        // Immutable result resolution is the original store owner, followed by
        // the normal product authorizer/publisher. No manual publication row.
        let expected = f
            .store
            .resolve_experiment_final(&ExperimentProductHandle::of(&origin)?, "gate-review")?;
        let publication = service.publish_final_product(&ExperimentPublishRequest { expected })?;
        assert_eq!(
            publication.transaction.state,
            crate::skills::lifecycle::TransactionState::Committed
        );

        let before_plan = server.count.load(Ordering::SeqCst);
        let calibration = service.plan_v01_study(
            study_draft(&f, &journal, "v01-calibration", StudyPhase::Calibration)?,
            &cfg,
        )?;
        assert_eq!(
            before_plan,
            server.count.load(Ordering::SeqCst),
            "planning does not dispatch physical calls"
        );
        let frozen = calibration.experiment.spec.clone().freeze()?;
        let digest = frozen.digest().to_owned();
        let study_sha = calibration.manifest.sha256()?;
        service.admit_v01_study(calibration.clone(), &cfg)?;
        let first_pair = calibration.manifest.pairs.first().context("first pair")?;
        // Complete both arms of one pair, leaving the next pair undispatched.
        for arm in [first_pair.first_arm, first_pair.first_arm.other()] {
            let stage = first_pair.stage_id(arm);
            let partial = run_stage(
                &service,
                &cfg,
                &frozen,
                &stage,
                calibration
                    .experiment
                    .operations
                    .get(&stage)
                    .context("frozen operation")?,
            )
            .await?;
            assert!(
                partial.state.stages.get(&stage).is_some_and(StageState::successful),
                "public execution status cannot reveal the baseline's failed private artifact grade"
            );
        }
        let status = service.inspect_v01_study(&cfg, &calibration.manifest.id, &digest, &study_sha)?;
        assert!(!status.protocol_closed && !status.qualification);
        assert_eq!(status.settled_and_clean_stages, 2);
        no_oracle_feedback(&status, &calibration)?;
        let generic = service.inspect_experiment(&calibration.manifest.id, &digest)?;
        no_oracle_feedback(
            &serde_json::json!({
                "spec": generic.plan.spec(),
                "plan_sha256": generic.plan.digest(),
                "entries": &generic.entries,
                // Match the existing CLI public projection and also scan
                // every in-memory state field without adding production serde.
                "state_debug": format!("{:?}", generic.state),
                "committed": generic.state.committed(&generic.plan)?,
                "cancelled": generic.state.cancelled,
                "qualified": service.experiment_qualified(&generic)?,
                "published": false,
                "physical_observations": service.experiment_physical_observations(&generic, &cfg)?,
                "development_probes": service.experiment_development_probes(&generic)?,
                "head_sha256": &generic.head_sha256,
            }),
            &calibration,
        )?;
        // A separately configured read service uses the public product DTO.
        // It receives no custom judge identity, stage capability or grant.
        let mut observer_config = cfg.clone();
        observer_config.skills_lifecycle.enabled = true;
        observer_config.skills_lifecycle.owner_scope = Some(f.store.owner_scope().as_str().into());
        observer_config.skills_lifecycle.mode = crate::config::skills_lifecycle::SkillsLifecycleModeConfig::Shadow;
        observer_config.skills_lifecycle.suites = vec![crate::config::skills_lifecycle::SkillSuiteRegistration {
            skill_id: f.pin.skill_id.as_str().into(),
            suite_digest: suite.digest().into(),
        }];
        let observer = SkillsLifecycleService::new(f.store.clone(), observer_config.skills_lifecycle.to_settings()?)?;
        let product =
            observer.inspect_product(&ExperimentProductHandle::of(&calibration.experiment)?, &observer_config)?;
        no_oracle_feedback(&product, &calibration)?;

        assert!(closed_report(&service, &cfg, &calibration).is_err());
        assert!(
            service
                .export_v01_study(&cfg, &calibration.manifest.id, &digest, &study_sha)
                .is_err()
        );

        // Whole-map preflight must inspect even an already-settled operation
        // before dispatching any later valid stage. A lazy per-stage check fails.
        let mut corrupt = calibration.experiment.clone();
        let first_stage = corrupt.spec.stages.first().context("stage")?.id.clone();
        let ExperimentOperation::StudyTrial { input } =
            corrupt.operations.get_mut(&first_stage).context("operation")?
        else {
            anyhow::bail!("not a study")
        };
        input.arm = input.arm.other();
        let calls = server.count.load(Ordering::SeqCst);
        let head = generic.head_sha256.clone();
        let physical_before = std::fs::read(f.tmp.path().join("state/costs.jsonl"))?;
        let reservations_before = std::fs::read(f.tmp.path().join("state/costs.reservations.jsonl"))?;
        let cancel = CancellationToken::new();
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&cfg, &corrupt, &cancel))
                .await
                .is_err()
        );
        assert_eq!(calls, server.count.load(Ordering::SeqCst));
        assert_eq!(
            head,
            service
                .inspect_experiment(&calibration.manifest.id, &digest)?
                .head_sha256
        );
        for mixed in [false, true] {
            let mut spec = calibration.experiment.spec.clone();
            spec.id = ExperimentId::parse(if mixed {
                "study-mixed-admission"
            } else {
                "study-unsealed-admission"
            })?;
            if mixed {
                spec.stages.last_mut().context("last stage")?.kind = ExperimentStageKind::GateReview;
            }
            assert!(service.admit_experiment(spec, &cfg).is_err());
        }
        let mut downgraded = calibration.experiment.clone();
        for stage in &mut downgraded.spec.stages {
            stage.kind = ExperimentStageKind::SolverTrial;
        }
        let cancel = CancellationToken::new();
        assert!(
            CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&cfg, &downgraded, &cancel))
                .await
                .is_err()
        );
        assert_eq!(calls, server.count.load(Ordering::SeqCst));

        assert_eq!(physical_before, std::fs::read(f.tmp.path().join("state/costs.jsonl"))?);
        assert_eq!(
            reservations_before,
            std::fs::read(f.tmp.path().join("state/costs.reservations.jsonl"))?
        );
        let mut changed_pin = calibration.clone();
        changed_pin.manifest.runtime.model.temperature_milli += 1;
        assert!(service.admit_v01_study(changed_pin, &cfg).is_err());
        assert_eq!(calls, server.count.load(Ordering::SeqCst));
        let mut changed_config = cfg.clone();
        changed_config.default_model = Some("unadmitted-changed-model".into());
        assert!(run_study(&service, &changed_config, &calibration).await.is_err());
        assert_eq!(calls, server.count.load(Ordering::SeqCst));
        let finished = run_study(&service, &cfg, &calibration).await?;
        assert!(finished.protocol_closed && !finished.qualification);
        let report = closed_report(&service, &cfg, &calibration)?;
        assert_eq!(report.baseline.declared, 2);
        assert_eq!(report.h2.declared, 2);
        assert_eq!(report.baseline.successes, 0);
        assert_eq!(
            report.h2.successes, 2,
            "real native file effect, not prose, determines success"
        );
        assert_eq!(report.analysis.independent_roots, 2);
        assert_eq!(report.analysis.environment_mismatches, 0);
        assert!(report.analysis.mechanism_complete);
        assert_eq!(report.analysis.benefit, StudyBenefitDecision::Inconclusive);
        assert!(!report.analysis.qualification && report.analysis.retain_baseline_or_shadow);
        assert!(report.rows.iter().all(|row| row.physical.calls > 0
            && row.physical.calls == row.physical.settled_calls
            && !row.physical.unresolved
            && !row.physical.attempt_ids.is_empty()));
        assert!(
            report
                .rows
                .iter()
                .all(|row| row.retained_liability == ExperimentResources::default())
        );
        let mut unique_attempts = std::collections::BTreeSet::new();
        let mut unique_scopes = std::collections::BTreeSet::new();
        for row in &report.rows {
            assert!(unique_scopes.insert(row.physical.scope_id.clone()));
            let actual = crate::cost::tracker::CostTracker::physical_scope_usage_read_only(
                f.tmp.path(),
                &row.physical.scope_id,
            )?;
            assert_eq!(actual.calls, row.physical.calls);
            assert_eq!(actual.tokens, row.physical.tokens);
            assert_eq!(actual.micro_usd, row.physical.micro_usd);
            assert_eq!(row.settled_usage.calls, actual.calls);
            for attempt in &row.physical.attempt_ids {
                assert!(unique_attempts.insert(attempt.clone()));
            }
        }
        let exact_calls = server.count.load(Ordering::SeqCst);
        run_study(&service, &cfg, &calibration).await?;
        assert_eq!(
            exact_calls,
            server.count.load(Ordering::SeqCst),
            "exact replay inspects; never redispatches"
        );
        assert_eq!(report.sha256()?, closed_report(&service, &cfg, &calibration)?.sha256()?);

        let mut confirm = study_draft(&f, &journal, "v01-confirmatory", StudyPhase::Confirmatory)?;
        confirm.protocol.calibration = Some(StudyCalibration {
            study: StudyExperimentRef {
                id: calibration.manifest.id.clone(),
                sha256: digest.clone(),
            },
            report_sha256: report.sha256()?,
            excluded_roots: vec![],
            sample_and_uncertainty_rationale:
                "Two independent mock root inputs test owner mechanics; no live efficacy assertion.".into(),
            independent_root_sampling_basis: "Distinct frozen mock task inputs; within-root calls are not samples."
                .into(),
        });
        let mut bad_pin = confirm.clone();
        bad_pin
            .protocol
            .calibration
            .as_mut()
            .context("calibration ref")?
            .report_sha256 = effect_hash(b"forged report");
        assert!(service.plan_v01_study(bad_pin, &cfg).is_err());
        let mut relabelled_calibration = confirm.clone();
        for (root, old) in relabelled_calibration.roots.iter_mut().zip(&calibration.manifest.roots) {
            root.cases.first_mut().context("case")?.prompt = old.cases.first().context("old case")?.prompt.clone();
        }
        assert!(
            service.plan_v01_study(relabelled_calibration, &cfg).is_err(),
            "renaming case/root/partition cannot erase actual calibration source content"
        );
        let confirm = service.plan_v01_study(confirm, &cfg)?;
        assert!(
            !confirm
                .manifest
                .protocol
                .calibration
                .as_ref()
                .context("calibration")?
                .excluded_roots
                .is_empty()
        );
        service.admit_v01_study(confirm.clone(), &cfg)?;
        assert_eq!(exact_calls, server.count.load(Ordering::SeqCst));
        run_study(&service, &cfg, &confirm).await?;
        let confirmed = closed_report(&service, &cfg, &confirm)?;
        assert_eq!(
            confirmed.analysis.benefit,
            StudyBenefitDecision::Inconclusive,
            "two roots cannot establish the preregistered gain even with positive mock outcomes"
        );
        assert_eq!(
            confirmed.calibration_physical_attempt_ids.len(),
            report.rows.iter().flat_map(|row| &row.physical.attempt_ids).count()
        );
        assert!(confirmed.calibration_settled_usage.is_some());

        // Actual evidence bytes remain necessary after closure. Neither a
        // matching receipt hash nor caller-edited usage can self-authenticate.
        let row = confirmed
            .rows
            .iter()
            .find(|row| row.input.arm == StudyArm::H2)
            .context("H2 row")?;
        let directory = f.store.root().join("studies").join(confirm.manifest.id.as_str());
        let artifact = directory.join(format!("effect-{}-{}.bin", row.stage, effect_hash(b"VERIFIED\n")));
        let original_artifact = std::fs::read(&artifact)?;
        std::fs::write(&artifact, b"TAMPERED\n")?;
        assert!(closed_report(&service, &cfg, &confirm).is_err());
        std::fs::write(&artifact, &original_artifact)?;
        let receipt = directory.join(format!("{}.json", row.stage));
        let original_receipt = std::fs::read(&receipt)?;
        let mut forged: serde_json::Value = serde_json::from_slice(&original_receipt)?;
        forged["value"]["physical"]["calls"] = serde_json::json!(0);
        std::fs::write(&receipt, serde_json::to_vec(&forged)?)?;
        assert!(closed_report(&service, &cfg, &confirm).is_err());
        std::fs::write(&receipt, &original_receipt)?;
        assert_eq!(confirmed.sha256()?, closed_report(&service, &cfg, &confirm)?.sha256()?);
        let export = service.export_v01_study(
            &cfg,
            &confirm.manifest.id,
            confirm.experiment.spec.clone().freeze()?.digest(),
            &confirm.manifest.sha256()?,
        )?;
        assert!(
            serde_json::from_slice::<serde_json::Value>(&export)?
                .get("mac")
                .is_some()
        );
        Ok(())
    }
    .await;
    let shutdown = server.finish().await;
    result?;
    shutdown
}
