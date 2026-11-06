//! Batch parent authority: version 3 reads the original store, version 4 needs its sealed row.
use crate::memory::{Memory, MemoryFabric, SqliteMemory};
use crate::session_worker::protocol::WorkerManifest;
use crate::session_worker::runner::tests::probe::with_prepared_worker_manifest;
use crate::tools::sessions_read_model::RunOwnerScope;
use crate::tools::sessions_spawn::batch_journal::{self, PlanFixture};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// Record a plan of `version` for the prepared worker's parent and read it back.
async fn read_plan(worker: &WorkerManifest, version: u8, batch: &str) -> Result<usize> {
    let source = worker.child_source.as_ref().context("test: child source")?;
    let owner = worker.owner_id.clone().context("test: owner")?;
    let parent = worker
        .capability_manifest
        .as_ref()
        .context("test: capability manifest")?
        .parent_decision_id
        .clone();
    let workspace = PathBuf::from(&source.parent_constraints.workspace);
    let memory_dir = tempfile::tempdir()?;
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(memory_dir.path())?);
    let fabric = MemoryFabric::new(memory, workspace.to_string_lossy().into_owned());
    let key = crate::decision::DecisionKey::load_existing(&workspace)?;
    batch_journal::record_plan_fixture(
        fabric.clone(),
        key,
        PlanFixture {
            version,
            owner: &owner,
            batch,
            parent_decision: &parent,
            parent_digest: &source.parent_decision_digest,
            store_workspace: &source.parent_store_workspace,
            run_ids: &[worker.run_id.as_str()],
        },
    )
    .await?;
    let roster = batch_journal::read(
        &fabric,
        &workspace,
        batch,
        &RunOwnerScope::Owner(Some(owner.clone())),
        None,
    )
    .await?
    .context("test: batch plan is readable")?;
    Ok(roster.members.len())
}

#[tokio::test]
async fn batch_v3_verifies_through_store_fallback() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |worker| async move {
        // The prepared parent lives in the causal store; no journal row exists for version 3.
        assert_eq!(read_plan(&worker, 3, "batch-v3-store").await?, 1);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn batch_v4_refuses_missing_parent_row() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |worker| async move {
        // The same parent is available in the store, so only the missing sealed row can refuse.
        let error = read_plan(&worker, 4, "batch-v4-missing")
            .await
            .expect_err("a version-4 batch without its parent row must be refused");
        assert!(error.to_string().contains("row is missing"), "{error:#}");
        Ok(())
    })
    .await
}
