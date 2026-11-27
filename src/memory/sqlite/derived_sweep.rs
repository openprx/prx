//! Sweeps of derived copies that name erased data. A tool result of a
//! session-listing tool prints run ids and session keys; once the session or
//! run is erased the printed text is replaced, in the erasing transaction.
use super::rf_forget;
use rusqlite::{Connection, params};

/// Tool results of the session-listing tools that name an erased run or session.
pub(super) const LISTING_TOOLS: [&str; 8] = [
    "sessions_list",
    "sessions_history",
    "sessions_send",
    "session_status",
    "sessions_spawn",
    "subagents",
    "delegate",
    "agents_list",
];
pub(super) const LISTING_REMOVED_TEXT: &str = "[output removed: it named a session that was erased]";
/// Run ids are UUIDs and child keys are `child-<64 hex>`; shorter identifiers
/// (`main`, short names) would match unrelated text.
pub(super) const MIN_IDENTIFIER_CHARS: usize = 32;

const TOOL_OUTPUT_KINDS: &str = "('tool_output', 'untrusted_external')";
const UNTRUSTED_PREFIX: &str = "untrusted:";

/// One tool-result row selected for rewriting.
struct MatchedRow {
    event_id: String,
    document_id: Option<String>,
}

/// Replace the printed text of listing-tool results (in `workspace_id`, newer
/// than `since_id`) that contain one of `identifiers`, then delete the stored
/// tool-output documents that only those results named. Returns the number of
/// rows rewritten. A row stays (the tool call and result pair must stay valid
/// for replay); a row with an unparseable payload, a row of a fenced scope and
/// a row of another tool are left as they are.
pub(super) fn rewrite_listings(
    tx: &Connection,
    workspace_id: &str,
    identifiers: &[String],
    since_id: i64,
    now: &str,
) -> anyhow::Result<usize> {
    let mut usable: Vec<&str> = identifiers
        .iter()
        .map(String::as_str)
        .filter(|id| id.chars().count() >= MIN_IDENTIFIER_CHARS)
        .collect();
    usable.sort_unstable();
    usable.dedup();
    if usable.is_empty() {
        return Ok(0);
    }
    let rows = select_listing_rows(tx, workspace_id, &usable, since_id)?;
    let documents = rewrite_rows(tx, &rows, LISTING_REMOVED_TEXT, now)?;
    delete_unquoted_documents(tx, &documents)?;
    Ok(rows.len())
}

fn select_listing_rows(
    tx: &Connection,
    workspace_id: &str,
    identifiers: &[&str],
    since_id: i64,
) -> anyhow::Result<Vec<MatchedRow>> {
    let identifiers_json = serde_json::to_string(identifiers)?;
    let tools = serde_json::to_string(&LISTING_TOOLS)?;
    let mut statement = tx.prepare(
        "SELECT event_id, json_extract(raw_payload_json, '$.document.document_id'), content
         FROM message_events
         WHERE workspace_id = ?1 AND event_type = 'tool.result.recorded' AND id > ?2
           AND json_valid(raw_payload_json)
           AND json_extract(raw_payload_json, '$.tool') IN (SELECT value FROM json_each(?4))
           AND EXISTS (SELECT 1 FROM json_each(?3) j WHERE instr(content, j.value) > 0)
           AND NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = message_events.ma_scope_id)",
    )?;
    let rows = statement
        .query_map(params![workspace_id, since_id, identifiers_json, tools], |row| {
            let document_id: rusqlite::types::Value = row.get(1)?;
            let content: String = row.get(2)?;
            Ok((
                MatchedRow {
                    event_id: row.get(0)?,
                    document_id: match document_id {
                        rusqlite::types::Value::Text(text) => Some(text),
                        _ => None,
                    },
                },
                content,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, content)| identifiers.iter().any(|id| names_identifier(content, id)))
        .map(|(row, _)| row)
        .collect())
}

fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-')
}

/// True when `text` holds `identifier` followed by the end of the text or a
/// character that cannot continue an identifier, so `child-segment:P:1` does
/// not match `child-segment:P:12`.
fn names_identifier(text: &str, identifier: &str) -> bool {
    text.match_indices(identifier).any(|(start, found)| {
        text.get(start + found.len()..)
            .and_then(|rest| rest.chars().next())
            .is_none_or(|next| !is_identifier_char(next))
    })
}

/// Replace the text of each row; returns the document ids the rows named.
/// Reusable by sweeps that select tool results by other means.
fn rewrite_rows(tx: &Connection, rows: &[MatchedRow], text: &str, now: &str) -> anyhow::Result<Vec<String>> {
    let mut documents = Vec::new();
    for row in rows {
        let updated = tx.execute(
            "UPDATE message_events SET content = ?2, content_hash = NULL,
                 raw_payload_json = json_set(raw_payload_json, '$.document', json('null'),
                     '$.inline_bytes', ?3, '$.listing_erased', json('true')),
                 updated_at = ?4
             WHERE event_id = ?1 AND event_type = 'tool.result.recorded' AND json_valid(raw_payload_json)",
            params![row.event_id, text, i64::try_from(text.len())?, now],
        )?;
        if updated > 0 {
            documents.extend(row.document_id.clone());
        }
    }
    documents.sort_unstable();
    documents.dedup();
    Ok(documents)
}

/// Delete the stored tool-output documents (and their `untrusted:` variants)
/// that no surviving tool result row names; rebuild the document index once.
fn delete_unquoted_documents(tx: &Connection, documents: &[String]) -> anyhow::Result<()> {
    let mut deleted = false;
    for document in documents {
        let base = document.strip_prefix(UNTRUSTED_PREFIX).unwrap_or(document);
        for id in [base.to_owned(), format!("{UNTRUSTED_PREFIX}{base}")] {
            let tool_output: bool = tx.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM documents WHERE document_id = ?1 AND source_kind IN {TOOL_OUTPUT_KINDS})"),
                params![id],
                |row| row.get(0),
            )?;
            let quoted: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM message_events
                     WHERE event_type = 'tool.result.recorded' AND json_valid(raw_payload_json)
                       AND json_extract(raw_payload_json, '$.document.document_id') IN (?1, replace(?1, ?2, '')))",
                params![id, UNTRUSTED_PREFIX],
                |row| row.get(0),
            )?;
            if tool_output && !quoted {
                deleted |= rf_forget::delete_document(tx, &id, true)?;
            }
        }
    }
    rf_forget::rebuild_indexes(tx, false, deleted)
}
