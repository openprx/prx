//! The full-text index over transcript rows (`message_events.content`): its
//! creation and its initial fill.
//!
//! The FTS5 table, its four triggers and the purge queue are not in `schema.rs`, so the schema
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
//!    `next_after = 0` in [`FILL_TABLE`], and creates the four triggers and the purge queue. From
//!    then on new and changed rows are kept in the index by the triggers.
//! 2. Each batch transaction indexes the next rows with ids in
//!    `(next_after, boundary]` (at most 500 rows, [`BATCH_ROWS`], and about
//!    256 KiB of content, [`BATCH_CHARS`], whichever comes first) from their
//!    current content and moves the
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
//! # Purge queue
//!
//! Removing a row's terms from the index (FTS5 `secure-delete`) costs tens of
//! milliseconds per row and is paid when the write transaction commits. The
//! transaction that deletes or blanks rows therefore does not pay it: its
//! triggers record the indexed text of each row in [`PURGE_TABLE`], and short
//! follow-up transactions remove the entries (see the drain in this module).
//! The rules the triggers keep:
//!
//! - A queued row's terms in the index are exactly its queue `content`. `ON
//!   CONFLICT DO NOTHING` keeps the first, indexed text when a blanked row is
//!   later deleted.
//! - A non-empty rewrite of a queued row removes the queued text from the index
//!   synchronously, drops the queue entry and indexes the new text, so an id
//!   never has two live entries (that would corrupt an external-content
//!   index).
//! - `UPDATE ... SET content = ''` on an already empty row queues nothing.
//! - Each of the three triggers that touch the queue also carries the unfilled
//!   range guard while the fill runs: a row in that range was never indexed,
//!   and queueing it would later issue an FTS5 `delete` of terms never inserted
//!   and corrupt the index.
//! - A row that survives a blank keeps an (empty) index document, as the
//!   earlier blank trigger left it: FTS5 `delete` also removes the row's
//!   document-size entry, and an external-content row without one fails the
//!   integrity check. The drain therefore indexes the row's current (empty)
//!   content again after the `delete`; for a deleted row that inserts nothing.
//! - FTS auxiliary functions that read row content (`highlight`, `snippet`)
//!   must not be used on this index: they fail on a row that is deleted but
//!   still queued. Search reads content through the join with `message_events`.
//! - The triggers are four: insert, delete, blank, and non-empty rewrite. A
//!   table rebuild migration must recreate all of them.
//!
//! `INSERT OR REPLACE` on `message_events` would delete the old row without
//! firing the delete trigger and so corrupt the index: never use it there.
//!
//! `secure-delete` is FTS5's own option: a deleted or blanked row's terms are
//! overwritten on disk instead of lingering in older segments. It is stored in
//! the index's `%_config` table (format version 5), so later connections keep
//! it without setting it. Erasure therefore needs no rebuild.
//!
//! The stored definition names the tokenizer (`tokenize='trigram'`). A complete
//! index whose definition names another one is treated as missing: it is
//! dropped and rebuilt through the chunked path, with one warn line.

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

/// The tokenizer clause of this build's index definition (`CREATE_TABLE_SQL`).
/// A complete index whose stored definition lacks it was built by another
/// tokenizer and is rebuilt.
const TOKENIZER_CLAUSE: &str = "tokenize='trigram'";

/// Most rows one batch transaction indexes.
const BATCH_ROWS: i64 = 500;

/// Characters of content after which a batch stops taking further rows (it
/// always takes at least one), so a run of very long rows keeps the batch short.
const BATCH_CHARS: i64 = 256 * 1024;

/// Shortest and longest pause between two batches. The pause is the last
/// batch's duration within these bounds, so the fill holds the write lock at
/// most about half of the time and other writers, including those of other
/// processes polling with a backoff, find it free.
const MIN_BATCH_PAUSE: Duration = Duration::from_millis(50);
const MAX_BATCH_PAUSE: Duration = Duration::from_millis(500);

/// Name of the purge queue: the ids of deleted or blanked rows whose terms are
/// still in the index, with the exact text that was indexed for each.
pub(super) const PURGE_TABLE: &str = "message_events_fts_purge";

/// The purge queue (see the module doc, "Purge queue").
const PURGE_TABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS message_events_fts_purge (
        row_id  INTEGER PRIMARY KEY,
        content TEXT NOT NULL
    ) STRICT;";

/// Drops the four triggers of either form and nothing else.
const DROP_TRIGGERS_SQL: &str = "
    DROP TRIGGER IF EXISTS message_events_fts_ai;
    DROP TRIGGER IF EXISTS message_events_fts_ad;
    DROP TRIGGER IF EXISTS message_events_fts_au_blank;
    DROP TRIGGER IF EXISTS message_events_fts_au;";

