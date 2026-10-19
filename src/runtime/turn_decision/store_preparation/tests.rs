//! Real SQLite preparation remains cleanup-owned after its publisher is dropped.
#![cfg(test)]
use super::*;
use crate::decision::telemetry::testing::RecordingSink;
use std::time::Duration;

#[tokio::test]
async fn cancelled_store_publication_keeps_blocking_preparation_and_queues_owned() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let directory = crate::decision::key::prepare_decision_dir(workspace.path()).map_err(anyhow::Error::msg)?;
    let database = directory.join("decisions.db");
    let blocker = rusqlite::Connection::open(&database)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600))?;
    }
    blocker.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")?;
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    let generation = crate::config::new_shared(config).pin();
    let owner = Arc::new(turn_cleanup::CleanupTracker::default());
    let mut publication = Box::pin(owner.scope(open_store(generation, Arc::new(RecordingSink::default()), None, true)));
    let publication_pending = futures::poll!(&mut publication).is_pending();
    drop(publication);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut drain = Box::pin(owner.drain(deadline));
    let drain_pending = futures::poll!(&mut drain).is_pending();
    // A single-thread Tokio executor is still responsive while SQLite is held.
    tokio::task::yield_now().await;
    blocker.execute_batch("ROLLBACK;")?;
    drain.await?;
    anyhow::ensure!(
        publication_pending && drain_pending,
        "cancelled publisher must leave real preparation pending under cleanup"
    );
    let store = CausalDecisionStore::existing_shared_writer(workspace.path())
        .ok_or_else(|| anyhow::anyhow!("prepared store lost before cleanup owner released it"))?;
    drop(owner);
    CausalDecisionStore::close(store, deadline.into_std()).await;
    assert!(CausalDecisionStore::existing_shared_writer(workspace.path()).is_none());
    Ok(())
}

#[tokio::test]
async fn published_then_retired_service_releases_its_preparation_inventory() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.causal_tree.enabled = false;
    let generation = crate::config::new_shared(config.clone()).pin();
    let owner = Arc::new(turn_cleanup::CleanupTracker::default());
    let service = owner
        .scope(super::super::tests::disabled_agent_service(
            &generation,
            &crate::providers::catalog::CatalogView::from_config(&config),
        ))
        .await;
    let store = Arc::downgrade(&service.store);
    owner.retain_service(Arc::clone(&service));
    let retirement = owner.retire(&service)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    service.shutdown(deadline.into_std()).await;
    drop(service);
    retirement.finish(true);
    assert!(
        store.upgrade().is_none(),
        "retirement must not leave a duplicate preparation owner"
    );
    owner.drain(deadline).await?;
    Ok(())
}
