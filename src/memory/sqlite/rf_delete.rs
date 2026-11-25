//! SQL of rf message and reply deletes (AC-1b2b, `message_delete_v1`).
//!
//! A delete tombstones rows: a `message_events` row keeps its id, its
//! idempotency key, its agent stamp (`ma_scope_id`, `agent_id`) and its
//! timestamps; its content, content hash, payload and references are
//! removed, its type becomes `message.deleted` and its role `event`, so no
//! reader of conversation rows selects it again. A `turn.finalized` or
//! admission-failure row keeps only the terminal `status` the replay and
//! cancel answers read. A `conversation_turns` row keeps its id, message id,
//! agent stamp and timestamp, and gets `deleted_at`. Copies derived from the
//! deleted rows go in the same transaction: embedding-cache entries keyed by
//! their text, retrieval traces of the run, memories, drafts and links that
//! name a deleted event as their source, the stored tool-output documents of
//! the run (unless a tool result of a surviving run still quotes them), and
//! compaction summaries and handoff
//! notes built from a deleted event. The attempt rows keep their key and get
//! the deleted mark; a message delete also clears the request digest, the
//! only derivative of the message the record held.
//!
//! The rows of a run whose job is still live in this process are not
//! rewritten under it (its terminal commit re-reads its own input and event
//! rows): the delete marks the attempt `erase_pending`, tombstones the
//! conversation rows at once, and [`settle`] rewrites the event rows once the
//! job has ended.

use super::SqliteMemory;
use super::rf_attachments;
use super::rf_forget::{self, ForgetState, Partition};
use crate::memory::rf_messages::{RfDeletedAgent, RfForget, RfForgetCounts, RfMessageDelete, RfReplyDelete};
use crate::runtime::sqlite_pool::begin_immediate;
use rusqlite::{Connection, OptionalExtension, params};

/// Idempotent DDL, run after the attempt table exists.
pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    // One content-free row while deleted content may still sit in WAL frames.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS rf_wal_scrub_pending (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            since TEXT NOT NULL
        ) STRICT;",
    )?;
    for (table, column, definition) in [
        ("rf_message_attempts", "deleted_at", "TEXT"),
        ("rf_message_attempts", "reply_deleted_at", "TEXT"),
        (
            "rf_message_attempts",
            "erase_pending",
            "INTEGER NOT NULL DEFAULT 0 CHECK (erase_pending IN (0, 1))",
        ),
        ("conversation_turns", "deleted_at", "TEXT"),
    ] {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            params![table, column],
            |row| row.get(0),
        )?;
        if !present {
            // Literals of this module only; no request data reaches the DDL.
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
        }
    }
    Ok(())
}

/// Which part of an attempt a delete removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The input and every row of its run.
    Message,
    /// The run's rows except the input and its ingress evidence.
    Reply,
}

impl Side {
    const fn label(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Reply => "reply",
        }
    }
}

/// One attempt a delete acts on.
struct Attempt {
    agent_key: String,
    run_id: Option<String>,
    job_id: Option<String>,
    input_event_id: String,
    scope: Option<String>,
    forget: ForgetState,
}

impl Attempt {
    /// The columns [`Attempt::from_row`] reads, in its order.
    const COLUMNS: &'static str = "agent_key, run_id, job_id, message_event_id, ma_scope_id, forget_requested,
         forgot_memories_created, forgot_memories_touched, forgot_documents_created, forgot_documents_touched";

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            agent_key: row.get(0)?,
            run_id: row.get(1)?,
            job_id: row.get(2)?,
            input_event_id: row.get(3)?,
            scope: row.get(4)?,
            forget: rf_forget::state_at(row, 5)?,
        })
    }

    /// The partition the attempt's forget acts in.
    fn partition(&self) -> Partition<'_> {
        Partition {
            scope: self.scope.as_deref(),
            main: self.agent_key == "main",
        }
    }
}

