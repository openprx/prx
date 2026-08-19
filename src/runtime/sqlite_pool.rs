//! Shared SQLite connection pooling: many readers, one serialized writer.
//!
//! Two stores need the same thing — [`crate::memory::sqlite`] for the agent
//! brain and [`crate::channels::whatsapp_storage`] for WhatsApp session state.
//! Neither may depend on the other, and both are consumed from
//! [`crate::runtime::blocking`] worker threads, so the pool lives next to the
//! blocking pool: the two halves encode the same runtime policy, which is that
//! work is offloaded and queued, never capped and never failed for waiting.
//!
//! # Why the pool is asymmetric
//!
//! SQLite is not PostgreSQL, so this deliberately does *not* mirror
//! [`crate::memory::postgres`]'s symmetric pool:
//!
//! - SQLite allows exactly one writer per database. Handing writes to N peer
//!   connections would not raise write throughput, it would only make our own
//!   connections collide on the database file lock and return `SQLITE_BUSY`.
//!   So every write shares one connection serialized by a mutex.
//! - Readers *are* genuinely concurrent, but only under WAL journalling.
//!   Without WAL a reader and the writer lock each other out and a reader pool
//!   would be pointless, so the pool turns WAL on itself rather than trusting a
//!   caller to have done it.
//!
//! # Why nothing here has a deadline
//!
//! This runtime places no cap on how many sessions, tools or turns run at once,
//! so a busy database must slow callers down rather than turn work that is
//! merely waiting its turn into an error. That applies at all three levels:
//!
//! - **Reader checkout** parks on a condvar with no deadline.
//! - **Writer checkout** blocks on the mutex with no deadline.
//! - **SQLite's own database lock** uses a busy *handler* that retries forever
//!   ([`wait_for_database_lock`]), not `busy_timeout`. A `busy_timeout` is a
//!   timeout by another name: after it expires the caller gets `SQLITE_BUSY`
//!   for a lock that is merely held by someone else, usually another process.
//!
//! Unbounded waiting is only acceptable because it is observable: every wait is
//! measured into [`SqlitePoolStats`], and a database lock held by another
//! process emits a periodic `warn!` for as long as it is held.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use parking_lot::{Condvar, Mutex, MutexGuard};
use rusqlite::Connection;

/// Upper bound applied to a caller-supplied database-open timeout.
///
/// Opening the file is storage I/O, not agent work, so it is the one place a
/// caller may ask for a deadline at all; the cap keeps a mistyped value from
/// turning into an effectively infinite startup hang.
const OPEN_TIMEOUT_CAP_SECS: u64 = 300;

/// Read connections granted per available CPU when the size is not configured.
///
/// Two per core rather than one: a read is a mix of syscall wait (page faults
/// on the database file) and CPU (row decoding), so a little oversubscription
/// keeps cores busy without turning the pool into a thread farm.
const READ_POOL_SIZE_PER_CPU: usize = 2;

/// Lower bound for the derived read-pool size.
///
/// A single-core container still runs many concurrent sessions, each performing
/// several lookups per inbound message, so the floor sits above the core count.
const MIN_READ_POOL_SIZE: usize = 4;

/// Upper bound for the derived read-pool size.
///
/// Each connection costs a file descriptor and a private page cache. Past this
/// point SQLite's shared-memory index, not connection count, is the limit.
const MAX_READ_POOL_SIZE: usize = 32;

/// Longest single sleep the busy handler performs between retries.
const BUSY_MAX_BACKOFF_MS: u64 = 50;

/// Retries between successive warnings while a database lock is held.
///
/// With the backoff below this puts the first warning roughly 0.8 s into a
/// contended wait and repeats it about every 2 s, which is what makes waiting
/// forever an observable state rather than a silent hang.
const BUSY_WARN_EVERY_ATTEMPTS: u64 = 40;

