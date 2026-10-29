//! Operator entrypoint for M1 bridges. No implicit stage scheduling or publication.
use super::{Arc, Config, Context, Result, SkillsLifecycleService};
use crate::SkillExperimentCommands;
use crate::skills::lifecycle::experiment::{ExperimentDraft, ExperimentId, ExperimentJournal, ExperimentOperation};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

async fn read_input<T: DeserializeOwned>(source: &str) -> Result<T> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    tokio::fs::File::open(source)
        .await?
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await?;
    anyhow::ensure!(bytes.len() <= 256 * 1024, "experiment input exceeds 256 KiB");
    Ok(serde_json::from_slice(&bytes)?)
}
fn render(service: &SkillsLifecycleService, config: &Config, journal: ExperimentJournal) -> Result<String> {
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "experiment_sha256":journal.plan.digest(), "spec":journal.plan.spec(), "entries":journal.entries,
        "committed":journal.state.committed(&journal.plan)?, "cancelled":journal.state.cancelled,
        "head_sha256":journal.head_sha256, "qualified":service.experiment_qualified(&journal)?, "published":false,
        "physical_observations":service.experiment_physical_observations(&journal, config)?,
    }))?)
}
pub(super) async fn dispatch(
    command: SkillExperimentCommands,
    service: Arc<SkillsLifecycleService>,
    config: &Config,
) -> Result<String> {
    match command {
        SkillExperimentCommands::Plan { source } => {
            let spec: ExperimentDraft = read_input(&source).await?;
            let config = config.clone();
            let frozen = blocking(&service, move |service| service.plan_experiment_draft(spec, &config)).await?;
            Ok(serde_json::to_string_pretty(frozen.spec())?)
        }
        SkillExperimentCommands::Admit { source } => {
            let spec = read_input(&source).await?;
            let admitted_config = config.clone();
            render(
                &service,
                config,
                blocking(&service, move |service| {
                    service.admit_experiment(spec, &admitted_config)
                })
                .await?,
            )
        }
        SkillExperimentCommands::Inspect { id, digest } => {
            let id = ExperimentId::parse(&id)?;
            render(
                &service,
                config,
                blocking(&service, move |service| service.inspect_experiment(&id, &digest)).await?,
            )
        }
        SkillExperimentCommands::Cancel { id, digest } => {
            let id = ExperimentId::parse(&id)?;
            render(
                &service,
                config,
                blocking(&service, move |service| service.cancel_experiment(&id, &digest)).await?,
            )
        }
        SkillExperimentCommands::Recover { id, digest } => {
            let id = ExperimentId::parse(&id)?;
            render(
                &service,
                config,
                blocking(&service, move |service| service.recover_experiment(&id, &digest)).await?,
            )
        }
        SkillExperimentCommands::Run {
            id,
            digest,
            stage,
            source,
        } => {
            let operation: ExperimentOperation = read_input(&source).await?;
            let cancel = CancellationToken::new();
            let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, cancel.clone())?;
            render(
                &service,
                config,
                cleanup
                    .run(service.run_experiment_stage(
                        config,
                        ExperimentId::parse(&id)?,
                        digest,
                        stage,
                        operation,
                        &cancel,
                    ))
                    .await?,
            )
        }
    }
}

async fn blocking<T: Send + 'static>(
    service: &Arc<SkillsLifecycleService>,
    call: impl FnOnce(&SkillsLifecycleService) -> Result<T> + Send + 'static,
) -> Result<T> {
    let service = Arc::clone(service);
    crate::runtime::blocking::spawn_blocking(move || call(&service))
        .await
        .context("experiment worker failed")?
}
