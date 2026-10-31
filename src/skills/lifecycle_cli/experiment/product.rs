//! Typed local CLI transport. Error JSON is carried by a nonzero CLI error;
//! an unsuccessful business operation never becomes an exit-zero result.
use super::{Arc, Config, Result, SkillsLifecycleService, read_input};
use crate::SkillExperimentProductCommands as Command;
use crate::skills::lifecycle::experiment::{ExperimentId, InstructionCycleDraft, InstructionExperimentPlan};
use crate::skills::lifecycle::{
    ExperimentAdoptionStatusRequest, ExperimentAnnotationRequest, ExperimentCatalogRequest, ExperimentFinalExpectation,
    ExperimentLessonQueryRequest, ExperimentProductError, ExperimentProductFailureKind, ExperimentProductHandle,
    ExperimentPublishRequest, ExperimentRollbackRequest, SkillId,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

type ProductResult<T> = std::result::Result<T, ExperimentProductError>;

pub(super) async fn dispatch(
    command: Command,
    service: Arc<SkillsLifecycleService>,
    config: &Config,
) -> Result<String> {
    match execute(command, service, config).await {
        Ok(value) => Ok(serde_json::to_string_pretty(&value)?),
        Err(error) => Err(anyhow::anyhow!(serde_json::to_string(&error)?)),
    }
}
async fn input<T: DeserializeOwned>(source: &str) -> ProductResult<T> {
    read_input(source).await.map_err(|error| ExperimentProductError {
        kind: ExperimentProductFailureKind::InvalidInput,
        message: error.to_string(),
        snapshot: None,
    })
}
fn value<T: Serialize>(result: ProductResult<T>) -> ProductResult<Value> {
    serde_json::to_value(result?).map_err(|error| anyhow::Error::new(error).into())
}
async fn blocking<T: Send + 'static>(
    service: &Arc<SkillsLifecycleService>,
    call: impl FnOnce(&SkillsLifecycleService) -> ProductResult<T> + Send + 'static,
) -> ProductResult<T> {
    let service = Arc::clone(service);
    crate::runtime::blocking::spawn_blocking(move || call(&service))
        .await
        .map_err(|error| ExperimentProductError {
            kind: ExperimentProductFailureKind::Uncertain,
            message: format!("product worker did not return a confirmed result: {error}"),
            snapshot: None,
        })?
}
fn observed_error(
    mut error: ExperimentProductError,
    service: &SkillsLifecycleService,
    handle: &ExperimentProductHandle,
    config: &Config,
) -> ExperimentProductError {
    error.snapshot = service.inspect_product(handle, config).ok().map(Box::new);
    error
}

async fn execute(command: Command, service: Arc<SkillsLifecycleService>, config: &Config) -> ProductResult<Value> {
    match command {
        Command::Catalog { after_skill } => {
            let request = ExperimentCatalogRequest {
                after_skill: after_skill.as_deref().map(SkillId::parse).transpose()?,
            };
            let config = config.clone();
            value(blocking(&service, move |service| service.catalog_product(&request, &config)).await)
        }
        Command::Plan { source } => {
            let draft: InstructionCycleDraft = input(&source).await?;
            let config = config.clone();
            value(blocking(&service, move |service| service.plan_product(draft, &config)).await)
        }
        Command::Admit { source } => {
            let plan: InstructionExperimentPlan = input(&source).await?;
            let config = config.clone();
            value(blocking(&service, move |service| service.admit_product(&plan, &config)).await)
        }
        Command::Run { source } => {
            let plan: InstructionExperimentPlan = input(&source).await?;
            let cancel = tokio_util::sync::CancellationToken::new();
            value(service.run_product(&plan, config, &cancel).await)
        }
        Command::Inspect { id, digest } => {
            let handle = ExperimentProductHandle {
                experiment_id: ExperimentId::parse(&id)?,
                plan_sha256: digest,
            };
            let config = config.clone();
            value(blocking(&service, move |service| service.inspect_product(&handle, &config)).await)
        }
        Command::Cancel { id, digest } => {
            let handle = ExperimentProductHandle {
                experiment_id: ExperimentId::parse(&id)?,
                plan_sha256: digest,
            };
            let config = config.clone();
            value(blocking(&service, move |service| service.cancel_product(&handle, &config)).await)
        }
        Command::Recover { id, digest } => {
            let handle = ExperimentProductHandle {
                experiment_id: ExperimentId::parse(&id)?,
                plan_sha256: digest,
            };
            let config = config.clone();
            value(blocking(&service, move |service| service.recover_product(&handle, &config)).await)
        }
        Command::Review { source } => {
            let expected: ExperimentFinalExpectation = input(&source).await?;
            let outcome = blocking(&service, move |service| service.review_final_product(&expected)).await?;
            Ok(json!({"review":outcome.record,"proposal":outcome.proposal}))
        }
        Command::Publish { source } => {
            let request: ExperimentPublishRequest = input(&source).await?;
            let handle = request.expected.handle.clone();
            value(
                blocking(&service, move |service| service.publish_final_product(&request))
                    .await
                    .map_err(|error| observed_error(error, &service, &handle, config)),
            )
        }
        Command::Annotate { source } => {
            let request: ExperimentAnnotationRequest = input(&source).await?;
            let handle = request.expected.handle.clone();
            let owned_config = config.clone();
            value(
                blocking(&service, move |service| {
                    service.annotate_product(&request, &owned_config)
                })
                .await
                .map_err(|error| observed_error(error, &service, &handle, config)),
            )
        }
        Command::QueryLessons { source } => {
            let request: ExperimentLessonQueryRequest = input(&source).await?;
            let config = config.clone();
            value(
                blocking(&service, move |service| {
                    service.query_product_lessons(&request, &config)
                })
                .await,
            )
        }
        Command::Rollback { source } => {
            let request: ExperimentRollbackRequest = input(&source).await?;
            let handle = request.handle.clone();
            let outcome = blocking(&service, move |service| service.rollback_product(&request))
                .await
                .map_err(|error| observed_error(error, &service, &handle, config))?;
            Ok(json!({"transaction":outcome.entry,"replayed":outcome.replayed}))
        }
        Command::SetAdoptionStatus { source } => {
            let request: ExperimentAdoptionStatusRequest = input(&source).await?;
            value(blocking(&service, move |service| service.set_product_adoption_status(&request)).await)
        }
    }
}