/// Read-pool size to use when no explicit size is configured.
///
/// `available_parallelism` is cgroup-aware on Linux, so the derived value
/// tracks the slice of hardware the process actually has rather than the host's
/// core count. Both SQLite stores derive their default this way; the *keys*
/// stay separate (`memory.sqlite_read_pool_size` and
/// `channels_config.whatsapp.read_pool_size`) so the two databases can be sized
/// independently.
#[must_use]
pub fn default_read_pool_size() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cpus.saturating_mul(READ_POOL_SIZE_PER_CPU)
        .clamp(MIN_READ_POOL_SIZE, MAX_READ_POOL_SIZE)
}

/// SQLite busy handler that waits for a contended database lock instead of
/// failing with `SQLITE_BUSY`.
///
/// In-process writes are already serialized by the pool, so this only fires
/// when another process (a second daemon, a CLI inspecting the database file)
/// holds the lock. Returning `true` asks SQLite to retry: contention must slow
/// a caller down rather than turn its work into an error.
///
/// The sleep grows with the retry count and is capped so a long wait neither
/// spins a CPU nor becomes invisible — every [`BUSY_WARN_EVERY_ATTEMPTS`]
/// retries it logs, because an unbounded wait is only defensible while it can
/// be seen from the outside.
fn wait_for_database_lock(attempts: i32) -> bool {
    let attempt = u64::try_from(attempts).unwrap_or(0);
    if attempt > 0 && attempt.is_multiple_of(BUSY_WARN_EVERY_ATTEMPTS) {
        tracing::warn!(
            attempts = attempt,
            "SQLite database is locked by another process; waiting rather than failing"
        );
    }
    let backoff = attempt.saturating_add(1).min(BUSY_MAX_BACKOFF_MS);
    thread::sleep(Duration::from_millis(backoff));
    true
}

/// Make `conn` wait for a contended database lock instead of returning
/// `SQLITE_BUSY`.
///
/// Pooled connections get this automatically; it is exported for the few
/// standalone connections that are opened outside a pool but share a database
/// file with one, so the whole process agrees on what a held lock means.
pub fn install_busy_handler(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_handler(Some(wait_for_database_lock))
}

/// Point-in-time view of a [`SqliteConnectionPool`].
///
/// Exposed so a health endpoint can tell "slow because the database is busy"
/// apart from "slow because the model is thinking" without attaching a
/// debugger. The `reader_*` fields describe the pool; the `write_*` fields
/// describe the single serialized writer, which SQLite requires and which
/// therefore has no pool of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlitePoolStats {
    /// Configured ceiling on live reader connections.
    pub max_readers: usize,
    /// Readers checked out, including in-flight open attempts.
    pub readers_in_use: usize,
    /// Established readers parked for reuse.
    pub readers_idle: usize,
    /// Read checkouts currently blocked waiting for a reader to come back.
    pub reader_waiters: usize,
    /// Total read checkouts served since startup.
    pub reader_acquisitions: u64,
    /// Read checkouts that had to wait because the pool was saturated.
    pub saturated_reader_acquisitions: u64,
    /// Cumulative time callers spent waiting for a reader.
    pub total_reader_wait: Duration,
    /// Reader connections opened since startup.
    pub reader_connects: u64,
    /// Reader connections torn down instead of recycled.
    pub reader_discards: u64,
    /// Total write checkouts served since startup.
    pub write_acquisitions: u64,
    /// Write checkouts that found the writer already busy.
    pub contended_write_acquisitions: u64,
    /// Callers currently queued for the writer.
    pub write_waiters: usize,
    /// Cumulative time callers spent waiting for the writer.
    pub total_write_wait: Duration,
}

/// Cumulative pool counters. All `Relaxed`: they are diagnostics, never used to
/// make a decision another thread depends on.
#[derive(Debug, Default)]
struct PoolMetrics {
    reader_acquisitions: AtomicU64,
    saturated_reader_acquisitions: AtomicU64,
    reader_wait_nanos: AtomicU64,
    reader_connects: AtomicU64,
    reader_discards: AtomicU64,
    write_acquisitions: AtomicU64,
    contended_write_acquisitions: AtomicU64,
    write_wait_nanos: AtomicU64,
    write_waiters: AtomicUsize,
}

