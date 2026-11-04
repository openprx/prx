#![cfg(test)]

//! Contention diagnostics for existing five-second boundaries; no timer changes.
use super::*;
use std::time::Duration;

pub(crate) fn wal_switch() -> anyhow::Result<String> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("probe.db");
    let held = Connection::open(&path)?;
    held.execute_batch("CREATE TABLE probe(value); BEGIN IMMEDIATE;")?;
    let contender = Connection::open(&path)?;
    contender.busy_timeout(Duration::ZERO)?;
    let error = enable_wal(&contender).expect_err("test: a held rollback journal prevents WAL switch");
    held.execute_batch("ROLLBACK;")?;
    Ok(format!("{error:#}"))
}

pub(crate) async fn writer_drain() -> anyhow::Result<String> {
    let temp = tempfile::tempdir()?;
    let hold = Arc::new(parking_lot::Mutex::new(()));
    let store = CausalDecisionStore::open(
        temp.path(),
        Retention::days(30, 30),
        Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
        StoreMode::ReadWrite,
        WriterSetup {
            hold: Some(Arc::clone(&hold)),
            ..WriterSetup::default()
        },
    )?;
    let writer = store.writer().ok_or_else(|| anyhow::anyhow!("test: writer missing"))?;
    let (ready, ready_rx) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _held = hold.lock();
        ready.send(()).unwrap();
        release_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("test: writer probe holder exceeded hang guard");
    });
    ready_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("test: writer probe holder did not start");
    let drained = writer.drain(Instant::now() + Duration::from_secs(5)).await;
    release.send(()).unwrap();
    holder.join().unwrap();
    assert!(!drained, "test: held writer must not acknowledge its drain");
    assert!(
        writer.drain(Instant::now() + Duration::from_secs(60)).await,
        "test: released writer drains under hang guard"
    );
    CausalDecisionStore::close(Arc::new(store), Instant::now() + Duration::from_secs(60)).await;
    Ok("writer.drain returned false; validate_durable_chain maps this to: decision chain did not reach durable storage before terminal claim".into())
}
