//! Later attachments that still quote deleted content (`message_delete_v1`).
//!
//! A `message.attachment.recorded` row stores the recalled text put before a
//! user message (`prefix`) and the date line after it (`tail`), and later
//! turns replay it. The recall of a later turn can quote an earlier message,
//! a memory made from it or a document, so a delete of that message (or a
//! forget of the memories it wrote) must rewrite the later row too.
//!
//! A row written with provenance (`sources`: memory, event and document ids)
//! is rewritten when any of its sources is gone: an event tombstoned as
//! `message.deleted`, a memory missing from `memories`, a document missing
//! from `documents`. A memory that still exists is not a reason: a plain
//! delete keeps the memories, and the attachments quoting only those stay.
//!
//! A row without `sources` (written before provenance) is matched by text:
//! its `prefix` is rewritten when it contains the probe of a deleted
//! conversational row, in the same session only. The probe is the first
//! [`PROBE_CHARS`] characters of the trimmed row with line breaks as spaces
//! (the form a recall line quotes it in), and a probe shorter than
//! [`MIN_PROBE_CHARS`] is never used.
//!
//! A rewritten row keeps its `tail` (the date line), gets an empty `prefix`
//! and empty `sources`, and is marked `"blanked": true`; replay then sends
//! the user message with its date line and no recalled text. The rewrite
//! happens in the delete's transaction and appends nothing to the sync
//! stream (the rows have role `tool`).

use rusqlite::{Connection, params};

const ATTACHMENT_EVENT_TYPE: &str = "message.attachment.recorded";

/// Replaces the payload of a row whose sealed prefix cannot be edited.
const BLANKED_WRAPPER: &str = r#"{"blanked":true}"#;

/// `json_each` reads this form of a payload, so a malformed or missing value
/// yields no rows and never raises.
fn guarded(payload: &str) -> String {
    format!("CASE WHEN json_valid({payload}) THEN {payload} ELSE '{{}}' END")
}

/// The side rows of one attachment payload: one `(kind, id, event)` row per
/// text element of each source list (a bare string counts as a one-element
/// list). `from` is the table clause, `condition` the extra row filter.
fn side_rows_sql(event_id: &str, payload: &str, from: &str, condition: &str) -> String {
    let doc = guarded(payload);
    format!(
        "INSERT OR IGNORE INTO attachment_sources(kind, source_id, attachment_event_id)
         SELECT k.kind, j.value, {event_id}
         FROM {from} (SELECT 'memory' AS kind, '$.sources.memories' AS path
                      UNION ALL SELECT 'event', '$.sources.events'
                      UNION ALL SELECT 'document', '$.sources.documents'
                      UNION ALL SELECT 'result', '$.sources.results') k,
              json_each({doc}, k.path) j
         WHERE {condition} j.type = 'text'
           AND json_type({doc}, k.path) IN ('array', 'text');"
    )
}

/// Rows of an erase-fenced scope are removed by their own erase.
const FENCE_GUARD: &str =
    "NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = message_events.ma_scope_id)";

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// Payload of a blanked row: the prefix and the id lists go, the tail, the
/// note and the result ids stay; a sealed (wrapper) payload is replaced.
fn blank_payload_sql(sources_value: &str) -> String {
    format!(
        "CASE WHEN json_type(raw_payload_json, '$.encrypted_child_payload') IS NULL
              THEN json_set(raw_payload_json, '$.prefix', '', {sources_value}, '$.blanked', json('true'))
              ELSE '{BLANKED_WRAPPER}' END"
    )
}

const SOURCES_EMPTY_LISTS: &str =
    "'$.sources.memories', json('[]'), '$.sources.events', json('[]'), '$.sources.documents', json('[]')";
const SOURCES_EMPTY_OBJECT: &str = "'$.sources', json('{\"memories\":[],\"events\":[],\"documents\":[]}')";

/// BLANK_PREFIX: blank every attachment row that lists `kind` id `id`.
fn blank_prefix_sql(kind: &str, id: &str) -> String {
    let payload = blank_payload_sql(SOURCES_EMPTY_LISTS);
    format!(
        "UPDATE message_events SET raw_payload_json = {payload}, updated_at = {NOW}
         WHERE event_id IN (SELECT attachment_event_id FROM attachment_sources
                            WHERE kind = '{kind}' AND source_id = {id})
           AND event_type = '{ATTACHMENT_EVENT_TYPE}' AND json_valid(raw_payload_json)
           AND {FENCE_GUARD};"
    )
}

