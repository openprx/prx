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

/// Everything a reopened restart needs. Plain data only, with no live handle.
struct ReopenedChild {
    workspace: PathBuf,
    key: Arc<crate::decision::DecisionKey>,
    expected: ChildOutcomeRow,
    launch_version: u8,
}

/// Reopens every handle and rebuilds the expected row from an independently
/// verified segment replay. Nothing from the run crosses this boundary.
async fn reopened_child_evidence(manifest: &WorkerManifest, result: &WorkerResult) -> Result<ReopenedChild> {
    use crate::agent::terminal::verify_terminal_replay;
    use crate::causal_tree::outcome::{OutcomeCorrelator, TerminalFacts};
    use crate::causal_tree::store::CausalDecisionStore;

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
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.as_path())?);
    let fabric = MemoryFabric::new(
        memory,
        manifest.memory_workspace_id.clone().context("test: memory workspace")?,
    );
    let key = crate::decision::DecisionKey::load_existing(&workspace)?;
    let launch = authority_journal::read_launch_parent(&workspace, &manifest.run_id, &key)?
        .context("test: launch parent authority row")?;
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
    Ok(ReopenedChild {
        workspace,
        key,
        expected,
        launch_version: launch.version,
    })
}

#[tokio::test]
async fn batch_child_completion_settles_after_restart_through_completion_port() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (manifest, result) = completed_child_fixture(&tmp).await?;
    let child = reopened_child_evidence(&manifest, &result).await?;

    // The completion port settled the row during the run. The normal reader
    // confirms it and must not repair it, so the raw rows stay identical.
    let before = child_outcome_rows(&child.workspace)?;
    anyhow::ensure!(
        before.len() == 1 && before[0].0 == child.expected.run && before[0].1 == child.expected.settlement,
        "test: completion must leave exactly one settled child outcome row: {before:?}"
    );
    // r10 delta: the first observation after the WorkerResult runs with no decision store.
    let store_db = quarantine_decision_store(&child.workspace)?;
    let source = manifest.child_source.as_ref().context("test: child source")?;
    let owner = manifest.owner_id.clone().context("test: owner")?;
    let evidence = result
        .completion_evidence
        .as_ref()
        .context("test: completion evidence")?;
    let [segment] = evidence.segments.as_slice() else {
        anyhow::bail!("test: expected one completed segment");
    };
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(child.workspace.as_path())?);
    let fabric = MemoryFabric::new(
        memory,
        manifest.memory_workspace_id.clone().context("test: memory workspace")?,
    );
    let terminal = fabric
        .memory()
        .find_message_event_by_idempotency_key(fabric.workspace_id(), &format!("turn:{}:final", segment.terminal_id))
        .await?
        .context("test: segment terminal marker")?;
    crate::agent::terminal::verify_terminal_replay_journal(
        fabric.memory().as_ref(),
        &child.workspace,
        &child.key,
        &source.parent_store_workspace,
        fabric.workspace_id(),
        &owner,
        &segment.terminal_id,
        &terminal,
    )
    .await?;
    anyhow::ensure!(
        authority_journal::verify_child_outcome(
            &child.workspace,
            &child.key,
            &child.expected,
            Some(authority_journal::JOURNAL_REQUIRED_VERSION),
        )?,
        "test: the settled child outcome row must verify"
    );
    anyhow::ensure!(
        child_outcome_rows(&child.workspace)? == before,
        "test: the normal reader changed the child outcome journal"
    );
    anyhow::ensure!(
        !store_db.exists(),
        "test: the journal-only observation must not recreate the decision store"
    );
    Ok(())
}

/// Raw seal-bearing entry, mirrored here so a test can flip one seal byte.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedEntry {
    payload: String,
    seal: [u8; 32],
}

