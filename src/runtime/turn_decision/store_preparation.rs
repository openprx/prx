//! Filesystem preparation runs off Tokio workers and retains cleanup ownership.
use super::{
    Arc, CausalDecisionStore, CausalEventSink, ConfigGeneration, DecisionKey, PublicationBuildError, Retention,
    StoreMode, TurnDecisionService,
};
use crate::runtime::{causal_events::DurableCausalEventSink, turn_cleanup};
use tokio_util::sync::CancellationToken;

impl TurnDecisionService {
    /// Delegates to the R25 owner in `runtime::ingress_recovery` (until Part 2).
    pub(crate) fn owns_prepared_queues(
        &self,
        store: Option<&Arc<CausalDecisionStore>>,
        events: Option<&Arc<DurableCausalEventSink>>,
    ) -> bool {
        crate::runtime::ingress_recovery::owns_prepared_queues(self, store, events)
    }
}

/// The turn's state key: the workspace runtime key at
/// `<ws>/self/runtime/runtime.key` (OV-2), loaded or created off the Tokio
/// workers. There is no plane condition and no ephemeral fallback: an
/// unavailable runtime key refuses the publication.
pub(in crate::runtime) async fn load_key(
    generation: Arc<ConfigGeneration>,
) -> Result<Arc<DecisionKey>, PublicationBuildError> {
    let ticket = turn_cleanup::current_tracker()
        .map(|owner| owner.register(CancellationToken::new()))
        .transpose()
        .map_err(PublicationBuildError::PreparationOwner)?;
    let key = tokio::task::spawn_blocking(move || {
        crate::runtime::decision_accounting::initialize_tokenizers();
        let key = DecisionKey::load_or_create(&generation.effective.workspace_dir);
        if let Some(ticket) = ticket {
            ticket.finish(true);
        }
        key
    })
    .await?;
    key.map_err(PublicationBuildError::RuntimeKey)
}

pub(in crate::runtime) async fn open_store(
    generation: Arc<ConfigGeneration>,
    events: Arc<dyn CausalEventSink>,
    durable_events: Option<Arc<DurableCausalEventSink>>,
    durable_key: bool,
) -> Result<Arc<CausalDecisionStore>, PublicationBuildError> {
    let ticket = turn_cleanup::current_tracker()
        .map(|owner| owner.register(CancellationToken::new()))
        .transpose()
        .map_err(PublicationBuildError::PreparationOwner)?;
    let config = &generation.effective;
    Ok(crate::runtime::decision_journal::store_preparation::open_store(
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