impl PoolMetrics {
    /// Add an elapsed duration to a nanosecond counter without overflowing.
    fn add_wait(counter: &AtomicU64, waited: Duration) {
        let nanos = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        counter.fetch_add(nanos, Ordering::Relaxed);
    }
}

/// Mutable reader-pool state guarded by a single lock.
struct ReaderPoolState {
    /// Established readers available for reuse.
    idle: Vec<Connection>,
    /// Readers handed to callers plus slots reserved for open attempts, tracked
    /// separately from `idle` so `leased + idle.len()` never exceeds
    /// `max_readers`.
    leased: usize,
    /// Callers currently parked on the condvar.
    waiters: usize,
}

impl crate::runtime::registry::PoolStatsSource for SqliteConnectionPool {
    fn pool_kind(&self) -> &'static str {
        "sqlite"
    }

    fn pool_name(&self) -> String {
        self.db_path.display().to_string()
    }

    fn pool_metrics(&self) -> serde_json::Value {
        let stats = self.stats();
        serde_json::json!({
            "max_readers": stats.max_readers,
            "readers_in_use": stats.readers_in_use,
            "readers_idle": stats.readers_idle,
            "reader_waiters": stats.reader_waiters,
            "reader_acquisitions": stats.reader_acquisitions,
            "saturated_reader_acquisitions": stats.saturated_reader_acquisitions,
            "total_reader_wait_ms": stats.total_reader_wait.as_millis(),
            "reader_connects": stats.reader_connects,
            "reader_discards": stats.reader_discards,
            "write_acquisitions": stats.write_acquisitions,
            "contended_write_acquisitions": stats.contended_write_acquisitions,
            "write_waiters": stats.write_waiters,
            "total_write_wait_ms": stats.total_write_wait.as_millis(),
        })
    }
}

/// Builder for [`SqliteConnectionPool`].
///
/// Everything the two stores disagree about is a knob here rather than a branch
/// inside the pool: connection-scoped PRAGMAs, the open timeout, and whether
/// guards scrub leftover transactions on release. The two invariants that make
/// the pool sound — WAL on the writer, `query_only` on every reader — are not
/// negotiable and are applied by [`SqlitePoolBuilder::build`] itself.
pub struct SqlitePoolBuilder {
    db_path: Arc<Path>,
    max_readers: usize,
    open_timeout_secs: Option<u64>,
    writer_pragmas: &'static str,
    reader_pragmas: &'static str,
    reset_transactions_on_release: bool,
}

impl SqlitePoolBuilder {
    /// Ceiling on concurrently checked-out readers.
    ///
    /// This bounds connections, not work: a caller that finds every reader busy
    /// queues for one instead of failing. Zero is clamped to one, because a
    /// pool that can never hand anything out would deadlock every reader.
    #[must_use]
    pub const fn max_readers(mut self, max_readers: usize) -> Self {
        self.max_readers = if max_readers == 0 { 1 } else { max_readers };
        self
    }

    /// Cap on how long *opening the database file* may take.
    ///
    /// `None` (the default) waits indefinitely. This is storage I/O, not agent
    /// work — it never limits how long a query, a tool call or a turn may run.
    #[must_use]
    pub const fn open_timeout_secs(mut self, open_timeout_secs: Option<u64>) -> Self {
        self.open_timeout_secs = open_timeout_secs;
        self
    }

    /// Extra connection-scoped PRAGMAs for the writer, applied after WAL.
    #[must_use]
    pub const fn writer_pragmas(mut self, pragmas: &'static str) -> Self {
        self.writer_pragmas = pragmas;
        self
    }

    /// Extra connection-scoped PRAGMAs for every reader.
    ///
    /// PRAGMAs are per-connection state, so a pooled reader is useless unless
    /// it is configured like the writer; whatever tuning the writer got must be
    /// repeated here. `query_only` is added by the pool afterwards and cannot
    /// be opted out of.
    #[must_use]
    pub const fn reader_pragmas(mut self, pragmas: &'static str) -> Self {
        self.reader_pragmas = pragmas;
        self
    }

