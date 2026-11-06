//! Batch parent authority: version 3 reads the original store, version 4 needs its sealed row.
use crate::memory::{Memory, MemoryFabric, SqliteMemory};
use crate::runtime::child_source::authority_journal;
use crate::runtime::child_source::authority_journal::ChildOutcomeRow;
use crate::session_worker::protocol::{WorkerManifest, WorkerResult};
use crate::session_worker::runner::tests::probe::{
    run_prepared_worker, with_prepared_worker_manifest, with_worker_probe_manifest,
};
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

/// A real child that completed through the worker port. Its parent authority is
/// sealed as a version-4 batch row before the child runs, so the batch reader
/// verifies the same run that settles below. Returns plain data only.
async fn completed_child_fixture(tmp: &tempfile::TempDir) -> Result<(WorkerManifest, WorkerResult)> {
    let outcome = {
        let probe = crate::providers::credential_probe::CredentialProbe::start_replying("r8 child completed").await;
        with_worker_probe_manifest(tmp, probe.custom_provider(), false, |manifest| async move {
            let source = manifest.child_source.as_ref().context("test: child source")?;
            let workspace = PathBuf::from(&source.parent_constraints.workspace);
            let owner = manifest.owner_id.clone().context("test: owner")?;
            let parent = manifest
                .capability_manifest
                .as_ref()
                .context("test: capability manifest")?
                .parent_decision_id
                .clone();
            let key = crate::decision::DecisionKey::load_existing(&workspace)?;
            // The signed launch row must describe this manifest and an eligible, non-legacy parent.
            let launch = authority_journal::read_launch_parent(&workspace, &manifest.run_id, &key)?
                .context("test: launch parent authority row")?;
            anyhow::ensure!(
                launch.version == 3
                    && launch.run == manifest.run_id
                    && launch.owner == owner
                    && launch.parent_decision == parent
                    && launch.parent_digest == source.parent_decision_digest
                    && launch.store_workspace == source.parent_store_workspace
                    && launch.enabled_snapshot
                    && !launch.legacy_v1
                    && launch.admits_issued_child,
                "test: launch parent authority differs from the signed manifest"
            );
            let batch = authority_journal::BatchParentRow {
                version: authority_journal::BATCH_PARENT_VERSION,
                batch: "r8-batch".to_owned(),
                owner: launch.owner.clone(),
                parent_decision: launch.parent_decision.clone(),
                parent_digest: launch.parent_digest.clone(),
                store_workspace: launch.store_workspace.clone(),
                enabled_snapshot: launch.enabled_snapshot,
                legacy_v1: launch.legacy_v1,
                admits_issued_child: launch.admits_issued_child,
            };
            authority_journal::persist_batch_parent(&workspace, &key, &batch)?;
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.as_path())?);
            let fabric = MemoryFabric::new(
                memory,
                manifest.memory_workspace_id.clone().context("test: memory workspace")?,
            );
            batch_journal::record_plan_fixture(
                fabric,
                Arc::clone(&key),
                PlanFixture {
                    version: authority_journal::BATCH_PARENT_VERSION,
                    owner: &batch.owner,
                    batch: &batch.batch,
                    parent_decision: &batch.parent_decision,
                    parent_digest: &batch.parent_digest,
                    store_workspace: &batch.store_workspace,
                    run_ids: &[manifest.run_id.as_str()],
                },
            )
            .await?;
            let result = run_prepared_worker(manifest.clone()).await?;
            anyhow::ensure!(result.success, "test: the child must complete: {:?}", result.error);
            Ok((manifest, result))
        })
        .await?
    };
    Ok(outcome)
}