/// The identity every statement of one delete shares.
struct Target<'a> {
    workspace_id: &'a str,
    owner_id: &'a str,
    session_key: &'a str,
    now: &'a str,
}

/// Delete one message for one agent or every agent, in one transaction.
/// No attempt of the message: nothing is written and the list is empty.
pub(super) fn delete_message(conn: &Connection, request: &RfMessageDelete) -> anyhow::Result<Vec<RfDeletedAgent>> {
    let now = chrono::Utc::now().to_rfc3339();
    let target = Target {
        workspace_id: &request.workspace_id,
        owner_id: &request.owner_id,
        session_key: &request.session_key,
        now: &now,
    };
    let tx = begin_immediate(conn)?;
    let attempts = {
        let mut statement = tx.prepare(&format!(
            "SELECT {} FROM rf_message_attempts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND client_message_id = ?4
               AND (?5 IS NULL OR agent_key = ?5)
             ORDER BY rowid",
            Attempt::COLUMNS
        ))?;
        statement
            .query_map(
                params![
                    request.workspace_id,
                    request.owner_id,
                    request.session_key,
                    request.client_message_id,
                    request.agent_key
                ],
                Attempt::from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if attempts.is_empty() {
        return Ok(Vec::new());
    }
    tx.execute(
        "UPDATE rf_message_attempts SET deleted_at = COALESCE(deleted_at, ?6), request_hash = ''
         WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND client_message_id = ?4
           AND (?5 IS NULL OR agent_key = ?5)",
        params![
            request.workspace_id,
            request.owner_id,
            request.session_key,
            request.client_message_id,
            request.agent_key,
            now
        ],
    )?;
    let mut agents: Vec<RfDeletedAgent> = Vec::new();
    for attempt in &attempts {
        let live = attempt
            .run_id
            .as_ref()
            .is_some_and(|run| request.live_runs.contains(run));
        // Before the erase, so a memory the erase would remove by its source
        // event is counted as forgotten.
        let forget = if request.forget_writes {
            Some(forget_attempt(&tx, &target, attempt, live)?)
        } else {
            None
        };
        if live {
            let mut ids = run_event_ids(&tx, &target, attempt.run_id.as_deref())?;
            ids.push(attempt.input_event_id.clone());
            tombstone_turns(&tx, &ids, &now)?;
            tx.execute(
                "UPDATE rf_message_attempts SET erase_pending = 1 WHERE workspace_id = ?1 AND run_id = ?2",
                params![request.workspace_id, attempt.run_id],
            )?;
        } else {
            erase(&tx, &target, Side::Message, attempt)?;
        }
        record_deleted(&tx, &target, Side::Message, attempt, &request.client_message_id)?;
        if !agents.iter().any(|agent| agent.agent_key == attempt.agent_key) {
            agents.push(RfDeletedAgent {
                agent_key: attempt.agent_key.clone(),
                runs: Vec::new(),
                jobs: Vec::new(),
                deferred: Vec::new(),
                forget: None,
            });
        }
        if let Some(agent) = agents.iter_mut().find(|agent| agent.agent_key == attempt.agent_key) {
            agent.runs.extend(attempt.run_id.iter().cloned());
            agent.jobs.extend(attempt.job_id.iter().cloned());
            if live {
                agent.deferred.extend(attempt.run_id.iter().cloned());
            }
            if let Some(forget) = forget {
                agent.forget = Some(agent.forget.map_or(forget, |earlier| earlier.merge(forget)));
            }
        }
    }
    if request.forget_writes {
        // A forget after a plain delete finds no row left to erase; the
        // attachments that quoted the forgotten memories are still checked.
        blank_attachments(&tx, &target, &[])?;
    }
    refresh_preview(&tx, &request.session_key)?;
    mark_scrub(&tx, &now)?;
    tx.commit()?;
    Ok(agents)
}

/// Delete the assistant side of one run, in one transaction.
pub(super) fn delete_reply(
    conn: &Connection,
    workspace_id: &str,
    owner_id: &str,
    session_key: &str,
    run_id: &str,
    forget_writes: bool,
) -> anyhow::Result<RfReplyDelete> {
    let now = chrono::Utc::now().to_rfc3339();
    let target = Target {
        workspace_id,
        owner_id,
        session_key,
        now: &now,
    };
    let tx = begin_immediate(conn)?;
    let row = tx
        .query_row(
            "SELECT agent_key, client_message_id, job_id, message_event_id, ma_scope_id, interrupted,
                    deleted_at IS NOT NULL OR reply_deleted_at IS NOT NULL, forget_requested,
                    forgot_memories_created, forgot_memories_touched, forgot_documents_created,
                    forgot_documents_touched
             FROM rf_message_attempts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND run_id = ?4",
            params![workspace_id, owner_id, session_key, run_id],
            |row| {
                Ok((
                    Attempt {
                        agent_key: row.get(0)?,
                        run_id: Some(run_id.to_owned()),
                        job_id: row.get(2)?,
                        input_event_id: row.get(3)?,
                        scope: row.get(4)?,
                        forget: rf_forget::state_at(row, 7)?,
                    },
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((attempt, client_message_id, interrupted, deleted)) = row else {
        return Ok(RfReplyDelete::Unknown);
    };
    let job_id = attempt.job_id.clone().unwrap_or_default();
    if deleted {
        if !forget_writes {
            return Ok(RfReplyDelete::Deleted { job_id, forget: None });
        }
        // A replay answers the recorded counts; a first forget on an
        // already deleted run runs now (its log rows are still there).
        let forget = forget_attempt(&tx, &target, &attempt, false)?;
        blank_attachments(&tx, &target, &[])?;
        if attempt.forget.recorded.is_none() {
            mark_scrub(&tx, &now)?;
        }
        tx.commit()?;
        return Ok(RfReplyDelete::Deleted {
            job_id,
            forget: Some(forget),
        });
    }
    if !interrupted && !super::rf_messages::has_terminal(&tx, workspace_id, owner_id, run_id)? {
        return Ok(RfReplyDelete::Active);
    }
    let forget = if forget_writes {
        Some(forget_attempt(&tx, &target, &attempt, false)?)
    } else {
        None
    };
    tx.execute(
        "UPDATE rf_message_attempts SET reply_deleted_at = ?3 WHERE workspace_id = ?1 AND run_id = ?2",
        params![workspace_id, run_id, now],
    )?;
    erase(&tx, &target, Side::Reply, &attempt)?;
    record_deleted(&tx, &target, Side::Reply, &attempt, &client_message_id)?;
    refresh_preview(&tx, session_key)?;
    mark_scrub(&tx, &now)?;
    tx.commit()?;
    Ok(RfReplyDelete::Deleted { job_id, forget })
}

/// Rewrite the event rows of every deleted run of the session whose job is
/// no longer live (`live_runs` are the runs still live in this process).
pub(super) fn settle(
    conn: &Connection,
    workspace_id: &str,
    owner_id: &str,
    session_key: &str,
    live_runs: &[String],
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let target = Target {
        workspace_id,
        owner_id,
        session_key,
        now: &now,
    };
    let any: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM rf_message_attempts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND erase_pending = 1)",
        params![workspace_id, owner_id, session_key],
        |row| row.get(0),
    )?;
    if !any {
        return Ok(());
    }
    let tx = begin_immediate(conn)?;
    let pending = {
        let mut statement = tx.prepare(&format!(
            "SELECT {} FROM rf_message_attempts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND erase_pending = 1",
            Attempt::COLUMNS
        ))?;
        statement
            .query_map(params![workspace_id, owner_id, session_key], Attempt::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut settled = false;
    for attempt in pending
        .iter()
        .filter(|attempt| attempt.run_id.as_ref().is_none_or(|run| !live_runs.contains(run)))
    {
        // A forget asked while the run was live runs now, before the erase.
        if attempt.forget.requested && attempt.forget.recorded.is_none() {
            forget_now(&tx, &target, attempt)?;
        }
        erase(&tx, &target, Side::Message, attempt)?;
        tx.execute(
            "UPDATE rf_message_attempts SET erase_pending = 0 WHERE workspace_id = ?1 AND message_event_id = ?2",
            params![workspace_id, attempt.input_event_id],
        )?;
        settled = true;
    }
    if settled {
        refresh_preview(&tx, session_key)?;
        mark_scrub(&tx, &now)?;
        tx.commit()?;
    }
    Ok(())
}

/// The forget answer of one attempt: the recorded counts on a replay, a
/// pending forget (settled with the run) for a live run, else the forget
/// executed now and recorded.
fn forget_attempt(tx: &Connection, target: &Target<'_>, attempt: &Attempt, live: bool) -> anyhow::Result<RfForget> {
    let writes_traceable = rf_forget::writes_traceable(
        tx,
        target.workspace_id,
        &attempt.input_event_id,
        attempt.run_id.as_deref(),
    )?;
    let counts = if let Some(recorded) = attempt.forget.recorded {
        Some(recorded)
    } else if live {
        rf_forget::request(tx, target.workspace_id, &attempt.input_event_id)?;
        None
    } else {
        Some(forget_now(tx, target, attempt)?)
    };
    Ok(RfForget {
        counts,
        writes_traceable,
    })
}

/// Forget the writes of the attempt's run family now and record the counts.
fn forget_now(tx: &Connection, target: &Target<'_>, attempt: &Attempt) -> anyhow::Result<RfForgetCounts> {
    let counts = match attempt.run_id.as_deref() {
        Some(run_id) => rf_forget::forget_family(tx, run_id, attempt.partition(), target.now)?,
        None => RfForgetCounts::default(),
    };
    rf_forget::record(tx, target.workspace_id, &attempt.input_event_id, counts)?;
    Ok(counts)
}

/// The event ids of a run's rows.
fn run_event_ids(tx: &Connection, target: &Target<'_>, run_id: Option<&str>) -> anyhow::Result<Vec<String>> {
    let Some(run_id) = run_id else {
        return Ok(Vec::new());
    };
    let mut statement =
        tx.prepare("SELECT event_id FROM message_events WHERE workspace_id = ?1 AND owner_id = ?2 AND run_id = ?3")?;
    Ok(statement
        .query_map(params![target.workspace_id, target.owner_id, run_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?)
}

/// One event row a delete rewrites.
struct EventRow {
    event_id: String,
    conversational: bool,
    content: String,
}

/// Tombstone one side of an attempt and every copy derived from it.
fn erase(tx: &Connection, target: &Target<'_>, side: Side, attempt: &Attempt) -> anyhow::Result<()> {
    // The usage row holds accounting only (tokens, model, cost) and stays; a
    // delete's own record and rows already tombstoned are never rewritten.
    let sql = match side {
        Side::Message => {
            "SELECT event_id, role IN ('user', 'assistant'), content FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2
               AND (event_id = ?3 OR (?4 IS NOT NULL AND run_id = ?4)
                    OR (event_type IN ('message.attachment.recorded', 'tool.call.recorded',
                                       'tool.result.recorded', 'turn.decision.claimed')
                        AND (causation_event_id = ?3
                             OR (json_valid(raw_payload_json)
                                 AND json_extract(raw_payload_json, '$.user_event_id') = ?3))))
               AND event_type NOT IN ('turn.deleted', 'usage.settled', 'message.deleted')"
        }
        Side::Reply => {
            "SELECT event_id, role IN ('user', 'assistant'), content FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND event_id <> ?3
               AND (run_id = ?4
                    OR (event_type IN ('tool.call.recorded', 'tool.result.recorded', 'turn.decision.claimed')
                        AND causation_event_id = ?3))
               AND event_type NOT IN ('turn.deleted', 'usage.settled', 'message.deleted')
               AND event_type NOT LIKE 'turn.ingress.%'"
        }
    };
    let rows = {
        let mut statement = tx.prepare(sql)?;
        statement
            .query_map(
                params![
                    target.workspace_id,
                    target.owner_id,
                    attempt.input_event_id,
                    attempt.run_id
                ],
                |row| {
                    Ok(EventRow {
                        event_id: row.get(0)?,
                        conversational: row.get(1)?,
                        content: row.get(2)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if rows.is_empty() {
        return Ok(());
    }
    let window_start: Option<String> = tx
        .query_row(
            "SELECT created_at FROM message_events WHERE workspace_id = ?1 AND event_id = ?2",
            params![target.workspace_id, attempt.input_event_id],
            |row| row.get(0),
        )
        .optional()?;
    for row in rows.iter().filter(|row| row.conversational && !row.content.is_empty()) {
        for hash in [
            SqliteMemory::scoped_content_hash(None, &row.content),
            SqliteMemory::scoped_content_hash(attempt.scope.as_deref(), &row.content),
        ] {
            tx.execute("DELETE FROM embedding_cache WHERE content_hash = ?1", params![hash])?;
        }
        tx.execute(
            "DELETE FROM retrieval_traces WHERE workspace_id = ?1 AND session_key = ?2 AND query = ?3",
            params![target.workspace_id, target.session_key, row.content],
        )?;
    }
    // Traces the run wrote (recall and tool queries) for this agent.
    tx.execute(
        "DELETE FROM retrieval_traces WHERE workspace_id = ?1 AND session_key = ?2 AND ma_scope_id IS ?3
           AND julianday(created_at) >= julianday(?4) AND julianday(created_at) <= julianday(?5)",
        params![
            target.workspace_id,
            target.session_key,
            attempt.scope,
            window_start,
            target.now
        ],
    )?;
    let direct = serde_json::to_string(&rows.iter().map(|row| row.event_id.as_str()).collect::<Vec<_>>())?;
    let contents = rows
        .iter()
        .filter(|row| row.conversational && !row.content.is_empty())
        .map(|row| row.content.as_str())
        .collect::<Vec<_>>();
    rf_forget::erase_context_pages(
        tx,
        target.session_key,
        attempt.partition(),
        &direct,
        &attempt.input_event_id,
        &contents,
    )?;
    let texts: Vec<String> = contents.iter().map(|text| (*text).to_owned()).collect();
    let mut ids = rows.into_iter().map(|row| row.event_id).collect::<Vec<_>>();
    let mut forgotten = tx.execute(
        "DELETE FROM memories WHERE source_event_id IN (SELECT value FROM json_each(?1))",
        params![direct],
    )?;
    for statement in [
        "DELETE FROM memory_drafts WHERE source_event_id IN (SELECT value FROM json_each(?1))",
        "DELETE FROM memory_links WHERE message_event_id IN (SELECT value FROM json_each(?1))",
    ] {
        tx.execute(statement, params![direct])?;
    }
    ids.extend(erase_compactions(tx, target, &direct, &mut forgotten)?);
    if forgotten > 0 {
        // FTS5 keeps a deleted row's terms in its segments until a merge;
        // a rebuild drops them as well as their tombstones.
        tx.execute("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')", [])?;
    }
    {
        let mut statement = tx.prepare(
            "SELECT event_id FROM message_events
             WHERE workspace_id = ?1 AND session_key = ?2 AND event_type = 'context.switch.created'
               AND (causation_event_id IN (SELECT value FROM json_each(?3))
                    OR correlation_id IN (SELECT value FROM json_each(?3))
                    OR EXISTS(SELECT 1 FROM json_each(?3) j WHERE instr(content, j.value) > 0))",
        )?;
        let notes = statement
            .query_map(params![target.workspace_id, target.session_key, direct], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.extend(notes);
    }
    let all = serde_json::to_string(&ids)?;
    // Read before the rewrite: the payload of a tool result names the stored
    // document that holds its large output.
    let output_documents = tool_output_document_ids(tx, target, &all)?;
    tx.execute(
        "UPDATE message_events SET content = '', content_hash = NULL, source_ref_json = NULL,
             subject_ref_json = NULL,
             raw_payload_json = CASE
                 WHEN (idempotency_key LIKE 'turn:%:final' OR idempotency_key LIKE 'turn:%:admission-failed')
                      AND json_valid(raw_payload_json)
                 THEN json_object('deleted', json('true'), 'status', json_extract(raw_payload_json, '$.status'))
                 ELSE '{\"deleted\":true}' END,
             event_type = 'message.deleted', role = 'event', updated_at = ?3
         WHERE workspace_id = ?1 AND event_id IN (SELECT value FROM json_each(?2))",
        params![target.workspace_id, all, target.now],
    )?;
    erase_tool_output_documents(tx, &attempt.input_event_id, &output_documents)?;
    // After the memories and events above are gone: a later attachment of the
    // session that quoted any of them is rewritten (`rf_attachments`).
    blank_attachments(tx, target, &texts.iter().map(String::as_str).collect::<Vec<_>>())?;
    tombstone_turns(tx, &ids, target.now)
}

/// Rewrite the later attachments of the session that quote deleted content;
/// `deleted_texts` serve the text match of attachments without provenance.
fn blank_attachments(tx: &Connection, target: &Target<'_>, deleted_texts: &[&str]) -> anyhow::Result<()> {
    rf_attachments::blank_stale_attachments(
        tx,
        target.workspace_id,
        target.owner_id,
        target.session_key,
        target.now,
        deleted_texts,
    )?;
    Ok(())
}

/// The document kinds a run's tool outputs are stored as: the oversized
/// `tool_output` document and the verbatim `untrusted_external` copy.
const TOOL_OUTPUT_KINDS: &str = "('tool_output', 'untrusted_external')";

/// The ids of the stored tool-output documents that the `tool.result.recorded`
/// rows among `event_ids` (a JSON array) reference, with the verbatim copy
/// each may have (`untrusted:<id>`).
fn tool_output_document_ids(tx: &Connection, target: &Target<'_>, event_ids: &str) -> anyhow::Result<Vec<String>> {
    let mut statement = tx.prepare(
        "SELECT DISTINCT json_extract(raw_payload_json, '$.document.document_id') FROM message_events
         WHERE workspace_id = ?1 AND event_type = 'tool.result.recorded' AND json_valid(raw_payload_json)
           AND event_id IN (SELECT value FROM json_each(?2))",
    )?;
    let ids = statement
        .query_map(params![target.workspace_id, event_ids], |row| row.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids
        .into_iter()
        .flatten()
        .flat_map(|id| [format!("untrusted:{id}"), id])
        .collect())
}

/// Delete the stored tool-output documents of a deleted run: those its tool
/// result rows referenced and those stored with its input event as source.
/// A document is content-addressed, so one that a tool result row of a
/// surviving run still references stays (that row quotes it by id).
fn erase_tool_output_documents(tx: &Connection, input_event_id: &str, referenced: &[String]) -> anyhow::Result<()> {
    let mut ids = referenced.to_vec();
    {
        let mut statement = tx.prepare(&format!(
            "SELECT document_id FROM documents WHERE source_message_event_id = ?1 AND source_kind IN {TOOL_OUTPUT_KINDS}"
        ))?;
        let sourced = statement
            .query_map(params![input_event_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.extend(sourced);
    }
    let mut deleted = false;
    for id in ids {
        let kind_ok: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM documents WHERE document_id = ?1 AND source_kind IN {TOOL_OUTPUT_KINDS})"),
            params![id],
            |row| row.get(0),
        )?;
        let quoted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_events
                 WHERE event_type = 'tool.result.recorded' AND json_valid(raw_payload_json)
                   AND json_extract(raw_payload_json, '$.document.document_id') IN (?1, replace(?1, 'untrusted:', '')))",
            params![id],
            |row| row.get(0),
        )?;
        if kind_ok && !quoted {
            deleted |= rf_forget::delete_document(tx, &id, true)?;
        }
    }
    rf_forget::rebuild_indexes(tx, false, deleted)
}

/// Compaction summaries built from a deleted event: the summary text, its
/// memory row and the summary event (returned, for tombstoning).
fn erase_compactions(
    tx: &Connection,
    target: &Target<'_>,
    ids: &str,
    forgotten: &mut usize,
) -> anyhow::Result<Vec<String>> {
    let runs = {
        let mut statement = tx.prepare(
            "SELECT run_id, summary_memory_key,
                    CASE WHEN json_valid(payload_json)
                         THEN json_extract(payload_json, '$.summary_message_event_id') END
             FROM compaction_runs c
             WHERE workspace_id = ?1 AND EXISTS(
                 SELECT 1 FROM json_each(CASE WHEN json_valid(c.source_event_ids_json)
                                              THEN c.source_event_ids_json ELSE '[]' END) j
                 WHERE j.value IN (SELECT value FROM json_each(?2)))",
        )?;
        statement
            .query_map(params![target.workspace_id, ids], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut summaries = Vec::new();
    for (run_id, memory_key, summary_event) in runs {
        tx.execute(
            "UPDATE compaction_runs SET summary = '' WHERE workspace_id = ?1 AND run_id = ?2",
            params![target.workspace_id, run_id],
        )?;
        if let Some(key) = memory_key {
            *forgotten = forgotten.saturating_add(tx.execute("DELETE FROM memories WHERE key = ?1", params![key])?);
        }
        summaries.extend(summary_event);
    }
    Ok(summaries)
}

/// Tombstone the conversation rows projected from these events.
fn tombstone_turns(tx: &Connection, event_ids: &[String], now: &str) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE conversation_turns SET content = '', deleted_at = COALESCE(deleted_at, ?2)
         WHERE message_event_id IN (SELECT value FROM json_each(?1))",
        params![serde_json::to_string(event_ids)?, now],
    )?;
    Ok(())
}

/// The content-free `turn.deleted` record of one deleted run (or of a
/// `context` message), kept for the sync stream; one per run and side. It
/// names the run in its payload only, so no read of the run's rows meets it.
fn record_deleted(
    tx: &Connection,
    target: &Target<'_>,
    side: Side,
    attempt: &Attempt,
    client_message_id: &str,
) -> anyhow::Result<()> {
    let subject = attempt.run_id.as_deref().unwrap_or(&attempt.input_event_id);
    let payload = serde_json::json!({
        "side": side.label(),
        "agent": attempt.agent_key,
        "message_id": client_message_id,
        "run_id": attempt.run_id,
    });
    tx.execute(
        "INSERT INTO message_events (
            event_id, idempotency_key, workspace_id, owner_id, source, session_key, role,
            event_type, content, raw_payload_json, created_at, updated_at, ma_scope_id
         ) VALUES (?1, ?2, ?3, ?4, 'console', ?5, 'event', 'turn.deleted', 'turn.deleted', ?6, ?7, ?7, ?8)
         ON CONFLICT DO NOTHING",
        params![
            uuid::Uuid::new_v4().to_string(),
            format!("rf-delete:{}:{subject}", side.label()),
            target.workspace_id,
            target.owner_id,
            target.session_key,
            payload.to_string(),
            target.now,
            attempt.scope,
        ],
    )?;
    let event_id: String = tx.query_row(
        "SELECT event_id FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND idempotency_key=?3",
        params![
            target.workspace_id,
            target.owner_id,
            format!("rf-delete:{}:{subject}", side.label())
        ],
        |r| r.get(0),
    )?;
    // The delete source is content-free and names its root attempt explicitly.
    let incarnation: Option<String> = tx
        .query_row(
            "SELECT incarnation FROM agent_scopes WHERE scope_id=?1",
            [&attempt.scope],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(incarnation) = incarnation {
        let mut header = super::sync_journal::Header::new(
            target.owner_id,
            "turn.deleted",
            target.session_key,
            &attempt.agent_key,
            &incarnation,
            format!("event:{event_id}"),
        );
        header.run_id = attempt.run_id.clone();
        let message_id = match side {
            Side::Message => Some(attempt.input_event_id.clone()),
            Side::Reply => tx
                .query_row(
                    "SELECT event_id FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND idempotency_key=?3",
                    params![
                        target.workspace_id,
                        target.owner_id,
                        format!("turn:{}:assistant", attempt.run_id.as_deref().unwrap_or_default())
                    ],
                    |r| r.get::<_, String>(0),
                )
                .optional()?,
        };
        // A failed/interrupted run can have no assistant row. Its input
        // remains the stable history anchor; side/run_id identify the reply.
        let message_id = message_id.unwrap_or_else(|| attempt.input_event_id.clone());
        header.source_ref = format!("deleted:{event_id}:{message_id}");
        super::sync_journal::append(tx, &header)?;
    }
    Ok(())
}

/// The session preview is taken from the newest row that is not deleted.
fn refresh_preview(tx: &Connection, session_key: &str) -> anyhow::Result<()> {
    let latest: Option<String> = tx
        .query_row(
            "SELECT content FROM conversation_turns WHERE session_key = ?1 AND deleted_at IS NULL
             ORDER BY id DESC LIMIT 1",
            params![session_key],
            |row| row.get(0),
        )
        .optional()?;
    tx.execute(
        "UPDATE sessions SET last_message_preview = ?2 WHERE session_key = ?1",
        params![
            session_key,
            latest
                .as_deref()
                .map(SqliteMemory::conversation_preview)
                .unwrap_or_default()
        ],
    )?;
    Ok(())
}

/// Note, in the delete's transaction, that its old row images may sit in
/// WAL frames until a truncating checkpoint runs.
fn mark_scrub(tx: &Connection, now: &str) -> anyhow::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO rf_wal_scrub_pending (singleton, since) VALUES (1, ?1)",
        params![now],
    )?;
    Ok(())
}

/// The longest one scrub waits for readers to leave the WAL.
const SCRUB_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// Empty the WAL of deleted content when a delete left the mark: a
/// truncating checkpoint, then the mark is cleared (its own frame holds only
/// the mark table's page) and the WAL truncated again, best effort. `false`
/// when a reader kept the checkpoint from completing within the budget; the
/// mark stays for the next settle, open or clean stop.
pub(super) fn scrub(pool: &super::SqliteConnectionPool) -> anyhow::Result<bool> {
    let pending: bool = pool
        .write()
        .query_row("SELECT EXISTS(SELECT 1 FROM rf_wal_scrub_pending)", [], |row| {
            row.get(0)
        })?;
    if !pending {
        return Ok(true);
    }
    if super::memory_impl::truncate_wal(pool, SCRUB_BUDGET).is_err() {
        return Ok(false);
    }
    {
        let conn = pool.write();
        conn.execute("DELETE FROM rf_wal_scrub_pending", [])?;
    }
    // The deleted content is gone; a failure here leaves only the mark
    // table's page in the WAL.
    if let Err(error) = super::memory_impl::truncate_wal(pool, SCRUB_BUDGET) {
        tracing::debug!(
            error = %crate::agent::diagnostics::error_class(&error),
            "WAL truncation after the scrub mark was cleared did not complete"
        );
    }
    Ok(true)
}