/// CUT_NOTE: a gone scheduled result removes the whole note from the tails
/// that quoted it (it was shown once already).
fn cut_note_sql(id: &str) -> String {
    format!(
        "UPDATE message_events SET raw_payload_json = CASE
             WHEN json_type(raw_payload_json, '$.encrypted_child_payload') IS NOT NULL THEN '{BLANKED_WRAPPER}'
             ELSE json_set(raw_payload_json,
                 '$.tail', replace(json_extract(raw_payload_json, '$.tail'), json_extract(raw_payload_json, '$.note'), ''),
                 '$.note', '', '$.sources.results', json('[]')) END,
             updated_at = {NOW}
         WHERE event_id IN (SELECT attachment_event_id FROM attachment_sources
                            WHERE kind = 'result' AND source_id = {id})
           AND event_type = '{ATTACHMENT_EVENT_TYPE}' AND json_valid(raw_payload_json)
           AND (json_type(raw_payload_json, '$.encrypted_child_payload') IS NOT NULL
                OR (json_type(raw_payload_json, '$.note') = 'text'
                    AND json_extract(raw_payload_json, '$.note') <> ''
                    AND json_type(raw_payload_json, '$.tail') = 'text'))
           AND {FENCE_GUARD};"
    )
}

/// Both actions of a gone event id (the event is a quoted event or a quoted result).
fn event_gone_sql(id: &str) -> String {
    format!("{} {}", blank_prefix_sql("event", id), cut_note_sql(id))
}

fn triggers_sql() -> String {
    let insert_new = side_rows_sql(
        "NEW.event_id",
        "NEW.raw_payload_json",
        "",
        "NEW.event_type = 'message.attachment.recorded' AND",
    );
    let event_tombstoned = event_gone_sql("NEW.event_id");
    let event_deleted = event_gone_sql("OLD.event_id");
    let memory_deleted = blank_prefix_sql("memory", "OLD.id");
    let document_deleted = blank_prefix_sql("document", "OLD.document_id");
    format!(
        "CREATE TRIGGER IF NOT EXISTS attachment_sources_ai
         AFTER INSERT ON message_events WHEN NEW.event_type = '{ATTACHMENT_EVENT_TYPE}'
         BEGIN {insert_new} END;
         CREATE TRIGGER IF NOT EXISTS attachment_sources_au
         AFTER UPDATE OF event_type, raw_payload_json ON message_events
         WHEN OLD.event_type = '{ATTACHMENT_EVENT_TYPE}' OR NEW.event_type = '{ATTACHMENT_EVENT_TYPE}'
         BEGIN DELETE FROM attachment_sources WHERE attachment_event_id = OLD.event_id; {insert_new} END;
         CREATE TRIGGER IF NOT EXISTS attachment_source_event_tombstoned
         AFTER UPDATE OF event_type ON message_events
         WHEN NEW.event_type = 'message.deleted' AND OLD.event_type <> 'message.deleted'
         BEGIN {event_tombstoned} END;
         CREATE TRIGGER IF NOT EXISTS attachment_source_event_deleted
         AFTER DELETE ON message_events
         BEGIN {event_deleted}
               DELETE FROM attachment_sources WHERE attachment_event_id = OLD.event_id; END;
         CREATE TRIGGER IF NOT EXISTS attachment_source_memory_deleted
         AFTER DELETE ON memories BEGIN {memory_deleted} END;
         CREATE TRIGGER IF NOT EXISTS attachment_source_document_deleted
         AFTER DELETE ON documents BEGIN {document_deleted} END;"
    )
}

/// Create the side table and the triggers; on the first run of a database
/// without the table, mirror the existing attachment rows into it and blank
/// the rows that already quote gone content. Idempotent. Runs inside the
/// schema migration's write transaction (`schema.rs`), so it is one atomic
/// step; the triggers exist before the backfill so a blanked row refreshes
/// its own side rows.
pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    let existed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'attachment_sources')",
        [],
        |row| row.get(0),
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS attachment_sources (
             kind TEXT NOT NULL CHECK (kind IN ('memory', 'event', 'document', 'result')),
             source_id TEXT NOT NULL,
             attachment_event_id TEXT NOT NULL,
             PRIMARY KEY (kind, source_id, attachment_event_id)
         ) STRICT, WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS attachment_sources_by_attachment
             ON attachment_sources(attachment_event_id);",
    )?;
    conn.execute_batch(&triggers_sql())?;
    if existed {
        return Ok(());
    }
    let backfill = side_rows_sql(
        "m.event_id",
        "m.raw_payload_json",
        "message_events m,",
        &format!("m.event_type = '{ATTACHMENT_EVENT_TYPE}' AND"),
    );
    conn.execute_batch(&backfill)?;
    // Legacy rows without provenance: the prefix is never replayed, so the
    // text is dropped once instead of being matched by probes.
    let legacy = blank_payload_sql(SOURCES_EMPTY_OBJECT);
    conn.execute_batch(&format!(
        "UPDATE message_events SET raw_payload_json = {legacy}, updated_at = {NOW}
         WHERE event_type = '{ATTACHMENT_EVENT_TYPE}' AND json_valid(raw_payload_json)
           AND json_type(raw_payload_json, '$.sources') IS NULL
           AND COALESCE(json_extract(raw_payload_json, '$.prefix'), '') <> ''
           AND {FENCE_GUARD};"
    ))?;
    // Rows that quote content which is already gone.
    let stale = blank_payload_sql(SOURCES_EMPTY_LISTS);
    conn.execute_batch(&format!(
        "UPDATE message_events SET raw_payload_json = {stale}, updated_at = {NOW}
         WHERE event_type = '{ATTACHMENT_EVENT_TYPE}' AND json_valid(raw_payload_json)
           AND {FENCE_GUARD}
           AND event_id IN (
               SELECT s.attachment_event_id FROM attachment_sources s
               WHERE (s.kind = 'memory' AND NOT EXISTS (SELECT 1 FROM memories x WHERE x.id = s.source_id))
                  OR (s.kind = 'document' AND NOT EXISTS (SELECT 1 FROM documents x WHERE x.document_id = s.source_id))
                  OR (s.kind = 'event' AND NOT EXISTS (
                        SELECT 1 FROM message_events x WHERE x.event_id = s.source_id AND x.event_type <> 'message.deleted')));"
    ))?;
    Ok(())
}