#[tokio::test]
async fn child_outcome_seal_tamper_is_refused_after_restart() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (manifest, result) = completed_child_fixture(&tmp).await?;
    let child = reopened_child_evidence(&manifest, &result).await?;
    let (run, settlement) = (child.expected.run.clone(), child.expected.settlement.clone());

    // Positive control: the matching row reads through the normal reader.
    anyhow::ensure!(
        authority_journal::verify_child_outcome(
            &child.workspace,
            &child.key,
            &child.expected,
            Some(child.launch_version),
        )?,
        "test: matching child outcome must verify before tampering"
    );

    // Flip only the seal. The payload, SQL keys and seal length stay identical.
    let db = rusqlite::Connection::open(authority_journal::path(&child.workspace))?;
    let stored: String = db.query_row(
        "SELECT entry FROM child_outcome WHERE run = ?1 AND settlement = ?2",
        (&run, &settlement),
        |row| row.get(0),
    )?;
    let mut entry: SealedEntry = serde_json::from_str(&stored)?;
    entry.seal[0] ^= 1;
    let tampered = serde_json::to_string(&entry)?;
    db.execute(
        "UPDATE child_outcome SET entry = ?1 WHERE run = ?2 AND settlement = ?3",
        (&tampered, &run, &settlement),
    )?;
    drop(db);

    // Independent replay still succeeds after reopen. Only the journal read refuses.
    let reopened = reopened_child_evidence(&manifest, &result).await?;
    let error = authority_journal::verify_child_outcome(
        &reopened.workspace,
        &reopened.key,
        &reopened.expected,
        Some(reopened.launch_version),
    )
    .expect_err("a child outcome with a tampered seal must be refused");
    assert!(error.to_string().contains("seal is invalid"), "{error:#}");
    anyhow::ensure!(
        child_outcome_rows(&reopened.workspace)? == vec![(run, settlement, tampered)],
        "test: the refused child outcome row must stay unchanged"
    );
    Ok(())
}

#[tokio::test]
async fn child_outcome_missing_row_is_refused_for_v3() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (manifest, result) = completed_child_fixture(&tmp).await?;
    let child = reopened_child_evidence(&manifest, &result).await?;
    let (run, settlement) = (child.expected.run.clone(), child.expected.settlement.clone());

    // Positive control: the matching row reads through the normal reader.
    anyhow::ensure!(
        authority_journal::verify_child_outcome(
            &child.workspace,
            &child.key,
            &child.expected,
            Some(child.launch_version),
        )?,
        "test: matching child outcome must verify before deletion"
    );

    // Delete only this row. Terminal, parent and hop evidence stay intact.
    let db = rusqlite::Connection::open(authority_journal::path(&child.workspace))?;
    let deleted = db.execute(
        "DELETE FROM child_outcome WHERE run = ?1 AND settlement = ?2",
        (&run, &settlement),
    )?;
    anyhow::ensure!(deleted == 1, "test: exactly one child outcome row must be deleted");
    drop(db);

    // The replay still verifies after reopen, so only the journal row is missing.
    let reopened = reopened_child_evidence(&manifest, &result).await?;
    let error = authority_journal::verify_child_outcome(
        &reopened.workspace,
        &reopened.key,
        &reopened.expected,
        Some(reopened.launch_version),
    )
    .expect_err("a version-3 child without its outcome row must be refused");
    assert!(error.to_string().contains("child outcome row is missing"), "{error:#}");
    anyhow::ensure!(
        child_outcome_rows(&reopened.workspace)?.is_empty(),
        "test: the refused read must not insert a row"
    );
    anyhow::ensure!(
        !authority_journal::verify_child_outcome(&reopened.workspace, &reopened.key, &reopened.expected, None)?,
        "test: an absent row without a journal version must be Ok(false)"
    );
    Ok(())
}

/// Decision evidence columns with their schema names, checked against the replay order.
fn named_evidence_columns(
    columns: Vec<(String, rusqlite::types::Value)>,
) -> Result<Vec<(&'static str, rusqlite::types::Value)>> {
    let names = crate::causal_tree::store::replay_column_names();
    anyhow::ensure!(columns.len() == names.len(), "test: evidence column count");
    columns
        .into_iter()
        .zip(names)
        .map(|((name, value), expected)| {
            anyhow::ensure!(name == expected, "test: evidence column order");
            Ok((expected, value))
        })
        .collect()
}

