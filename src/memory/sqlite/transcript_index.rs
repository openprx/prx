//! The full-text index over transcript rows (`message_events.content`): its
//! creation and its initial fill.
//!
//! The FTS5 table and its three triggers are not in `schema.rs`, so the schema
//! stamp is unchanged. They are created by the warm-up at process start, which
//! every process that registers tools runs in the background. From then on
//! every message insert pays the trigger cost. The index is external-content:
//! it stores no copy of the text, only the FTS5 structures, and it indexes
//! every row. It uses the trigram tokenizer, so Chinese and other scripts
//! without spaces match as substrings. Filters (owner, workspace, agent
//! partition, erased scopes, inactive incarnations, rf-deleted messages,
//! message role and type) are applied at query time, never in the triggers, so
//! a row's index entry always matches its `content` and the `delete` command
//! always removes what was indexed.
//!
//! # Chunked initial fill
//!
//! The rows that exist when the index is created are indexed in short write
//! transactions, so the first start after an upgrade never holds the writer for
//! longer than one batch:
//!
//! 1. One short transaction creates the FTS5 table, sets `secure-delete`,
//!    records `boundary` (the largest row id at that moment) and a cursor
//!    `next_after = 0` in [`FILL_TABLE`], and creates the three triggers. From
//!    then on new and changed rows are kept in the index by the triggers.
//! 2. Each batch transaction indexes the next rows with ids in
//!    `(next_after, boundary]` (at most [`BATCH_ROWS`] rows and about
//!    [`BATCH_CHARS`] characters) from their current content and moves the
//!    cursor past them. The cursor is persisted with the batch, so a restart
//!    resumes where the last committed batch ended.
//! 3. The transaction that finds no row left drops [`FILL_TABLE`] and replaces
//!    the triggers with their unconditional form. The index is then exactly
//!    what a single `rebuild` would have produced, and a database in this state
//!    needs no further work.
//!
//! While the fill runs, a row is in the index exactly when
//! `id <= next_after OR id > boundary`. The triggers keep that invariant: each
//! carries a `WHEN` clause that skips rows in the unfilled range
//! `(next_after, boundary]`. A row there was never indexed, so its delete
//! trigger must not issue an FTS5 `delete` (an external-content `delete` with
//! terms that were never inserted corrupts the index), its update trigger must
//! not either, and its insert trigger (only reachable by an explicit id) must
//! not index it, because the batch that reaches it indexes it from its content
//! at that moment. A row updated, blanked or deleted before its batch is
//! therefore indexed with its final content or not at all. Every trigger and
//! every batch runs under the database write lock, so the cursor a trigger reads
//! is the one the last committed batch wrote.
//!
//! Searches read the index only once it is complete (see [`index_state`]);
//! before that they match every word with `LIKE` on the rows themselves.
//!
//! Failure semantics: an error inside a trigger aborts the statement that fired
//! it, as a storage error on `message_events` itself would. A failing FTS write
//! therefore fails the message write (fail-closed); it is never swallowed.
//!
//! `INSERT OR REPLACE` on `message_events` would delete the old row without
//! firing the delete trigger and so corrupt the index: never use it there. A
//! table rebuild migration must recreate these triggers.
//!
//! `secure-delete` is FTS5's own option: a deleted or blanked row's terms are
//! overwritten on disk instead of lingering in older segments. It is stored in
//! the index's `%_config` table (format version 5), so later connections keep
//! it without setting it. Erasure therefore needs no rebuild.

use super::erasure_pool::MemoryPool;
use crate::runtime::sqlite_pool::begin_immediate;
use rusqlite::Connection;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Name of the FTS5 table over `message_events`.
pub(super) const INDEX_TABLE: &str = "message_events_fts";

/// Name of the one-row table that holds the fill cursor while the initial fill
/// runs. It exists exactly while the fill is unfinished.
const FILL_TABLE: &str = "transcript_index_fill";

/// Most rows one batch transaction indexes.
const BATCH_ROWS: i64 = 2_000;

/// Characters of content after which a batch stops taking further rows (it
/// always takes at least one), so a run of very long rows keeps the batch short.
const BATCH_CHARS: i64 = 2 * 1024 * 1024;

