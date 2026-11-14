#![cfg(test)]
use super::*;
use crate::runtime::decision_journal::store::{DecisionId, now_ms, store_schema, testing};
use anyhow::Context;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn initialized() -> anyhow::Result<(Connection, String)> {
    let mut connection = Connection::open_in_memory()?;
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    store_schema::migrate(&mut connection)?;
    let workspace = store_schema::workspace_id(&connection)?.context("workspace")?;
    Ok((connection, workspace))
}

fn commit_counter(connection: &mut Connection) -> anyhow::Result<Arc<AtomicUsize>> {
    let counter = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&counter);
    connection.commit_hook(Some(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        false
    }))?;
    Ok(counter)
}

fn record(workspace: &str) -> DecisionRecord {
    testing::record(DecisionId::new_runtime(), workspace, Some("prx_user"), now_ms())
}

fn rows(connection: &Connection) -> anyhow::Result<i64> {
    Ok(connection.query_row("SELECT count(*) FROM decisions", [], |row| row.get(0))?)
}

#[test]
fn a_conflicting_chain_rolls_back_without_losing_its_neighbors() -> anyhow::Result<()> {
    let (mut connection, workspace) = initialized()?;
    let existing = record(&workspace);
    super::super::insert_decision_batch(&mut connection, std::slice::from_ref(&existing))?;
    let new_parent = record(&workspace);
    let parent_id = new_parent.decision_id;
    let mut conflict = existing;
    conflict.lineage.prior_decision_id = Some(parent_id);
    let batches = vec![
        vec![record(&workspace)].into_boxed_slice(),
        vec![new_parent, conflict].into_boxed_slice(),
        vec![record(&workspace)].into_boxed_slice(),
    ];
    let counter = commit_counter(&mut connection)?;
    let results = insert_ready(&mut connection, &batches)?;
    let mut results = results.into_iter();
    assert!(results.next().context("first chain")??.is_some());
    assert!(results.next().context("conflicting chain")??.is_none());
    assert!(results.next().context("last chain")??.is_some());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(rows(&connection)?, 3);
    let partial: i64 = connection.query_row(
        "SELECT count(*) FROM decisions WHERE decision_id=?1",
        [parent_id.to_string()],
        |row| row.get(0),
    )?;
    assert_eq!(partial, 0);
    Ok(())
}

#[test]
fn an_sql_failure_rolls_back_the_whole_chain_but_keeps_valid_neighbors() -> anyhow::Result<()> {
    let (mut connection, workspace) = initialized()?;
    connection.execute_batch("CREATE TRIGGER prx_refuse_decision BEFORE INSERT ON decisions WHEN NEW.configured_model='prx_refused' BEGIN SELECT RAISE(ABORT,'prx rejected chain'); END;")?;
    let parent = record(&workspace);
    let mut child = record(&workspace);
    child.lineage.prior_decision_id = Some(parent.decision_id);
    child.configured_model = "prx_refused".to_owned();
    let batches = vec![
        vec![record(&workspace)].into_boxed_slice(),
        vec![parent, child].into_boxed_slice(),
        vec![record(&workspace)].into_boxed_slice(),
    ];
    let counter = commit_counter(&mut connection)?;
    let results = insert_ready(&mut connection, &batches)?;
    let mut results = results.into_iter();
    assert!(results.next().context("first chain")??.is_some());
    assert!(results.next().context("refused chain")?.is_err());
    assert!(results.next().context("last chain")??.is_some());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(rows(&connection)?, 2);
    Ok(())
}

#[test]
fn an_outer_commit_failure_acknowledges_no_chain() -> anyhow::Result<()> {
    let (mut connection, workspace) = initialized()?;
    connection.commit_hook(Some(|| true))?;
    let batches = vec![
        vec![record(&workspace)].into_boxed_slice(),
        vec![record(&workspace)].into_boxed_slice(),
    ];
    assert!(insert_ready(&mut connection, &batches).is_err());
    assert!(connection.is_autocommit());
    assert_eq!(rows(&connection)?, 0);
    Ok(())
}
