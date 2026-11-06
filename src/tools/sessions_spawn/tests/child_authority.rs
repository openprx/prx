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

/// Replaces one named column, failing if the schema has no such column.
fn set_column(
    columns: &mut [(&'static str, rusqlite::types::Value)],
    name: &str,
    value: rusqlite::types::Value,
) -> Result<()> {
    let column = columns
        .iter_mut()
        .find(|(column, _)| *column == name)
        .with_context(|| format!("test: column {name}"))?;
    column.1 = value;
    Ok(())
}

/// Equality that compares reals by bits, so NaN payloads and -0.0 are exact.
fn same_value(left: &rusqlite::types::Value, right: &rusqlite::types::Value) -> bool {
    match (left, right) {
        (rusqlite::types::Value::Real(left), rusqlite::types::Value::Real(right)) => left.to_bits() == right.to_bits(),
        _ => left == right,
    }
}

#[test]
fn decision_evidence_roundtrips_typed_columns() -> Result<()> {
    use crate::causal_tree::ids::DecisionId;
    use crate::causal_tree::store::testing;
    use crate::runtime::child_source::authority_journal as journal;
    use rusqlite::types::Value;

    let tmp = tempfile::tempdir()?;
    let key = crate::decision::DecisionKey::load_or_create(tmp.path())?;
    let record = testing::record(
        DecisionId::new_runtime(),
        "store-ws-1",
        Some("owner-1"),
        1_700_000_000_000,
    );
    let decision = record.decision_id.to_string();
    let mut columns = record.replay_columns("store-ws-1")?;
    // A NaN payload, a negative zero, a non-ASCII JSON text and the integer extremes.
    set_column(
        &mut columns,
        "propensity",
        Value::Real(f64::from_bits(0x7ff8_0000_0000_1234)),
    )?;
    set_column(&mut columns, "explored", Value::Real(-0.0))?;
    set_column(&mut columns, "latency_ms", Value::Integer(i64::MIN))?;
    set_column(
        &mut columns,
        "features_json",
        Value::Text("{\"α\":\"\\u0000\"}".to_owned()),
    )?;
    journal::write_decision_evidence(tmp.path(), &key, std::slice::from_ref(&columns))?;

    let read = journal::read_decision_evidence(tmp.path(), "store-ws-1", "owner-1", &decision, &key)?
        .context("test: decision evidence row")?;
    assert_eq!(read.len(), 42);
    for ((name, expected), (read_name, actual)) in columns.iter().zip(&read) {
        assert_eq!(name, read_name);
        assert!(same_value(expected, actual), "column {name} changed on readback");
    }
    assert!(read.iter().any(|(_, value)| matches!(value, Value::Null)));

    // Ownerless issuance writes no row and still succeeds. Owned records in the
    // same chain still produce their rows.
    let ownerless = testing::record(DecisionId::new_runtime(), "store-ws-1", None, 1_700_000_000_001);
    let rows = journal::decision_evidence_rows(std::slice::from_ref(&ownerless), "store-ws-1")?;
    anyhow::ensure!(rows.is_empty(), "ownerless record produced an evidence row");
    let fresh = tempfile::tempdir()?;
    journal::write_decision_evidence(fresh.path(), &key, &rows)?;
    assert!(!journal::path(fresh.path()).exists());
    let mixed = journal::decision_evidence_rows(&[ownerless, record], "store-ws-1")?;
    assert_eq!(mixed.len(), 1);
    Ok(())
}

#[test]
fn decision_evidence_rejects_scope_substitution() -> Result<()> {
    use crate::causal_tree::ids::DecisionId;
    use crate::causal_tree::store::testing;
    use crate::runtime::child_source::authority_journal as journal;

    let tmp = tempfile::tempdir()?;
    let key = crate::decision::DecisionKey::load_or_create(tmp.path())?;
    let record = testing::record(
        DecisionId::new_runtime(),
        "store-ws-1",
        Some("owner-1"),
        1_700_000_000_000,
    );
    let decision = record.decision_id.to_string();
    let rows = journal::decision_evidence_rows(std::slice::from_ref(&record), "store-ws-1")?;
    journal::write_decision_evidence(tmp.path(), &key, &rows)?;

    // Positive control: the matching SQL key reads its own sealed row.
    anyhow::ensure!(
        journal::read_decision_evidence(tmp.path(), "store-ws-1", "owner-1", &decision, &key)?.is_some(),
        "test: matching key must read"
    );

    // Copy the otherwise valid sealed entry under a different workspace and a different owner.
    let db = rusqlite::Connection::open(journal::path(tmp.path()))?;
    let entry: String = db.query_row("SELECT entry FROM decision_evidence", [], |row| row.get(0))?;
    for (workspace, owner) in [("store-ws-2", "owner-1"), ("store-ws-1", "owner-2")] {
        db.execute(
            "INSERT INTO decision_evidence(workspace, owner, decision, entry) VALUES (?1, ?2, ?3, ?4)",
            (workspace, owner, &decision, &entry),
        )?;
        anyhow::ensure!(
            journal::read_decision_evidence(tmp.path(), workspace, owner, &decision, &key).is_err(),
            "substituted decision evidence key was accepted: {workspace}/{owner}"
        );
    }
    Ok(())
}
