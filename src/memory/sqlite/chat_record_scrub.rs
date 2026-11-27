//! One-time scrub of saved chat records (`chat_record_v1`).
//!
//! A saved terminal chat record (`memories`, key `chat_session:<id>`) used to
//! keep two copies of memory text: `turns[].tool_evidence.sent.prefix` (the
//! recalled memory context, never replayed from a saved record) and the
//! `content` of `memory_store` tool-call `args_preview`. New saves no longer
//! write them; this pass removes them from records written before. It runs
//! once, inside the schema migration transaction, and a marker row records
//! that it ran. Records that are not valid JSON (for example sealed records of
//! an encrypted scope) are left alone. The text index follows through the
//! `memories` triggers.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

const MARKER: &str = "chat_record_v1";
const MEMORY_TEXT_NOT_KEPT: &str = "[memory text not kept]";

/// Idempotent; called from the schema migration, which holds the write
/// transaction.
pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS derived_copy_scrubs (
            name TEXT PRIMARY KEY,
            done_at TEXT NOT NULL
        );",
    )?;
    let done: Option<String> = conn
        .query_row(
            "SELECT name FROM derived_copy_scrubs WHERE name = ?1",
            params![MARKER],
            |row| row.get(0),
        )
        .optional()?;
    if done.is_some() {
        return Ok(());
    }
    let records: Vec<(String, String)> = {
        let mut statement = conn.prepare(
            "SELECT key, content FROM memories
             WHERE key LIKE 'chat_session:%' AND json_valid(content)",
        )?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut rewritten = 0_u64;
    for (key, content) in records {
        let Ok(mut record) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        if !scrub_record(&mut record) {
            continue;
        }
        let Ok(updated) = serde_json::to_string(&record) else {
            continue;
        };
        conn.execute("UPDATE memories SET content = ?1 WHERE key = ?2", params![updated, key])?;
        rewritten += 1;
    }
    conn.execute(
        "INSERT INTO derived_copy_scrubs(name, done_at) VALUES(?1, ?2)",
        params![MARKER, chrono::Local::now().to_rfc3339()],
    )?;
    tracing::info!(rewritten, "chat record scrub done");
    Ok(())
}

/// Edit one record in place; true when something changed.
fn scrub_record(record: &mut Value) -> bool {
    let Some(turns) = record.get_mut("turns").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for turn in turns {
        if let Some(prefix) = turn.pointer_mut("/tool_evidence/sent/prefix")
            && prefix.as_str().is_some_and(|text| !text.is_empty())
        {
            *prefix = Value::String(String::new());
            changed = true;
        }
        let Some(calls) = turn.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for call in calls {
            if call.get("name").and_then(Value::as_str) != Some("memory_store") {
                continue;
            }
            let Some(preview) = call.get_mut("args_preview") else {
                continue;
            };
            let Some(text) = preview.as_str() else {
                continue;
            };
            let replacement = match serde_json::from_str::<Value>(text) {
                Ok(Value::Object(mut object)) => {
                    match object.get_mut("content") {
                        Some(content) if content.as_str() != Some(MEMORY_TEXT_NOT_KEPT) => {
                            *content = Value::String(MEMORY_TEXT_NOT_KEPT.to_string());
                        }
                        _ => continue,
                    }
                    serde_json::to_string(&Value::Object(object)).unwrap_or_else(|_| MEMORY_TEXT_NOT_KEPT.to_string())
                }
                _ => {
                    if text == MEMORY_TEXT_NOT_KEPT {
                        continue;
                    }
                    MEMORY_TEXT_NOT_KEPT.to_string()
                }
            };
            *preview = Value::String(replacement);
            changed = true;
        }
    }
    changed
}
