//! Durable backing for the chat-session registry and its assignment mailbox.
//!
//! # Why a store at all
//!
//! The mailbox used to be process memory, and the module above said so: a daemon
//! restart forgot every session and every queued assignment, and a chat that had
//! been handed work discovered the loss only by being told its session no longer
//! existed. That is defensible for a cache and indefensible for a *queue* —
//! somebody asked for the work, was told "accepted", and the acceptance quietly
//! evaporated on the next upgrade.
//!
//! # Write-through, never write-behind
//!
//! Every accepted mutation is written here before the caller is answered, so
//! "accepted" and "durable" are the same instant. `synchronous = FULL` is what
//! makes that literal rather than approximate: under WAL the cheaper `NORMAL`
//! setting leaves a committed transaction unflushed, and a power cut — the very
//! incident this store was built after — can take back an acceptance the caller
//! was already told about. The cost is one fsync per accepted mutation on a
//! mailbox that writes at human pace, which is the cheaper side of that trade.
//! The converse is deliberately *not* true: a failed write does **not** fail the
//! mailbox operation. The
//! mailbox is how a message on a phone reaches a terminal, and refusing live
//! work because a disk is full would trade a recoverable durability gap for an
//! outright outage. A write that fails is logged at `error` and the in-memory
//! queue carries on — see [`super::ChatSessions::write_through`].
//!
//! # What is stored, and what is deliberately not
//!
//! * **Session tokens are stored as SHA-256 only**, exactly as they are held in
//!   memory. A stolen database file therefore cannot drain a mailbox; it can
//!   only say that a session with some hash existed.
//! * **Assignment origins are stored in the clear.** They have to be: after a
//!   restart the result of a queued assignment still has to find its way back to
//!   whoever asked for it, and a fingerprint cannot be routed to. The database
//!   file *and its `-wal` / `-shm` sidecars* are kept `0600` under the workspace
//!   for that reason: under WAL the sidecar is where an assignment body lives
//!   until the next checkpoint, so restricting the main file alone would protect
//!   nothing.
//! * **Work-registry rows are not stored.** They are process-local addresses, so
//!   hydration mints fresh ones rather than pretending an old `WorkId` still
//!   means anything.
//!
//! # Ordering
//!
//! Queue order is carried by an explicit `position` column rather than by insert
//! order, because the in-memory mailbox pushes at both ends: an immediate
//! disposition jumps the queue, and an unacknowledged delivery is reclaimed to
//! the *head*. Positions are therefore handed out from two counters — one
//! walking up for the tail, one walking down for the head — and every read is
//! `ORDER BY position`. The counters are seeded from the stored extremes when
//! the database is opened, so ids minted after a restart cannot collide with
//! ones minted before it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

use super::{AssignPrincipal, Disposition, ResultStatus};

/// How many finished assignments the database keeps.
///
/// Larger than the in-memory feed ([`super::RESULT_LOG_CAPACITY`]) on purpose:
/// the feed is a report buffer for a live consumer, while this is the record a
/// restart reads back, and a restart that landed on a smaller buffer would lose
/// results the consumer had not collected yet.
pub const STORED_RESULT_CAPACITY: u64 = 500;

/// Which of a session's three queues an assignment is sitting in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    /// Waiting to be handed out.
    Inbox,
    /// Handed out, not yet acknowledged.
    Delivered,
    /// Acknowledged; the session says it holds it.
    Accepted,
}

impl Queue {
    /// Stable lowercase tag, as stored.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Delivered => "delivered",
            Self::Accepted => "accepted",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "inbox" => Some(Self::Inbox),
            "delivered" => Some(Self::Delivered),
            "accepted" => Some(Self::Accepted),
            _ => None,
        }
    }
}

/// One registered session, as stored.
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub session_id: String,
    pub label: String,
    pub pid: Option<u32>,
    /// SHA-256 of the mailbox token. The plaintext is never written.
    pub token_hash: String,
    pub registered_at_unix_ms: u64,
    pub polls: u64,
    /// Wall-clock stamp of the last pull, or `None` if it never pulled. Wall
    /// clock rather than a monotonic reading because it has to survive the
    /// process that took it.
    pub last_poll_at_unix_ms: Option<u64>,
}