/// Rewrites the `candidates_json` column through `edit`, keeping its JSON array shape.
fn edit_candidates(
    columns: &mut [(&'static str, rusqlite::types::Value)],
    edit: impl FnOnce(&mut Vec<crate::causal_tree::store::RecordedCandidate>) -> Result<()>,
) -> Result<()> {
    let Some((_, rusqlite::types::Value::Text(text))) = columns.iter().find(|(name, _)| *name == "candidates_json")
    else {
        anyhow::bail!("test: candidates_json is not text");
    };
    let mut candidates: Vec<crate::causal_tree::store::RecordedCandidate> = serde_json::from_str(text)?;
    edit(&mut candidates)?;
    set_column(
        columns,
        "candidates_json",
        rusqlite::types::Value::Text(serde_json::to_string(&candidates)?),
    )
}

/// The replay digest of one evidence row, recomputed from its typed columns.
fn evidence_digest(columns: &[(&'static str, rusqlite::types::Value)]) -> Result<String> {
    let owned: Vec<(String, rusqlite::types::Value)> = columns
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect();
    Ok(crate::causal_tree::store::DecisionReplayRecord::from_evidence_columns(&owned)?.digest)
}

/// Replaces the single `decision_evidence` row of one decision, test-only.
fn replace_evidence_row(
    workspace: &std::path::Path,
    key: &crate::decision::DecisionKey,
    store_workspace: &str,
    owner: &str,
    decision: &str,
    columns: &[(&'static str, rusqlite::types::Value)],
) -> Result<()> {
    let db = rusqlite::Connection::open(authority_journal::path(workspace))?;
    let deleted = db.execute(
        "DELETE FROM decision_evidence WHERE workspace = ?1 AND owner = ?2 AND decision = ?3",
        (store_workspace, owner, decision),
    )?;
    anyhow::ensure!(deleted == 1, "test: exactly one decision evidence row must be replaced");
    drop(db);
    authority_journal::write_decision_evidence(workspace, key, &[columns.to_vec()])
}

/// Points the last signed hop at `digest` and reseals the marker with its assistant.
fn reseal_last_hop(
    key: &crate::decision::DecisionKey,
    event: &mut crate::memory::MessageEvent,
    digest: &str,
    assistant: Option<&crate::memory::MessageEvent>,
) -> Result<()> {
    let mut payload: serde_json::Value =
        serde_json::from_str(event.raw_payload_json.as_deref().context("test: payload")?)?;
    let hops = payload
        .get_mut("terminal_replay_v1")
        .and_then(|binding| binding.get_mut("hops"))
        .and_then(serde_json::Value::as_array_mut)
        .context("test: replay hops")?;
    hops.last_mut()
        .and_then(serde_json::Value::as_object_mut)
        .context("test: last replay hop")?
        .insert("digest".to_owned(), serde_json::Value::String(digest.to_owned()));
    crate::agent::terminal::reseal_payload_for_test(key, &mut payload, assistant)?;
    event.raw_payload_json = Some(serde_json::to_string(&payload)?);
    Ok(())
}

#[tokio::test]
async fn terminal_replay_refuses_hop_digest_substitution() -> Result<()> {
    use crate::agent::terminal::{verify_terminal_replay, verify_terminal_replay_journal};
    use crate::causal_tree::store::{CausalDecisionStore, RecordedCandidate};

    let tmp = tempfile::tempdir()?;
    let (manifest, result) = completed_child_fixture(&tmp).await?;
    let source = manifest.child_source.as_ref().context("test: child source")?;
    let workspace = PathBuf::from(&source.parent_constraints.workspace);
    let owner = manifest.owner_id.clone().context("test: owner")?;
    let store_workspace = source.parent_store_workspace.clone();
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
    let terminal_id = segment.terminal_id.clone();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.as_path())?);
    let fabric = MemoryFabric::new(
        memory,
        manifest.memory_workspace_id.clone().context("test: memory workspace")?,
    );
    let key = crate::decision::DecisionKey::load_existing(&workspace)?;
    let mut terminal = fabric
        .memory()
        .find_message_event_by_idempotency_key(fabric.workspace_id(), &format!("turn:{terminal_id}:final"))
        .await?
        .context("test: segment terminal marker")?;

    // Positive store replay of an ordinary terminal: no Router, no collection.
    // Every store handle closes before the journal-only reads below.
    let (decision_id, selected_id, assistant) = {
        let store = CausalDecisionStore::open_unshared_read_only(&workspace);
        let reader = store.reader();
        let replay = verify_terminal_replay(
            fabric.memory().as_ref(),
            &reader,
            &key,
            &store_workspace,
            fabric.workspace_id(),
            &owner,
            &terminal_id,
            &terminal,
        )
        .await?;
        let payload: serde_json::Value =
            serde_json::from_str(terminal.raw_payload_json.as_deref().context("test: payload")?)?;
        anyhow::ensure!(
            !replay.collected_pending()
                && !replay.collected_declined()
                && replay.decision().directive_kind != "collect_approval_arguments"
                && payload
                    .get("terminal_replay_v1")
                    .and_then(|binding| binding.get("router_read"))
                    .is_none_or(serde_json::Value::is_null),
            "test: fixture must be an ordinary terminal without Router or collection evidence"
        );
        (
            replay.decision().decision_id.to_string(),
            replay.decision().selected_candidate_id.clone(),
            replay.assistant().cloned(),
        )
    };
    let selected_id = selected_id.context("test: selected candidate")?;

    // Baseline: append an unselected candidate (propensity 0.25) to the final row,
    // then sign that row's digest into the last hop and reseal the marker.
    let stored = authority_journal::read_decision_evidence(&workspace, &store_workspace, &owner, &decision_id, &key)?
        .context("test: final decision evidence row")?;
    let mut columns = named_evidence_columns(stored)?;
    edit_candidates(&mut columns, |candidates| {
        anyhow::ensure!(
            candidates.iter().all(|candidate| candidate.id != "r9-unselected"),
            "test: r9-unselected must be a new candidate"
        );
        let seed = candidates
            .iter()
            .find(|candidate| candidate.id == selected_id)
            .cloned()
            .context("test: selected candidate row")?;
        candidates.push(RecordedCandidate {
            id: "r9-unselected".to_owned(),
            propensity: Some(0.25),
            ..seed
        });
        Ok(())
    })?;
    replace_evidence_row(&workspace, &key, &store_workspace, &owner, &decision_id, &columns)?;
    reseal_last_hop(&key, &mut terminal, &evidence_digest(&columns)?, assistant.as_ref())?;

    // Remove every decision store file, so the journal-only reads cannot reach it.
    let db = crate::causal_tree::store::db_path(&workspace);
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", db.display()));
        if path.exists() {
            std::fs::rename(&path, format!("{}.unavailable", path.display()))?;
        }
    }
    anyhow::ensure!(!db.exists(), "test: the decision store database must be quarantined");

    let baseline = verify_terminal_replay_journal(
        fabric.memory().as_ref(),
        &workspace,
        &key,
        &store_workspace,
        fabric.workspace_id(),
        &owner,
        &terminal_id,
        &terminal,
    )
    .await?;
    anyhow::ensure!(
        baseline.decision().selected_candidate_id.as_deref() == Some(selected_id.as_str()),
        "test: the baseline must keep the selected candidate"
    );

    // Substitution: change only the unselected propensity (0.25 to 0.75). The
    // signed hop digest still names the 0.25 row, so the replay must refuse.
    let stored = authority_journal::read_decision_evidence(&workspace, &store_workspace, &owner, &decision_id, &key)?
        .context("test: final decision evidence row")?;
    let mut substituted = named_evidence_columns(stored)?;
    edit_candidates(&mut substituted, |candidates| {
        let unselected = candidates
            .iter_mut()
            .find(|candidate| candidate.id == "r9-unselected")
            .context("test: unselected candidate")?;
        unselected.propensity = Some(0.75);
        Ok(())
    })?;
    replace_evidence_row(&workspace, &key, &store_workspace, &owner, &decision_id, &substituted)?;
    let Err(error) = verify_terminal_replay_journal(
        fabric.memory().as_ref(),
        &workspace,
        &key,
        &store_workspace,
        fabric.workspace_id(),
        &owner,
        &terminal_id,
        &terminal,
    )
    .await
    else {
        anyhow::bail!("substituted hop digest was accepted");
    };
    anyhow::ensure!(
        format!("{error:?}").contains("terminal replay decision chain changed"),
        "test: substitution must fail the hop digest check: {error:?}"
    );
    anyhow::ensure!(
        !db.exists(),
        "test: the journal replay must not create the decision store database"
    );
    Ok(())
}

/// Moves the decision store database and its sidecars aside, so only journals remain.
fn quarantine_decision_store(workspace: &std::path::Path) -> Result<PathBuf> {
    let db = crate::causal_tree::store::db_path(workspace);
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", db.display()));
        if path.exists() {
            std::fs::rename(&path, format!("{}.unavailable", path.display()))?;
        }
    }
    anyhow::ensure!(!db.exists(), "test: the decision store database must be quarantined");
    Ok(db)
}

/// Raw journal entries for one fixed query, read without repair.
fn journal_entries(workspace: &std::path::Path, query: &'static str) -> Result<Vec<String>> {
    let db = rusqlite::Connection::open_with_flags(
        authority_journal::path(workspace),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = db.prepare(query)?;
    let rows = statement
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows)
}

const EVIDENCE_ENTRIES: &str = "SELECT entry FROM decision_evidence ORDER BY workspace, owner, decision";

#[tokio::test]
async fn process_terminal_repair_uses_child_journal() -> Result<()> {
    use crate::agent::terminal::verify_terminal_replay_journal;
    use crate::runtime::child_source::completion::{ChildOutcomeHook, JournalChildOutcomeHook};

    let tmp = tempfile::tempdir()?;
    let (manifest, result) = completed_child_fixture(&tmp).await?;
    let ReopenedChild {
        workspace,
        key,
        expected,
        launch_version,
    } = reopened_child_evidence(&manifest, &result).await?;
    anyhow::ensure!(launch_version == 3, "test: the launch authority must be version 3");
    let source = manifest.child_source.as_ref().context("test: child source")?;
    let store_workspace = source.parent_store_workspace.clone();
    let owner = manifest.owner_id.clone().context("test: owner")?;
    let evidence = result
        .completion_evidence
        .as_ref()
        .context("test: completion evidence")?;
    let [segment] = evidence.segments.as_slice() else {
        anyhow::bail!("test: expected one completed segment");
    };
    let evidence_before = journal_entries(&workspace, EVIDENCE_ENTRIES)?;

    // Delete only the settled child outcome row, then drop the decision store.
    let db = rusqlite::Connection::open(authority_journal::path(&workspace))?;
    let deleted = db.execute(
        "DELETE FROM child_outcome WHERE run = ?1 AND settlement = ?2",
        (&expected.run, &expected.settlement),
    )?;
    anyhow::ensure!(deleted == 1, "test: exactly one child outcome row must be deleted");
    drop(db);
    let store_db = quarantine_decision_store(&workspace)?;

    // Reopen memory, fabric, key and the segment marker without any store.
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.as_path())?);
    let fabric = MemoryFabric::new(
        memory,
        manifest.memory_workspace_id.clone().context("test: memory workspace")?,
    );
    let terminal = fabric
        .memory()
        .find_message_event_by_idempotency_key(fabric.workspace_id(), &format!("turn:{}:final", segment.terminal_id))
        .await?
        .context("test: segment terminal marker")?;
    let error = authority_journal::verify_child_outcome(&workspace, &key, &expected, Some(launch_version))
        .expect_err("a version-3 child without its outcome row must be refused");
    assert!(error.to_string().contains("child outcome row is missing"), "{error:#}");
    anyhow::ensure!(
        child_outcome_rows(&workspace)?.is_empty(),
        "test: the refused read must not insert a row"
    );

    let memory = fabric.memory();
    let replay_from_journal = || {
        verify_terminal_replay_journal(
            memory.as_ref(),
            &workspace,
            &key,
            &store_workspace,
            fabric.workspace_id(),
            &owner,
            &segment.terminal_id,
            &terminal,
        )
    };
    let replay = replay_from_journal().await?;
    anyhow::ensure!(
        replay.parent_decision_id().map(|id| id.to_string()) == expected.parent_decision
            && replay.parent_digest().map(str::to_owned) == expected.parent_digest
            && replay.decision().decision_id.to_string() == expected.child_decision
            && replay.decision_digest() == expected.child_digest
            && replay.source().run_id.as_deref() == Some(expected.run.as_str()),
        "test: the journal replay must match the saved expected row"
    );
    let hook = JournalChildOutcomeHook::new(workspace.clone(), Arc::clone(&key), Arc::from(store_workspace.as_str()));
    hook.persist(&replay, &terminal, None, None).await?;
    assert!(
        matches!(
            authority_journal::verify_child_outcome(&workspace, &key, &expected, Some(launch_version)),
            Ok(true)
        ),
        "journal repair did not restore the authenticated child outcome"
    );
    let repaired = child_outcome_rows(&workspace)?;
    anyhow::ensure!(
        repaired.len() == 1 && repaired[0].0 == expected.run && repaired[0].1 == expected.settlement,
        "test: repair must leave exactly one row at the expected key: {repaired:?}"
    );
    anyhow::ensure!(
        journal_entries(&workspace, EVIDENCE_ENTRIES)? == evidence_before && !store_db.exists(),
        "test: repair must not touch decision evidence or recreate the store"
    );

    // Idempotent: the same adapter and hook leave identical bytes.
    let replay = replay_from_journal().await?;
    hook.persist(&replay, &terminal, None, None).await?;
    anyhow::ensure!(
        child_outcome_rows(&workspace)? == repaired,
        "test: a repeated repair must be byte-identical"
    );

    // Corruption: flip one seal byte. Replay still works, the hook refuses to overwrite.
    let db = rusqlite::Connection::open(authority_journal::path(&workspace))?;
    let mut entry: SealedEntry = serde_json::from_str(&repaired[0].2)?;
    entry.seal[0] ^= 1;
    let tampered = serde_json::to_string(&entry)?;
    db.execute(
        "UPDATE child_outcome SET entry = ?1 WHERE run = ?2 AND settlement = ?3",
        (&tampered, &expected.run, &expected.settlement),
    )?;
    drop(db);
    let replay = replay_from_journal().await?;
    anyhow::ensure!(
        hook.persist(&replay, &terminal, None, None).await.is_err(),
        "test: the hook must refuse to overwrite a corrupt child outcome row"
    );
    anyhow::ensure!(
        child_outcome_rows(&workspace)? == vec![(expected.run.clone(), expected.settlement.clone(), tampered)],
        "test: the corrupt entry must stay unchanged"
    );
    Ok(())
}

