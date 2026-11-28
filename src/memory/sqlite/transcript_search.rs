//! Search over the full-text index of transcript rows
//! (`message_events.content`). The index itself, its creation and its initial
//! fill are in [`super::transcript_index`]. Until that fill is complete a search
//! matches every word with `LIKE` on the rows, so it never misses a row the
//! index does not cover yet.

use super::SqliteMemory;
use super::events::{MA_SCOPE_COLUMN, numbered_placeholders, partition_conjunct};
use super::transcript_index::{INDEX_TABLE, IndexState, build_index, index_state};
use crate::memory::traits::{MemoryPrincipal, TranscriptHit};
use crate::memory::{AgentPartition, PartitionKey};
use std::collections::HashSet;

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
pub(super) const SEARCHABLE_ROW_SQL: &str = "NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = e.ma_scope_id)
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

/// The query terms and how they are matched against `message_events.content`.
struct QueryMatch {
    /// The first word of the query; it occurs in every hit. A row longer than
    /// [`WHOLE_ROW_MAX_CHARS`] is returned as the region around it.
    anchor: String,
    terms: TermMatch,
}

/// How the query terms are matched against `message_events.content`.
enum TermMatch {
    /// One FTS5 `MATCH` over the long terms, quoted and ANDed (see
    /// [`fts_match_text`]), and a `LIKE` filter per short term.
    Fts { long: Vec<String>, short: Vec<String> },
    /// `LIKE` per term, ANDed, when no term is long enough for the trigram
    /// index or the index is not complete yet.
    Like { terms: Vec<String> },
}

impl TermMatch {
    /// The same terms matched without the index: `LIKE` per term, ANDed.
    fn without_index(self) -> Self {
        match self {
            Self::Fts { mut long, short } => {
                long.extend(short);
                Self::Like { terms: long }
            }
            like @ Self::Like { .. } => like,
        }
    }
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
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut values: Vec<rusqlite::types::Value> = Vec::new();
            let QueryMatch { anchor, terms } = matcher;
            // Before the index is complete it does not cover every row, so
            // every word is matched on the rows themselves.
            let terms = if index_state(conn)? == IndexState::Complete {
                terms
            } else {
                terms.without_index()
            };
            let (from_sql, term_sql, order_sql) = match terms {
                TermMatch::Fts { long, short } => {
                    values.push(rusqlite::types::Value::from(fts_match_text(&long)));
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

    /// Create the index if missing and fill it in short batches until it is
    /// complete (resuming an interrupted fill); a no-op on a complete index.
    pub(super) async fn warm_transcript_index_impl(&self) -> anyhow::Result<()> {
        build_index(self.pool.clone()).await
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
    let long = words
        .iter()
        .filter(|word| is_long_term(word))
        .map(|word| (*word).to_owned())
        .collect();
    let short = words
        .iter()
        .filter(|word| !is_long_term(word))
        .map(|word| (*word).to_owned())
        .collect();
    Some(QueryMatch {
        anchor,
        terms: TermMatch::Fts { long, short },
    })
}

/// The FTS5 query of the long terms: each double-quoted (embedded quotes
/// doubled), ANDed.
fn fts_match_text(long: &[String]) -> String {
    long.iter()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
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
