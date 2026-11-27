//! Sweeps of derived copies that name erased data. A tool result of a
//! session-listing tool prints run ids and session keys; once the session or
//! run is erased the printed text is replaced, in the erasing transaction. A
//! tool result of a memory tool prints the key of each memory it quotes; once
//! the memory is forgotten the printed text is replaced the same way.
use super::rf_forget;
use rusqlite::{Connection, params, types::Value};

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

/// Tool results of the memory tools that quote a memory's text.
const MEMORY_TOOLS: [&str; 3] = ["memory_recall", "memory_search", "memory_get"];
/// Tools whose call arguments carry a memory's key (`key`) and text
/// (`content`), `tools/memory_store.rs`; it is the only memory write tool.
const MEMORY_WRITE_TOOLS: [&str; 1] = ["memory_store"];
pub(super) const MEMORY_REMOVED_TEXT: &str = "[output removed: it quoted a memory that was forgotten]";

const TOOL_OUTPUT_KINDS: &str = "('tool_output', 'untrusted_external')";
const UNTRUSTED_PREFIX: &str = "untrusted:";

/// What an erase removed, for the listing sweep.
pub(super) struct ErasedIdentifiers {
    /// Run ids of the erased rows; a listing that names one was written after
    /// `since_id`.
    pub(super) run_ids: Vec<String>,
    /// Smallest row id of the erased events.
    pub(super) since_id: i64,
    /// Session keys with no surviving row. Listings naming them are matched
    /// from the first row: an earlier erase may have removed the session's
    /// older rows while it still survived.
    pub(super) swept_sessions: Vec<String>,
}

/// One tool-result row selected for rewriting.
struct MatchedRow {
    event_id: String,
    document_id: Option<String>,
    /// The call the result answers, when the row names it.
    call: Option<CallRef>,
}

/// Replace the printed text of listing-tool results in `workspace_id` that
/// name an erased run (newer than `since_id`) or a swept session (any row),
/// then delete the stored tool-output documents that only those results
/// named. Returns the number of rows rewritten. A row stays (the tool call and
/// result pair must stay valid for replay); a row with an unparseable payload
/// or non-text content, a row of a fenced scope and a row of another tool are
/// left as they are.
pub(super) fn rewrite_listings(
    tx: &Connection,
    workspace_id: &str,
    erased: &ErasedIdentifiers,
    now: &str,
) -> anyhow::Result<usize> {
    let mut rows = select_listing_rows(tx, workspace_id, &usable(&erased.run_ids), erased.since_id)?;
    rows.extend(select_listing_rows(
        tx,
        workspace_id,
        &usable(&erased.swept_sessions),
        0,
    )?);
    rows.sort_unstable_by(|left, right| left.event_id.cmp(&right.event_id));
    rows.dedup_by(|left, right| left.event_id == right.event_id);
    if rows.is_empty() {
        return Ok(0);
    }
    let documents = rewrite_rows(tx, &rows, LISTING_REMOVED_TEXT, now)?;
    delete_unquoted_documents(tx, &documents)?;
    let calls: Vec<CallRef> = rows.iter().filter_map(|row| row.call.clone()).collect();
    rewrite_call_terminals(tx, &calls, LISTING_REMOVED_TEXT, now)?;
    Ok(rows.len())
}

/// The identifiers long enough to match safely, sorted and without repeats.
fn usable(identifiers: &[String]) -> Vec<&str> {
    let mut usable: Vec<&str> = identifiers
        .iter()
        .map(String::as_str)
        .filter(|id| id.chars().count() >= MIN_IDENTIFIER_CHARS)
        .collect();
    usable.sort_unstable();
    usable.dedup();
    usable
}

