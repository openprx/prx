//! Full-text index over transcript rows (`message_events.content`) and the
//! search over it.
//!
//! The FTS5 table and its three triggers are not in `schema.rs`, so the schema
//! stamp is unchanged. They are created by the warm-up at process start, which
//! every process that registers tools runs in the background. From then on
//! every message insert pays the trigger cost. When the warm-up did not run (no
//! runtime, or a failure), the first search creates them instead. The index is
//! external-content: it stores no copy
//! of the text, only the FTS5 structures, and it indexes every row. It uses the
//! trigram tokenizer, so Chinese and other scripts without spaces match as
//! substrings. Filters (owner, workspace, agent partition, erased scopes,
//! inactive incarnations, rf-deleted messages, message role and type) are
//! applied at query time, never in the triggers, so a row's index entry always
//! matches its `content` and the `delete` command always removes what was
//! indexed.

use super::SqliteMemory;
use super::erasure_pool::MemoryPool;
use super::events::{MA_SCOPE_COLUMN, numbered_placeholders, partition_conjunct};
use crate::memory::traits::{MemoryPrincipal, TranscriptHit};
use crate::memory::{AgentPartition, PartitionKey};
use crate::runtime::sqlite_pool::begin_immediate;
use rusqlite::Connection;
use std::collections::HashSet;

/// Name of the FTS5 table over `message_events`.
const INDEX_TABLE: &str = "message_events_fts";

/// Shortest term (in characters) the trigram tokenizer can match. Shorter terms
/// are matched with `LIKE` instead.
const TRIGRAM_MIN_CHARS: usize = 3;

/// Rows up to this many characters are returned whole.
const WHOLE_ROW_MAX_CHARS: usize = 65_536;

/// For a longer row, the characters returned on each side of the anchor's
/// first match (the region starts at the row start when no match is found).
const REGION_SIDE_CHARS: usize = 4_096;

/// The deletion exclusions of a transcript row aliased `e`: the row is not in
/// an erase-fenced scope, its scope (if any) belongs to an active incarnation,
/// and no rf delete covers it (a message delete covers the input and every row
/// of its run; a reply delete covers the run's rows except the input). A delete
/// of a live run sets `deleted_at` at once and only defers the row rewrite
/// (`erase_pending`), so the row is excluded from that moment. Shared by the
/// search and by [`SqliteMemory::excluded_transcript_rows_impl`], so both apply
/// the same predicate.
const SEARCHABLE_ROW_SQL: &str = "NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = e.ma_scope_id)
                   AND (e.ma_scope_id IS NULL OR EXISTS (
                        SELECT 1 FROM agent_scopes s
                        JOIN agent_incarnations i
                          ON i.workspace_id = s.workspace_id AND i.owner_id = s.owner_id
                         AND i.agent_key = s.agent_key AND i.incarnation = s.incarnation
                        WHERE s.scope_id = e.ma_scope_id AND i.state = 'active'))
                   AND NOT EXISTS (SELECT 1 FROM rf_message_attempts a
                        WHERE a.workspace_id = e.workspace_id AND a.owner_id = e.owner_id
                          AND ((a.deleted_at IS NOT NULL
                                AND (a.message_event_id = e.event_id OR a.run_id = e.run_id))
                            OR (a.reply_deleted_at IS NOT NULL AND a.run_id = e.run_id
                                AND a.message_event_id <> e.event_id)))";

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
/// lock. The first search or the process-start warm-up builds it, whichever
/// comes first.
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

/// The query terms and how they are matched against `message_events.content`.
struct QueryMatch {
    /// The first word of the query; it occurs in every hit. A row longer than
    /// [`WHOLE_ROW_MAX_CHARS`] is returned as the region around it.
    anchor: String,
    terms: TermMatch,
}