/// Drops whatever trigger or fill table a previous, interrupted creation could
/// not have left (creation is one transaction, so normally nothing). The purge
/// queue is not dropped: its rows are index entries still to remove.
const CLEAR_SQL: &str = "
    DROP TRIGGER IF EXISTS message_events_fts_ai;
    DROP TRIGGER IF EXISTS message_events_fts_ad;
    DROP TRIGGER IF EXISTS message_events_fts_au_blank;
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
        INSERT INTO message_events_fts_purge(row_id, content) VALUES (old.id, old.content)
            ON CONFLICT(row_id) DO NOTHING;
    END;
    CREATE TRIGGER message_events_fts_au_blank AFTER UPDATE OF content ON message_events
    WHEN new.content = '' AND old.content <> '' BEGIN
        INSERT INTO message_events_fts_purge(row_id, content) VALUES (old.id, old.content)
            ON CONFLICT(row_id) DO NOTHING;
    END;
    CREATE TRIGGER message_events_fts_au AFTER UPDATE OF content ON message_events
    WHEN new.content <> '' BEGIN
        INSERT INTO message_events_fts(message_events_fts, rowid, content)
            VALUES ('delete', old.id, COALESCE(
                (SELECT p.content FROM message_events_fts_purge p WHERE p.row_id = old.id), old.content));
        DELETE FROM message_events_fts_purge WHERE row_id = old.id;
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;";

/// The fill cursor table.
const FILL_TABLE_SQL: &str = "
    CREATE TABLE transcript_index_fill (
        id         INTEGER PRIMARY KEY CHECK(id = 1),
        next_after INTEGER NOT NULL,
        boundary   INTEGER NOT NULL);";

/// The triggers used while the fill runs: the same bodies, skipping rows in the
/// unfilled range (see the module doc). The three that write the queue carry
/// the guard combined with their own condition.
const FILLING_TRIGGERS_SQL: &str = "
    CREATE TRIGGER message_events_fts_ai AFTER INSERT ON message_events
    WHEN NOT EXISTS (SELECT 1 FROM transcript_index_fill f
                     WHERE new.id > f.next_after AND new.id <= f.boundary) BEGIN
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;
    CREATE TRIGGER message_events_fts_ad AFTER DELETE ON message_events
    WHEN NOT EXISTS (SELECT 1 FROM transcript_index_fill f
                     WHERE old.id > f.next_after AND old.id <= f.boundary) BEGIN
        INSERT INTO message_events_fts_purge(row_id, content) VALUES (old.id, old.content)
            ON CONFLICT(row_id) DO NOTHING;
    END;
    CREATE TRIGGER message_events_fts_au_blank AFTER UPDATE OF content ON message_events
    WHEN new.content = '' AND old.content <> ''
         AND NOT EXISTS (SELECT 1 FROM transcript_index_fill f
                         WHERE old.id > f.next_after AND old.id <= f.boundary) BEGIN
        INSERT INTO message_events_fts_purge(row_id, content) VALUES (old.id, old.content)
            ON CONFLICT(row_id) DO NOTHING;
    END;
    CREATE TRIGGER message_events_fts_au AFTER UPDATE OF content ON message_events
    WHEN new.content <> ''
         AND NOT EXISTS (SELECT 1 FROM transcript_index_fill f
                         WHERE old.id > f.next_after AND old.id <= f.boundary) BEGIN
        INSERT INTO message_events_fts(message_events_fts, rowid, content)
            VALUES ('delete', old.id, COALESCE(
                (SELECT p.content FROM message_events_fts_purge p WHERE p.row_id = old.id), old.content));
        DELETE FROM message_events_fts_purge WHERE row_id = old.id;
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;";

/// Most queue rows one drain transaction removes, and the durations that steer
/// it: a chunk faster than [`DRAIN_FAST`] doubles the next chunk, one slower
/// than [`DRAIN_SLOW`] halves it.
const DRAIN_MAX_ROWS: usize = 64;
const DRAIN_FAST: Duration = Duration::from_millis(25);
const DRAIN_SLOW: Duration = Duration::from_millis(100);

/// Shortest and longest pause between two drain chunks (the last chunk's
/// duration within these bounds, the rule of the fill).
const MIN_DRAIN_PAUSE: Duration = Duration::from_millis(10);
const MAX_DRAIN_PAUSE: Duration = Duration::from_millis(250);

