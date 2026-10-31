//! Product DTOs over original journals, physical calls and owned cleanup.
use super::*;
use crate::skills::lifecycle::{
    ExperimentCatalogRequest, ExperimentCleanupStatus, ExperimentExecutionStatus, ExperimentProductFailureKind,
    ExperimentProductHandle, ExperimentRootDrainStatus, ExperimentStageStatus,
};

fn configured_service(f: &Fixture, server: &Server) -> Result<(Config, Arc<SkillsLifecycleService>)> {
    let mut cfg = config(f, server);
    let suite = suite(f);
    cfg.skills_lifecycle.enabled = true;
    cfg.skills_lifecycle.owner_scope = Some(f.store.owner_scope().as_str().into());
    cfg.skills_lifecycle.mode = crate::config::skills_lifecycle::SkillsLifecycleModeConfig::Shadow;
    cfg.skills_lifecycle.suites = vec![crate::config::skills_lifecycle::SkillSuiteRegistration {
        skill_id: f.pin.skill_id.as_str().into(),
        suite_digest: suite.digest().into(),
    }];
    let service = Arc::new(SkillsLifecycleService::new(
        f.store.clone(),
        cfg.skills_lifecycle.to_settings()?,
    )?);
    service.install_operator_suite(&suite)?;
    Ok((cfg, service))
}
fn instruction_plan(
    frozen: &FrozenExperiment,
    operations: BTreeMap<String, ExperimentOperation>,
) -> InstructionExperimentPlan {
    InstructionExperimentPlan {
        spec: frozen.spec().clone(),
        operations,
    }
}

