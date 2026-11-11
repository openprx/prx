//! C5b: SIGKILL the receiving process at semantic barriers; inspect its DB
//! through an independent connection before and after a real restart.
#![cfg(all(feature = "test-mock", unix))]
use anyhow::{Context, Result, ensure};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::io::Read as _;
use std::time::{Duration, Instant};

fn crash_recovery(stage: &str, transcript_before: i64, state_before: i64) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let barrier = dir.path().join("barrier");
    std::fs::write(&barrier, b"hold")?;
    let mut child = Receiver(Command::new(env!("CARGO_BIN_EXE_prx"))
        .args(["__mailbox-harness", "accept-adopt", "--workspace"])
        .arg(dir.path())
        .env("OPENPRX_TEST_MAILBOX_BARRIER", format!("{stage}={}", barrier.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?);
    let started = dir.path().join("barrier.started");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !started.exists() {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("receiving process exited before {stage}: {status}");
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("receiving process did not reach {stage}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill()?;
    child.wait()?;
    let database = dir.path().join("memory/brain.db");
    inspect(&database, transcript_before, state_before)?;
    restart(dir.path())?;
    inspect(&database, 1, 2)?;
    // A second restart must neither append nor consume twice.
    restart(dir.path())?;
    inspect(&database, 1, 2)
}
struct Receiver(Child);
impl std::ops::Deref for Receiver {
    type Target=Child;
    fn deref(&self)->&Child {&self.0}
}
impl std::ops::DerefMut for Receiver {
    fn deref_mut(&mut self)->&mut Child {&mut self.0}
}
impl Drop for Receiver {
    fn drop(&mut self) {let _=self.0.kill();let _=self.0.wait();}
}
fn restart(workspace:&Path)->Result<()> {
    let mut receiver=Receiver(Command::new(env!("CARGO_BIN_EXE_prx"))
        .args(["__mailbox-harness","accept-adopt","--workspace"]).arg(workspace)
        .env_remove("OPENPRX_TEST_MAILBOX_BARRIER").stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?);
    let deadline=Instant::now()+Duration::from_secs(20);
    loop {
        if let Some(status)=receiver.try_wait()? {
            let mut stderr=String::new();
            if let Some(mut pipe)=receiver.stderr.take() {pipe.read_to_string(&mut stderr)?;}
            ensure!(status.success(),"restart failed: {stderr}");
            return Ok(());
        }
        ensure!(Instant::now()<deadline,"mailbox restart exceeded hang guard");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn inspect(path: &Path, transcript: i64, state: i64) -> Result<()> {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("open independent mailbox inspector")?;
    let turns: i64 = conn.query_row("SELECT count(*) FROM conversation_turns", [], |r| r.get(0))?;
    let events: i64 = conn.query_row(
        "SELECT count(*) FROM message_events WHERE source='prx_mailbox'",
        [],
        |r| r.get(0),
    )?;
    let (receipts, receipt_state): (i64, i64) =
        conn.query_row("SELECT count(*),min(state) FROM prx_mailbox_receipts", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let (messages, message_state): (i64, i64) =
        conn.query_row("SELECT count(*),min(state) FROM prx_mailbox_messages", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    ensure!(
        turns == transcript && events == transcript,
        "transcript adoption count: turns={turns}, events={events}, expected={transcript}"
    );
    ensure!(
        receipts == 1 && messages == 1 && receipt_state == state && message_state == state,
        "durable mailbox states differ: receipts={receipts}/{receipt_state}, messages={messages}/{message_state}, expected={state}"
    );
    let adopted: i64 = conn.query_row(
        "SELECT count(*) FROM prx_mailbox_receipts WHERE adopted_turn_id IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        adopted == transcript,
        "receipt adoption marker did not commit atomically"
    );
    Ok(())
}
#[test]
fn mailbox_crash_recovery_after_accept_commit() -> Result<()> {
    crash_recovery("after_accept_commit", 0, 0)
}
#[test]
fn mailbox_crash_recovery_inside_adopt_tx() -> Result<()> {
    crash_recovery("inside_adopt_tx", 0, 1)
}
#[test]
fn mailbox_crash_recovery_after_adopt_commit() -> Result<()> {
    crash_recovery("after_adopt_commit", 1, 2)
}