/// How the query terms are matched against `message_events.content`.
enum TermMatch {
    /// One FTS5 `MATCH` over the quoted long terms, ANDed, and a `LIKE` filter
    /// per short term.
    Fts { match_text: String, short: Vec<String> },
    /// `LIKE` per term, ANDed, when no term is long enough for the trigram
    /// index.
    Like { terms: Vec<String> },
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
            let mut values: Vec<rusqlite::types::Value> = Vec::new();
            let QueryMatch { anchor, terms } = matcher;
            let (from_sql, term_sql, order_sql) = match terms {
                TermMatch::Fts { match_text, short } => {
                    values.push(rusqlite::types::Value::from(match_text));
                    let mut conditions = vec![format!("{INDEX_TABLE} MATCH ?1")];
                    for term in short {
                        values.push(like_pattern(&term));
                        conditions.push(like_condition(values.len()));
                    }
                    (
                        format!("{INDEX_TABLE} JOIN message_events e ON e.id = {INDEX_TABLE}.rowid"),
                        conditions.join(" AND "),
                        format!("bm25({INDEX_TABLE})"),
                    )
                }
                TermMatch::Like { terms } => {
                    let mut conditions = Vec::with_capacity(terms.len());
                    for term in terms {
                        values.push(like_pattern(&term));
                        conditions.push(like_condition(values.len()));
                    }
                    (
                        "message_events e".to_owned(),
                        conditions.join(" AND "),
                        "e.id DESC".to_owned(),
                    )
                }
            };
            // The raw row, unredacted: the reader redacts it before cutting an
            // excerpt. A row longer than WHOLE_ROW_MAX_CHARS is cut to the
            // region of REGION_SIDE_CHARS on each side of the anchor's first
            // match (ASCII case-insensitive, as `lower` is), so a hit reads a
            // bounded amount of text.
            values.push(rusqlite::types::Value::from(anchor));
            let anchor_marker = values.len();
            let content_sql = format!(
                "CASE WHEN length(e.content) <= {WHOLE_ROW_MAX_CHARS} THEN e.content
                      ELSE substr(e.content,
                                  max(1, instr(lower(e.content), lower(?{anchor_marker})) - {REGION_SIDE_CHARS}),
                                  2 * {REGION_SIDE_CHARS} + length(?{anchor_marker})) END"
            );
            // Placeholders after the term values: workspace, owner, session key
            // (NULL for any session), the partition, then the limit.
            let first = values.len() + 1;
            let (partition_sql, partition_values) =
                partition_conjunct(MA_SCOPE_COLUMN, Some(&partition), numbered_placeholders(first + 3));
            let limit_marker = first + 3 + partition_values.len();
            let sql = format!(
                "SELECT e.session_key, e.event_id, e.role, e.created_at, {content_sql}
                 FROM {from_sql}
                 WHERE {term_sql}
                   AND e.workspace_id = ?{first}
                   AND e.owner_id = ?{owner_marker}
                   AND (?{session_marker} IS NULL OR e.session_key = ?{session_marker})
                   AND e.event_type = 'message.created'
                   AND e.role IN ('user', 'assistant')
                   AND {SEARCHABLE_ROW_SQL}{partition_sql}
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

    /// The ids among `row_ids` (`message_events.id`) of rows the search leaves
    /// out for deletion: [`SEARCHABLE_ROW_SQL`] does not hold for them. Ids
    /// with no row are not returned. Reads no content.
    pub(super) async fn excluded_transcript_rows_impl(&self, row_ids: &[i64]) -> anyhow::Result<HashSet<i64>> {
        if row_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let row_ids = row_ids.to_vec();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<HashSet<i64>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut excluded = HashSet::new();
            // Chunks stay far below SQLite's bound-variable limit.
            for chunk in row_ids.chunks(500) {
                let markers = (1..=chunk.len())
                    .map(|at| format!("?{at}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT e.id FROM message_events e
                     WHERE e.id IN ({markers}) AND NOT ({SEARCHABLE_ROW_SQL})"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| row.get::<_, i64>(0))?;
                for row in rows {
                    excluded.insert(row?);
                }
            }
            Ok(excluded)
        })
        .await?
    }

    /// Build the index now, so the first search does not. Same creation and
    /// fill as the first search; a no-op when the index exists.
    pub(super) async fn warm_transcript_index_impl(&self) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || ensure_index(&pool)).await?
    }
}

/// Matching for free user input: whitespace-separated words, ANDed, each word
/// once (see [`distinct_words`]). Words of at least [`TRIGRAM_MIN_CHARS`]
/// characters go to FTS5 as double-quoted strings (embedded quotes doubled), so
/// FTS5 operators, column filters, `*`, `NEAR(...)` and parentheses in the input
/// are literal words. Shorter words are `LIKE` post-filters on that match. When
/// no word is long enough, every word is matched with `LIKE`. The first word is
/// the anchor. `None` when the input has no word.
fn term_match(query: &str) -> Option<QueryMatch> {
    let words = distinct_words(query);
    let anchor = (*words.first()?).to_owned();
    if !words.iter().any(|word| is_long_term(word)) {
        return Some(QueryMatch {
            anchor,
            terms: TermMatch::Like {
                terms: words.into_iter().map(str::to_owned).collect(),
            },
        });
    }
    let match_text = words
        .iter()
        .filter(|word| is_long_term(word))
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let short = words
        .iter()
        .filter(|word| !is_long_term(word))
        .map(|word| (*word).to_owned())
        .collect();
    Some(QueryMatch {
        anchor,
        terms: TermMatch::Fts { match_text, short },
    })
}

/// Whether a word is long enough for the trigram index.
fn is_long_term(word: &str) -> bool {
    word.chars().count() >= TRIGRAM_MIN_CHARS
}

/// The query's whitespace-separated words, each once, in order. The first
/// spelling of a word is kept; words are compared ignoring ASCII case. There is
/// no cap on the number of words.
fn distinct_words(query: &str) -> Vec<&str> {
    let mut seen = HashSet::new();
    query
        .split_whitespace()
        .filter(|word| seen.insert(word.to_ascii_lowercase()))
        .collect()
}

/// The `LIKE` pattern of a term: the term, escaped, between two `%`.
fn like_pattern(term: &str) -> rusqlite::types::Value {
    rusqlite::types::Value::from(format!("%{}%", escape_like(term)))
}

/// The `LIKE` condition on the content with the pattern at placeholder `marker`.
fn like_condition(marker: usize) -> String {
    format!(r"e.content LIKE ?{marker} ESCAPE '\'")
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
