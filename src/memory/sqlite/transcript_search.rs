//! Full-text index over transcript rows (`message_events.content`) and the
//! search over it.
//!
//! The FTS5 table and its three triggers are created on the first search, not
//! in `schema.rs`, so a brain.db that never searches carries no index and the
//! schema stamp is unchanged. The index is external-content: it stores no copy
//! of the text, only the FTS5 structures, and it indexes every row. Filters
//! (owner, workspace, agent partition, erased scopes, inactive incarnations,
//! message role and type) are applied at query time, never in the triggers, so
//! a row's index entry always matches its `content` and the `delete` command
//! always removes what was indexed.

use super::SqliteMemory;
use super::erasure_pool::MemoryPool;
use super::events::{MA_SCOPE_COLUMN, numbered_placeholders, partition_conjunct};
use crate::memory::traits::{MemoryPrincipal, TranscriptHit};
use crate::memory::{AgentPartition, PartitionKey};
use crate::runtime::sqlite_pool::begin_immediate;
use rusqlite::Connection;

/// Name of the FTS5 table over `message_events`.
const INDEX_TABLE: &str = "message_events_fts";

/// Index objects and their initial fill, run once in one write transaction.
///
/// The triggers never fail on their own: they only insert into and delete from
/// the FTS5 table, using the stored `content` of the row they fire for. The
/// `rebuild` indexes the rows that exist when the index is created; its cost is
/// proportional to the number of `message_events` rows.
const CREATE_INDEX_SQL: &str = "
    CREATE VIRTUAL TABLE IF NOT EXISTS message_events_fts USING fts5(
        content, content='message_events', content_rowid='id');
    CREATE TRIGGER IF NOT EXISTS message_events_fts_ai AFTER INSERT ON message_events BEGIN
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;
    CREATE TRIGGER IF NOT EXISTS message_events_fts_ad AFTER DELETE ON message_events BEGIN
        INSERT INTO message_events_fts(message_events_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
    END;
    CREATE TRIGGER IF NOT EXISTS message_events_fts_au AFTER UPDATE OF content ON message_events BEGIN
        INSERT INTO message_events_fts(message_events_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
        INSERT INTO message_events_fts(rowid, content) VALUES (new.id, new.content);
    END;
    INSERT INTO message_events_fts(message_events_fts) VALUES ('rebuild');";

impl SqliteMemory {
    pub(super) async fn search_transcripts_impl(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        session_key: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<TranscriptHit>> {
        // Owner and partition rules are those of `newest_core_entries`: no
        // owner reads nothing; no partition is the Main partition; a partition
        // of another owner or workspace reads nothing.
        let Some(owner) = principal.owner_id.clone() else {
            return Ok(Vec::new());
        };
        let Some(match_query) = fts_match_query(query) else {
            return Ok(Vec::new());
        };
        let workspace = principal.workspace_id.clone();
        let partition = match principal.agent_partition.clone() {
            Some(partition) => partition,
            None => AgentPartition {
                workspace_id: workspace.clone(),
                owner_id: owner.clone(),
                key: PartitionKey::Main,
            },
        };
        if partition.owner_id != owner || partition.workspace_id != workspace {
            return Ok(Vec::new());
        }
        let session_key = session_key.map(str::to_owned);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<TranscriptHit>> {
            ensure_index(&pool)?;
            let reader = pool.read()?;
            let conn = reader.connection()?;
            // Placeholders: ?1 match text, ?2 workspace, ?3 owner, ?4 session
            // key (NULL for any session), the partition from ?5, then the limit.
            let (partition_sql, partition_values) =
                partition_conjunct(MA_SCOPE_COLUMN, Some(&partition), numbered_placeholders(5));
            let limit_marker = 5 + partition_values.len();
            let sql = format!(
                "SELECT e.session_key, e.event_id, e.role, e.created_at,
                        snippet({INDEX_TABLE}, 0, '[', ']', '...', 24)
                 FROM {INDEX_TABLE}
                 JOIN message_events e ON e.id = {INDEX_TABLE}.rowid
                 WHERE {INDEX_TABLE} MATCH ?1
                   AND e.workspace_id = ?2
                   AND e.owner_id = ?3
                   AND (?4 IS NULL OR e.session_key = ?4)
                   AND e.event_type = 'message.created'
                   AND e.role IN ('user', 'assistant')
                   AND NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = e.ma_scope_id)
                   AND (e.ma_scope_id IS NULL OR EXISTS (
                        SELECT 1 FROM agent_scopes s
                        JOIN agent_incarnations i
                          ON i.workspace_id = s.workspace_id AND i.owner_id = s.owner_id
                         AND i.agent_key = s.agent_key AND i.incarnation = s.incarnation
                        WHERE s.scope_id = e.ma_scope_id AND i.state = 'active')){partition_sql}
                 ORDER BY bm25({INDEX_TABLE})
                 LIMIT ?{limit_marker}"
            );
            let mut values: Vec<rusqlite::types::Value> = vec![
                rusqlite::types::Value::from(match_query),
                rusqlite::types::Value::from(workspace),
                rusqlite::types::Value::from(owner),
                rusqlite::types::Value::from(session_key),
            ];
            values.extend(partition_values.into_iter().map(rusqlite::types::Value::from));
            values.push(rusqlite::types::Value::from(limit));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
                Ok(TranscriptHit {
                    session_key: row.get(0)?,
                    event_id: row.get(1)?,
                    role: row.get(2)?,
                    created_at: row.get(3)?,
                    excerpt: row.get(4)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(anyhow::Error::from)
        })
        .await?
    }
}

/// FTS5 `MATCH` text for free user input: each whitespace-separated word becomes
/// one double-quoted string (embedded quotes doubled), so FTS5 operators, column
/// filters, `*`, `NEAR(...)` and parentheses in the input are literal words.
/// The words are ANDed. `None` when the input has no word.
fn fts_match_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// Create the index if the database does not have it. The check is a read; the
/// write lock is taken only when the table is missing, and the check is repeated
/// under the lock, so the creation and its fill run once per database.
fn ensure_index(pool: &MemoryPool) -> anyhow::Result<()> {
    {
        let reader = pool.read()?;
        if index_exists(reader.connection()?)? {
            return Ok(());
        }
    }
    let guard = pool.write();
    let tx = begin_immediate(&guard)?;
    if !index_exists(&tx)? {
        tx.execute_batch(CREATE_INDEX_SQL)?;
    }
    tx.commit()?;
    Ok(())
}

fn index_exists(conn: &Connection) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [INDEX_TABLE],
        |row| row.get(0),
    )?)
}
