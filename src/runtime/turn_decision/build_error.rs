//! Failures while constructing a source-bound decision service.
use super::{CatalogError, FactoryBuildError, ParityError, StoreError};

#[derive(Debug, thiserror::Error)]
pub(crate) enum TurnServiceBuildError {
    #[error("child service does not match its issued parent authority")]
    ChildAuthority,
    #[error("catalog sources do not match the pinned configuration generation")]
    SourceGeneration,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Parity(#[from] ParityError),
    #[error(transparent)]
    Factory(#[from] FactoryBuildError),
    #[error("pinned decision policy cannot be fingerprinted")]
    PolicyFingerprint(#[source] serde_json::Error),
    #[error("durable causal events require an active runtime")]
    EventRuntime(#[from] tokio::runtime::TryCurrentError),
    #[error("decision service storage preparation task failed")]
    PreparationTask(#[from] tokio::task::JoinError),
    #[error("decision service preparation cleanup owner is unavailable")]
    PreparationOwner(#[source] anyhow::Error),
    #[error("enabled causal decisions require a durable message-event memory backend")]
    EventStoreUnsupported,
    #[error("enabled causal decisions require user message event recording")]
    InboundEventsDisabled,
}