    /// Roll back a transaction left open on a connection when its guard drops.
    ///
    /// `rusqlite::Transaction` already rolls itself back, so this only matters
    /// for stores that issue a bare `BEGIN` through `execute`/`execute_batch`:
    /// on the writer such a transaction would stall every later write, and on a
    /// reader it would pin an old WAL snapshot for every later user of that
    /// connection. Off by default, since the check costs a call on every
    /// release and only pays for itself where bare `BEGIN` is reachable.
    #[must_use]
    pub const fn reset_transactions_on_release(mut self, reset: bool) -> Self {
        self.reset_transactions_on_release = reset;
        self
    }

    /// Open the writer connection and return a pool with an empty reader set.
    ///
    /// Readers are opened lazily, so a workload that never reads concurrently
    /// never pays for connections it does not use.
    pub fn build(self) -> anyhow::Result<SqliteConnectionPool> {
        let writer = open_connection(&self.db_path, self.open_timeout_secs)?;

        // WAL is a prerequisite for the reader pool, not a tuning knob: without
        // it readers and the writer lock each other out and pooling readers
        // would only add `SQLITE_BUSY`. It is set on the writer because the
        // journal mode is a property of the database file, not of a connection.
        writer
            .execute_batch("PRAGMA journal_mode = WAL;")
            .context("SQLite failed to enable WAL journalling")?;
        if !self.writer_pragmas.is_empty() {
            writer
                .execute_batch(self.writer_pragmas)
                .context("SQLite failed to configure writer connection")?;
        }

        Ok(SqliteConnectionPool {
            writer: Mutex::new(writer),
            readers: Mutex::new(ReaderPoolState {
                idle: Vec::new(),
                leased: 0,
                waiters: 0,
            }),
            reader_available: Condvar::new(),
            db_path: self.db_path,
            open_timeout_secs: self.open_timeout_secs,
            reader_pragmas: self.reader_pragmas,
            max_readers: self.max_readers,
            reset_transactions_on_release: self.reset_transactions_on_release,
            metrics: PoolMetrics::default(),
        })
    }
}

/// SQLite connection management for one database file: many readers, one
/// writer. See the module documentation for why it is shaped this way.
pub struct SqliteConnectionPool {
    /// The single writer. Every mutating statement and every transaction runs
    /// here, which is what keeps SQLite's one-writer rule from turning into
    /// `SQLITE_BUSY` storms between our own connections.
    writer: Mutex<Connection>,
    /// Read connections, grown lazily up to `max_readers`.
    readers: Mutex<ReaderPoolState>,
    /// Signalled whenever a reader is returned or a reserved slot is freed.
    reader_available: Condvar,
    db_path: Arc<Path>,
    open_timeout_secs: Option<u64>,
    reader_pragmas: &'static str,
    max_readers: usize,
    reset_transactions_on_release: bool,
    metrics: PoolMetrics,
}

impl SqliteConnectionPool {
    /// Start configuring a pool for `db_path`.
    #[must_use]
    pub fn builder(db_path: impl Into<PathBuf>) -> SqlitePoolBuilder {
        SqlitePoolBuilder {
            db_path: Arc::from(db_path.into()),
            max_readers: default_read_pool_size(),
            open_timeout_secs: None,
            writer_pragmas: "",
            reader_pragmas: "",
            reset_transactions_on_release: false,
        }
    }

    /// Check out a reader, waiting indefinitely while the pool is saturated.
    pub fn read(&self) -> anyhow::Result<SqliteReadGuard<'_>> {
        let started = Instant::now();
        let mut queued = false;

        let mut readers = self.readers.lock();
        let recycled = loop {
            if let Some(conn) = readers.idle.pop() {
                readers.leased = readers.leased.saturating_add(1);
                break Some(conn);
            }
            if readers.leased < self.max_readers {
                // Reserve the slot before dropping the lock so concurrent
                // openers can never overshoot `max_readers`; opening a
                // connection touches the filesystem and must not hold the lock.
                readers.leased = readers.leased.saturating_add(1);
                break None;
            }
            queued = true;
            readers.waiters = readers.waiters.saturating_add(1);
            self.reader_available.wait(&mut readers);
            readers.waiters = readers.waiters.saturating_sub(1);
        };
        drop(readers);

