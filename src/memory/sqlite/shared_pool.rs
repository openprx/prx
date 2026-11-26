//! One `brain.db` pool per process.
//!
//! Every [`super::SqliteMemory`] opened on the same database file in this
//! process shares one base [`MemoryPool`]: one writer connection, the readers,
//! the `ramflux_origin` function and the commit hook. Schema init, trigger
//! install, the rf WAL scrub and the notify install run once, on the first open.
//! Later openers reuse the live pool, so they neither open a second writer nor
//! wait on a second `BEGIN IMMEDIATE` schema transaction.
//!
//! The registry holds `Weak` entries. The pool closes when its last user drops,
//! so a later open in the same process (a test, or a reload) starts a fresh pool.
//! Per-instance state (ACL mode, embedder, weights, caches, ingress and recovery
//! caches, write provenance) is not shared here; it stays on each
//! `SqliteMemory`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::Context;
use parking_lot::Mutex;

use super::erasure_pool::MemoryPool;
use super::{SQLITE_READER_PRAGMAS, SQLITE_WRITER_PRAGMAS, SqliteMemory, restrict_owner_mode, rf_delete, sync_journal};
use crate::runtime::sqlite_pool::SqliteConnectionPool as RawSqliteConnectionPool;

/// One slot per database path. The mutex is held across that path's first open
/// and guards the `Weak` to its live pool, so concurrent openers of the same path
/// serialize on the slot while openers of other paths never touch it.
type Slot = Mutex<Weak<MemoryPool>>;

/// Map from path to slot. Its lock is held only to look up, insert or prune a
/// slot, never while a pool is opened.
type Registry = Mutex<BTreeMap<PathBuf, Arc<Slot>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// Drop slots whose pool is gone. A slot with another holder is in use and kept.
/// An unheld slot cannot be locked by anyone else, because locking needs a clone
/// of the `Arc` and the map lock held here blocks new clones.
fn prune_dead_slots(slots: &mut BTreeMap<PathBuf, Arc<Slot>>) {
    slots.retain(|_, slot| Arc::strong_count(slot) > 1 || slot.lock().strong_count() > 0);
}

/// Registry key for `db_path`.
///
/// Once the file exists its canonical path is the key, so symlinked paths map to
/// one entry. Before the first open creates the file, the key is the canonical
/// parent directory joined with the file name. The caller has already created
/// the parent directory.
fn registry_key(db_path: &Path) -> anyhow::Result<PathBuf> {
    if db_path.exists() {
        return std::fs::canonicalize(db_path).context("canonicalize brain.db path");
    }
    let name = db_path.file_name().context("brain.db path has no file name")?;
    let parent = match db_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Ok(std::fs::canonicalize(parent)
        .context("canonicalize brain.db parent directory")?
        .join(name))
}

/// Return the process-wide pool and sync runtime for `db_path`.
///
/// The first caller in the process builds the pool, runs the whole sequence once
/// and installs the notify hook. A caller that arrives while a live pool exists
/// reuses it and skips that sequence. `max_readers` and `open_timeout_secs` are
/// applied only by the opener that builds the pool, so the first opener's values
/// win for the process. A failed first open leaves no registry entry.
pub(super) fn open_shared(
    db_path: &Path,
    max_readers: usize,
    open_timeout_secs: Option<u64>,
) -> anyhow::Result<(Arc<MemoryPool>, Arc<sync_journal::Runtime>)> {
    let key = registry_key(db_path)?;
    // Lock order is always map, then slot, and never the reverse. `open_first`
    // runs with only the slot lock held: the map lock is released first. The
    // prune in the block above takes a slot lock while the map is held, but only
    // on a slot with no other holder, so that lock is uncontended and cannot wait.
    let slot = {
        let mut slots = registry().lock();
        prune_dead_slots(&mut slots);
        Arc::clone(slots.entry(key).or_default())
    };
    // The slot lock is held across this path's first open. A racing opener of
    // the same path waits here and then reuses the pool. Other paths have their
    // own slot and do not wait.
    let mut live = slot.lock();
    if let Some(pool) = live.upgrade() {
        let sync_runtime = sync_journal::for_database(db_path)?;
        return Ok((pool, sync_runtime));
    }
    let (pool, sync_runtime) = open_first(db_path, max_readers, open_timeout_secs)?;
    *live = Arc::downgrade(&pool);
    Ok((pool, sync_runtime))
}

/// The one-time sequence for a process's first open of `db_path`.
fn open_first(
    db_path: &Path,
    max_readers: usize,
    open_timeout_secs: Option<u64>,
) -> anyhow::Result<(Arc<MemoryPool>, Arc<sync_journal::Runtime>)> {
    // `reset_transactions_on_release` is on because this module issues bare
    // `BEGIN` through `execute_batch` in places: `rusqlite::Transaction`
    // would clean up after itself, a bare `BEGIN` would not, and a leftover
    // transaction stalls every later write (writer) or pins a stale WAL
    // snapshot for every later caller (reader).
    let raw = RawSqliteConnectionPool::builder(db_path.to_path_buf())
        .max_readers(max_readers)
        .open_timeout_secs(open_timeout_secs)
        .writer_pragmas(SQLITE_WRITER_PRAGMAS)
        .reader_pragmas(SQLITE_READER_PRAGMAS)
        .reset_transactions_on_release(true)
        .build()?;
    // SQLite creates the file as 0666 masked by the process umask. A
    // group-writable umask makes the ingress lease reject the database.
    // This must run before schema init.
    restrict_owner_mode(db_path)?;

    SqliteMemory::init_schema(&raw.write())?;

    let raw = Arc::new(raw);
    RawSqliteConnectionPool::publish_metrics(&raw);
    let pool = Arc::new(MemoryPool::new(raw)?);
    // Deleted rf content a previous process left in the WAL (AC-1b2b).
    match rf_delete::scrub(&pool) {
        Ok(true) => {}
        Ok(false) => tracing::warn!("deleted rf content stays in brain.db-wal while another process reads it"),
        Err(error) => tracing::warn!(
            error = %crate::agent::diagnostics::error_class(&error),
            "deleted rf content WAL scrub failed at open"
        ),
    }

    let sync_runtime = sync_journal::for_database(db_path)?;
    sync_journal::install_notify(&mut pool.write(), &sync_runtime)?;
    Ok((pool, sync_runtime))
}
