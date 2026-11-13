//! Failures while building one publication (R26).
use crate::decision_engine::prediction::factory::FactoryBuildError;
use crate::runtime::decision_journal::store::StoreError;
use crate::tools::capability::catalog::{CatalogError, ParityError};

#[derive(Debug, thiserror::Error)]
pub(crate) enum PublicationBuildError {
    #[error("durable recovery initialization failed")]
    RecoveryPreparation(#[source] anyhow::Error),
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
    #[error("decision service storage preparation task failed")]
    PreparationTask(#[from] tokio::task::JoinError),
    #[error("decision service preparation cleanup owner is unavailable")]
    PreparationOwner(#[source] anyhow::Error),
    #[error("enabled causal decisions require a durable message-event memory backend")]
    EventStoreUnsupported,
    #[error("enabled causal decisions require user message event recording")]
    InboundEventsDisabled,
}