        let conn = match recycled {
            Some(conn) => conn,
            None => self.open_reserved_reader()?,
        };
        self.record_read_acquisition(started, queued);
        Ok(SqliteReadGuard {
            pool: self,
            conn: Some(conn),
        })
    }

    /// Check out the writer, waiting indefinitely while another writer runs.
    ///
    /// Infallible by construction: the writer connection is opened once during
    /// startup, so acquiring it can only ever mean waiting for the mutex.
    pub fn write(&self) -> SqliteWriteGuard<'_> {
        if let Some(guard) = self.writer.try_lock() {
            self.metrics.write_acquisitions.fetch_add(1, Ordering::Relaxed);
            return SqliteWriteGuard { pool: self, guard };
        }

        // Another write is in flight. Wait it out without a deadline: SQLite
        // has exactly one writer, so queueing here is precisely what stops our
        // own connections from fighting over the database write lock, and work
        // that is merely waiting its turn must never be failed for waiting.
        let started = Instant::now();
        self.metrics.write_waiters.fetch_add(1, Ordering::Relaxed);
        let guard = self.writer.lock();
        self.metrics.write_waiters.fetch_sub(1, Ordering::Relaxed);

        let waited = started.elapsed();
        self.metrics.write_acquisitions.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .contended_write_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        PoolMetrics::add_wait(&self.metrics.write_wait_nanos, waited);
        tracing::debug!(
            waited_ms = waited.as_millis(),
            "SQLite writer busy; caller queued instead of failing"
        );
        SqliteWriteGuard { pool: self, guard }
    }

    /// Snapshot the pool counters.
    ///
    /// Nothing here throttles: the numbers exist so saturation is observable
    /// rather than enforced.
    #[must_use]
    pub fn stats(&self) -> SqlitePoolStats {
        let readers = self.readers.lock();
        let readers_in_use = readers.leased;
        let readers_idle = readers.idle.len();
        let reader_waiters = readers.waiters;
        drop(readers);

        SqlitePoolStats {
            max_readers: self.max_readers,
            readers_in_use,
            readers_idle,
            reader_waiters,
            reader_acquisitions: self.metrics.reader_acquisitions.load(Ordering::Relaxed),
            saturated_reader_acquisitions: self.metrics.saturated_reader_acquisitions.load(Ordering::Relaxed),
            total_reader_wait: Duration::from_nanos(self.metrics.reader_wait_nanos.load(Ordering::Relaxed)),
            reader_connects: self.metrics.reader_connects.load(Ordering::Relaxed),
            reader_discards: self.metrics.reader_discards.load(Ordering::Relaxed),
            write_acquisitions: self.metrics.write_acquisitions.load(Ordering::Relaxed),
            contended_write_acquisitions: self.metrics.contended_write_acquisitions.load(Ordering::Relaxed),
            write_waiters: self.metrics.write_waiters.load(Ordering::Relaxed),
            total_write_wait: Duration::from_nanos(self.metrics.write_wait_nanos.load(Ordering::Relaxed)),
        }
    }

    /// Publish this pool's counters to the runtime pool report.
    ///
    /// Called once per pool, right after it is placed in an `Arc`: the registry
    /// keeps only a `Weak`, so a pool that is dropped disappears from the report
    /// without any deregistration step.
    pub fn publish_metrics(pool: &Arc<Self>) {
        let concrete = Arc::clone(pool);
        let source: Arc<dyn crate::runtime::registry::PoolStatsSource> = concrete;
        crate::runtime::registry::register_pool(&source);
    }

    /// Open one reader connection.
    ///
    /// Opened read/write but pinned to `query_only`: the flag rejects writes at
    /// the SQLite layer just as `SQLITE_OPEN_READ_ONLY` would, while still
    /// letting the connection create the WAL shared-memory index, which a
    /// read-only handle cannot always do. It is the safety net for the
    /// read/write split at the call sites: a statement misrouted to a reader
    /// fails loudly instead of silently competing for SQLite's write lock.
    fn open_reader(&self) -> anyhow::Result<Connection> {
        let conn = open_connection(&self.db_path, self.open_timeout_secs)?;
        if !self.reader_pragmas.is_empty() {
            conn.execute_batch(self.reader_pragmas)
                .context("SQLite failed to configure reader connection")?;
        }
        conn.execute_batch("PRAGMA query_only = ON;")
            .context("SQLite failed to pin reader connection to read-only")?;
        Ok(conn)
    }

    /// Open a reader for an already-reserved slot, releasing the slot again if
    /// the open fails — otherwise a transient failure would permanently shrink
    /// the pool.
    fn open_reserved_reader(&self) -> anyhow::Result<Connection> {
        match self.open_reader() {
            Ok(conn) => {
                self.metrics.reader_connects.fetch_add(1, Ordering::Relaxed);
                Ok(conn)
            }
            Err(error) => {
                self.release_reader(None);
                Err(error)
            }
        }
    }

    fn record_read_acquisition(&self, started: Instant, queued: bool) {
        let waited = started.elapsed();
        self.metrics.reader_acquisitions.fetch_add(1, Ordering::Relaxed);
        PoolMetrics::add_wait(&self.metrics.reader_wait_nanos, waited);
        if queued {
            self.metrics
                .saturated_reader_acquisitions
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                waited_ms = waited.as_millis(),
                max_readers = self.max_readers,
                "SQLite reader pool saturated; caller queued instead of failing"
            );
        }
    }

    /// Return a reader, or release a reserved slot when `conn` is `None`.
    fn release_reader(&self, conn: Option<Connection>) {
        {
            let mut readers = self.readers.lock();
            readers.leased = readers.leased.saturating_sub(1);
            if let Some(conn) = conn {
                readers.idle.push(conn);
            }
        }
        self.reader_available.notify_one();
    }
}

