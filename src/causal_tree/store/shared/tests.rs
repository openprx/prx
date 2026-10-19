//! A blocked SQLite migration must not serialize unrelated workspace opens.
#![cfg(test)]
use super::*;
use crate::decision::telemetry::testing::RecordingSink;
use std::time::{Duration, Instant};

#[test]
fn blocked_migration_leaves_other_workspace_open_and_existing_borrow_available() -> anyhow::Result<()> {
    let blocked = tempfile::tempdir()?;
    let independent = tempfile::tempdir()?;
    crate::decision::key::prepare_decision_dir(blocked.path()).map_err(anyhow::Error::msg)?;
    let database = db_path(blocked.path());
    let blocker = rusqlite::Connection::open(&database)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600))?;
    }
    blocker.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")?;
    let blocked_slot = slot(database);
    let workspace = blocked.path().to_owned();
    let opening = std::thread::spawn(move || {
        open(
            &workspace,
            Retention::days(0, 0),
            Arc::new(RecordingSink::default()),
            StoreMode::ReadWrite,
        )
    });
    let watchdog = Instant::now() + Duration::from_secs(2);
    while blocked_slot.opening.try_lock().is_some() && Instant::now() < watchdog {
        std::thread::yield_now();
    }
    let entered = blocked_slot.opening.try_lock().is_none();
    let other_workspace = independent.path().to_owned();
    let (sent, received) = std::sync::mpsc::channel();
    let other = std::thread::spawn(move || {
        let result = open(
            &other_workspace,
            Retention::days(0, 0),
            Arc::new(RecordingSink::default()),
            StoreMode::ReadWrite,
        );
        let _ = sent.send(result);
    });
    let independent_result = received.recv_timeout(Duration::from_secs(2));
    let blocked_still_opening = blocked_slot.opening.try_lock().is_none();
    // Always release our SQLite lock and join owned threads before assertions.
    blocker.execute_batch("ROLLBACK;")?;
    let _blocked_store = opening.join().map_err(|_| anyhow::anyhow!("store opener panicked"))??;
    other
        .join()
        .map_err(|_| anyhow::anyhow!("independent opener panicked"))?;
    let independent_store = independent_result??;
    anyhow::ensure!(
        entered && blocked_still_opening,
        "migration must still be blocked while independent open completes"
    );
    let borrowed =
        existing_writer(independent.path()).ok_or_else(|| anyhow::anyhow!("independent writer disappeared"))?;
    assert!(Arc::ptr_eq(&borrowed, &independent_store));
    Ok(())
}