#[tokio::test]
async fn launch_v3_succeeds_with_matching_parent_row() -> Result<()> {
    const AUTHORITY_ENTRIES: &str = "SELECT entry FROM authority ORDER BY run";
    const LAUNCH_ENTRIES: &str = "SELECT entry FROM launch_parent ORDER BY run";
    let tmp = tempfile::tempdir()?;
    with_prepared_worker_manifest(&tmp, "ollama".into(), None, |manifest| async move {
        let source = manifest.child_source.as_ref().context("test: child source")?;
        anyhow::ensure!(source.journal_version == Some(3), "test: the child must be version 3");
        let workspace = PathBuf::from(&source.parent_constraints.workspace);
        let owner = manifest.owner_id.clone().context("test: owner")?;
        let parent = manifest
            .capability_manifest
            .as_ref()
            .context("test: capability manifest")?
            .parent_decision_id
            .clone();
        let key = crate::decision::DecisionKey::load_existing(&workspace)?;
        let authority_before = journal_entries(&workspace, AUTHORITY_ENTRIES)?;
        let launch_before = journal_entries(&workspace, LAUNCH_ENTRIES)?;
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
        anyhow::ensure!(
            authority_journal::verify(&manifest, &workspace, &key)?,
            "test: a version-3 child with its matching parent row must verify"
        );
        anyhow::ensure!(
            journal_entries(&workspace, AUTHORITY_ENTRIES)? == authority_before
                && journal_entries(&workspace, LAUNCH_ENTRIES)? == launch_before,
            "test: verification must leave the journal rows unchanged"
        );
        Ok(())
    })
    .await
}

