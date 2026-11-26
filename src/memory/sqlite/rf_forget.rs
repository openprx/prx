//! Forget the writes of a deleted run family (`message_forget_writes_v1`).
//!
//! The family of a deleted message's run R is every `memory_write_log` row
//! with `root_run_id = R` (a message deleted through the rf routes always
//! belongs to a top-level run); when R is not a root in the log, the rows
//! with `run_id = R`. Only rows of the answering agent's partition count;
//! unstamped rows belong to Main. Every target of the family (one memory or
//! document, deduplicated) is deleted with its derived copies: embedding-cache
//! entries, trash copies, links, drafts, `memory_events` rows and the topic a
//! memory's text created once nothing else references it. A target
//! already gone is not counted. The family's log rows stay (content-free) and
//! get `forgotten_at`; the attempt row records the request and the counts, so
//! a replay answers the first execution's counts and a late write of the run
//! is refused ([`is_forgotten`]).
//!
//! A plain delete also removes the OS-paging context pages that hold a
//! deleted row ([`erase_context_pages`]); they are not counted.

use super::SqliteMemory;
use crate::memory::branch_copy::SESSION_PREFIX_EVENT_TYPE;
use crate::memory::rf_messages::{RfForgetCounts, RfForgetRefused};
use crate::memory::topic::delete_orphan_topic;
use crate::memory::write_provenance::WriteProvenance;
use rusqlite::{Connection, OptionalExtension, Row, params};

/// The most write-log rows one forget reads.
const FAMILY_MAX_ROWS: i64 = 20_000;

/// The source kind of OS-paging context pages (`agent/context_paging.rs`).
const CONTEXT_PAGE_KIND: &str = "context_page";

/// The longest text probe a context-page match uses, in characters.
const PAGE_PROBE_CHARS: usize = 64;

/// The predicate "the log row `l` belongs to the agent": `?2` is the agent's
/// scope, `?3` whether it is Main. A row stamped with another scope of the
/// same agent incarnation (another session's Main scope) is the agent's too.
macro_rules! log_in_partition {
    () => {
        "(l.agent_partition IS ?2 OR (?3 AND l.agent_partition IS NULL)
          OR EXISTS(SELECT 1 FROM agent_scopes o JOIN agent_scopes n
                    ON o.workspace_id = n.workspace_id AND o.owner_id = n.owner_id
                   AND o.agent_key = n.agent_key AND o.incarnation = n.incarnation
                    WHERE o.scope_id = l.agent_partition AND n.scope_id = ?2))"
    };
}

/// The family rows of `?1`: rooted at it, or written by it when it is not a
/// root in the log.
macro_rules! family_rows {
    () => {
        concat!(
            "FROM memory_write_log l WHERE (l.root_run_id = ?1 OR l.run_id = ?1) AND ",
            log_in_partition!()
        )
    };
}

/// Idempotent DDL (migration 29), run after the attempt table and the write
/// log exist.
pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    for (table, column, definition) in [
        (
            "rf_message_attempts",
            "forget_requested",
            "INTEGER NOT NULL DEFAULT 0 CHECK (forget_requested IN (0, 1))",
        ),
        ("rf_message_attempts", "forgot_memories_created", "INTEGER"),
        ("rf_message_attempts", "forgot_memories_touched", "INTEGER"),
        ("rf_message_attempts", "forgot_documents_created", "INTEGER"),
        ("rf_message_attempts", "forgot_documents_touched", "INTEGER"),
        ("memory_write_log", "forgotten_at", "TEXT"),
    ] {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            params![table, column],
            |row| row.get(0),
        )?;
        if !present {
            // Literals of this module only; no request data reaches the DDL.
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS rf_message_attempts_forgotten_run
             ON rf_message_attempts(run_id) WHERE forget_requested = 1;",
    )?;
    Ok(())
}

/// The forget columns of one attempt row: whether a forget was asked and
/// the counts recorded when it ran.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ForgetState {
    pub requested: bool,
    pub recorded: Option<RfForgetCounts>,
}

/// Read the five forget columns (`forget_requested`, then the memory and
/// document `created`/`touched` counts) starting at column `first` of `row`.
pub(super) fn state_at(row: &Row<'_>, first: usize) -> rusqlite::Result<ForgetState> {
    let column = |offset: usize| first.saturating_add(offset);
    let requested: bool = row.get(first)?;
    let memories_created: Option<i64> = row.get(column(1))?;
    let memories_touched: Option<i64> = row.get(column(2))?;
    let documents_created: Option<i64> = row.get(column(3))?;
    let documents_touched: Option<i64> = row.get(column(4))?;
    let recorded = match (memories_created, memories_touched, documents_created, documents_touched) {
        (Some(memories_created), Some(memories_touched), Some(documents_created), Some(documents_touched)) => {
            Some(RfForgetCounts {
                memories_created,
                memories_touched,
                documents_created,
                documents_touched,
            })
        }
        _ => None,
    };
    Ok(ForgetState { requested, recorded })
}