/// Shortest and longest pause between two batches. The pause is the last
/// batch's duration within these bounds, so the fill holds the write lock at
/// most about half of the time and other writers, including those of other
/// processes polling with a backoff, find it free.
const MIN_BATCH_PAUSE: Duration = Duration::from_millis(50);
const MAX_BATCH_PAUSE: Duration = Duration::from_millis(500);

/// Drops whatever trigger or fill table a previous, interrupted creation could
/// not have left (creation is one transaction, so normally nothing).
const CLEAR_SQL: &str = "
    DROP TRIGGER IF EXISTS message_events_fts_ai;
    DROP TRIGGER IF EXISTS message_events_fts_ad;
    DROP TRIGGER IF EXISTS message_events_fts_au;
    DROP TABLE IF EXISTS transcript_index_fill;";

const CREATE_TABLE_SQL: &str = "
    CREATE VIRTUAL TABLE message_events_fts USING fts5(
        content, content='message_events', content_rowid='id', tokenize='trigram');
    INSERT INTO message_events_fts(message_events_fts, rank) VALUES ('secure-delete', 1);";

/// The triggers of a complete index.
const TRIGGERS_SQL: &str = "
    CREATE TRIGGER message_events_fts_ai AFTER INSERT ON message_events BEGIN
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;
    CREATE TRIGGER message_events_fts_ad AFTER DELETE ON message_events BEGIN
        INSERT INTO message_events_fts(message_events_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
    END;
    CREATE TRIGGER message_events_fts_au AFTER UPDATE OF content ON message_events BEGIN
        INSERT INTO message_events_fts(message_events_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;";

/// The fill table and the triggers used while the fill runs: the same bodies,
/// skipping rows in the unfilled range (see the module doc).
const FILLING_TRIGGERS_SQL: &str = "
    CREATE TABLE transcript_index_fill (
        id         INTEGER PRIMARY KEY CHECK(id = 1),
        next_after INTEGER NOT NULL,
        boundary   INTEGER NOT NULL);
    CREATE TRIGGER message_events_fts_ai AFTER INSERT ON message_events
    WHEN NOT EXISTS (SELECT 1 FROM transcript_index_fill f
                     WHERE new.id > f.next_after AND new.id <= f.boundary) BEGIN
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;
    CREATE TRIGGER message_events_fts_ad AFTER DELETE ON message_events
    WHEN NOT EXISTS (SELECT 1 FROM transcript_index_fill f
                     WHERE old.id > f.next_after AND old.id <= f.boundary) BEGIN
        INSERT INTO message_events_fts(message_events_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
    END;
    CREATE TRIGGER message_events_fts_au AFTER UPDATE OF content ON message_events
    WHEN NOT EXISTS (SELECT 1 FROM transcript_index_fill f
                     WHERE old.id > f.next_after AND old.id <= f.boundary) BEGIN
        INSERT INTO message_events_fts(message_events_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;";

/// Whether the index exists and whether its initial fill is finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IndexState {
    Missing,
    Filling,
    Complete,
}

/// What one fill step did.
enum FillStep {
    /// Indexed this many rows; more may follow.
    Indexed(usize),
    /// Found no row left and completed the index.
    Finished,
    /// The index was already complete (another process finished it).
    AlreadyComplete,
}

/// The index's state on `conn`; a read.
pub(super) fn index_state(conn: &Connection) -> anyhow::Result<IndexState> {
    if !table_exists(conn, INDEX_TABLE)? {
        return Ok(IndexState::Missing);
    }
    if table_exists(conn, FILL_TABLE)? {
        return Ok(IndexState::Filling);
    }
    Ok(IndexState::Complete)
}

/// Create the index if the database lacks it, then fill it in batches until it
/// is complete. A no-op on a complete index. Logs one line when this call
/// completes a fill.
pub(super) async fn build_index(pool: Arc<MemoryPool>) -> anyhow::Result<()> {
    let creating = Arc::clone(&pool);
    let state = crate::runtime::blocking::spawn_blocking(move || create_index(&creating)).await??;
    if state == IndexState::Complete {
        return Ok(());
    }
    let started = Instant::now();
    let mut rows: usize = 0;
    let mut batches: u64 = 0;
    loop {
        let filling = Arc::clone(&pool);
        let batch_started = Instant::now();
        let step = crate::runtime::blocking::spawn_blocking(move || fill_step(&filling)).await??;
        match step {
            FillStep::Indexed(count) => {
                rows = rows.saturating_add(count);
                batches = batches.saturating_add(1);
            }
            FillStep::Finished => {
                tracing::info!(
                    rows,
                    batches,
                    elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "transcript index fill complete"
                );
                return Ok(());
            }
            FillStep::AlreadyComplete => return Ok(()),
        }
        let pause = batch_started.elapsed().clamp(MIN_BATCH_PAUSE, MAX_BATCH_PAUSE);
        tokio::time::sleep(pause).await;
    }
}

/// Create the index objects when the table is missing, in one short
/// transaction, and return the state after it. The check is a read; the write
/// lock is taken only when the table is missing, and the check is repeated
/// under the lock, so the creation runs once per database. An empty table is
/// complete at once.
fn create_index(pool: &MemoryPool) -> anyhow::Result<IndexState> {
    {
        let reader = pool.read()?;
        let state = index_state(reader.connection()?)?;
        if state != IndexState::Missing {
            return Ok(state);
        }
    }
    let guard = pool.write();
    let tx = begin_immediate(&guard)?;
    let state = index_state(&tx)?;
    if state != IndexState::Missing {
        tx.commit()?;
        return Ok(state);
    }
    tx.execute_batch(CLEAR_SQL)?;
    tx.execute_batch(CREATE_TABLE_SQL)?;
    let boundary: Option<i64> = tx.query_row("SELECT max(id) FROM message_events", [], |row| row.get(0))?;
    let state = if let Some(boundary) = boundary {
        tx.execute_batch(FILLING_TRIGGERS_SQL)?;
        tx.execute(
            "INSERT INTO transcript_index_fill(id, next_after, boundary) VALUES (1, 0, ?1)",
            [boundary],
        )?;
        IndexState::Filling
    } else {
        tx.execute_batch(TRIGGERS_SQL)?;
        IndexState::Complete
    };
    tx.commit()?;
    Ok(state)
}

/// One fill transaction: index the next batch of the unfilled range and move
/// the cursor past it, or complete the index when the range is empty.
fn fill_step(pool: &MemoryPool) -> anyhow::Result<FillStep> {
    let guard = pool.write();
    let tx = begin_immediate(&guard)?;
    if index_state(&tx)? != IndexState::Filling {
        tx.commit()?;
        return Ok(FillStep::AlreadyComplete);
    }
    let (next_after, boundary): (i64, i64) = tx.query_row(
        "SELECT next_after, boundary FROM transcript_index_fill WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let step = if let Some(last) = batch_end(&tx, next_after, boundary)? {
        let count = tx.execute(
            "INSERT INTO message_events_fts(rowid, content)
             SELECT id, content FROM message_events WHERE id > ?1 AND id <= ?2",
            [next_after, last],
        )?;
        tx.execute("UPDATE transcript_index_fill SET next_after = ?1 WHERE id = 1", [last])?;
        FillStep::Indexed(count)
    } else {
        tx.execute_batch(CLEAR_SQL)?;
        tx.execute_batch(TRIGGERS_SQL)?;
        FillStep::Finished
    };
    tx.commit()?;
    Ok(step)
}

/// The id of the last row of the next batch in `(next_after, boundary]`, or
/// `None` when the range holds no row.
fn batch_end(conn: &Connection, next_after: i64, boundary: i64) -> anyhow::Result<Option<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id, length(content) FROM message_events
         WHERE id > ?1 AND id <= ?2 ORDER BY id LIMIT ?3",
    )?;
    let mut rows = stmt.query([next_after, boundary, BATCH_ROWS])?;
    let mut last = None;
    let mut chars: i64 = 0;
    while let Some(row) = rows.next()? {
        last = Some(row.get::<_, i64>(0)?);
        chars = chars.saturating_add(row.get::<_, Option<i64>>(1)?.unwrap_or(0));
        if chars >= BATCH_CHARS {
            break;
        }
    }
    Ok(last)
}

fn table_exists(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}