/// Real evidence rows of one owner, as raw `(decision, entry)` pairs.
fn owner_evidence(db: &rusqlite::Connection, store_workspace: &str, owner: &str) -> Result<Vec<(String, String)>> {
    let mut statement =
        db.prepare("SELECT decision, entry FROM decision_evidence WHERE workspace = ?1 AND owner = ?2 ORDER BY rowid")?;
    let rows = statement
        .query_map((store_workspace, owner), |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
    Ok(rows)
}

/// Reads a discovered child's terminal marker once, as discovery does, and replays it from journal evidence.
async fn replay_candidate(
    memory: &dyn Memory,
    workspace: &std::path::Path,
    key: &Arc<crate::decision::DecisionKey>,
    store_workspace: &str,
    scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
    lineage: &crate::causal_tree::store::ChildRecoveryLineage,
    tracker: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    deadline: std::time::Instant,
) -> Result<
    Option<(
        crate::memory::MessageEvent,
        crate::agent::terminal::VerifiedTerminalReplay,
    )>,
> {
    let Some(event) = memory
        .read_ingress_event(
            scope,
            &crate::memory::ingress_recovery::IngressEventLookup::OwnedIdempotencyKey(format!(
                "turn:{}:final",
                lineage.run_id
            )),
            deadline,
        )
        .await?
    else {
        return Ok(None);
    };
    let replay = crate::runtime::child_source::recovery::verify_candidate(
        memory,
        workspace,
        key,
        store_workspace,
        scope,
        lineage,
        &event,
        tracker,
        deadline,
    )
    .await?;
    Ok(Some((event, replay)))
}

#[tokio::test]
async fn child_recovery_journal_cursor_wraps() -> Result<()> {
    use crate::causal_tree::ids::DecisionId;
    use crate::memory::ingress_recovery::IngressRecoveryScope;
    use crate::runtime::child_source::recovery;
    use crate::runtime::turn_cleanup::CleanupTracker;
    use rusqlite::types::Value;

    let tmp = tempfile::tempdir()?;
    let (manifest, result) = completed_child_fixture(&tmp).await?;
    let ReopenedChild {
        workspace,
        key,
        expected,
        ..
    } = reopened_child_evidence(&manifest, &result).await?;
    let store_workspace = manifest
        .child_source
        .as_ref()
        .context("test: child source")?
        .parent_store_workspace
        .clone();
    let owner = manifest.owner_id.clone().context("test: owner")?;

    // Set the real chain aside (bytes kept), drop the settled row, drop the store.
    let db = rusqlite::Connection::open(authority_journal::path(&workspace))?;
    let real = owner_evidence(&db, &store_workspace, &owner)?;
    anyhow::ensure!(!real.is_empty(), "test: the child must have evidence rows");
    let template = {
        let columns =
            authority_journal::read_decision_evidence(&workspace, &store_workspace, &owner, &real[0].0, &key)?
                .context("test: template evidence row")?;
        named_evidence_columns(columns)?
    };
    db.execute(
        "DELETE FROM decision_evidence WHERE workspace = ?1 AND owner = ?2",
        (&store_workspace, &owner),
    )?;
    anyhow::ensure!(
        db.execute(
            "DELETE FROM child_outcome WHERE run = ?1 AND settlement = ?2",
            (&expected.run, &expected.settlement),
        )? == 1,
        "test: exactly one child outcome row must be deleted"
    );
    quarantine_decision_store(&workspace)?;

    // 65 initial rows: one invalid entry in the middle, the rest valid with no terminal.
    for index in 0..65 {
        if index == 30 {
            db.execute(
                "INSERT INTO decision_evidence(workspace, owner, decision, entry) VALUES (?1, ?2, 'invalid', 'not-an-entry')",
                (&store_workspace, &owner),
            )?;
            continue;
        }
        let mut columns = template.clone();
        set_column(
            &mut columns,
            "decision_id",
            Value::Text(DecisionId::new_runtime().to_string()),
        )?;
        set_column(&mut columns, "run_id", Value::Text(format!("filler-{index}")))?;
        authority_journal::write_decision_evidence(&workspace, &key, &[columns])?;
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut position = None;
    let next_page = |position: &mut Option<authority_journal::EvidenceCursor>| {
        authority_journal::next_evidence_page(position, &workspace, &key, &store_workspace, &owner, 64, deadline)
    };
    let first = next_page(&mut position)?;
    anyhow::ensure!(first.len() == 64, "test: the first page must hold exactly 64 rows");
    anyhow::ensure!(
        first.iter().filter(|candidate| candidate.lineage.is_none()).count() == 1,
        "test: the invalid entry must be reported without lineage"
    );
    for candidate in &first {
        position = Some(candidate.cursor);
    }

    // Late rows land above the captured high water: the real chain, byte for byte.
    for (decision, entry) in &real {
        db.execute(
            "INSERT INTO decision_evidence(workspace, owner, decision, entry) VALUES (?1, ?2, ?3, ?4)",
            (&store_workspace, &owner, decision, entry),
        )?;
    }
    let second = next_page(&mut position)?;
    anyhow::ensure!(
        second.len() == 1,
        "test: only the last initial row remains in this pass"
    );
    for candidate in &second {
        position = Some(candidate.cursor);
    }
    let empty = next_page(&mut position)?;
    anyhow::ensure!(
        empty.is_empty(),
        "test: late rows must stay above this pass's high water"
    );

    // The next pass restarts at the first row and walks, page by page, to the late rows.
    let mut third = Vec::new();
    loop {
        let page = next_page(&mut position)?;
        anyhow::ensure!(page.len() <= 64, "test: every page is bounded");
        if page.is_empty() {
            break;
        }
        for candidate in page {
            position = Some(candidate.cursor);
            third.push(candidate);
        }
    }
    anyhow::ensure!(
        third.len() == 65 + real.len(),
        "test: the next pass must reach the late rows, saw {}",
        third.len()
    );
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.as_path())?);
    let fabric = MemoryFabric::new(
        memory,
        manifest.memory_workspace_id.clone().context("test: memory workspace")?,
    );
    let scope = IngressRecoveryScope {
        workspace_id: fabric.workspace_id().to_owned(),
        owner_id: owner.clone(),
        source: crate::memory::MessageEventSource::SessionWorker,
    };
    let tracker = Arc::new(CleanupTracker::default());
    let store_arc: Arc<str> = Arc::from(store_workspace.as_str());
    let mut repaired = 0;
    for candidate in &third {
        let Some(lineage) = candidate.lineage.as_ref() else {
            continue;
        };
        let Some((event, replay)) = replay_candidate(
            fabric.memory().as_ref(),
            &workspace,
            &key,
            &store_workspace,
            &scope,
            lineage,
            &tracker,
            deadline,
        )
        .await?
        else {
            continue;
        };
        if replay.parent_decision_id().is_some()
            && recovery::settle(&workspace, &key, &store_arc, &replay, &event, &tracker, deadline).await?
        {
            repaired += 1;
        }
    }
    anyhow::ensure!(
        repaired == 1,
        "test: the late child must be repaired exactly once, got {repaired}"
    );
    // Structural check, kept after the repair proof: the terminating empty page
    // of the walk must leave the cursor reset.
    anyhow::ensure!(
        position.is_none(),
        "test: an empty page must reset the exhausted cursor"
    );
    anyhow::ensure!(
        authority_journal::verify_child_outcome(
            &workspace,
            &key,
            &expected,
            Some(authority_journal::JOURNAL_REQUIRED_VERSION),
        )?,
        "test: the repaired child outcome must verify"
    );
    let rows = child_outcome_rows(&workspace)?;
    anyhow::ensure!(
        rows.len() == 1 && rows[0].0 == expected.run && rows[0].1 == expected.settlement,
        "test: repair must leave exactly one row at the expected key: {rows:?}"
    );

    // A second pass finds the row settled and writes nothing.
    let (event, replay) = {
        let lineage = third
            .iter()
            .filter_map(|candidate| candidate.lineage.as_ref())
            .find(|lineage| lineage.run_id == expected.run)
            .context("test: the child's own evidence row")?;
        replay_candidate(
            fabric.memory().as_ref(),
            &workspace,
            &key,
            &store_workspace,
            &scope,
            lineage,
            &tracker,
            deadline,
        )
        .await?
        .context("test: the child terminal must replay again")?
    };
    anyhow::ensure!(
        !recovery::settle(&workspace, &key, &store_arc, &replay, &event, &tracker, deadline).await?
            && child_outcome_rows(&workspace)? == rows,
        "test: a settled child must not be rewritten"
    );
    Ok(())
}

