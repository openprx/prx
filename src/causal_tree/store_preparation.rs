//! Store migration and filesystem synchronization run outside Tokio workers.
use super::store::{CausalDecisionStore, Retention, StoreError, StoreMode};
use crate::decision::telemetry::CausalEventSink;
use std::{path::PathBuf, sync::Arc};

/// `None` opens the runtime's unshared, noncreating read-only view. The
/// completion hook runs inside the blocking owner even if its caller cancels.
pub(crate) async fn open_store(
    workspace: PathBuf,
    retention: Retention,
    events: Arc<dyn CausalEventSink>,
    mode: Option<StoreMode>,
    prepared: impl FnOnce(&Result<Arc<CausalDecisionStore>, StoreError>) + Send + 'static,
) -> Result<Arc<CausalDecisionStore>, StoreError> {
    tokio::task::spawn_blocking(move || {
        let result = mode.map_or_else(
            || Ok(CausalDecisionStore::open_unshared_read_only(&workspace)),
            |mode| CausalDecisionStore::open_shared(&workspace, retention, events, mode),
        );
        prepared(&result);
        result
    })
    .await
    .map_err(|error| StoreError::Io(std::io::Error::other(error)))?
}

/// Key creation includes filesystem synchronization, just like store opening.
pub(crate) async fn load_key(config: crate::config::Config) -> Option<Arc<crate::decision::DecisionKey>> {
    match tokio::task::spawn_blocking(move || crate::decision::factory::FactoryDeps::load_key(&config)).await {
        Ok(key) => key,
        Err(error) => {
            tracing::warn!(%error, "decision key preparation failed");
            None
        }
    }
}