/// Open one connection and give it this runtime's lock-waiting policy.
fn open_connection(db_path: &Path, open_timeout_secs: Option<u64>) -> anyhow::Result<Connection> {
    let conn = match open_timeout_secs {
        Some(secs) => open_with_deadline(db_path, secs)?,
        None => Connection::open(db_path).context("SQLite failed to open database")?,
    };
    install_busy_handler(&conn).context("SQLite failed to install busy handler")?;
    Ok(conn)
}

/// Open a connection, giving up if the file cannot be opened within `secs`.
///
/// `Connection::open` has no cancellation of its own, so the open runs on a
/// throwaway thread and the caller waits on a channel; a stuck open leaks that
/// one thread rather than wedging the caller forever.
fn open_with_deadline(db_path: &Path, secs: u64) -> anyhow::Result<Connection> {
    let capped = secs.min(OPEN_TIMEOUT_CAP_SECS);
    let owned = db_path.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = Connection::open(&owned);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(Duration::from_secs(capped)) {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(error)) => Err(error).context("SQLite failed to open database"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            anyhow::bail!("SQLite connection open timed out after {capped} seconds")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("SQLite open thread exited unexpectedly")
        }
    }
}

/// A reader checked out of a [`SqliteConnectionPool`].
///
/// Returning it happens in `Drop`, so a caller that fails or unwinds mid-query
/// can never leak a slot. The connection is held in an `Option` only so `Drop`
/// can move it back into the pool; use [`SqliteReadGuard::connection`] to reach
/// it. `Deref` is deliberately not implemented: it could not report the empty
/// case without an `unwrap`, which this codebase forbids.
pub struct SqliteReadGuard<'pool> {
    pool: &'pool SqliteConnectionPool,
    conn: Option<Connection>,
}

impl SqliteReadGuard<'_> {
    /// Borrow the checked-out reader.
    pub fn connection(&self) -> anyhow::Result<&Connection> {
        self.conn.as_ref().context("pooled SQLite reader is unavailable")
    }
}