fn select_listing_rows(
    tx: &Connection,
    workspace_id: &str,
    identifiers: &[&str],
    since_id: i64,
) -> anyhow::Result<Vec<MatchedRow>> {
    if identifiers.is_empty() {
        return Ok(Vec::new());
    }
    let identifiers_json = serde_json::to_string(identifiers)?;
    let tools = serde_json::to_string(&LISTING_TOOLS)?;
    let mut statement = tx.prepare(
        "SELECT event_id, json_extract(raw_payload_json, '$.document.document_id'), content,
                workspace_id, run_id, json_extract(raw_payload_json, '$.call_id')
         FROM message_events
         WHERE workspace_id = ?1 AND event_type = 'tool.result.recorded' AND id > ?2
           AND json_valid(raw_payload_json)
           AND json_extract(raw_payload_json, '$.tool') IN (SELECT value FROM json_each(?4))
           AND EXISTS (SELECT 1 FROM json_each(?3) j WHERE instr(content, j.value) > 0)
           AND NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = message_events.ma_scope_id)",
    )?;
    let rows = statement
        .query_map(params![workspace_id, since_id, identifiers_json, tools], |row| {
            let event_id: rusqlite::types::Value = row.get(0)?;
            let document_id: rusqlite::types::Value = row.get(1)?;
            let content: rusqlite::types::Value = row.get(2)?;
            let workspace: rusqlite::types::Value = row.get(3)?;
            let run_id: rusqlite::types::Value = row.get(4)?;
            let call_id: rusqlite::types::Value = row.get(5)?;
            Ok((event_id, document_id, content, workspace, run_id, call_id))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(event_id, document_id, content, workspace, run_id, call_id)| {
            let (Value::Text(event_id), Value::Text(content)) = (event_id, content) else {
                return None;
            };
            identifiers
                .iter()
                .any(|id| names_identifier(&content, id))
                .then(|| MatchedRow {
                    event_id,
                    document_id: text_of(document_id),
                    call: call_ref(workspace, run_id, call_id),
                })
        })
        .collect())
}

/// Characters an identifier is made of.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-')
}

