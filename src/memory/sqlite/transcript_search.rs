//! Full-text index over transcript rows (`message_events.content`) and the
//! search over it.
//!
//! The FTS5 table and its three triggers are created on the first search, not
//! in `schema.rs`, so a brain.db that never searches carries no index and the
//! schema stamp is unchanged. The index is external-content: it stores no copy
//! of the text, only the FTS5 structures, and it indexes every row. It uses the
//! trigram tokenizer, so Chinese and other scripts without spaces match as
//! substrings. Filters (owner, workspace, agent partition, erased scopes,
//! inactive incarnations, message role and type) are applied at query time,
//! never in the triggers, so a row's index entry always matches its `content`
//! and the `delete` command always removes what was indexed.

use super::SqliteMemory;
use super::erasure_pool::MemoryPool;
use super::events::{MA_SCOPE_COLUMN, numbered_placeholders, partition_conjunct};
use crate::memory::traits::{MemoryPrincipal, TranscriptHit};
use crate::memory::{AgentPartition, PartitionKey};
use crate::runtime::sqlite_pool::begin_immediate;
use rusqlite::Connection;

/// Name of the FTS5 table over `message_events`.
const INDEX_TABLE: &str = "message_events_fts";

/// Shortest term (in characters) the trigram tokenizer can match. Shorter terms
/// are matched with `LIKE` instead.
const TRIGRAM_MIN_CHARS: usize = 3;

/// Index objects and their initial fill, run once in one write transaction.
///
/// Failure semantics: an error inside a trigger aborts the statement that fired
/// it, as a storage error on `message_events` itself would. A failing FTS write
/// therefore fails the message write (fail-closed); it is never swallowed.
///
/// `INSERT OR REPLACE` on `message_events` would delete the old row without
/// firing the delete trigger and so corrupt the index: never use it there. A
/// table rebuild migration must recreate these triggers.
///
/// The `rebuild` indexes the rows that exist when the index is created; its cost
/// is proportional to the number of `message_events` rows. So the first search
/// builds the index over all transcript rows of the database, under the write
/// lock.
///
/// `secure-delete` is FTS5's own option: a deleted or blanked row's terms are
/// overwritten on disk instead of lingering in older segments. It is stored in
/// the index's `%_config` table (format version 5), so later connections keep
/// it without setting it. Erasure therefore needs no rebuild.
const CREATE_INDEX_SQL: &str = "
    CREATE VIRTUAL TABLE IF NOT EXISTS message_events_fts USING fts5(
        content, content='message_events', content_rowid='id', tokenize='trigram');
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
    INSERT INTO message_events_fts(message_events_fts, rank) VALUES ('secure-delete', 1);
    INSERT INTO message_events_fts(message_events_fts) VALUES ('rebuild');";

/// How the query terms are matched against `message_events.content`.
enum TermMatch {
    /// One FTS5 `MATCH` over the quoted terms, ANDed.
    Fts(String),
    /// `LIKE` per term, ANDed, when some term is too short for the trigram index.
    Like(Vec<String>),
}

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
        let Some(matcher) = term_match(query) else {
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
            let (from_sql, term_sql, excerpt_sql, order_sql, mut values) = match matcher {
                TermMatch::Fts(match_text) => (
                    format!("{INDEX_TABLE} JOIN message_events e ON e.id = {INDEX_TABLE}.rowid"),
                    format!("{INDEX_TABLE} MATCH ?1"),
                    format!("snippet({INDEX_TABLE}, 0, '[', ']', '...', 24)"),
                    format!("bm25({INDEX_TABLE})"),
                    vec![rusqlite::types::Value::from(match_text)],
                ),
                TermMatch::Like(terms) => {
                    let mut conditions = Vec::with_capacity(terms.len());
                    let mut values = Vec::with_capacity(terms.len());
                    for term in terms {
                        conditions.push(format!(r"e.content LIKE ?{} ESCAPE '\'", values.len() + 1));
                        values.push(rusqlite::types::Value::from(format!("%{}%", escape_like(&term))));
                    }
                    (
                        "message_events e".to_owned(),
                        conditions.join(" AND "),
                        "substr(e.content, 1, 200)".to_owned(),
                        "e.id DESC".to_owned(),
                        values,
                    )
                }
            };
            // Placeholders after the term values: workspace, owner, session key
            // (NULL for any session), the partition, then the limit.
            let first = values.len() + 1;
            let (partition_sql, partition_values) =
                partition_conjunct(MA_SCOPE_COLUMN, Some(&partition), numbered_placeholders(first + 3));
            let limit_marker = first + 3 + partition_values.len();
            let sql = format!(
                "SELECT e.session_key, e.event_id, e.role, e.created_at, {excerpt_sql}
                 FROM {from_sql}
                 WHERE {term_sql}
                   AND e.workspace_id = ?{first}
                   AND e.owner_id = ?{owner_marker}
                   AND (?{session_marker} IS NULL OR e.session_key = ?{session_marker})
                   AND e.event_type = 'message.created'
                   AND e.role IN ('user', 'assistant')
                   AND NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = e.ma_scope_id)
                   AND (e.ma_scope_id IS NULL OR EXISTS (
                        SELECT 1 FROM agent_scopes s
                        JOIN agent_incarnations i
                          ON i.workspace_id = s.workspace_id AND i.owner_id = s.owner_id
                         AND i.agent_key = s.agent_key AND i.incarnation = s.incarnation
                        WHERE s.scope_id = e.ma_scope_id AND i.state = 'active')){partition_sql}
                 ORDER BY {order_sql}
                 LIMIT ?{limit_marker}",
                owner_marker = first + 1,
                session_marker = first + 2,
            );
            values.push(rusqlite::types::Value::from(workspace));
            values.push(rusqlite::types::Value::from(owner));
            values.push(rusqlite::types::Value::from(session_key));
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

/// Matching for free user input: whitespace-separated words, ANDed. When every
/// word has at least [`TRIGRAM_MIN_CHARS`] characters, the words go to FTS5 as
/// double-quoted strings (embedded quotes doubled), so FTS5 operators, column
/// filters, `*`, `NEAR(...)` and parentheses in the input are literal words.
/// When any word is shorter, every word is matched with `LIKE` instead. `None`
/// when the input has no word.
fn term_match(query: &str) -> Option<TermMatch> {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    if words.iter().all(|word| word.chars().count() >= TRIGRAM_MIN_CHARS) {
        let terms: Vec<String> = words
            .iter()
            .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
            .collect();
        Some(TermMatch::Fts(terms.join(" ")))
    } else {
        Some(TermMatch::Like(words.into_iter().map(str::to_owned).collect()))
    }
}

/// Escape a term for a `LIKE` pattern whose escape character is `\`.
fn escape_like(term: &str) -> String {
    let mut escaped = String::with_capacity(term.len());
    for ch in term.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
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
