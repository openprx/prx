//! Immutable launch authority, independent of the causal decision database.
//!
//! Three sealed tables live in `self/child_authority/journal.db`. Each row is
//! `(key, entry)`, where `entry` is `{payload, seal}`. The seal domain differs per
//! table, and the seal is checked before the payload is interpreted.
use crate::causal_tree::store::DecisionRecord;
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::session_worker::protocol::WorkerManifest;
use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Worker children sealed at this version or later must have an authority row.
pub(crate) const JOURNAL_REQUIRED_VERSION: u8 = 3;
/// Launch recovery records at this version require a `launch_parent` row.
pub(crate) const LAUNCH_PARENT_VERSION: u8 = 3;
/// Batch plans at this version require a `batch_parent` row.
pub(crate) const BATCH_PARENT_VERSION: u8 = 4;

const AUTHORITY_DOMAIN: &[u8] = b"worker-launch-authority-v1";
const LAUNCH_PARENT_DOMAIN: &[u8] = b"child-launch-parent-authority-v1";
const BATCH_PARENT_DOMAIN: &[u8] = b"child-batch-parent-authority-v1";

const SELECT_AUTHORITY: &str = "SELECT entry FROM authority WHERE run=?1";
const INSERT_AUTHORITY: &str = "INSERT OR IGNORE INTO authority(run, entry) VALUES (?1, ?2)";
const SELECT_LAUNCH_PARENT: &str = "SELECT entry FROM launch_parent WHERE run=?1";
const INSERT_LAUNCH_PARENT: &str = "INSERT OR IGNORE INTO launch_parent(run, entry) VALUES (?1, ?2)";
const SELECT_BATCH_PARENT: &str = "SELECT entry FROM batch_parent WHERE batch=?1";
const INSERT_BATCH_PARENT: &str = "INSERT OR IGNORE INTO batch_parent(batch, entry) VALUES (?1, ?2)";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    payload: String,
    seal: [u8; 32],
}

/// Facts of the live parent decision that issued a grant, copied from the
/// record that was persisted and digested for that grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParentIssuerEvidence {
    pub(crate) enabled_snapshot: bool,
    pub(crate) legacy_v1: bool,
    pub(crate) admits_issued_child: bool,
    pub(crate) run_id: Option<String>,
    pub(crate) session_key: Option<String>,
    pub(crate) task_id: Option<String>,
    pub(crate) source_event_id: Option<String>,
}

impl ParentIssuerEvidence {
    /// Only an enabled, non-legacy decision that admits issued children may
    /// authorize a durable child row.
    pub(crate) fn from_record(record: &DecisionRecord) -> anyhow::Result<Self> {
        let evidence = Self {
            enabled_snapshot: record.enabled_snapshot,
            legacy_v1: record.legacy_v1,
            admits_issued_child: crate::causal_tree::store::directive_admits_issued_child(&record.directive_kind),
            run_id: record.lineage.run_id.as_deref().map(str::to_owned),
            session_key: record.lineage.session_key.as_deref().map(str::to_owned),
            task_id: record.lineage.task_id.as_deref().map(str::to_owned),
            source_event_id: record.lineage.source_event_id.as_deref().map(str::to_owned),
        };
        anyhow::ensure!(
            evidence.enabled_snapshot && !evidence.legacy_v1 && evidence.admits_issued_child,
            "parent issuer evidence does not admit child authority"
        );
        Ok(evidence)
    }
}

/// `launch_parent` payload. Parent facts are sealed here so that readers need
/// no decision-store lookup. Eligibility checks run against these fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchParentRow {
    pub(crate) version: u8,
    pub(crate) run: String,
    pub(crate) owner: String,
    pub(crate) parent_decision: String,
    pub(crate) parent_digest: String,
    pub(crate) store_workspace: String,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) session_key: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) parent_task_run_id: Option<String>,
    pub(crate) config_generation_id: u64,
    pub(crate) config_source_revision: String,
    pub(crate) enabled_snapshot: bool,
    pub(crate) legacy_v1: bool,
    pub(crate) admits_issued_child: bool,
}

/// `batch_parent` payload, keyed by batch identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchParentRow {
    pub(crate) version: u8,
    pub(crate) batch: String,
    pub(crate) owner: String,
    pub(crate) parent_decision: String,
    pub(crate) parent_digest: String,
    pub(crate) store_workspace: String,
    pub(crate) enabled_snapshot: bool,
    pub(crate) legacy_v1: bool,
    pub(crate) admits_issued_child: bool,
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

fn journal_required(worker: &WorkerManifest) -> anyhow::Result<bool> {
    let source = worker.child_source.as_ref().context("worker child source missing")?;
    Ok(source
        .journal_version
        .is_some_and(|version| version >= JOURNAL_REQUIRED_VERSION))
}

fn seal_entry(key: &DecisionKey, domain: &[u8], payload: String) -> anyhow::Result<String> {
    let seal = key.hmac(KeyDomain::StateFingerprint, &[domain, payload.as_bytes()]);
    Ok(serde_json::to_string(&Entry { payload, seal })?)
}

fn open_entry(key: &DecisionKey, domain: &[u8], label: &str, entry: &str) -> anyhow::Result<String> {
    let entry: Entry = serde_json::from_str(entry)?;
    anyhow::ensure!(
        key.verify_hmac(
            KeyDomain::StateFingerprint,
            &[domain, entry.payload.as_bytes()],
            &entry.seal
        ),
        "{label} journal seal is invalid"
    );
    Ok(entry.payload)
}

