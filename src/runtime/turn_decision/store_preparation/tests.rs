//! Real SQLite preparation remains cleanup-owned after its publisher is dropped.
#![cfg(test)]
use super::*;
use std::time::Duration;

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