#[tokio::test]
async fn non_worker_child_replays_with_parent_evidence() -> Result<()> {
    use crate::agent::terminal::{replay_requires_journal, reseal_payload_for_test, verify_terminal_replay_journal};
    use crate::causal_tree::store::{CausalDecisionStore, DecisionReplayRecord};

    let tmp = tempfile::tempdir()?;
    let workspace = tmp.path().to_path_buf();
    let terminal_id = crate::tools::delegate::tests::retained_delegation_fixture(&workspace).await?;
    let store_workspace = CausalDecisionStore::open_unshared_read_only(&workspace)
        .workspace_id()
        .context("test: store namespace")?
        .to_string();
    let store_db = quarantine_decision_store(&workspace)?;

    // Reopen memory, fabric and key; only the journals remain.
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.as_path())?);
    let fabric = MemoryFabric::new(memory, workspace.to_string_lossy().into_owned());
    let key = crate::decision::DecisionKey::load_existing(&workspace)?;
    let marker = fabric
        .memory()
        .find_message_event_by_idempotency_key(fabric.workspace_id(), &format!("turn:{terminal_id}:final"))
        .await?
        .context("test: delegate terminal marker")?;
    let owner = marker.owner_id.clone().context("test: terminal owner")?;

    anyhow::ensure!(
        replay_requires_journal(&key, &marker, None)?,
        "non-worker v3 terminal did not select journal authority"
    );
    let replay = verify_terminal_replay_journal(
        fabric.memory().as_ref(),
        &workspace,
        &key,
        &store_workspace,
        fabric.workspace_id(),
        &owner,
        &terminal_id,
        &marker,
    )
    .await?;
    let parent = replay
        .parent_decision_id()
        .context("test: delegated child has a parent")?;
    let parent_columns =
        authority_journal::read_decision_evidence(&workspace, &store_workspace, &owner, &parent.to_string(), &key)?
            .context("test: parent decision_evidence row")?;
    anyhow::ensure!(
        replay.parent_digest()
            == Some(
                DecisionReplayRecord::from_evidence_columns(&parent_columns)?
                    .digest
                    .as_str()
            ),
        "test: the replayed parent digest differs from its decision_evidence row"
    );
    anyhow::ensure!(
        authority_journal::read_launch_parent(&workspace, &terminal_id, &key)?.is_none(),
        "test: a non-worker child has no launch parent row"
    );
    anyhow::ensure!(!store_db.exists(), "test: the decision store must stay absent");

    // Controls: without the signed field the marker is legacy, and a flipped MAC is refused.
    let mut payload: serde_json::Value =
        serde_json::from_str(marker.raw_payload_json.as_deref().context("test: payload")?)?;
    payload
        .get_mut("terminal_replay_v1")
        .and_then(serde_json::Value::as_object_mut)
        .context("test: replay binding")?
        .remove("journal_version");
    reseal_payload_for_test(&key, &mut payload, replay.assistant())?;
    let mut legacy = marker.clone();
    legacy.raw_payload_json = Some(serde_json::to_string(&payload)?);
    anyhow::ensure!(
        !replay_requires_journal(&key, &legacy, None)?,
        "test: a marker without the journal field must select the store"
    );
    let mut forged = marker.clone();
    let mut payload: serde_json::Value =
        serde_json::from_str(marker.raw_payload_json.as_deref().context("test: payload")?)?;
    let mac = payload.get_mut("terminal_replay_mac_v1").context("test: replay MAC")?;
    let flipped = mac.as_str().context("test: MAC text")?.replacen(['0', '1'], "f", 1);
    anyhow::ensure!(Some(flipped.as_str()) != mac.as_str(), "test: the MAC must change");
    *mac = serde_json::Value::String(flipped);
    forged.raw_payload_json = Some(serde_json::to_string(&payload)?);
    anyhow::ensure!(
        replay_requires_journal(&key, &forged, None).is_err(),
        "test: a flipped MAC byte must be refused"
    );
    Ok(())
}