/// The statements of one drain chunk, per queue row: remove the queued text
/// from the index, index the row again if it still exists (a blanked row keeps
/// an empty document; a deleted row inserts nothing), then drop the queue row.
const PURGE_READ_SQL: &str = "SELECT row_id, content FROM message_events_fts_purge ORDER BY row_id LIMIT ?1";
const PURGE_DELETE_SQL: &str =
    "INSERT INTO message_events_fts(message_events_fts, rowid, content) VALUES ('delete', ?1, ?2)";
const PURGE_REINDEX_SQL: &str =
    "INSERT INTO message_events_fts(rowid, content) SELECT id, content FROM message_events WHERE id = ?1";
const PURGE_POP_SQL: &str = "DELETE FROM message_events_fts_purge WHERE row_id = ?1";

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
    if !has_expected_tokenizer(conn)? {
        return Ok(IndexState::Missing);
    }
    Ok(IndexState::Complete)
}

/// Whether the stored definition of the index names this build's tokenizer.
fn has_expected_tokenizer(conn: &Connection) -> anyhow::Result<bool> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
        [INDEX_TABLE],
        |row| row.get(0),
    )?;
    Ok(sql.contains(TOKENIZER_CLAUSE))
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
/// lock is taken only when the table is missing, the purge queue is missing, or
/// the triggers are not the set this build creates, and the check is repeated
/// under the lock, so each of them runs once per database. An empty table is
/// complete at once.
///
/// An index built by an earlier build has no purge queue and carries the
/// triggers that delete from the index themselves. It is upgraded here, in one
/// short transaction: the queue is created and the triggers are replaced by
/// the form of its state (complete or filling). The absence of the queue is
/// the marker; the schema stamp does not cover the index objects.
///
/// With the queue present, the `message_events_fts_*` triggers must be exactly
/// the set this build creates for the state (same names, same text). An older
/// binary run on an upgraded store, or a table rebuild that dropped the
/// triggers, leaves another set, and the index can no longer be trusted: the
/// store is then handled as if the index were missing (dropped and refilled in
/// batches). The check is one read of `sqlite_schema`.
fn create_index(pool: &MemoryPool) -> anyhow::Result<IndexState> {
    {
        let reader = pool.read()?;
        let conn = reader.connection()?;
        let state = index_state(conn)?;
        if state != IndexState::Missing && table_exists(conn, PURGE_TABLE)? && trigger_set_matches(conn, state)? {
            return Ok(state);
        }
    }
    let guard = pool.write();
    let tx = begin_immediate(&guard)?;
    let state = index_state(&tx)?;
    let mut repairing = false;
    if state != IndexState::Missing {
        if !table_exists(&tx, PURGE_TABLE)? {
            tx.execute_batch(PURGE_TABLE_SQL)?;
            tx.execute_batch(DROP_TRIGGERS_SQL)?;
            tx.execute_batch(if state == IndexState::Filling {
                FILLING_TRIGGERS_SQL
            } else {
                TRIGGERS_SQL
            })?;
            tx.commit()?;
            return Ok(state);
        }
        if trigger_set_matches(&tx, state)? {
            tx.commit()?;
            return Ok(state);
        }
        let found: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name GLOB 'message_events_fts_*'",
            [],
            |row| row.get(0),
        )?;
        tracing::warn!(
            found,
            "transcript index triggers are not the expected set; rebuilding the index"
        );
        repairing = true;
    }
    tx.execute_batch(CLEAR_SQL)?;
    // The queue refers to the index being dropped, if any.
    tx.execute_batch(PURGE_TABLE_SQL)?;
    tx.execute_batch(&format!("DELETE FROM {PURGE_TABLE};"))?;
    if table_exists(&tx, INDEX_TABLE)? {
        if !repairing {
            tracing::warn!("transcript index was built with another tokenizer; rebuilding it");
        }
        tx.execute_batch(&format!("DROP TABLE {INDEX_TABLE};"))?;
    }
    tx.execute_batch(CREATE_TABLE_SQL)?;
    let boundary: Option<i64> = tx.query_row("SELECT max(id) FROM message_events", [], |row| row.get(0))?;
    let state = if let Some(boundary) = boundary {
        tx.execute_batch(FILL_TABLE_SQL)?;
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

/// Whitespace collapsed, so stored trigger text compares with the constants.
fn normalise_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The triggers a script of `CREATE TRIGGER` statements creates, as
/// (name, normalised text without the trailing semicolon), sorted by name.
fn expected_triggers(script: &str) -> Vec<(String, String)> {
    let mut triggers: Vec<(String, String)> = script
        .split("CREATE TRIGGER ")
        .skip(1)
        .map(|statement| {
            let text = normalise_sql(&format!("CREATE TRIGGER {statement}"));
            let text = text.trim_end_matches(';').trim_end().to_owned();
            let name = statement.split_whitespace().next().unwrap_or_default().to_owned();
            (name, text)
        })
        .collect();
    triggers.sort();
    triggers
}

/// Whether the `message_events_fts_*` triggers of `conn` are exactly the set
/// this build creates for `state`: the same names and the same text. A read.
fn trigger_set_matches(conn: &Connection, state: IndexState) -> anyhow::Result<bool> {
    let mut stmt =
        conn.prepare("SELECT name, sql FROM sqlite_schema WHERE type='trigger' AND name GLOB 'message_events_fts_*'")?;
    let mut actual = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(name, sql)| (name, normalise_sql(&sql)))
        .collect::<Vec<_>>();
    actual.sort();
    let expected = expected_triggers(if state == IndexState::Filling {
        FILLING_TRIGGERS_SQL
    } else {
        TRIGGERS_SQL
    });
    Ok(actual == expected)
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

/// Whether the purge queue exists and holds a row; a read. A database whose
/// index was never created has no queue and nothing pending.
pub(super) fn purge_pending(conn: &Connection) -> anyhow::Result<bool> {
    if !table_exists(conn, PURGE_TABLE)? {
        return Ok(false);
    }
    let pending: bool = conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {PURGE_TABLE})"), [], |row| {
        row.get(0)
    })?;
    Ok(pending)
}

