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
//! name a deleted event as their source, and compaction summaries and handoff
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
use crate::memory::rf_messages::{RfDeletedAgent, RfMessageDelete, RfReplyDelete};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

/// Idempotent DDL, run after the attempt table exists.
pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
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
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let attempts = {
        let mut statement = tx.prepare(
            "SELECT agent_key, run_id, job_id, message_event_id, ma_scope_id FROM rf_message_attempts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND client_message_id = ?4
               AND (?5 IS NULL OR agent_key = ?5)
             ORDER BY rowid",
        )?;
        statement
            .query_map(
                params![
                    request.workspace_id,
                    request.owner_id,
                    request.session_key,
                    request.client_message_id,
                    request.agent_key
                ],
                |row| {
                    Ok(Attempt {
                        agent_key: row.get(0)?,
                        run_id: row.get(1)?,
                        job_id: row.get(2)?,
                        input_event_id: row.get(3)?,
                        scope: row.get(4)?,
                    })
                },
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
        let index = match agents.iter().position(|agent| agent.agent_key == attempt.agent_key) {
            Some(index) => index,
            None => {
                agents.push(RfDeletedAgent {
                    agent_key: attempt.agent_key.clone(),
                    runs: Vec::new(),
                    jobs: Vec::new(),
                    deferred: Vec::new(),
                });
                agents.len().saturating_sub(1)
            }
        };
        if let Some(agent) = agents.get_mut(index) {
            agent.runs.extend(attempt.run_id.iter().cloned());
            agent.jobs.extend(attempt.job_id.iter().cloned());
            if live {
                agent.deferred.extend(attempt.run_id.iter().cloned());
            }
        }
    }
    refresh_preview(&tx, &request.session_key)?;
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
) -> anyhow::Result<RfReplyDelete> {
    let now = chrono::Utc::now().to_rfc3339();
    let target = Target {
        workspace_id,
        owner_id,
        session_key,
        now: &now,
    };
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let row = tx
        .query_row(
            "SELECT agent_key, client_message_id, job_id, message_event_id, ma_scope_id, interrupted,
                    deleted_at IS NOT NULL OR reply_deleted_at IS NOT NULL
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
        return Ok(RfReplyDelete::Deleted { job_id });
    }
    if !interrupted && !super::rf_messages::has_terminal(&tx, workspace_id, owner_id, run_id)? {
        return Ok(RfReplyDelete::Active);
    }
    tx.execute(
        "UPDATE rf_message_attempts SET reply_deleted_at = ?3 WHERE workspace_id = ?1 AND run_id = ?2",
        params![workspace_id, run_id, now],
    )?;
    erase(&tx, &target, Side::Reply, &attempt)?;
    record_deleted(&tx, &target, Side::Reply, &attempt, &client_message_id)?;
    refresh_preview(&tx, session_key)?;
    tx.commit()?;
    Ok(RfReplyDelete::Deleted { job_id })
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
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let pending = {
        let mut statement = tx.prepare(
            "SELECT agent_key, run_id, job_id, message_event_id, ma_scope_id FROM rf_message_attempts
             WHERE workspace_id = ?1 AND owner_id = ?2 AND session_key = ?3 AND erase_pending = 1",
        )?;
        statement
            .query_map(params![workspace_id, owner_id, session_key], |row| {
                Ok(Attempt {
                    agent_key: row.get(0)?,
                    run_id: row.get(1)?,
                    job_id: row.get(2)?,
                    input_event_id: row.get(3)?,
                    scope: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut settled = false;
    for attempt in pending
        .iter()
        .filter(|attempt| attempt.run_id.as_ref().is_none_or(|run| !live_runs.contains(run)))
    {
        erase(&tx, &target, Side::Message, attempt)?;
        tx.execute(
            "UPDATE rf_message_attempts SET erase_pending = 0 WHERE workspace_id = ?1 AND message_event_id = ?2",
            params![workspace_id, attempt.input_event_id],
        )?;
        settled = true;
    }
    if settled {
        refresh_preview(&tx, session_key)?;
        tx.commit()?;
    }
    Ok(())
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
             WHERE workspace_id = ?1 AND owner_id = ?2 AND (event_id = ?3 OR (?4 IS NOT NULL AND run_id = ?4))
               AND event_type NOT IN ('turn.deleted', 'usage.settled', 'message.deleted')"
        }
        Side::Reply => {
            "SELECT event_id, role IN ('user', 'assistant'), content FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND run_id = ?4 AND event_id <> ?3
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
    let mut ids = rows.into_iter().map(|row| row.event_id).collect::<Vec<_>>();
    let direct = serde_json::to_string(&ids)?;
    for statement in [
        "DELETE FROM memories WHERE source_event_id IN (SELECT value FROM json_each(?1))",
        "DELETE FROM memory_drafts WHERE source_event_id IN (SELECT value FROM json_each(?1))",
        "DELETE FROM memory_links WHERE message_event_id IN (SELECT value FROM json_each(?1))",
    ] {
        tx.execute(statement, params![direct])?;
    }
    ids.extend(erase_compactions(tx, target, &direct)?);
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
    tombstone_turns(tx, &ids, target.now)
}

/// Compaction summaries built from a deleted event: the summary text, its
/// memory row and the summary event (returned, for tombstoning).
fn erase_compactions(tx: &Connection, target: &Target<'_>, ids: &str) -> anyhow::Result<Vec<String>> {
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
            tx.execute("DELETE FROM memories WHERE key = ?1", params![key])?;
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