/// Characters that continue an identifier after a `.` or `:`.
fn continues_after_separator(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// True when `rest`, the text after a match, ends the identifier: the end of
/// the text, a character outside the identifier set, or a `.` or `:` that
/// closes a sentence or a label (`<id>.` at the end, `<id>: requested=...`).
fn ends_identifier(rest: &str) -> bool {
    let mut chars = rest.chars();
    match chars.next() {
        None => true,
        Some('.' | ':') => chars.next().is_none_or(|next| !continues_after_separator(next)),
        Some(next) => !is_identifier_char(next),
    }
}

/// True when `text` holds `identifier` at a right boundary, so
/// `child-segment:P:1` matches neither `child-segment:P:12` nor
/// `child-segment:P:1:2`. Every occurrence is tried, overlapping ones too.
fn names_identifier(text: &str, identifier: &str) -> bool {
    if identifier.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(found) = text.get(from..).and_then(|tail| tail.find(identifier)) {
        let start = from + found;
        if text.get(start + identifier.len()..).is_some_and(ends_identifier) {
            return true;
        }
        // Step one character past the start of this occurrence.
        from = start
            + text
                .get(start..)
                .and_then(|tail| tail.chars().next())
                .map_or(1, char::len_utf8);
    }
    false
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

/// What a forgotten memory leaves to look for in tool results: its key, in the
/// workspace and of the owner it was stored for.
pub(super) struct MemoryProbe {
    /// The memory's workspace; `None` matches results of every workspace of
    /// the owner.
    workspace_id: Option<String>,
    /// The memory's owner; `None` matches results of every owner of the
    /// workspace.
    owner_id: Option<String>,
    key: String,
    /// `redact_secrets(key)` when redaction changes the key: the stored copy of
    /// a result prints the key in that form.
    key_redacted: Option<String>,
    /// True for a memory with neither workspace nor owner: its copies are
    /// rewritten only when no surviving memory of the key is visible to the row.
    unscoped: bool,
    /// Creation time of the memory in UTC, cut to whole seconds (a prefix of
    /// every later UTC timestamp of the same second); empty when it does not
    /// parse, which widens the time bound to every row. No result older than
    /// it can quote the memory.
    created_utc: String,
}

impl MemoryProbe {
    /// The probe of a memory; `None` for an empty key.
    fn new(workspace_id: Option<String>, owner_id: Option<String>, key: String, created_at: &str) -> Option<Self> {
        if key.is_empty() {
            return None;
        }
        let redacted = crate::security::redaction::redact_secrets(&key);
        let key_redacted = (redacted != key && !redacted.is_empty()).then_some(redacted);
        let unscoped = workspace_id.is_none() && owner_id.is_none();
        let created_utc = chrono::DateTime::parse_from_rfc3339(created_at)
            .map(|time| time.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();
        Some(Self {
            workspace_id,
            owner_id,
            key,
            key_redacted,
            unscoped,
            created_utc,
        })
    }

    /// True when a result row of `workspace` and `owner` may quote this memory.
    fn covers(&self, workspace: Option<&str>, owner: Option<&str>) -> bool {
        self.workspace_id.as_deref().is_none_or(|own| workspace == Some(own))
            && self.owner_id.as_deref().is_none_or(|own| owner == Some(own))
    }

    /// True when a row of `workspace` and `owner` that names this memory
    /// should be rewritten: always for a scoped probe; for an unscoped one only
    /// when no memory of the key survives that the row's owner can see (the
    /// output can then only have quoted the forgotten one). Call after the
    /// memory is deleted.
    fn applies(&self, tx: &Connection, workspace: Option<&str>, owner: Option<&str>) -> anyhow::Result<bool> {
        if !self.unscoped {
            return Ok(true);
        }
        let survives: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM memories WHERE key = ?1
                 AND (workspace_id IS NULL OR workspace_id = ?2) AND (owner_id IS NULL OR owner_id = ?3))",
            params![self.key, workspace, owner],
            |row| row.get(0),
        )?;
        Ok(!survives)
    }

    /// True when `output` of `tool` holds an entry of this memory, by its key
    /// or by its redacted form.
    fn named_in(&self, tool: &str, output: &str) -> bool {
        named_key(&self.key, tool, output)
            || self
                .key_redacted
                .as_deref()
                .is_some_and(|key| named_key(key, tool, output))
    }
}

/// True when the printed `output` of `tool` holds a memory entry of `key` in
/// the form the tool prints it:
/// - `memory_recall` (`tools/memory_recall.rs`): `- [category] key: content`;
/// - `memory_search` (`tools/memory_search.rs`): a line `- key: key`;
/// - `memory_get` (`tools/memory_get.rs`, `render_range`): the output starts
///   with `key lines a-b:` (or `key is empty.`).
fn named_key(key: &str, tool: &str, output: &str) -> bool {
    match tool {
        "memory_recall" => {
            let entry = format!("] {key}: ");
            let bare = format!("] {key}:");
            output
                .lines()
                .any(|line| line.starts_with("- [") && (line.contains(&entry) || line.ends_with(&bare)))
        }
        "memory_search" => output.lines().any(|line| line.strip_prefix("- key: ") == Some(key)),
        "memory_get" => output.strip_prefix(key).is_some_and(|rest| {
            rest.starts_with(" lines ") || rest == " is empty." || rest.starts_with(" is empty.\n")
        }),
        _ => false,
    }
}

/// The probes of the memories selected by `memories_sql`, a constant statement
/// selecting `workspace_id, owner_id, key, created_at` with bound `values`.
/// Call before the memories are deleted. A row whose key or creation time is
/// not text is skipped.
pub(super) fn memory_probes(
    conn: &Connection,
    memories_sql: &str,
    values: impl rusqlite::Params,
) -> anyhow::Result<Vec<MemoryProbe>> {
    let mut statement = conn.prepare(memories_sql)?;
    let rows = statement
        .query_map(values, |row| {
            Ok((
                row.get::<_, Value>(0)?,
                row.get::<_, Value>(1)?,
                row.get::<_, Value>(2)?,
                row.get::<_, Value>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let text = |value: Value| match value {
        Value::Text(text) => Some(text),
        _ => None,
    };
    Ok(rows
        .into_iter()
        .filter_map(|(workspace, owner, key, created_at)| {
            let (Value::Text(key), Value::Text(created_at)) = (key, created_at) else {
                return None;
            };
            MemoryProbe::new(text(workspace), text(owner), key, &created_at)
        })
        .collect())
}

/// Replace the printed text of memory-tool results that name a forgotten
/// memory, in one scan for all `probes`, then delete the stored tool-output
/// documents that only those results named. A result is rewritten when it was
/// recorded no earlier than the first row created at the oldest probe's time,
/// lies in the probe's workspace and owner, and prints the memory's key in the
/// tool's own entry form (see [`MemoryProbe::named_in`]). A large output keeps
/// its first part inline, so an entry printed past that part is not seen; the
/// row is rewritten only when an entry it keeps names the key. The same
/// transaction also rewrites the other rows that hold the memory's text by
/// key: the execution terminal of each rewritten call (see
/// [`rewrite_call_terminals`]) and the write calls that stored it (see
/// [`rewrite_write_calls`]). Returns the number of rows rewritten. Not for age
/// expiry: hygiene and retention do not call it.
pub(super) fn rewrite_memory_copies(tx: &Connection, probes: &[MemoryProbe], now: &str) -> anyhow::Result<usize> {
    let Some(oldest) = probes.iter().map(|probe| probe.created_utc.as_str()).min() else {
        return Ok(0);
    };
    let wanted: Vec<serde_json::Value> = probes
        .iter()
        .map(|probe| {
            serde_json::json!({
                "workspace": probe.workspace_id,
                "owner": probe.owner_id,
                "key": probe.key,
                "key_redacted": probe.key_redacted,
            })
        })
        .collect();
    let probes_json = serde_json::to_string(&wanted)?;
    let tools = serde_json::to_string(&MEMORY_TOOLS)?;
    let mut statement = tx.prepare(
        "SELECT event_id, json_extract(raw_payload_json, '$.document.document_id'), content,
                json_extract(raw_payload_json, '$.tool'), workspace_id, owner_id, run_id,
                json_extract(raw_payload_json, '$.call_id')
         FROM message_events
         WHERE event_type = 'tool.result.recorded'
           AND id >= COALESCE((SELECT MIN(id) FROM message_events WHERE created_at >= ?1), 9223372036854775807)
           AND typeof(content) = 'text' AND json_valid(raw_payload_json)
           AND json_extract(raw_payload_json, '$.tool') IN (SELECT value FROM json_each(?3))
           AND EXISTS (SELECT 1 FROM json_each(?2) j
                       WHERE (instr(message_events.content, json_extract(j.value, '$.key')) > 0
                              OR instr(message_events.content, COALESCE(json_extract(j.value, '$.key_redacted'), '\u{1}')) > 0
                              OR json_type(message_events.raw_payload_json, '$.document.document_id') = 'text')
                         AND (json_extract(j.value, '$.workspace') IS NULL
                              OR message_events.workspace_id = json_extract(j.value, '$.workspace'))
                         AND (json_extract(j.value, '$.owner') IS NULL
                              OR message_events.owner_id = json_extract(j.value, '$.owner')))
           AND NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = message_events.ma_scope_id)",
    )?;
    let found = statement
        .query_map(params![oldest, probes_json, tools], |row| {
            Ok([
                row.get::<_, Value>(0)?,
                row.get::<_, Value>(1)?,
                row.get::<_, Value>(2)?,
                row.get::<_, Value>(3)?,
                row.get::<_, Value>(4)?,
                row.get::<_, Value>(5)?,
                row.get::<_, Value>(6)?,
                row.get::<_, Value>(7)?,
            ])
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut rows: Vec<MatchedRow> = Vec::new();
    for [event_id, document_id, content, tool, workspace, owner, run_id, call_id] in found {
        let (Value::Text(event_id), Value::Text(content), Value::Text(tool)) = (event_id, content, tool) else {
            continue;
        };
        let document_id = text_of(document_id);
        let (workspace_text, owner) = (text_of(workspace.clone()), text_of(owner));
        let mut document_text: Option<Option<String>> = None;
        let mut matched = false;
        for probe in probes {
            if !probe.covers(workspace_text.as_deref(), owner.as_deref()) {
                continue;
            }
            let names = probe.named_in(&tool, &content)
                || document_id.as_deref().is_some_and(|document| {
                    // The entry may sit past the inline part: read the stored
                    // document once per row, only when the inline text does not
                    // name the memory.
                    if document_text.is_none() {
                        document_text = Some(stored_document_text(tx, document));
                    }
                    document_text
                        .as_ref()
                        .and_then(Option::as_deref)
                        .is_some_and(|text| probe.named_in(&tool, text))
                });
            if names && probe.applies(tx, workspace_text.as_deref(), owner.as_deref())? {
                matched = true;
                break;
            }
        }
        if matched {
            rows.push(MatchedRow {
                event_id,
                document_id,
                call: call_ref(workspace, run_id, call_id),
            });
        }
    }
    let mut rewritten = 0usize;
    if !rows.is_empty() {
        let documents = rewrite_rows(tx, &rows, MEMORY_REMOVED_TEXT, now)?;
        delete_unquoted_documents(tx, &documents)?;
        rewritten = rows.len();
        let calls: Vec<CallRef> = rows.iter().filter_map(|row| row.call.clone()).collect();
        rewritten = rewritten.saturating_add(rewrite_call_terminals(tx, &calls, MEMORY_REMOVED_TEXT, now)?);
    }
    Ok(rewritten.saturating_add(rewrite_write_calls(tx, probes, now)?))
}

/// The text of a value, `None` for any other type.
fn text_of(value: Value) -> Option<String> {
    match value {
        Value::Text(text) => Some(text),
        _ => None,
    }
}

/// The stored tool-output document of a result, its chunks joined in order;
/// the `untrusted:` variant is read too. `None` when there is none or it
/// cannot be read: the row is then judged by its inline text only.
fn stored_document_text(tx: &Connection, document_id: &str) -> Option<String> {
    let mut statement = tx
        .prepare(
            "SELECT content FROM document_chunks
             WHERE document_id IN (?1, 'untrusted:' || ?1) ORDER BY document_id, chunk_index",
        )
        .ok()?;
    let chunks = statement
        .query_map(params![document_id], |row| row.get::<_, Value>(0))
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok()?;
    let texts: Vec<String> = chunks.into_iter().filter_map(text_of).collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// The call of a tool result: its workspace, run and call id; `None` without
/// a workspace and call id.
fn call_ref(workspace: Value, run_id: Value, call_id: Value) -> Option<CallRef> {
    Some(CallRef {
        workspace: text_of(workspace)?,
        run_id: text_of(run_id),
        call_id: text_of(call_id)?,
    })
}

/// The call of a rewritten tool result: its workspace, run and call id.
#[derive(Clone)]
struct CallRef {
    workspace: String,
    run_id: Option<String>,
    call_id: String,
}

/// Replace the model text of the execution terminal of each call
/// (`tool.execution.finalized`, written by `tools/execution/section_2.rs`
/// `commit_idempotent_execution`; its payload keeps the outcome with
/// `model_content` and the tool's `result.output`). The terminal is linked by
/// key: same workspace and run, and `outcome.operation_id` equal to the call
/// id (`agent/tool_scheduler.rs` uses the call id as operation id). A row with
/// an unparseable payload or no outcome object is left as it is.
fn rewrite_call_terminals(tx: &Connection, calls: &[CallRef], text: &str, now: &str) -> anyhow::Result<usize> {
    let mut rewritten = 0usize;
    for call in calls {
        let updated = tx.execute(
            "UPDATE message_events SET
                 raw_payload_json = CASE WHEN json_type(raw_payload_json, '$.outcome.result') = 'object'
                     THEN json_set(raw_payload_json, '$.outcome.model_content', ?4, '$.outcome.result.output', ?4)
                     ELSE json_set(raw_payload_json, '$.outcome.model_content', ?4) END,
                 updated_at = ?5
             WHERE workspace_id = ?1 AND event_type = 'tool.execution.finalized' AND run_id IS ?2
               AND json_valid(raw_payload_json) AND json_type(raw_payload_json, '$.outcome') = 'object'
               AND json_extract(raw_payload_json, '$.outcome.operation_id') = ?3",
            params![call.workspace, call.run_id, call.call_id, text, now],
        )?;
        rewritten = rewritten.saturating_add(updated);
    }
    Ok(rewritten)
}

/// Replace the stored text in the recorded calls of the memory write tools
/// whose arguments name a forgotten key (`arguments.key`), in the probe's
/// workspace and owner: `arguments.content` becomes the removed-text marker
/// and the row's content (`<tool> <arguments>`, as `context/transcript/tool_rows.rs`
/// writes it) is derived again from the rewritten arguments. The call stays a
/// valid call for replay. No time bound: the call is recorded before the
/// memory row exists, and every value stored under the key belongs to the
/// forgotten memory. A row with an unparseable payload, non-object arguments
/// or no `content` argument is left as it is; a row of a fenced scope too.
fn rewrite_write_calls(tx: &Connection, probes: &[MemoryProbe], now: &str) -> anyhow::Result<usize> {
    let mut keys: Vec<&str> = probes.iter().map(|probe| probe.key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    let mut statement = tx.prepare(
        "SELECT event_id, workspace_id, owner_id, json_extract(raw_payload_json, '$.arguments.key')
         FROM message_events
         WHERE event_type = 'tool.call.recorded' AND json_valid(raw_payload_json)
           AND json_extract(raw_payload_json, '$.tool') IN (SELECT value FROM json_each(?1))
           AND json_type(raw_payload_json, '$.arguments.content') IS NOT NULL
           AND json_extract(raw_payload_json, '$.arguments.key') IN (SELECT value FROM json_each(?2))
           AND NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = message_events.ma_scope_id)",
    )?;
    let found = statement
        .query_map(
            params![
                serde_json::to_string(&MEMORY_WRITE_TOOLS)?,
                serde_json::to_string(&keys)?
            ],
            |row| {
                Ok((
                    row.get::<_, Value>(0)?,
                    row.get::<_, Value>(1)?,
                    row.get::<_, Value>(2)?,
                    row.get::<_, Value>(3)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut rewritten = 0usize;
    for (event_id, workspace, owner, key) in found {
        let (Value::Text(event_id), Value::Text(key)) = (event_id, key) else {
            continue;
        };
        let (workspace, owner) = (text_of(workspace), text_of(owner));
        let mut applies = false;
        for probe in probes {
            if key == probe.key
                && probe.covers(workspace.as_deref(), owner.as_deref())
                && probe.applies(tx, workspace.as_deref(), owner.as_deref())?
            {
                applies = true;
                break;
            }
        }
        if !applies {
            continue;
        }
        let updated = tx.execute(
            "UPDATE message_events SET
                 raw_payload_json = json_set(raw_payload_json, '$.arguments.content', ?2),
                 content = json_extract(raw_payload_json, '$.tool') || ' '
                     || json_extract(json_set(raw_payload_json, '$.arguments.content', ?2), '$.arguments'),
                 content_hash = NULL, updated_at = ?3
             WHERE event_id = ?1 AND event_type = 'tool.call.recorded' AND json_valid(raw_payload_json)
               AND json_type(raw_payload_json, '$.arguments') = 'object'
               AND typeof(json_extract(raw_payload_json, '$.tool')) = 'text'",
            params![event_id, MEMORY_REMOVED_TEXT, now],
        )?;
        rewritten = rewritten.saturating_add(updated);
    }
    Ok(rewritten)
}
