#![cfg(test)]
use super::*;
use crate::causal_tree::store::{DecisionId, Retention, now_ms, store_schema, testing};
use crate::decision::telemetry::CausalEventSink;
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

fn commit_counter(connection: &Connection) -> anyhow::Result<Arc<AtomicUsize>> {
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
    let counter = commit_counter(&connection)?;
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
    let counter = commit_counter(&connection)?;
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

struct CommittedNotifications {
    counter: Arc<AtomicUsize>,
    seen: parking_lot::Mutex<Vec<usize>>,
}
impl CausalEventSink for CommittedNotifications {
    fn emit(&self, event: CausalEvent) {
        if event.kind == CausalEventKind::DecisionCommitted {
            self.seen.lock().push(self.counter.load(Ordering::SeqCst));
        }
    }
}

#[test]
fn the_real_writer_stops_batches_at_drain_and_capacity_before_notifying() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("prx_decisions.db");
    let mut connection = Connection::open(&path)?;
    store_schema::migrate(&mut connection)?;
    let workspace = store_schema::workspace_id(&connection)?.context("workspace")?;
    let counter = commit_counter(&connection)?;
    let events = Arc::new(CommittedNotifications {
        counter: Arc::clone(&counter),
        seen: parking_lot::Mutex::new(Vec::new()),
    });
    let (queue, commands) = std::sync::mpsc::sync_channel(64);
    for _ in 0..2 {
        queue.send(WriterMessage::DecisionBatch(
            vec![record(&workspace)].into_boxed_slice(),
        ))?;
    }
    let (done, drained) = std::sync::mpsc::channel();
    queue.send(WriterMessage::Drain(done))?;
    for _ in 0..40 {
        queue.send(WriterMessage::DecisionBatch(
            vec![record(&workspace)].into_boxed_slice(),
        ))?;
    }
    let (done, closed) = std::sync::mpsc::channel();
    queue.send(WriterMessage::Close(done))?;
    let sink: Arc<dyn CausalEventSink> = events.clone();
    super::super::run_writer(WriterThread {
        conn: connection,
        commands,
        events: sink,
        retention: Retention::days(30, 30),
        path: path.clone(),
        writes_since_prune: 0,
        hold: None,
    });
    assert!(drained.try_recv().is_ok());
    assert!(closed.try_recv().is_ok());
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    let seen = events.seen.lock();
    assert_eq!(seen.len(), 42);
    assert!(seen.iter().take(2).all(|count| *count == 1));
    assert!(seen.iter().skip(2).take(32).all(|count| *count == 2));
    assert!(seen.iter().skip(34).all(|count| *count == 3));
    drop(seen);
    assert_eq!(rows(&Connection::open(&path)?)?, 42);
    Ok(())
}
