//! Filesystem preparation runs off Tokio workers and retains cleanup ownership.
use super::{
    Arc, CausalDecisionStore, CausalEventSink, ConfigGeneration, DecisionKey, FactoryDeps, Retention, StoreMode,
    TurnDecisionService, TurnServiceBuildError,
};
use crate::runtime::{causal_events::DurableCausalEventSink, turn_cleanup};
use tokio_util::sync::CancellationToken;

impl TurnDecisionService {
    /// Cleanup transfers temporary construction ownership to the published
    /// service under its existing retention lock, matching both actual queues.
    pub(crate) fn owns_prepared_queues(
        &self,
        store: Option<&Arc<CausalDecisionStore>>,
        events: Option<&Arc<DurableCausalEventSink>>,
    ) -> bool {
        store.is_some_and(|store| Arc::ptr_eq(store, &self.store))
            && match (self.durable_events.as_ref(), events) {
                (Some(published), Some(prepared)) => Arc::ptr_eq(published, prepared),
                (None, None) => true,
                _ => false,
            }
    }
}

pub(super) async fn load_key(
    generation: Arc<ConfigGeneration>,
) -> Result<Option<Arc<DecisionKey>>, TurnServiceBuildError> {
    let ticket = turn_cleanup::current_tracker()
        .map(|owner| owner.register(CancellationToken::new()))
        .transpose()
        .map_err(TurnServiceBuildError::PreparationOwner)?;
    Ok(tokio::task::spawn_blocking(move || {
        crate::runtime::decision_accounting::initialize_tokenizers();
        let key = FactoryDeps::load_key(&generation.effective);
        if let Some(ticket) = ticket {
            ticket.finish(true);
        }
        key
    })
    .await?)
}

pub(super) async fn open_store(
    generation: Arc<ConfigGeneration>,
    events: Arc<dyn CausalEventSink>,
    durable_events: Option<Arc<DurableCausalEventSink>>,
    durable_key: bool,
) -> Result<Arc<CausalDecisionStore>, TurnServiceBuildError> {
    let ticket = turn_cleanup::current_tracker()
        .map(|owner| owner.register(CancellationToken::new()))
        .transpose()
        .map_err(TurnServiceBuildError::PreparationOwner)?;
    let config = &generation.effective;
    Ok(crate::causal_tree::store_preparation::open_store(
        config.workspace_dir.clone(),
        Retention::days(
            config.causal_tree.store.retention_days,
            config.self_system.fitness.retention_days,
        ),
        events,
        durable_key.then_some(StoreMode::ReadWrite),
        move |result| {
            if let Some(ticket) = ticket {
                ticket.retain_prepared_store(result.as_ref().ok().cloned(), durable_events);
                ticket.finish(true);
            }
        },
    )
    .await?)
}

#[cfg(test)]
mod tests;