impl Drop for SqliteReadGuard<'_> {
    fn drop(&mut self) {
        let conn = self.conn.take();
        let recycled = match conn {
            Some(conn) if !self.pool.reset_transactions_on_release || conn.is_autocommit() => Some(conn),
            // A reader that still has a transaction open would pin an old WAL
            // snapshot for every later user of this connection, so roll it back
            // before recycling and drop the connection if even that fails.
            Some(conn) => match conn.execute_batch("ROLLBACK") {
                Ok(()) => Some(conn),
                Err(error) => {
                    tracing::warn!(%error, "discarding SQLite reader with an unfinished transaction");
                    self.pool.metrics.reader_discards.fetch_add(1, Ordering::Relaxed);
                    None
                }
            },
            None => None,
        };
        self.pool.release_reader(recycled);
    }
}

/// Exclusive access to a pool's single writer.
///
/// Derefs to the connection, and to `&mut Connection` so multi-statement work
/// can open a transaction — which is the only way to make it atomic.
pub struct SqliteWriteGuard<'pool> {
    pool: &'pool SqliteConnectionPool,
    guard: MutexGuard<'pool, Connection>,
}

impl std::ops::Deref for SqliteWriteGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.guard
    }
}

impl std::ops::DerefMut for SqliteWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.guard
    }
}

impl Drop for SqliteWriteGuard<'_> {
    fn drop(&mut self) {
        // `rusqlite::Transaction` rolls itself back on drop, but a raw `BEGIN`
        // issued through `execute` would otherwise stay open on the shared
        // writer and stall every later write.
        if self.pool.reset_transactions_on_release && !self.guard.is_autocommit() {
            if let Err(error) = self.guard.execute_batch("ROLLBACK") {
                tracing::warn!(%error, "failed to roll back leftover SQLite write transaction");
            }
        }
    }
}

/// Test-only `tracing` capture, shared by every store that must prove an
/// unbounded wait stayed observable.
///
/// The busy handler's promise is "wait forever, but say so": a test that asserts
/// only the wait would let the warning rot away silently, so the assertion needs
/// the log text. It lives here rather than in each store's test module because
/// the policy it verifies is defined here.
#[cfg(test)]
pub(crate) mod log_capture {
    use std::io::Write;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use tracing_subscriber::fmt::writer::MakeWriter;

    /// Collects formatted `tracing` output so a test can assert that a log line
    /// was actually emitted rather than assuming it was.
    #[derive(Clone, Default)]
    pub(crate) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock()).into_owned()
        }
    }

    impl Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedLogs {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `f` with every `tracing` event on this thread captured.
    pub(crate) fn capturing_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let value = tracing::subscriber::with_default(subscriber, f);
        let text = logs.text();
        (value, text)
    }
}

#[cfg(test)]
mod tests {
    use super::log_capture::capturing_logs;
    use super::*;

    fn temp_pool(dir: &tempfile::TempDir, max_readers: usize) -> SqliteConnectionPool {
        let pool = SqliteConnectionPool::builder(dir.path().join("pool-test.db"))
            .max_readers(max_readers)
            .reset_transactions_on_release(true)
            .build()
            .expect("test: build pool");
        pool.write()
            .execute_batch("CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY);")
            .expect("test: create table");
        pool
    }

    #[test]
    fn derived_read_pool_size_respects_bounds() {
        let derived = default_read_pool_size();
        assert!(derived >= MIN_READ_POOL_SIZE);
        assert!(derived <= MAX_READ_POOL_SIZE);
    }

    #[test]
    fn busy_handler_warns_periodically_and_never_gives_up() {
        // Attempt 1 is far too early to be worth a log line.
        let (retry, quiet) = capturing_logs(|| wait_for_database_lock(1));
        assert!(retry, "the handler must always ask SQLite to retry");
        assert!(
            !quiet.contains("locked by another process"),
            "an early retry must not warn: {quiet}"
        );

        // The periodic warning is what makes an unbounded wait observable.
        let attempt = i32::try_from(BUSY_WARN_EVERY_ATTEMPTS).unwrap_or(i32::MAX);
        let (retry, warned) = capturing_logs(|| wait_for_database_lock(attempt));
        assert!(retry, "the handler must always ask SQLite to retry");
        assert!(
            warned.contains("WARN") && warned.contains("locked by another process"),
            "every {BUSY_WARN_EVERY_ATTEMPTS} retries must warn, got: {warned}"
        );
    }