/// Remove queued index entries in short write transactions until the queue is
/// empty. Returns the number of rows removed.
///
/// Each transaction takes up to `k` queue rows (`k` starts at one, doubles to
/// at most [`DRAIN_MAX_ROWS`] while a chunk takes under [`DRAIN_FAST`], and
/// halves, not below one, when it takes over [`DRAIN_SLOW`]; the cost is in the
/// commit, so the duration includes it). The writer is released and the thread
/// sleeps for the chunk's duration within [`MIN_DRAIN_PAUSE`] and
/// [`MAX_DRAIN_PAUSE`] between chunks, so other writers find the lock free at
/// least half of the time. A chunk that leaves the queue empty returns at once.
/// There is no deadline and no row cap: a drain always finishes. The queue rows
/// are removed with `secure_delete` on, so their pages are overwritten.
///
/// Blocking: run it on a blocking thread. It logs counts and durations only.
pub(super) fn drain_fts_purge(pool: &MemoryPool) -> anyhow::Result<u64> {
    let started = Instant::now();
    let mut removed: u64 = 0;
    let mut chunks: u64 = 0;
    let mut k: usize = 1;
    loop {
        let chunk_started = Instant::now();
        let (count, empty) = {
            let guard = pool.write();
            if !purge_pending(&guard)? {
                break;
            }
            let tx = begin_immediate(&guard)?;
            let rows: Vec<(i64, String)> = {
                let mut stmt = tx.prepare_cached(PURGE_READ_SQL)?;
                let limit = i64::try_from(k).unwrap_or(1);
                stmt.query_map([limit], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (row_id, text) in &rows {
                tx.execute(PURGE_DELETE_SQL, rusqlite::params![row_id, text])?;
                tx.execute(PURGE_REINDEX_SQL, [row_id])?;
                tx.execute(PURGE_POP_SQL, [row_id])?;
            }
            let empty = !tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {PURGE_TABLE})"), [], |row| {
                row.get::<_, bool>(0)
            })?;
            tx.commit()?;
            (rows.len(), empty)
        };
        let duration = chunk_started.elapsed();
        removed = removed.saturating_add(u64::try_from(count).unwrap_or(0));
        chunks = chunks.saturating_add(1);
        if empty || count == 0 {
            break;
        }
        if duration < DRAIN_FAST {
            k = k.saturating_mul(2).min(DRAIN_MAX_ROWS);
        } else if duration > DRAIN_SLOW {
            k = (k / 2).max(1);
        }
        std::thread::sleep(duration.clamp(MIN_DRAIN_PAUSE, MAX_DRAIN_PAUSE));
    }
    if removed > 0 {
        tracing::info!(
            rows = removed,
            chunks,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "transcript index purge queue drained"
        );
    }
    Ok(removed)
}

/// Drain after a writer outside the erasure paths committed its row deletes:
/// no WAL truncation, as before the queue existed. A failure is logged and does
/// not fail the operation, whose delete is committed; the next scrub retries.
pub(super) fn drain_after_commit(pool: &MemoryPool) {
    if let Err(error) = drain_fts_purge(pool) {
        tracing::warn!(
            error = %crate::agent::diagnostics::error_class(&error),
            "transcript index purge after a delete failed; the next scrub retries"
        );
    }
}

fn table_exists(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}