/// Raw `(run, settlement, entry)` rows of the child outcome journal, read without repair.
fn child_outcome_rows(workspace: &std::path::Path) -> Result<Vec<(String, String, String)>> {
    let db = rusqlite::Connection::open_with_flags(
        authority_journal::path(workspace),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = db.prepare("SELECT run, settlement, entry FROM child_outcome ORDER BY run, settlement")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<(String, String, String)>>>()?;
    Ok(rows)
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

#[tokio::test]
async fn batch_child_completion_settles_after_restart_through_completion_port() -> Result<()> {
    use crate::agent::terminal::verify_terminal_replay;
    use crate::causal_tree::outcome::{OutcomeCorrelator, TerminalFacts};
    use crate::causal_tree::store::CausalDecisionStore;

    let tmp = tempfile::tempdir()?;
    let (manifest, result) = completed_child_fixture(&tmp).await?;
    let source = manifest.child_source.as_ref().context("test: child source")?;
    let owner = manifest.owner_id.clone().context("test: owner")?;
    let workspace = PathBuf::from(&source.parent_constraints.workspace);
    let evidence = result
        .completion_evidence
        .as_ref()
        .context("test: completion evidence")?;
    let [segment] = evidence.segments.as_slice() else {
        anyhow::bail!(
            "test: expected one completed segment, found {}",
            evidence.segments.len()
        );
    };

    // Reopen every handle. Nothing from the run crosses this boundary.
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.as_path())?);
    let fabric = MemoryFabric::new(
        memory,
        manifest.memory_workspace_id.clone().context("test: memory workspace")?,
    );
    let key = crate::decision::DecisionKey::load_existing(&workspace)?;
    let store = CausalDecisionStore::open_unshared_read_only(&workspace);
    let store_workspace = store.workspace_id().context("test: store namespace")?;
    anyhow::ensure!(
        store_workspace.as_ref() == source.parent_store_workspace,
        "test: store namespace differs from the manifest"
    );
    let reader = store.reader();

    // The sealed version-4 plan still names the worker.
    let roster = batch_journal::read(
        &fabric,
        &workspace,
        "r8-batch",
        &RunOwnerScope::Owner(Some(owner.clone())),
        None,
    )
    .await?
    .context("test: batch plan is readable")?;
    anyhow::ensure!(
        roster.members.iter().any(|member| member.run_id == manifest.run_id),
        "test: roster lacks the worker"
    );

    // Independent positive replay of the completed segment terminal.
    let terminal = fabric
        .memory()
        .find_message_event_by_idempotency_key(fabric.workspace_id(), &format!("turn:{}:final", segment.terminal_id))
        .await?
        .context("test: segment terminal marker")?;
    let replay = verify_terminal_replay(
        fabric.memory().as_ref(),
        &reader,
        &key,
        &store_workspace,
        fabric.workspace_id(),
        &owner,
        &segment.terminal_id,
        &terminal,
    )
    .await?;
    let facts = TerminalFacts::from_persisted_marker(&terminal, replay.decision(), &owner, fabric.workspace_id())?
        .with_verified_resume(&replay);
    let outcome = OutcomeCorrelator::persisted_record(replay.decision(), &facts)?;
    let run = replay.source().run_id.clone().context("test: child run")?;
    let expected = ChildOutcomeRow {
        version: authority_journal::CHILD_OUTCOME_VERSION,
        run,
        settlement: outcome.settlement_id().to_owned(),
        terminal_id: segment.terminal_id.clone(),
        owner: owner.clone(),
        store_workspace: source.parent_store_workspace.clone(),
        parent_decision: replay.parent_decision_id().map(|id| id.to_string()),
        parent_digest: replay.parent_digest().map(str::to_owned),
        child_decision: replay.decision().decision_id.to_string(),
        child_digest: replay.decision_digest().to_owned(),
        outcome_hash: outcome.journal_hash()?,
    };

    // The completion port settled the row during the run. The normal reader
    // confirms it and must not repair it, so the raw rows stay identical.
    let before = child_outcome_rows(&workspace)?;
    anyhow::ensure!(
        before.len() == 1 && before[0].0 == expected.run && before[0].1 == expected.settlement,
        "test: completion must leave exactly one settled child outcome row: {before:?}"
    );
    anyhow::ensure!(
        authority_journal::verify_child_outcome(
            &workspace,
            &key,
            &expected,
            Some(authority_journal::JOURNAL_REQUIRED_VERSION),
        )?,
        "test: the settled child outcome row must verify"
    );
    anyhow::ensure!(
        child_outcome_rows(&workspace)? == before,
        "test: the normal reader changed the child outcome journal"
    );
    Ok(())
}
