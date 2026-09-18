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
//! "accepted" and "durable" are the same instant. The converse is deliberately
//! *not* true: a failed write does **not** fail the mailbox operation. The
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
//!   whoever asked for it, and a fingerprint cannot be routed to. The file is
//!   created `0600` under the workspace for that reason.
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
        let conn =
            Connection::open(&path).with_context(|| format!("failed to open mailbox database {}", path.display()))?;
        // A busy handler, not a `busy_timeout`: a lock held by another process is
        // something to wait out, not an error to hand a caller. Same policy as
        // every other SQLite store in this runtime.
        crate::runtime::sqlite_pool::install_busy_handler(&conn)?;
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
     PRAGMA synchronous = NORMAL;
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

/// Keep the file to its owner: it carries plaintext assignment origins.
///
/// A failure here is reported and tolerated — a mailbox that refused to run on a
/// filesystem without Unix modes would be a worse outcome than a file with the
/// directory's permissions.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(path = %path.display(), "could not restrict mailbox database permissions: {error}");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
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
