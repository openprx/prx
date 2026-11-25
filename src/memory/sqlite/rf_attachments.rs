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
    for id in sources.get(key).and_then(serde_json::Value::as_array).into_iter().flatten() {
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
            .query_map(params![workspace_id, owner_id, session_key, ATTACHMENT_EVENT_TYPE], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
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
                    Ok(!tx.query_row("SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)", params![id], |row| {
                        row.get::<_, bool>(0)
                    })?)
                })? || any_gone(sources, "documents", |id| {
                    Ok(!tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM documents WHERE document_id = ?1)",
                        params![id],
                        |row| row.get::<_, bool>(0),
                    )?)
                })?
            }
            None => {
                let prefix = payload.get("prefix").and_then(serde_json::Value::as_str).unwrap_or_default();
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