/// One queued assignment, as stored.
#[derive(Debug, Clone)]
pub struct StoredAssignment {
    pub assignment_id: String,
    pub session_id: String,
    pub task: String,
    pub disposition: Disposition,
    pub queue: Queue,
    pub deliveries: u32,
    pub created_at_unix_ms: u64,
    pub origin: AssignPrincipal,
}

/// One finished assignment, as stored.
#[derive(Debug, Clone)]
pub struct StoredResult {
    pub seq: u64,
    pub assignment_id: String,
    pub session_id: String,
    pub session_label: String,
    pub disposition: Disposition,
    pub status: ResultStatus,
    pub summary: String,
    pub completed_at_unix_ms: u64,
    pub origin: AssignPrincipal,
}

/// Everything a restart reads back, in the order it has to be replayed.
#[derive(Debug, Default)]
pub struct StoredState {
    pub sessions: Vec<StoredSession>,
    /// Every queued assignment of every stored session, ordered by `position`.
    pub assignments: Vec<StoredAssignment>,
    /// The retained result feed, oldest sequence first.
    pub results: Vec<StoredResult>,
}

/// The persistence seam of the mailbox.
///
/// A trait rather than a concrete type so the registry can be exercised against
/// a store that fails every write — the availability trade-off above is only a
/// claim until something asserts that a failing store still delivers work.
pub trait MailboxStore: Send + Sync {
    /// Read the whole mailbox back, for a process that has just started.
    fn hydrate(&self) -> Result<StoredState>;
    /// Record a newly registered session.
    fn insert_session(&self, session: &StoredSession) -> Result<()>;
    /// Forget a session and, by cascade, everything still queued for it.
    fn delete_session(&self, session_id: &str) -> Result<()>;
    /// Record the evidence of one pull.
    fn record_poll(&self, session_id: &str, polls: u64, at_unix_ms: u64) -> Result<()>;
    /// Add one assignment at the tail (`at_front == false`) or the head.
    fn enqueue(&self, assignment: &StoredAssignment, at_front: bool) -> Result<()>;
    /// Move one assignment between queues, optionally back to the head.
    fn move_to_queue(&self, assignment_id: &str, queue: Queue, deliveries: u32, to_front: bool) -> Result<()>;
    /// Drop one assignment: it was reported, killed, or cancelled.
    fn delete_assignment(&self, assignment_id: &str) -> Result<()>;
    /// Append one finished assignment to the stored feed.
    fn append_result(&self, result: &StoredResult) -> Result<()>;
}

/// The SQLite implementation, one file under the workspace.
pub struct SqliteMailboxStore {
    path: PathBuf,
    /// One connection, serialized. SQLite allows exactly one writer per
    /// database and this store is write-dominated, so a pool would only make
    /// our own connections collide on the file lock.
    conn: Mutex<Connection>,
    /// Next position handed out at the tail of a queue.
    tail: AtomicI64,
    /// Next position handed out at the head of a queue.
    head: AtomicI64,
}

