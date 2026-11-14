//! Audit r1 F5 (end to end): one reviewed `annotate` of a genuinely published
//! experiment result, dispatched through production `skills_manage`
//! execution in a controlled Gateway launch. Approved: exactly one adoption
//! record is written. Denied: none. Repeated: nothing more.
use super::*;
use crate::runtime::h2_entry::test_support::ReviewedGateway;
use crate::skills::lifecycle::{ExperimentAnnotationRequest, ExperimentProductHandle, ExperimentPublishRequest};

/// Bound on the adoption records this test reads.
const MAX_RECORDS: usize = 64;

/// The adoption record files of `f`'s store.
fn adoption_records(f: &Fixture) -> Result<usize> {
    let directory = f.store.root().join("adoptions").join("records");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut count = 0;
    for entry in entries.take(MAX_RECORDS + 1) {
        if entry?.file_type()?.is_file() {
            count += 1;
        }
    }
    anyhow::ensure!(count <= MAX_RECORDS, "too many adoption records");
    Ok(count)
}

#[tokio::test]
async fn controlled_gateway_annotate_records_exactly_one_adoption() -> Result<()> {
    Box::pin(async {
        let f = fixture();
        let server = Server::start(false).await?;
        let result = async {
            let mut cfg = config(&f, &server);
            let suite = suite(&f);
            cfg.skills_lifecycle.enabled = true;
            cfg.skills_lifecycle.owner_scope = Some(f.store.owner_scope().as_str().into());
            cfg.skills_lifecycle.mode = crate::config::skills_lifecycle::SkillsLifecycleModeConfig::Auto;
            cfg.skills_lifecycle.suites = vec![crate::config::skills_lifecycle::SkillSuiteRegistration {
                skill_id: f.pin.skill_id.as_str().into(),
                suite_digest: suite.digest().into(),
            }];
            let _profile = f.store.register_lesson_fixture_profile(&ModelBinding {
                provider_label: cfg.default_provider.clone().context("fixture provider")?,
                model: "experiment-loopback".into(),
                temperature_milli: 100,
            })?;
            // The production service of this configuration.
            let service = Arc::new(SkillsLifecycleService::new(
                f.store.clone(),
                cfg.skills_lifecycle.to_settings()?,
            )?);
            service.install_operator_suite(&suite)?;
            let draft = gate_review::draft(&f, &service, &cfg, usable_pattern(&f).pattern_id)?;
            let plan = service.plan_instruction_cycle(draft, &cfg)?;
            service.admit_experiment(plan.spec.clone(), &cfg)?;
            let cancel = CancellationToken::new();
            let journal = CleanupScope::new(None, cancel.clone())?
                .run(service.run_instruction_plan(&cfg, &plan, &cancel))
                .await?;
            assert!(
                service.experiment_qualified(&journal)?,
                "the experiment genuinely qualifies"
            );
            let expected = f
                .store
                .resolve_experiment_final(&ExperimentProductHandle::of(&plan)?, "gate-review")?;
            let publication = service.publish_final_product(&ExperimentPublishRequest {
                expected: expected.clone(),
            })?;
            let request = ExperimentAnnotationRequest {
                expected,
                publication_tx: publication.transaction.transaction_id.to_string(),
                task_family: "deploy".into(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            };

            // The controlled Gateway launch over the same workspace and owner.
            let mut controlled = cfg.clone();
            controlled.config_path = std::path::PathBuf::new();
            controlled.memory.events.record_user_messages = true;
            controlled.memory.backend = "sqlite".into();
            controlled.runtime.kind = "native".into();
            controlled.tunnel.provider = "none".into();
            controlled.plugins.enabled = false;
            let gateway = ReviewedGateway::over(&controlled);
            let tool = gateway.tool(&controlled, Arc::clone(&service));
            let args = gateway.granted_args("lifecycle_experiment_annotate", serde_json::to_value(&request)?);
            assert_eq!(adoption_records(&f)?, 0);

            // Denied: nothing is admitted and nothing is written.
            assert!(gateway.admit(&args, false).await.is_err());
            assert_eq!(adoption_records(&f)?, 0, "a denied annotation writes nothing");

            // Approved: exactly one adoption record.
            let invocation = gateway.admit(&args, true).await?;
            let annotated = gateway
                .dispatch(Some(Arc::clone(&invocation)), &tool, args.clone())
                .await?;
            assert!(annotated.success, "{:?}", annotated.error);
            assert_eq!(
                adoption_records(&f)?,
                1,
                "an approved annotation writes exactly one record"
            );

            // Repeated: the consumed invocation never runs again.
            assert!(gateway.dispatch(Some(invocation), &tool, args).await.is_err());
            assert_eq!(adoption_records(&f)?, 1, "a repeat writes nothing more");
            Ok(())
        }
        .await;
        server.finish().await?;
        result
    })
    .await
}