/// The longest probe of a deleted row, in characters.
const PROBE_CHARS: usize = 64;

/// The shortest text a legacy match is made on, in characters.
const MIN_PROBE_CHARS: usize = 12;

/// The text a recall line quotes of a deleted row, or `None` when it is too
/// short to match on without blanking unrelated attachments.
fn probe(content: &str) -> Option<String> {
    let probe: String = content
        .trim()
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(PROBE_CHARS)
        .collect();
    (probe.trim().chars().count() >= MIN_PROBE_CHARS).then_some(probe)
}

/// Whether any of `ids` (a list of strings in `payload[key]`) satisfies `gone`.
fn any_gone(
    sources: &serde_json::Value,
    key: &str,
    mut gone: impl FnMut(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    for id in sources
        .get(key)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = id.as_str()
            && gone(id)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Rewrite the attachments of `session_key` that quote deleted content;
/// `deleted_texts` are the contents of the conversational rows this delete
/// tombstones (for the text match of rows without provenance; empty when the
/// call only re-checks provenance, e.g. after a forget). Returns the number
/// of rows rewritten.
pub(super) fn blank_stale_attachments(
    tx: &Connection,
    workspace_id: &str,
    owner_id: &str,
    session_key: &str,
    now: &str,
    deleted_texts: &[&str],
) -> anyhow::Result<usize> {
    let probes: Vec<String> = deleted_texts.iter().filter_map(|text| probe(text)).collect();
    let rows = {
        let mut statement = tx.prepare(
            "SELECT event_id, raw_payload_json FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND event_type = ?4
               AND json_valid(raw_payload_json)
               AND COALESCE(json_extract(raw_payload_json, '$.prefix'), '') <> ''",
        )?;
        statement
            .query_map(
                params![workspace_id, owner_id, session_key, ATTACHMENT_EVENT_TYPE],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut rewritten = 0;
    for (event_id, payload_json) in rows {
        let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(&payload_json) else {
            continue;
        };
        let stale = match payload.get("sources") {
            Some(sources) => {
                any_gone(sources, "events", |id| {
                    Ok(tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM message_events
                         WHERE workspace_id = ?1 AND event_id = ?2 AND event_type = 'message.deleted')",
                        params![workspace_id, id],
                        |row| row.get(0),
                    )?)
                })? || any_gone(sources, "memories", |id| {
                    Ok(!tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
                        params![id],
                        |row| row.get::<_, bool>(0),
                    )?)
                })? || any_gone(sources, "documents", |id| {
                    Ok(!tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM documents WHERE document_id = ?1)",
                        params![id],
                        |row| row.get::<_, bool>(0),
                    )?)
                })?
            }
            None => {
                let prefix = payload
                    .get("prefix")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                probes.iter().any(|probe| prefix.contains(probe.as_str()))
            }
        };
        if !stale {
            continue;
        }
        if let Some(object) = payload.as_object_mut() {
            object.insert("prefix".to_owned(), serde_json::Value::String(String::new()));
            object.insert(
                "sources".to_owned(),
                serde_json::json!({ "memories": [], "events": [], "documents": [] }),
            );
            object.insert("blanked".to_owned(), serde_json::Value::Bool(true));
        }
        tx.execute(
            "UPDATE message_events SET raw_payload_json = ?3, updated_at = ?4
             WHERE workspace_id = ?1 AND event_id = ?2",
            params![workspace_id, event_id, payload.to_string(), now],
        )?;
        rewritten += 1;
    }
    Ok(rewritten)
}