impl SqliteMailboxStore {
    /// Open (and migrate) the mailbox database at `path`.
    ///
    /// # Errors
    ///
    /// When the parent directory cannot be created, the file cannot be opened,
    /// or the schema cannot be applied.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create mailbox directory {}", parent.display()))?;
        }
        // Create the file ourselves, private, *before* SQLite sees it: SQLite
        // gives the `-wal` and `-shm` sidecars the mode of the main database
        // file it finds, and those sidecars are where an un-checkpointed
        // assignment body and its plaintext origin actually live. Letting SQLite
        // create the main file means 0644 everywhere, and a later chmod of the
        // main file alone leaves the data readable by every local user.
        precreate_private(&path);
        let conn =
            Connection::open(&path).with_context(|| format!("failed to open mailbox database {}", path.display()))?;
        // A *bounded* busy handler, unlike the other stores here: the mailbox is
        // written from a request path, and its documented trade-off is that a
        // failed write costs durability for that mutation and nothing else. An
        // unbounded wait would mean a second process holding the file lock could
        // stall a chat pull indefinitely — the one outcome the trade-off was
        // chosen to avoid — and the tested error branch would never be reached.
        crate::runtime::sqlite_pool::install_bounded_busy_handler(&conn)?;
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("failed to migrate mailbox database {}", path.display()))?;
        restrict_permissions(&path);

        // Seed both counters from what is already stored, so a position minted
        // after a restart can never land on top of one minted before it.
        let (min, max) = conn
            .query_row(
                "SELECT COALESCE(MIN(position), 0), COALESCE(MAX(position), 0) FROM chat_assignments",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .context("failed to read mailbox queue positions")?;

        Ok(Self {
            path,
            conn: Mutex::new(conn),
            tail: AtomicI64::new(max.saturating_add(1)),
            head: AtomicI64::new(min.saturating_sub(1)),
        })
    }

    /// Where this store keeps its file, for logs and for the operator.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn next_tail(&self) -> i64 {
        self.tail.fetch_add(1, Ordering::SeqCst)
    }

    fn next_head(&self) -> i64 {
        self.head.fetch_sub(1, Ordering::SeqCst)
    }

    fn position(&self, at_front: bool) -> i64 {
        if at_front { self.next_head() } else { self.next_tail() }
    }
}

/// The whole schema. `IF NOT EXISTS` throughout, in the same migration style as
/// the other SQLite stores here: opening an older file adds what is missing
/// instead of demanding a migration step nobody ran.
const SCHEMA: &str = "PRAGMA journal_mode = WAL;
     PRAGMA synchronous = FULL;
     PRAGMA foreign_keys = ON;

     CREATE TABLE IF NOT EXISTS chat_sessions (
        session_id            TEXT PRIMARY KEY,
        label                 TEXT NOT NULL,
        pid                   INTEGER,
        token_hash            TEXT NOT NULL,
        registered_at_unix_ms INTEGER NOT NULL,
        polls                 INTEGER NOT NULL DEFAULT 0,
        last_poll_at_unix_ms  INTEGER
     );

     CREATE TABLE IF NOT EXISTS chat_assignments (
        assignment_id      TEXT PRIMARY KEY,
        session_id         TEXT NOT NULL REFERENCES chat_sessions(session_id) ON DELETE CASCADE,
        task               TEXT NOT NULL,
        disposition        TEXT NOT NULL,
        queue              TEXT NOT NULL,
        position           INTEGER NOT NULL,
        deliveries         INTEGER NOT NULL DEFAULT 0,
        created_at_unix_ms INTEGER NOT NULL,
        origin_kind        TEXT NOT NULL,
        origin_channel     TEXT NOT NULL,
        origin_sender      TEXT NOT NULL DEFAULT '',
        origin_chat_type   TEXT NOT NULL DEFAULT ''
     );
     CREATE INDEX IF NOT EXISTS idx_chat_assignments_queue
        ON chat_assignments(session_id, position);

     CREATE TABLE IF NOT EXISTS chat_results (
        seq                  INTEGER PRIMARY KEY,
        assignment_id        TEXT NOT NULL,
        session_id           TEXT NOT NULL,
        session_label        TEXT NOT NULL,
        disposition          TEXT NOT NULL,
        status               TEXT NOT NULL,
        summary              TEXT NOT NULL,
        completed_at_unix_ms INTEGER NOT NULL,
        origin_kind          TEXT NOT NULL,
        origin_channel       TEXT NOT NULL,
        origin_sender        TEXT NOT NULL DEFAULT '',
        origin_chat_type     TEXT NOT NULL DEFAULT ''
     );";