    #[test]
    fn pooled_connections_carry_no_busy_deadline() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let pool = temp_pool(&dir, 2);

        // Installing a busy handler clears SQLite's `busy_timeout`, so a zero
        // here is the direct evidence that no deadline survives on either kind
        // of connection: a contended lock waits instead of failing.
        let writer_timeout: i64 = pool
            .write()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("test: writer busy_timeout");
        assert_eq!(writer_timeout, 0, "the writer must not carry a lock deadline");

        let reader = pool.read().expect("test: reader checkout");
        let reader_timeout: i64 = reader
            .connection()
            .expect("test: reader connection")
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("test: reader busy_timeout");
        assert_eq!(reader_timeout, 0, "readers must not carry a lock deadline either");
    }

    #[test]
    fn write_blocked_by_another_process_waits_and_warns_instead_of_failing() {
        /// How long the simulated other process keeps the database locked. Long
        /// enough for the periodic warning to fire at least once.
        const HELD_FOR: Duration = Duration::from_millis(1_400);

        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let pool = temp_pool(&dir, 2);
        let holder_path = dir.path().join("pool-test.db");

        // A second connection stands in for another process holding the
        // database write lock. It has no busy handler of its own, so it takes
        // the lock immediately.
        let (locked_tx, locked_rx) = mpsc::channel::<()>();
        let holder = thread::spawn(move || {
            let conn = Connection::open(&holder_path).expect("test: holder connection");
            conn.execute_batch("BEGIN IMMEDIATE; INSERT INTO items (id) VALUES (1);")
                .expect("test: hold the write lock");
            locked_tx.send(()).expect("test: report lock held");
            thread::sleep(HELD_FOR);
            conn.execute_batch("COMMIT").expect("test: release the write lock");
        });
        locked_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("test: the holder must take the lock");

        // The blocked write runs on *this* thread so the busy handler's
        // warnings land in this thread's captured subscriber.
        let started = Instant::now();
        let (result, logs) = capturing_logs(|| {
            pool.write()
                .execute("INSERT INTO items (id) VALUES (?1)", rusqlite::params![2])
        });
        let waited = started.elapsed();
        holder.join().expect("test: holder thread");

        assert!(
            result.is_ok(),
            "a lock held by another process must delay the write, never fail it: {:?}",
            result.err()
        );
        assert!(
            waited >= Duration::from_secs(1),
            "the write must have actually waited for the lock, waited {waited:?}"
        );
        assert!(
            logs.contains("locked by another process"),
            "waiting forever is only allowed while it is observable; no warning was emitted: {logs}"
        );

        let rows: i64 = pool
            .write()
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .expect("test: count rows");
        assert_eq!(rows, 2, "both the holder's row and the delayed write landed");
    }

    #[test]
    fn reader_pragmas_are_applied_and_writes_are_refused() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let pool = SqliteConnectionPool::builder(dir.path().join("pragma-test.db"))
            .max_readers(1)
            .reader_pragmas("PRAGMA cache_size = -4000;")
            .build()
            .expect("test: build pool");

        let reader = pool.read().expect("test: reader checkout");
        let conn = reader.connection().expect("test: reader connection");
        let cache_size: i64 = conn
            .query_row("PRAGMA cache_size", [], |row| row.get(0))
            .expect("test: reader cache_size");
        assert_eq!(cache_size, -4000, "builder pragmas must reach every reader");

        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("test: reader journal_mode");
        assert_eq!(
            journal.to_lowercase(),
            "wal",
            "the reader pool is only sound under WAL journalling"
        );

        let error = conn
            .execute_batch("CREATE TABLE nope (id INTEGER);")
            .expect_err("test: a reader must refuse to mutate the database");
        assert!(
            error.to_string().to_lowercase().contains("readonly"),
            "query_only must reject misrouted writes loudly, got: {error}"
        );
    }
}
