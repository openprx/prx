//! Derived copies of the deleted message log: removed with their rows in each
//! batch, or by age once per pass (tool-output documents, traces, compactions).
use super::super::{rf_delete, rf_forget};
use rusqlite::{Connection, params};
use std::collections::HashSet;

/// Tool-output documents removed per pass.
const MAX_DOCUMENTS: usize = 500;
/// Retrieval traces and compaction runs removed per pass.
const PASS_ROWS: i64 = 5_000;

/// Counts of the per-pass copies.
#[derive(Default)]
pub(super) struct PassCopies {
    pub documents: usize,
    pub traces: usize,
    pub compactions: usize,
}

/// Conversation turns projected from the deleted events.
pub(super) fn drop_turns(tx: &Connection, events_json: &str) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM conversation_turns WHERE message_event_id IN (SELECT value FROM json_each(?1))",
        params![events_json],
    )
}

/// Recompute the session's message count and refresh its preview after its
/// turns were removed. The session row itself stays.
pub(super) fn refresh_session(tx: &Connection, session: &str) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE sessions SET message_count = (SELECT count(*) FROM conversation_turns
             WHERE session_key = ?1 AND deleted_at IS NULL) WHERE session_key = ?1",
        params![session],
    )?;
    rf_delete::refresh_preview(tx, session)
}

/// Attempts of the deleted runs and of the deleted input events. The batch's
/// eligibility check (E3) already proved they are settled.
pub(super) fn drop_attempts(
    tx: &Connection,
    workspace: &str,
    runs_json: &str,
    events_json: &str,
) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM rf_message_attempts WHERE workspace_id = ?1
           AND (run_id IN (SELECT value FROM json_each(?2)) OR message_event_id IN (SELECT value FROM json_each(?3)))",
        params![workspace, runs_json, events_json],
    )
}

/// Link metadata of the deleted events.
pub(super) fn drop_links(tx: &Connection, events_json: &str) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM memory_links WHERE message_event_id IN (SELECT value FROM json_each(?1))",
        params![events_json],
    )
}

/// Whether the table `name` exists; the optional tables are created lazily.
pub(super) fn has_table(tx: &Connection, name: &str) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )
}

/// Ingress recovery records of the deleted rows (by input, evidence or cursor).
/// Their foreign keys to `message_events` have no delete action, so they go
/// first; the eligibility check keeps every unit whose input row still has an
/// unreconciled record. Recovery attestations of the deleted rows follow by
/// `ON DELETE CASCADE`.
pub(super) fn drop_ingress(tx: &Connection, events_json: &str, ids_json: &str) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM ingress_recovery WHERE input_event_id IN (SELECT value FROM json_each(?1))
            OR evidence_event_id IN (SELECT value FROM json_each(?1))
            OR cursor IN (SELECT value FROM json_each(?2))",
        params![events_json, ids_json],
    )
}

/// The age-based copies, once per pass after the batches.
pub(super) fn pass(tx: &Connection, cutoff: &str) -> anyhow::Result<PassCopies> {
    let documents = documents(tx, cutoff)?;
    let traces = tx.execute(
        "DELETE FROM retrieval_traces WHERE id IN (SELECT id FROM retrieval_traces
             WHERE julianday(created_at) < julianday(?1) LIMIT ?2)",
        params![cutoff, PASS_ROWS],
    )?;
    let compactions = tx.execute(
        "DELETE FROM compaction_runs WHERE id IN (SELECT id FROM compaction_runs
             WHERE julianday(created_at) < julianday(?1) LIMIT ?2)",
        params![cutoff, PASS_ROWS],
    )?;
    Ok(PassCopies {
        documents,
        traces,
        compactions,
    })
}

/// Tool-output documents (not untrusted external ones) older than the cutoff that no surviving tool result
/// row still quotes (by id, or by id without the `untrusted:` prefix). The
/// quoted set is read once for the pass.
fn documents(tx: &Connection, cutoff: &str) -> anyhow::Result<usize> {
    let candidates = {
        let mut statement = tx.prepare(
            "SELECT document_id FROM documents
             WHERE source_kind = 'tool_output'
               AND julianday(created_at) < julianday(?1)
             ORDER BY id",
        )?;
        statement
            .query_map(params![cutoff], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if candidates.is_empty() {
        return Ok(0);
    }
    let quoted: HashSet<String> = {
        let mut statement = tx.prepare(
            "SELECT DISTINCT json_extract(raw_payload_json, '$.document.document_id') FROM message_events
             WHERE event_type = 'tool.result.recorded' AND json_valid(raw_payload_json)",
        )?;
        statement
            .query_map([], |row| row.get::<_, Option<String>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect()
    };
    let mut deleted = 0;
    for id in candidates {
        if deleted >= MAX_DOCUMENTS {
            break;
        }
        let stripped = id.replace("untrusted:", "");
        if quoted.contains(&id) || quoted.contains(&stripped) {
            continue;
        }
        if rf_forget::delete_document(tx, &id, true)? {
            deleted += 1;
        }
    }
    rf_forget::rebuild_indexes(tx, false, deleted > 0)?;
    Ok(deleted)
}