/// Create the database file `0600` before SQLite opens it.
///
/// Best effort: an existing file is left with the bytes it has (only its mode is
/// fixed by [`restrict_permissions`] afterwards), and a filesystem that refuses
/// the call is tolerated for the same reason a failed chmod is.
fn precreate_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Err(error) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
        {
            tracing::warn!(path = %path.display(), "could not pre-create mailbox database privately: {error}");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Keep the database *and its sidecars* to their owner: they carry plaintext
/// assignment bodies and origins.
///
/// The `-wal` and `-shm` files are not an implementation detail here — under
/// `journal_mode = WAL` everything written since the last checkpoint is in the
/// `-wal`, so a 0600 main file next to a 0644 `-wal` keeps no secret at all.
///
/// A failure here is reported and tolerated — a mailbox that refused to run on a
/// filesystem without Unix modes would be a worse outcome than a file with the
/// directory's permissions.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for target in sidecar_paths(path) {
            if !target.exists() {
                continue;
            }
            if let Err(error) = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!(path = %target.display(), "could not restrict mailbox database permissions: {error}");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// The database file and every file SQLite keeps beside it.
#[cfg(any(unix, test))]
fn sidecar_paths(path: &Path) -> [PathBuf; 3] {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    [path.to_path_buf(), PathBuf::from(wal), PathBuf::from(shm)]
}

/// Split a principal into the four columns that carry it.
const fn origin_columns(origin: &AssignPrincipal) -> (&'static str, &str, &str, &str) {
    match origin {
        AssignPrincipal::OperatorPlane => ("operator", super::OPERATOR_PLANE_CHANNEL, "", ""),
        AssignPrincipal::Correspondent {
            channel,
            sender,
            chat_type,
        } => ("correspondent", channel.as_str(), sender.as_str(), chat_type.as_str()),
    }
}

/// Rebuild a principal from those four columns.
///
/// Anything that is not literally `correspondent` reads back as the operator
/// plane, which is the variant that names nobody — a corrupted row must not be
/// able to invent a correspondent identity that a routing rule could match.
fn origin_from_columns(kind: &str, channel: String, sender: String, chat_type: String) -> AssignPrincipal {
    if kind == "correspondent" && !sender.is_empty() && !channel.is_empty() {
        AssignPrincipal::Correspondent {
            channel,
            sender,
            chat_type,
        }
    } else {
        AssignPrincipal::OperatorPlane
    }
}

/// Read a status back, including the one the wire form refuses.
///
/// [`ResultStatus::parse`] deliberately rejects `cancelled` so a chat session
/// cannot claim this runtime's own verdict. Reading our own record back is the
/// one place that verdict must survive, so it has its own reader rather than a
/// loosened parser.
fn status_from_stored(value: &str) -> Option<ResultStatus> {
    match value {
        "completed" => Some(ResultStatus::Completed),
        "failed" => Some(ResultStatus::Failed),
        "rejected" => Some(ResultStatus::Rejected),
        "cancelled" => Some(ResultStatus::Cancelled),
        _ => None,
    }
}

impl MailboxStore for SqliteMailboxStore {
    fn hydrate(&self) -> Result<StoredState> {
        let conn = self.conn.lock();
        let mut state = StoredState::default();

        {
            let mut stmt = conn.prepare(
                "SELECT session_id, label, pid, token_hash, registered_at_unix_ms, polls, last_poll_at_unix_ms
                 FROM chat_sessions ORDER BY registered_at_unix_ms, session_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(StoredSession {
                    session_id: row.get(0)?,
                    label: row.get(1)?,
                    pid: row.get::<_, Option<i64>>(2)?.and_then(|pid| u32::try_from(pid).ok()),
                    token_hash: row.get(3)?,
                    registered_at_unix_ms: row.get::<_, i64>(4)?.unsigned_abs(),
                    polls: row.get::<_, i64>(5)?.unsigned_abs(),
                    last_poll_at_unix_ms: row.get::<_, Option<i64>>(6)?.map(i64::unsigned_abs),
                })
            })?;
            for session in rows {
                state.sessions.push(session?);
            }
        }

        {
            let mut stmt = conn.prepare(
                "SELECT assignment_id, session_id, task, disposition, queue, deliveries, created_at_unix_ms,
                        origin_kind, origin_channel, origin_sender, origin_chat_type
                 FROM chat_assignments ORDER BY position, assignment_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                ))
            })?;
            for row in rows {
                let (id, session_id, task, disposition, queue, deliveries, created, kind, channel, sender, chat_type) =
                    row?;
                let (Some(disposition), Some(queue)) = (Disposition::parse(&disposition), Queue::parse(&queue)) else {
                    tracing::warn!(assignment = %id, "skipping unreadable stored assignment row");
                    continue;
                };
                state.assignments.push(StoredAssignment {
                    assignment_id: id,
                    session_id,
                    task,
                    disposition,
                    queue,
                    deliveries: u32::try_from(deliveries).unwrap_or(u32::MAX),
                    created_at_unix_ms: created.unsigned_abs(),
                    origin: origin_from_columns(&kind, channel, sender, chat_type),
                });
            }
        }

        {
            let mut stmt = conn.prepare(
                "SELECT seq, assignment_id, session_id, session_label, disposition, status, summary,
                        completed_at_unix_ms, origin_kind, origin_channel, origin_sender, origin_chat_type
                 FROM chat_results ORDER BY seq",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                ))
            })?;
            for row in rows {
                let (
                    seq,
                    assignment_id,
                    session_id,
                    session_label,
                    disposition,
                    status,
                    summary,
                    completed,
                    kind,
                    channel,
                    sender,
                    chat_type,
                ) = row?;
                let (Some(disposition), Some(status)) = (Disposition::parse(&disposition), status_from_stored(&status))
                else {
                    tracing::warn!(seq, "skipping unreadable stored result row");
                    continue;
                };
                state.results.push(StoredResult {
                    seq: seq.unsigned_abs(),
                    assignment_id,
                    session_id,
                    session_label,
                    disposition,
                    status,
                    summary,
                    completed_at_unix_ms: completed.unsigned_abs(),
                    origin: origin_from_columns(&kind, channel, sender, chat_type),
                });
            }
        }

        Ok(state)
    }

    fn insert_session(&self, session: &StoredSession) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO chat_sessions
                (session_id, label, pid, token_hash, registered_at_unix_ms, polls, last_poll_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.session_id,
                session.label,
                session.pid.map(i64::from),
                session.token_hash,
                i64::try_from(session.registered_at_unix_ms).unwrap_or(i64::MAX),
                i64::try_from(session.polls).unwrap_or(i64::MAX),
                session
                    .last_poll_at_unix_ms
                    .map(|at| i64::try_from(at).unwrap_or(i64::MAX)),
            ],
        )?;
        Ok(())
    }

    fn delete_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock();
        // The cascade is what removes the queued rows, so `foreign_keys` being
        // on is load-bearing rather than decorative.
        conn.execute("DELETE FROM chat_sessions WHERE session_id = ?1", params![session_id])?;
        Ok(())
    }

    fn record_poll(&self, session_id: &str, polls: u64, at_unix_ms: u64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE chat_sessions SET polls = ?2, last_poll_at_unix_ms = ?3 WHERE session_id = ?1",
            params![
                session_id,
                i64::try_from(polls).unwrap_or(i64::MAX),
                i64::try_from(at_unix_ms).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    fn enqueue(&self, assignment: &StoredAssignment, at_front: bool) -> Result<()> {
        let position = self.position(at_front);
        let (kind, channel, sender, chat_type) = origin_columns(&assignment.origin);
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO chat_assignments
                (assignment_id, session_id, task, disposition, queue, position, deliveries,
                 created_at_unix_ms, origin_kind, origin_channel, origin_sender, origin_chat_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                assignment.assignment_id,
                assignment.session_id,
                assignment.task,
                assignment.disposition.as_str(),
                assignment.queue.as_str(),
                position,
                i64::from(assignment.deliveries),
                i64::try_from(assignment.created_at_unix_ms).unwrap_or(i64::MAX),
                kind,
                channel,
                sender,
                chat_type,
            ],
        )?;
        Ok(())
    }

    fn move_to_queue(&self, assignment_id: &str, queue: Queue, deliveries: u32, to_front: bool) -> Result<()> {
        let conn = self.conn.lock();
        if to_front {
            let position = self.next_head();
            conn.execute(
                "UPDATE chat_assignments SET queue = ?2, deliveries = ?3, position = ?4 WHERE assignment_id = ?1",
                params![assignment_id, queue.as_str(), i64::from(deliveries), position],
            )?;
        } else {
            conn.execute(
                "UPDATE chat_assignments SET queue = ?2, deliveries = ?3 WHERE assignment_id = ?1",
                params![assignment_id, queue.as_str(), i64::from(deliveries)],
            )?;
        }
        Ok(())
    }

    fn delete_assignment(&self, assignment_id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM chat_assignments WHERE assignment_id = ?1",
            params![assignment_id],
        )?;
        Ok(())
    }

    fn append_result(&self, result: &StoredResult) -> Result<()> {
        let (kind, channel, sender, chat_type) = origin_columns(&result.origin);
        let seq = i64::try_from(result.seq).unwrap_or(i64::MAX);
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO chat_results
                (seq, assignment_id, session_id, session_label, disposition, status, summary,
                 completed_at_unix_ms, origin_kind, origin_channel, origin_sender, origin_chat_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                seq,
                result.assignment_id,
                result.session_id,
                result.session_label,
                result.disposition.as_str(),
                result.status.as_str(),
                result.summary,
                i64::try_from(result.completed_at_unix_ms).unwrap_or(i64::MAX),
                kind,
                channel,
                sender,
                chat_type,
            ],
        )?;
        // Retention is a trim, not a quota: the oldest rows go and the sequence
        // keeps climbing, so a consumer that fell behind can still tell it
        // missed results instead of silently believing it saw them all.
        let floor = conn
            .query_row("SELECT MAX(seq) FROM chat_results", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten()
            .unwrap_or(seq)
            .saturating_sub(i64::try_from(STORED_RESULT_CAPACITY).unwrap_or(i64::MAX));
        conn.execute("DELETE FROM chat_results WHERE seq <= ?1", params![floor])?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// SQLite gives a `-wal` the mode of the database file it finds, so the file
    /// has to be *born* private rather than chmod-ed a moment later: between
    /// SQLite creating it `0644` and a chmod fixing it, the mailbox is readable
    /// by every local user.
    ///
    /// MUTATION GUARD: make `precreate_private` a no-op and the mode assertion
    /// fails with the process umask's `0644`.
    #[cfg(unix)]
    #[test]
    fn a_fresh_mailbox_file_is_born_private_and_an_existing_one_keeps_its_bytes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("test: tempdir");
        let fresh = dir.path().join("fresh.db");
        precreate_private(&fresh);
        let mode = std::fs::metadata(&fresh)
            .expect("test: the file must exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "a mailbox file must never exist world-readable");

        let existing = dir.path().join("existing.db");
        std::fs::write(&existing, b"already here").expect("test: write");
        precreate_private(&existing);
        assert_eq!(
            std::fs::read(&existing).expect("test: read"),
            b"already here",
            "pre-creation must never truncate a database that is already there"
        );
    }

    /// The module header promises that "accepted" and "durable" are the same
    /// instant. Under WAL that is only true with `synchronous = FULL`: with
    /// `NORMAL` a commit is not flushed and a power cut can take back an
    /// acceptance the caller was already given.
    ///
    /// MUTATION GUARD: put `synchronous = NORMAL` back in `SCHEMA` and this goes
    /// red.
    #[test]
    fn the_mailbox_commits_are_flushed_and_journalled_in_wal_mode() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        let store = SqliteMailboxStore::open(dir.path().join("mailbox.db")).expect("test: open");
        let conn = store.conn.lock();
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("test: journal_mode");
        assert_eq!(journal.to_lowercase(), "wal");
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("test: synchronous");
        assert_eq!(synchronous, 2, "2 is FULL: an accepted mutation must be flushed");
    }

    #[test]
    fn the_sidecar_set_is_the_database_and_its_two_wal_files() {
        let paths = sidecar_paths(Path::new("/tmp/chat-mailbox.db"));
        assert_eq!(
            paths.map(|path| path.display().to_string()),
            [
                "/tmp/chat-mailbox.db".to_string(),
                "/tmp/chat-mailbox.db-wal".to_string(),
                "/tmp/chat-mailbox.db-shm".to_string(),
            ]
        );
    }
}