/// The partition a forget acts in: the agent's scope stamp and whether the
/// agent is Main (whose unstamped rows are its own).
#[derive(Debug, Clone, Copy)]
pub(super) struct Partition<'a> {
    pub scope: Option<&'a str>,
    pub main: bool,
}

/// Mark the attempt's forget as asked, for a live run whose forget waits for
/// settle. Late writes of the run are refused from now on.
pub(super) fn request(tx: &Connection, workspace_id: &str, input_event_id: &str) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE rf_message_attempts SET forget_requested = 1 WHERE workspace_id = ?1 AND message_event_id = ?2",
        params![workspace_id, input_event_id],
    )?;
    Ok(())
}

/// Record the counts of the attempt's executed forget.
pub(super) fn record(
    tx: &Connection,
    workspace_id: &str,
    input_event_id: &str,
    counts: RfForgetCounts,
) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE rf_message_attempts SET forget_requested = 1, forgot_memories_created = ?3,
             forgot_memories_touched = ?4, forgot_documents_created = ?5, forgot_documents_touched = ?6
         WHERE workspace_id = ?1 AND message_event_id = ?2",
        params![
            workspace_id,
            input_event_id,
            counts.memories_created,
            counts.memories_touched,
            counts.documents_created,
            counts.documents_touched
        ],
    )?;
    Ok(())
}

/// Whether the writes of a run can be attributed: `false` when its first
/// event is older than the instant this store started recording write
/// provenance (`applied_at` of migration 28). A `context` message has no run
/// and wrote nothing.
pub(super) fn writes_traceable(
    tx: &Connection,
    workspace_id: &str,
    input_event_id: &str,
    run_id: Option<&str>,
) -> anyhow::Result<bool> {
    let Some(run_id) = run_id else {
        return Ok(true);
    };
    let traceable: Option<bool> = tx.query_row(
        "SELECT (SELECT MIN(julianday(created_at)) FROM message_events
                 WHERE workspace_id = ?1 AND (event_id = ?2 OR run_id = ?3))
                >= (SELECT julianday(applied_at) FROM memory_schema_migrations WHERE version = 28)",
        params![workspace_id, input_event_id, run_id],
        |row| row.get(0),
    )?;
    Ok(traceable.unwrap_or(false))
}

/// Whether the run family of `provenance` was forgotten: a delete asked to
/// forget its root or its run, or its run's log rows are marked forgotten.
pub(super) fn is_forgotten(conn: &Connection, provenance: &WriteProvenance) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM rf_message_attempts WHERE forget_requested = 1 AND run_id IN (?1, ?2))
             OR EXISTS(SELECT 1 FROM memory_write_log WHERE run_id = ?2 AND forgotten_at IS NOT NULL)",
        params![provenance.root_run_id(), provenance.run_id()],
        |row| row.get(0),
    )?)
}

