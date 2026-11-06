//! Immutable launch authority, independent of the causal decision database.
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::session_worker::protocol::WorkerManifest;
use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    payload: String,
    seal: [u8; 32],
}

pub(crate) fn path(workspace: &Path) -> PathBuf {
    workspace.join("self/child_authority/journal.db")
}

fn binding(worker: &WorkerManifest) -> anyhow::Result<String> {
    let source = worker.child_source.as_ref().context("worker child source missing")?;
    Ok(serde_json::to_string(&(
        1_u8,
        &worker.run_id,
        &worker.parent_run_id,
        &worker.owner_id,
        &source.parent_constraints,
        &source.parent_source,
        &source.parent_store_workspace,
        &source.parent_decision_digest,
    ))?)
}

/// Called only by the live grant issuer before handing off the signed manifest.
pub(super) fn persist(worker: &WorkerManifest, key: &DecisionKey) -> anyhow::Result<()> {
    let source = worker.child_source.as_ref().context("worker child source missing")?;
    let path = path(Path::new(&source.parent_constraints.workspace));
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(path.parent().context("authority journal directory missing")?)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let conn = Connection::open(&path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS authority (run TEXT PRIMARY KEY, entry TEXT NOT NULL);",
    )?;
    let payload = binding(worker)?;
    let seal = key.hmac(
        KeyDomain::StateFingerprint,
        &[b"worker-launch-authority-v1", payload.as_bytes()],
    );
    let entry = serde_json::to_string(&Entry { payload, seal })?;
    conn.execute(
        "INSERT OR IGNORE INTO authority(run, entry) VALUES (?1, ?2)",
        (&worker.run_id, &entry),
    )?;
    let stored: String = conn.query_row("SELECT entry FROM authority WHERE run=?1", [&worker.run_id], |row| {
        row.get(0)
    })?;
    anyhow::ensure!(stored == entry, "worker authority journal launch conflict");
    Ok(())
}

/// Only an absent file/row permits legacy fallback. Corruption and mismatches fail closed.
pub(crate) fn verify(worker: &WorkerManifest, workspace: &Path, key: &DecisionKey) -> anyhow::Result<bool> {
    let path = path(workspace);
    match std::fs::metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        result => {
            result?;
        }
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let entry: Option<String> = conn
        .query_row("SELECT entry FROM authority WHERE run=?1", [&worker.run_id], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(entry) = entry else {
        return Ok(false);
    };
    let entry: Entry = serde_json::from_str(&entry)?;
    anyhow::ensure!(
        key.verify_hmac(
            KeyDomain::StateFingerprint,
            &[b"worker-launch-authority-v1", entry.payload.as_bytes()],
            &entry.seal
        ),
        "worker authority journal seal is invalid"
    );
    anyhow::ensure!(
        entry.payload == binding(worker)?,
        "worker authority journal differs from issued parent"
    );
    Ok(true)
}