#[tokio::test]
async fn product_between_stages_is_incomplete_and_only_the_owned_run_can_confirm_root_drain() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let (cfg, service) = configured_service(&f, &server)?;
        let (frozen, operations) = plan(&f, &service, &cfg, vec![pattern.pattern_id])?;
        let plan = instruction_plan(&frozen, operations);
        let handle = ExperimentProductHandle::of(&plan)?;
        let initial = service.inspect_product(&handle, &cfg)?;
        assert_eq!(initial.execution, ExperimentExecutionStatus::Planned);
        assert_eq!(initial.cleanup, ExperimentCleanupStatus::NotStarted);
        assert!(!initial.termination_confirmed);
        let journal = run_stage(&service, &cfg, &frozen, "proposal", &plan.operations["proposal"]).await?;
        assert!(
            journal.state.stages["proposal"].successful(),
            "real stage must settle and clean"
        );
        let partial = service.inspect_product(&handle, &cfg)?;
        assert_eq!(partial.execution, ExperimentExecutionStatus::Incomplete);
        assert!(
            partial
                .stages
                .iter()
                .any(|stage| stage.state == ExperimentStageStatus::Planned)
        );
        assert!(
            partial
                .stages
                .iter()
                .any(|stage| stage.state == ExperimentStageStatus::Settled && stage.cleanup_confirmed)
        );
        assert!(
            !partial.termination_confirmed,
            "settled stage plus future work does not prove root exit"
        );
        assert_eq!(partial.root_worker_drain, ExperimentRootDrainStatus::Unconfirmed);
        let completed = service.run_product(&plan, &cfg, &CancellationToken::new()).await?;
        assert_eq!(completed.execution, ExperimentExecutionStatus::Completed);
        assert_eq!(completed.root_worker_drain, ExperimentRootDrainStatus::Confirmed);
        assert!(
            completed.termination_confirmed,
            "original run must finish its actual cleanup drain"
        );
        let sends = server.count.load(Ordering::SeqCst);
        let historical = service.inspect_product(&handle, &cfg)?;
        assert_eq!(historical.execution, ExperimentExecutionStatus::Completed);
        assert!(
            !historical.termination_confirmed,
            "read-only history cannot mint a current drain receipt"
        );
        assert_eq!(historical.root_worker_drain, ExperimentRootDrainStatus::Unconfirmed);
        for _ in 0..2 {
            let cancelled = service.cancel_product(&handle, &cfg)?;
            assert_eq!(cancelled.journal_head_sha256, historical.journal_head_sha256);
            assert!(
                !cancelled.cancel_requested,
                "terminal cancellation is an idempotent history read"
            );
        }
        assert_eq!(server.count.load(Ordering::SeqCst), sends);
        assert_eq!(
            f.store.active_binding(&f.pin.skill_id)?.context("baseline")?.generation,
            f.pin.generation
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn product_cancelled_unknown_retains_full_original_liability_without_fabricating_cleanup_or_replay() -> Result<()>
{
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(true).await?;
    let result = async {
        let (cfg, service) = configured_service(&f, &server)?;
        let (frozen, operations) = plan(&f, &service, &cfg, vec![pattern.pattern_id])?;
        let plan = instruction_plan(&frozen, operations);
        let handle = ExperimentProductHandle::of(&plan)?;
        let mut running = Box::pin(run_stage(
            &service,
            &cfg,
            &frozen,
            "proposal",
            &plan.operations["proposal"],
        ));
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    result = &mut running => anyhow::bail!("provider ended before the actual cancellation: {result:?}"),
                    () = tokio::time::sleep(Duration::from_millis(20)) => {
                        if server.count.load(Ordering::SeqCst) == 1 { break Ok::<(), anyhow::Error>(()); }
                    }
                }
            }
        })
        .await??;
        let requested = service.cancel_product(&handle, &cfg)?;
        assert!(requested.cancel_requested);
        assert!(!requested.termination_confirmed);
        assert!(tokio::time::timeout(Duration::from_secs(20), running).await?.is_err());
        let unknown = service.recover_product(&handle, &cfg)?;
        assert_eq!(unknown.execution, ExperimentExecutionStatus::Unknown);
        assert_eq!(unknown.cleanup, ExperimentCleanupStatus::Unknown);
        assert_eq!(
            unknown.committed_or_reserved,
            frozen.spec().stages.first().context("stage")?.allocation
        );
        assert!(!unknown.termination_confirmed);
        assert_eq!(unknown.root_worker_drain, ExperimentRootDrainStatus::Unconfirmed);
        let repeated = service.cancel_product(&handle, &cfg)?;
        assert_eq!(repeated.journal_head_sha256, unknown.journal_head_sha256);
        assert_eq!(repeated.committed_or_reserved, unknown.committed_or_reserved);
        assert!(
            service
                .run_product(&plan, &cfg, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(
            server.count.load(Ordering::SeqCst),
            1,
            "unknown attempt never silently resends"
        );
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn product_catalog_uses_fixed_existing_owner_and_stale_or_foreign_config_cannot_redirect_it() -> Result<()> {
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let result = async {
        let (cfg, service) = configured_service(&f, &server)?;
        let result = service.catalog_product(&ExperimentCatalogRequest::default(), &cfg)?;
        assert!(result.skills.iter().any(|skill| {
            skill.skill_id == f.pin.skill_id
                && skill
                    .patterns
                    .iter()
                    .any(|candidate| candidate.pattern_id == pattern.pattern_id.as_str())
        }));
        assert!(
            serde_json::from_value::<ExperimentCatalogRequest>(
                json!({"after_skill":null,"owner_scope":"owner:ws:cli:prx_other"})
            )
            .is_err()
        );
        let mut foreign = cfg.clone();
        foreign.skills_lifecycle.owner_scope = Some("owner:ws:cli:prx_other".into());
        assert_eq!(
            service
                .catalog_product(&ExperimentCatalogRequest::default(), &foreign)
                .expect_err("foreign owner denied")
                .kind,
            ExperimentProductFailureKind::WrongOwner
        );
        let mut stale = cfg.clone();
        stale.skills_lifecycle.max_edit_tokens += 1;
        assert_eq!(
            service
                .catalog_product(&ExperimentCatalogRequest::default(), &stale)
                .expect_err("changed service settings denied")
                .kind,
            ExperimentProductFailureKind::Stale
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 0);
        assert_eq!(service.owner_scope(), f.store.owner_scope());
        Ok(())
    }
    .await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn product_plan_revalidates_discovered_candidate_after_record_loss_corruption_or_contradiction() -> Result<()> {
    for fault in ["missing", "corrupt", "contradicted"] {
        let f = fixture();
        let original = usable_pattern(&f);
        let server = Server::start(false).await?;
        let result: Result<()> = async {
            let (cfg, service) = configured_service(&f, &server)?;
            let discovered = service.catalog_product(&ExperimentCatalogRequest::default(), &cfg)?;
            assert!(
                discovered
                    .skills
                    .iter()
                    .flat_map(|skill| &skill.patterns)
                    .any(|candidate| candidate.pattern_id == original.pattern_id.as_str())
            );
            let draft = holdout::draft(
                &f,
                &service,
                &cfg,
                original.pattern_id.clone(),
                InstructionSchedule::Single,
                true,
            )?;
            let path = f
                .store
                .root()
                .join("patterns")
                .join(format!("{}.json", original.pattern_id));
            match fault {
                "missing" => std::fs::remove_file(&path)?,
                "corrupt" => std::fs::write(&path, b"{invalid candidate record")?,
                _ => {
                    let counter_a = failing(&f, "catalog-counter-a");
                    let counter_b = failing(&f, "catalog-counter-b");
                    let contested = f.store.consolidate_pattern(&ConsolidationRequest {
                        task_family: original.task_family.clone(),
                        failure_class: original.failure_class,
                        skill_id: original.skill_id.clone(),
                        scope: original.scope.clone(),
                        statement: original.statement.clone(),
                        applicability: original.applicability.clone(),
                        exceptions: original.exceptions.clone(),
                        evidence: original.evidence_ids.clone(),
                        counterevidence: vec![counter_a, counter_b],
                    })?;
                    assert!(!contested.pattern.usable_for_proposal());
                }
            }
            assert!(
                service.plan_product(draft, &cfg).is_err(),
                "catalog IDs cannot freeze stale or unusable source evidence: {fault}"
            );
            assert_eq!(
                server.count.load(Ordering::SeqCst),
                0,
                "failed planning is read-only with no physical dispatch"
            );
            Ok(())
        }
        .await;
        server.finish().await?;
        result?;
    }
    Ok(())
}