/// Delete every target the family of `run_id` wrote in `partition`, inside
/// the caller's delete transaction, and mark the family's log rows
/// forgotten. More than [`FAMILY_MAX_ROWS`] log rows refuse with
/// [`RfForgetRefused::FamilyTooLarge`] before anything is deleted.
pub(super) fn forget_family(
    tx: &Connection,
    run_id: &str,
    partition: Partition<'_>,
    now: &str,
) -> anyhow::Result<RfForgetCounts> {
    let rows: i64 = tx.query_row(
        concat!("SELECT count(*) FROM (SELECT 1 ", family_rows!(), " LIMIT ?4)"),
        params![
            run_id,
            partition.scope,
            partition.main,
            FAMILY_MAX_ROWS.saturating_add(1)
        ],
        |row| row.get(0),
    )?;
    if rows > FAMILY_MAX_ROWS {
        return Err(RfForgetRefused::FamilyTooLarge.into());
    }
    let targets = {
        let mut statement = tx.prepare(concat!(
            "SELECT l.target_kind, l.target_id, MAX(l.op = 'insert') ",
            family_rows!(),
            " GROUP BY l.target_kind, l.target_id ORDER BY MIN(l.id)"
        ))?;
        statement
            .query_map(params![run_id, partition.scope, partition.main], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut counts = RfForgetCounts::default();
    for (kind, target_id, created) in &targets {
        match kind.as_str() {
            "memory" => {
                if delete_memory(tx, target_id)? {
                    let count = if *created {
                        &mut counts.memories_created
                    } else {
                        &mut counts.memories_touched
                    };
                    *count = count.saturating_add(1);
                }
            }
            "document" => {
                // Context pages are transcripts of older turns, not the run's
                // own content; a page holding a deleted row goes with the
                // plain delete ([`erase_context_pages`]).
                if delete_document(tx, target_id, true)? {
                    let count = if *created {
                        &mut counts.documents_created
                    } else {
                        &mut counts.documents_touched
                    };
                    *count = count.saturating_add(1);
                }
            }
            _ => anyhow::bail!("a write-log row names an unknown target kind"),
        }
    }
    rebuild_indexes(
        tx,
        counts.memories_created.saturating_add(counts.memories_touched) > 0,
        counts.documents_created.saturating_add(counts.documents_touched) > 0,
    )?;
    tx.execute(
        concat!(
            "UPDATE memory_write_log SET forgotten_at = COALESCE(forgotten_at, ?4)
             WHERE id IN (SELECT l.id ",
            family_rows!(),
            ")"
        ),
        params![run_id, partition.scope, partition.main, now],
    )?;
    Ok(counts)
}

/// FTS5 keeps a deleted row's terms in its segments until a merge; a rebuild
/// drops them as well as their tombstones.
pub(super) fn rebuild_indexes(tx: &Connection, memories: bool, documents: bool) -> anyhow::Result<()> {
    if memories {
        tx.execute("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')", [])?;
    }
    if documents {
        tx.execute(
            "INSERT INTO document_chunks_fts(document_chunks_fts) VALUES('rebuild')",
            [],
        )?;
    }
    Ok(())
}

/// Evict the embedding-cache entries of `text`, unscoped and under `scope`.
fn evict_embeddings(tx: &Connection, scope: Option<&str>, text: &str) -> anyhow::Result<()> {
    for hash in [
        SqliteMemory::scoped_content_hash(None, text),
        SqliteMemory::scoped_content_hash(scope, text),
    ] {
        tx.execute("DELETE FROM embedding_cache WHERE content_hash = ?1", params![hash])?;
    }
    Ok(())
}

/// Delete the session prefix rows (`context.prefix.recorded`) of every
/// (workspace, owner) pair that the memory rows selected by `owners_sql` belong
/// to, in every partition. The prefix row keeps the memory index text of a
/// session, so a forgotten memory must not survive in it; the next turn of such
/// a session renders a fresh index. `owners_sql` selects
/// `workspace_id, owner_id` and is a constant statement with bound values.
/// Returns the number of rows deleted.
pub(super) fn drop_prefix_rows_of(
    tx: &Connection,
    owners_sql: &str,
    values: impl rusqlite::Params,
) -> anyhow::Result<usize> {
    let owners: Vec<(Option<String>, Option<String>)> = {
        let mut statement = tx.prepare(owners_sql)?;
        statement
            .query_map(values, |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut deleted: usize = 0;
    for (workspace, owner) in owners {
        deleted = deleted.saturating_add(tx.execute(
            "DELETE FROM message_events WHERE event_type = ?3 AND workspace_id IS ?1 AND owner_id IS ?2",
            params![workspace, owner, SESSION_PREFIX_EVENT_TYPE],
        )?);
    }
    Ok(deleted)
}

/// Delete one memory by its stable id with its derived copies; `false` when
/// it is already gone.
pub(super) fn delete_memory(tx: &Connection, memory_id: &str) -> anyhow::Result<bool> {
    let row: Option<(String, String, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT key, content, ma_scope_id, topic_id FROM memories WHERE id = ?1",
            params![memory_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((key, content, scope, topic_id)) = row else {
        return Ok(false);
    };
    drop_prefix_rows_of(
        tx,
        "SELECT DISTINCT workspace_id, owner_id FROM memories WHERE id = ?1",
        params![memory_id],
    )?;
    evict_embeddings(tx, scope.as_deref(), &content)?;
    for statement in [
        "DELETE FROM memory_trash WHERE memory_key = ?1",
        "DELETE FROM memory_links WHERE memory_key = ?1",
        "DELETE FROM memory_drafts WHERE key = ?1",
    ] {
        tx.execute(statement, params![key])?;
    }
    // `memory.stored` names the memory by its key; other events by its id.
    tx.execute(
        "DELETE FROM memory_events WHERE subject_table = 'memories' AND subject_id IN (?1, ?2)",
        params![memory_id, key],
    )?;
    let deleted = tx.execute("DELETE FROM memories WHERE id = ?1", params![memory_id])? > 0;
    if let Some(topic_id) = topic_id {
        delete_orphan_topic(tx, &topic_id)?;
    }
    Ok(deleted)
}

/// Delete one document with its chunks and derived copies; `false` when it
/// is already gone, or when it is a context page and `skip_pages` is set.
pub(super) fn delete_document(tx: &Connection, document_id: &str, skip_pages: bool) -> anyhow::Result<bool> {
    let row: Option<(Option<String>, String)> = tx
        .query_row(
            "SELECT ma_scope_id, source_kind FROM documents WHERE document_id = ?1",
            params![document_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((scope, source_kind)) = row else {
        return Ok(false);
    };
    if skip_pages && source_kind == CONTEXT_PAGE_KIND {
        return Ok(false);
    }
    let chunks = {
        let mut statement = tx.prepare("SELECT content FROM document_chunks WHERE document_id = ?1")?;
        statement
            .query_map(params![document_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for chunk in &chunks {
        evict_embeddings(tx, scope.as_deref(), chunk)?;
    }
    for statement in [
        "DELETE FROM memory_links WHERE document_id = ?1",
        "DELETE FROM memory_events WHERE subject_table = 'documents' AND subject_id = ?1",
        // The foreign key cascades too; deleting first keeps this independent
        // of the connection's `foreign_keys` setting.
        "DELETE FROM document_chunks WHERE document_id = ?1",
    ] {
        tx.execute(statement, params![document_id])?;
    }
    Ok(tx.execute("DELETE FROM documents WHERE document_id = ?1", params![document_id])? > 0)
}

/// The text a context page must hold to contain a deleted row: the row's
/// first non-blank line, at most [`PAGE_PROBE_CHARS`] characters (a page
/// transcript holds each message as `ROLE: <trimmed content>`).
fn page_probe(content: &str) -> Option<String> {
    let line = content.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.chars().take(PAGE_PROBE_CHARS).collect())
}

/// Delete the agent's OS-paging context pages of `session_key` that hold a
/// deleted row. A page records the event ids it was built from in its
/// metadata: `message_event_ids` (stored events) and `turn_event_ids` (the
/// input event of the turn whose own output the page holds). It is deleted
/// whole when one of its `message_event_ids` is a deleted event (`event_ids`,
/// a JSON array) or `input_event_id` is among its `turn_event_ids`. A page
/// without a `message_event_ids` array (written before pages recorded ids) is
/// matched by its source event or by the text of a deleted row. Pages of other
/// agents stay. Not counted as forgotten.
pub(super) fn erase_context_pages(
    tx: &Connection,
    session_key: &str,
    partition: Partition<'_>,
    event_ids: &str,
    input_event_id: &str,
    contents: &[&str],
) -> anyhow::Result<()> {
    let probes = serde_json::to_string(&contents.iter().filter_map(|text| page_probe(text)).collect::<Vec<_>>())?;
    let pages = {
        let mut statement = tx.prepare(
            "SELECT d.document_id FROM documents d
             WHERE d.source_kind = ?6
               AND (d.ma_scope_id IS ?2 OR (?3 AND d.ma_scope_id IS NULL)
                    OR EXISTS(SELECT 1 FROM agent_scopes o JOIN agent_scopes n
                              ON o.workspace_id = n.workspace_id AND o.owner_id = n.owner_id
                             AND o.agent_key = n.agent_key AND o.incarnation = n.incarnation
                              WHERE o.scope_id = d.ma_scope_id AND n.scope_id = ?2))
               AND d.source_uri = ?1
               AND (EXISTS(SELECT 1 FROM json_each(CASE WHEN json_valid(d.metadata_json) THEN d.metadata_json END,
                                                       '$.message_event_ids') m
                           WHERE m.value IN (SELECT value FROM json_each(?4)))
                    OR EXISTS(SELECT 1 FROM json_each(CASE WHEN json_valid(d.metadata_json) THEN d.metadata_json END,
                                                       '$.turn_event_ids') t
                              WHERE t.value = ?7)
                    OR ((CASE WHEN json_valid(d.metadata_json)
                              THEN json_type(d.metadata_json, '$.message_event_ids') END) IS NULL
                        AND (d.source_message_event_id IN (SELECT value FROM json_each(?4))
                             OR EXISTS(SELECT 1 FROM document_chunks c, json_each(?5) p
                                       WHERE c.document_id = d.document_id AND instr(c.content, p.value) > 0))))",
        )?;
        statement
            .query_map(
                params![
                    format!("context-page:{session_key}"),
                    partition.scope,
                    partition.main,
                    event_ids,
                    probes,
                    CONTEXT_PAGE_KIND,
                    input_event_id
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut deleted = false;
    for document_id in &pages {
        deleted |= delete_document(tx, document_id, false)?;
    }
    rebuild_indexes(tx, false, deleted)
}