fn open_writer(workspace: &Path) -> anyhow::Result<Connection> {
    let path = path(workspace);
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
        "PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS authority (run TEXT PRIMARY KEY, entry TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS launch_parent (run TEXT PRIMARY KEY, entry TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS batch_parent (batch TEXT PRIMARY KEY, entry TEXT NOT NULL);",
    )?;
    Ok(conn)
}

struct Write<'a> {
    label: &'static str,
    insert: &'static str,
    select: &'static str,
    key: &'a str,
    entry: &'a str,
}

/// One transaction, `INSERT OR IGNORE`, then readback equality for every row.
fn write_entries(conn: &mut Connection, writes: &[Write<'_>]) -> anyhow::Result<()> {
    let transaction = conn.transaction()?;
    for write in writes {
        transaction.execute(write.insert, (write.key, write.entry))?;
        let stored: String = transaction.query_row(write.select, [write.key], |row| row.get(0))?;
        anyhow::ensure!(stored == write.entry, "{} journal conflict", write.label);
    }
    transaction.commit()?;
    Ok(())
}

/// Missing file or row is `None`. Callers decide whether that is a refusal.
fn read_row(workspace: &Path, select: &str, key: &str) -> anyhow::Result<Option<String>> {
    let path = path(workspace);
    match std::fs::metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => {
            result?;
        }
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    Ok(conn.query_row(select, [key], |row| row.get(0)).optional()?)
}

/// Called only by the live grant issuer before handing off the signed manifest.
/// The worker authority row and its launch-parent row commit together.
pub(super) fn persist(worker: &WorkerManifest, key: &DecisionKey, launch: &LaunchParentRow) -> anyhow::Result<()> {
    let source = worker.child_source.as_ref().context("worker child source missing")?;
    anyhow::ensure!(
        launch.version == LAUNCH_PARENT_VERSION && launch.run == worker.run_id,
        "launch parent row differs from worker run"
    );
    let mut conn = open_writer(Path::new(&source.parent_constraints.workspace))?;
    let authority = seal_entry(key, AUTHORITY_DOMAIN, binding(worker)?)?;
    let launch_entry = seal_entry(key, LAUNCH_PARENT_DOMAIN, serde_json::to_string(launch)?)?;
    write_entries(
        &mut conn,
        &[
            Write {
                label: "worker authority",
                insert: INSERT_AUTHORITY,
                select: SELECT_AUTHORITY,
                key: &worker.run_id,
                entry: &authority,
            },
            Write {
                label: "launch parent authority",
                insert: INSERT_LAUNCH_PARENT,
                select: SELECT_LAUNCH_PARENT,
                key: &launch.run,
                entry: &launch_entry,
            },
        ],
    )
}

/// Called by the live batch preparer before the plan event is recorded.
pub(crate) fn persist_batch_parent(workspace: &Path, key: &DecisionKey, batch: &BatchParentRow) -> anyhow::Result<()> {
    anyhow::ensure!(
        batch.version == BATCH_PARENT_VERSION,
        "batch parent row version differs"
    );
    let mut conn = open_writer(workspace)?;
    let entry = seal_entry(key, BATCH_PARENT_DOMAIN, serde_json::to_string(batch)?)?;
    write_entries(
        &mut conn,
        &[Write {
            label: "batch parent authority",
            insert: INSERT_BATCH_PARENT,
            select: SELECT_BATCH_PARENT,
            key: &batch.batch,
            entry: &entry,
        }],
    )
}

/// Returns `None` only when no row exists. A seal failure is always an error.
pub(crate) fn read_launch_parent(
    workspace: &Path,
    run: &str,
    key: &DecisionKey,
) -> anyhow::Result<Option<LaunchParentRow>> {
    let Some(entry) = read_row(workspace, SELECT_LAUNCH_PARENT, run)? else {
        return Ok(None);
    };
    let payload = open_entry(key, LAUNCH_PARENT_DOMAIN, "launch parent authority", &entry)?;
    Ok(Some(serde_json::from_str(&payload)?))
}

/// Returns `None` only when no row exists. A seal failure is always an error.
pub(crate) fn read_batch_parent(
    workspace: &Path,
    batch: &str,
    key: &DecisionKey,
) -> anyhow::Result<Option<BatchParentRow>> {
    let Some(entry) = read_row(workspace, SELECT_BATCH_PARENT, batch)? else {
        return Ok(None);
    };
    let payload = open_entry(key, BATCH_PARENT_DOMAIN, "batch parent authority", &entry)?;
    Ok(Some(serde_json::from_str(&payload)?))
}

/// A version-3+ child with no authority row is refused, never downgraded to the
/// causal store. Only a child without a version may fall back to the store.
pub(crate) fn verify(worker: &WorkerManifest, workspace: &Path, key: &DecisionKey) -> anyhow::Result<bool> {
    let required = journal_required(worker)?;
    let Some(entry) = read_row(workspace, SELECT_AUTHORITY, &worker.run_id)? else {
        anyhow::ensure!(
            !required,
            "worker authority journal row is missing for a journal-required child"
        );
        return Ok(false);
    };
    let payload = open_entry(key, AUTHORITY_DOMAIN, "worker authority", &entry)?;
    anyhow::ensure!(
        payload == binding(worker)?,
        "session-worker parent decision differs from issued authority"
    );
    Ok(true)
}
