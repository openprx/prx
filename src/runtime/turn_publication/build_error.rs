//! Failures while building one publication (R26).
use crate::decision_engine::prediction::factory::FactoryBuildError;
use crate::tools::capability::catalog::{CatalogError, ParityError};

#[derive(Debug, thiserror::Error)]
pub(crate) enum TurnRuntimeBuildError {
    #[error("durable recovery initialization failed")]
    RecoveryPreparation(#[source] anyhow::Error),
    #[error("worker children publish only through the ordinary or operator process path")]
    WorkerSurface,
    #[error("catalog sources do not match the pinned configuration generation")]
    SourceGeneration,
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Parity(#[from] ParityError),
    #[error(transparent)]
    Factory(#[from] FactoryBuildError),
    #[error("workspace runtime key unavailable")]
    RuntimeKey(#[source] crate::cost::decision_attempt::DecisionError),
    #[error("pinned decision policy cannot be fingerprinted")]
    PolicyFingerprint(#[source] serde_json::Error),
    #[error("turn service preparation task failed")]
    PreparationTask(#[from] tokio::task::JoinError),
    #[error("turn service preparation cleanup owner is unavailable")]
    PreparationOwner(#[source] anyhow::Error),
}
