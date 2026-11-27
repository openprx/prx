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
//! compaction summaries (and the summaries built from those) and handoff
//! notes built from a deleted event. A delete also takes the runs the turn
//! spawned (child and grandchild sessions, by `parent_run_id`) with all their
//! rows, each with its own `turn.deleted` record, and a message delete takes
//! the scheduled-result rows of the job its message created (`correlation_id`).
//! Every handoff note of a session created after the session's first deleted
//! row goes, by position. Attachments that quoted deleted events, memories or
//! documents are rewritten by the triggers of `rf_attachments`, and listings of
//! the erased runs by `derived_sweep`, in the same transaction. The attempt rows keep their key and get
//! the deleted mark; a message delete also clears the request digest, the
//! only derivative of the message the record held.
//!
//! The rows of a run whose job is still live in this process are not
//! rewritten under it (its terminal commit re-reads its own input and event
//! rows): the delete marks the attempt `erase_pending`, tombstones the
//! conversation rows at once, and [`settle`] rewrites the event rows once the
//! job has ended.

use super::SqliteMemory;
use super::derived_sweep;
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
        let live = is_live(&tx, &target, attempt, &request.live_runs)?;
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
    for attempt in &pending {
        // A run is live while it or a run it spawned is.
        if is_live(&tx, &target, attempt, live_runs)? {
            continue;
        }
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

/// The runs spawned by `run_id`, children and deeper (`parent_run_id`
/// closure of this owner's rows), without `run_id` itself.
fn descendant_runs(tx: &Connection, workspace_id: &str, owner_id: &str, run_id: &str) -> anyhow::Result<Vec<String>> {
    let mut statement = tx.prepare(
        "WITH RECURSIVE family(run) AS (
             SELECT ?3
             UNION
             SELECT e.run_id FROM message_events e JOIN family f ON e.parent_run_id = f.run
             WHERE e.workspace_id = ?1 AND e.owner_id = ?2 AND e.run_id IS NOT NULL)
         SELECT run FROM family WHERE run <> ?3",
    )?;
    Ok(statement
        .query_map(params![workspace_id, owner_id, run_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?)
}

/// Whether the attempt's run, or a run it spawned, is still live in this
/// process (`live_runs`).
fn is_live(tx: &Connection, target: &Target<'_>, attempt: &Attempt, live_runs: &[String]) -> anyhow::Result<bool> {
    let Some(run) = attempt.run_id.as_deref() else {
        return Ok(false);
    };
    if live_runs.is_empty() {
        return Ok(false);
    }
    if live_runs.iter().any(|live| live == run) {
        return Ok(true);
    }
    Ok(descendant_runs(tx, target.workspace_id, target.owner_id, run)?
        .iter()
        .any(|descendant| live_runs.contains(descendant)))
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
    id: i64,
    event_id: String,
    role: String,
    content: String,
    run_id: Option<String>,
    session_key: Option<String>,
    scope: Option<String>,
    /// A scheduled job's stored result excerpt, matched by correlation and
    /// kept out of the positional anchor of the handoff-note rule.
    schedule_result: bool,
}

impl EventRow {
    /// A user or assistant message (its text is cached and recalled).
    fn conversational(&self) -> bool {
        matches!(self.role.as_str(), "user" | "assistant")
    }
}

/// A run spawned by the deleted run, erased with it, in its own session.
struct Child {
    run_id: String,
    session_key: String,
    scope: Option<String>,
    /// Smallest row id of the run among the rows the delete rewrites.
    first_id: i64,
    /// The run's first user row, else `None`.
    first_user: Option<String>,
    event_ids: Vec<String>,
    contents: Vec<String>,
}

impl Child {
    fn partition(&self, main: bool) -> Partition<'_> {
        Partition {
            scope: self.scope.as_deref(),
            main,
        }
    }
}

/// Group the rewritten rows of the spawned runs by run, in row order.
fn children_of(rows: &[EventRow], descendants: &[String], target: &Target<'_>) -> Vec<Child> {
    let mut children: Vec<Child> = Vec::new();
    for row in rows {
        let Some(run) = row.run_id.as_deref().filter(|run| descendants.iter().any(|d| d == run)) else {
            continue;
        };
        if !children.iter().any(|child| child.run_id == run) {
            children.push(Child {
                run_id: run.to_owned(),
                session_key: row.session_key.clone().unwrap_or_else(|| target.session_key.to_owned()),
                scope: row.scope.clone(),
                first_id: row.id,
                first_user: None,
                event_ids: Vec::new(),
                contents: Vec::new(),
            });
        }
        if let Some(child) = children.iter_mut().find(|child| child.run_id == run) {
            child.first_id = child.first_id.min(row.id);
            if child.first_user.is_none() && row.role == "user" {
                child.first_user = Some(row.event_id.clone());
            }
            child.event_ids.push(row.event_id.clone());
            if row.conversational() && !row.content.is_empty() {
                child.contents.push(row.content.clone());
            }
        }
    }
    children
}

/// The `(agent_key, incarnation)` of an agent scope, `None` for no scope.
fn scope_agent(tx: &Connection, scope: Option<&str>) -> anyhow::Result<Option<(String, String)>> {
    let Some(scope) = scope else {
        return Ok(None);
    };
    Ok(tx
        .query_row(
            "SELECT agent_key, incarnation FROM agent_scopes WHERE scope_id = ?1",
            [scope],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Tombstone one side of an attempt and every copy derived from it: the
/// rows of the run and of every run it spawned, the result rows of a job its
/// message created (message side only), the handoff notes after the first
/// deleted row of a session, and the compaction summaries built from any of
/// them. The attachments and listings that quoted them are rewritten by the
/// store's triggers and `derived_sweep`.
fn erase(tx: &Connection, target: &Target<'_>, side: Side, attempt: &Attempt) -> anyhow::Result<()> {
    let descendants = match attempt.run_id.as_deref() {
        Some(run) => descendant_runs(tx, target.workspace_id, target.owner_id, run)?,
        None => Vec::new(),
    };
    // The usage row holds accounting only (tokens, model, cost) and stays; a
    // delete's own record and rows already tombstoned are never rewritten.
    let sql = match side {
        Side::Message => {
            "SELECT id, event_id, role, content, run_id, session_key, ma_scope_id,
                    event_type = 'schedule.result.recorded' FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2
               AND (event_id = ?3 OR (?4 IS NOT NULL AND run_id = ?4)
                    OR run_id IN (SELECT value FROM json_each(?5))
                    OR (event_type IN ('message.attachment.recorded', 'tool.call.recorded',
                                       'tool.result.recorded', 'turn.decision.claimed')
                        AND (causation_event_id = ?3
                             OR (json_valid(raw_payload_json)
                                 AND json_extract(raw_payload_json, '$.user_event_id') = ?3)))
                    OR (event_type = 'schedule.result.recorded' AND correlation_id = ?3))
               AND event_type NOT IN ('turn.deleted', 'usage.settled', 'message.deleted')
             ORDER BY id"
        }
        Side::Reply => {
            "SELECT id, event_id, role, content, run_id, session_key, ma_scope_id,
                    event_type = 'schedule.result.recorded' FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND event_id <> ?3
               AND (run_id IN (SELECT value FROM json_each(?5))
                    OR ((run_id = ?4
                         OR (event_type IN ('tool.call.recorded', 'tool.result.recorded', 'turn.decision.claimed')
                             AND causation_event_id = ?3))
                        AND event_type NOT LIKE 'turn.ingress.%'))
               AND event_type NOT IN ('turn.deleted', 'usage.settled', 'message.deleted')
             ORDER BY id"
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
                    attempt.run_id,
                    serde_json::to_string(&descendants)?
                ],
                |row| {
                    Ok(EventRow {
                        id: row.get(0)?,
                        event_id: row.get(1)?,
                        role: row.get(2)?,
                        content: row.get(3)?,
                        run_id: row.get(4)?,
                        session_key: row.get(5)?,
                        scope: row.get(6)?,
                        schedule_result: row.get(7)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if rows.is_empty() {
        return Ok(());
    }
    let children = children_of(&rows, &descendants, target);
    let window_start: Option<String> = tx
        .query_row(
            "SELECT created_at FROM message_events WHERE workspace_id = ?1 AND event_id = ?2",
            params![target.workspace_id, attempt.input_event_id],
            |row| row.get(0),
        )
        .optional()?;
    for row in rows
        .iter()
        .filter(|row| row.conversational() && !row.content.is_empty())
    {
        // A spawned run's rows belong to its own session and agent.
        let spawned = row
            .run_id
            .as_deref()
            .is_some_and(|run| descendants.iter().any(|descendant| descendant == run));
        let (scope, session_key) = if spawned {
            (
                row.scope.as_deref(),
                row.session_key.as_deref().unwrap_or(target.session_key),
            )
        } else {
            (attempt.scope.as_deref(), target.session_key)
        };
        for hash in [
            SqliteMemory::scoped_content_hash(None, &row.content),
            SqliteMemory::scoped_content_hash(scope, &row.content),
        ] {
            tx.execute("DELETE FROM embedding_cache WHERE content_hash = ?1", params![hash])?;
        }
        tx.execute(
            "DELETE FROM retrieval_traces WHERE workspace_id = ?1 AND session_key = ?2 AND query = ?3",
            params![target.workspace_id, session_key, row.content],
        )?;
    }
    // Traces the run and the runs it spawned wrote (recall and tool queries)
    // for their agents, each in its own session.
    let trace_windows = std::iter::once((target.session_key, attempt.scope.as_deref())).chain(
        children
            .iter()
            .map(|child| (child.session_key.as_str(), child.scope.as_deref())),
    );
    for (session_key, scope) in trace_windows {
        tx.execute(
            "DELETE FROM retrieval_traces WHERE workspace_id = ?1 AND session_key = ?2 AND ma_scope_id IS ?3
               AND julianday(created_at) >= julianday(?4) AND julianday(created_at) <= julianday(?5)",
            params![target.workspace_id, session_key, scope, window_start, target.now],
        )?;
    }
    let direct_ids: Vec<String> = rows.iter().map(|row| row.event_id.clone()).collect();
    let direct = serde_json::to_string(&direct_ids)?;
    // The rows of the deleted conversation itself; a scheduled job's result
    // excerpt belongs to the job's session, whose later runs are kept.
    let anchor = serde_json::to_string(
        &rows
            .iter()
            .filter(|row| !row.schedule_result)
            .map(|row| row.event_id.as_str())
            .collect::<Vec<_>>(),
    )?;
    let contents = rows
        .iter()
        .filter(|row| row.conversational() && !row.content.is_empty())
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
    for child in &children {
        let main = scope_agent(tx, child.scope.as_deref())?.is_none_or(|(agent_key, _)| agent_key == "m");
        let child_contents: Vec<&str> = child.contents.iter().map(String::as_str).collect();
        rf_forget::erase_context_pages(
            tx,
            &child.session_key,
            child.partition(main),
            &serde_json::to_string(&child.event_ids)?,
            child
                .first_user
                .as_deref()
                .or_else(|| child.event_ids.first().map(String::as_str))
                .unwrap_or_default(),
            &child_contents,
        )?;
    }
    let mut ids = rows.into_iter().map(|row| row.event_id).collect::<Vec<_>>();
    rf_forget::drop_prefix_rows_of(
        tx,
        "SELECT DISTINCT workspace_id, owner_id FROM memories WHERE source_event_id IN (SELECT value FROM json_each(?1))",
        params![direct],
    )?;
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
    // A summary of a summary: each pass matches the deleted ids plus the
    // summary events found so far and handles only the compaction runs no
    // earlier pass found, so the loop ends when a pass finds none new.
    let mut summary_sources = direct_ids;
    let mut handled_runs: Vec<String> = Vec::new();
    loop {
        let found = erase_compactions(
            tx,
            target,
            &serde_json::to_string(&summary_sources)?,
            &mut handled_runs,
            &mut forgotten,
        )?;
        if found.is_empty() {
            break;
        }
        for summary in found.into_iter().flatten() {
            summary_sources.push(summary.clone());
            ids.push(summary);
        }
    }
    if forgotten > 0 {
        // FTS5 keeps a deleted row's terms in its segments until a merge;
        // a rebuild drops them as well as their tombstones.
        tx.execute("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')", [])?;
    }
    // A handoff note summarizes the window before it, so every note of a
    // session created after its first deleted row can hold deleted content
    // without naming the row (position, not text).
    ids.extend(later_handoff_notes(tx, target, &anchor)?);
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
    let mut inputs = vec![attempt.input_event_id.clone()];
    inputs.extend(children.iter().filter_map(|child| child.first_user.clone()));
    erase_tool_output_documents(tx, &inputs, &output_documents)?;
    // The attachments that quoted a deleted event, memory or document were
    // rewritten by the triggers of those deletes, in this transaction.
    for child in &children {
        record_child_deleted(tx, target, side, child)?;
    }
    tombstone_turns(tx, &ids, target.now)?;
    sweep_listings(tx, target, &children)
}

/// Replace the printed text of listing results, workspace-wide and newer than
/// the first erased row, that name an erased run id (and so its session key, as
/// both are identifiers). A spawned run's session key is swept from the first
/// row only when the tombstone left no row of that session: a later run may
/// have continued it. Call after the tombstone.
fn sweep_listings(tx: &Connection, target: &Target<'_>, children: &[Child]) -> anyhow::Result<()> {
    let Some(since_id) = children.iter().map(|child| child.first_id).min() else {
        return Ok(());
    };
    let mut swept_sessions: Vec<String> = Vec::new();
    for child in children {
        if swept_sessions.contains(&child.session_key) {
            continue;
        }
        // The survival rule of `agent_erasure::erased_identifiers`, with the
        // tombstone event types not counted as rows (they are written here).
        let survives: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_events WHERE workspace_id = ?2 AND session_key = ?1
                           AND event_type NOT IN ('message.deleted', 'turn.deleted'))
                 OR EXISTS(SELECT 1 FROM conversation_turns c WHERE c.session_key = ?1 AND c.deleted_at IS NULL
                     AND (c.ma_scope_id IS NULL OR EXISTS(SELECT 1 FROM agent_scopes a
                         WHERE a.scope_id = c.ma_scope_id AND a.workspace_id = ?2)))",
            params![child.session_key, target.workspace_id],
            |row| row.get(0),
        )?;
        if !survives {
            swept_sessions.push(child.session_key.clone());
        }
    }
    let erased = derived_sweep::ErasedIdentifiers {
        run_ids: children.iter().map(|child| child.run_id.clone()).collect(),
        since_id,
        swept_sessions,
    };
    derived_sweep::rewrite_listings(tx, target.workspace_id, &erased, target.now)?;
    Ok(())
}

/// The handoff notes created after the first deleted row of their session
/// (`direct`, a JSON array of the event ids of the deleted conversation: its
/// rows in the target session and the rows of its spawned runs), except the
/// notes already among them.
fn later_handoff_notes(tx: &Connection, target: &Target<'_>, direct: &str) -> anyhow::Result<Vec<String>> {
    let mut statement = tx.prepare(
        "SELECT n.event_id FROM message_events n
         WHERE n.workspace_id = ?1 AND n.event_type = 'context.switch.created' AND n.session_key IS NOT NULL
           AND n.event_id NOT IN (SELECT value FROM json_each(?2))
           AND n.id > (SELECT MIN(d.id) FROM message_events d
                       WHERE d.workspace_id = ?1 AND d.session_key = n.session_key
                         AND d.event_id IN (SELECT value FROM json_each(?2)))",
    )?;
    Ok(statement
        .query_map(params![target.workspace_id, direct], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?)
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
        .query_map(params![target.workspace_id, event_ids], |row| {
            row.get::<_, Option<String>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids
        .into_iter()
        .flatten()
        .flat_map(|id| [format!("untrusted:{id}"), id])
        .collect())
}

/// Delete the stored tool-output documents of a deleted run: those its tool
/// result rows referenced and those stored with one of its input events
/// (`input_event_ids`: the run's and its spawned runs') as source.
/// A document is content-addressed, so one that a tool result row of a
/// surviving run still references stays (that row quotes it by id).
fn erase_tool_output_documents(
    tx: &Connection,
    input_event_ids: &[String],
    referenced: &[String],
) -> anyhow::Result<()> {
    let mut ids = referenced.to_vec();
    for input_event_id in input_event_ids {
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
            &format!(
                "SELECT EXISTS(SELECT 1 FROM documents WHERE document_id = ?1 AND source_kind IN {TOOL_OUTPUT_KINDS})"
            ),
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

/// Compaction summaries built from a deleted event: the summary text and its
/// memory row are removed, and the summary event is returned (for
/// tombstoning), one entry per compaction run not in `handled` yet; the runs
/// found are added to `handled`.
fn erase_compactions(
    tx: &Connection,
    target: &Target<'_>,
    ids: &str,
    handled: &mut Vec<String>,
    forgotten: &mut usize,
) -> anyhow::Result<Vec<Option<String>>> {
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
        if handled.contains(&run_id) {
            continue;
        }
        tx.execute(
            "UPDATE compaction_runs SET summary = '' WHERE workspace_id = ?1 AND run_id = ?2",
            params![target.workspace_id, run_id],
        )?;
        handled.push(run_id);
        if let Some(key) = memory_key {
            rf_forget::drop_prefix_rows_of(
                tx,
                "SELECT DISTINCT workspace_id, owner_id FROM memories WHERE key = ?1",
                params![key],
            )?;
            *forgotten = forgotten.saturating_add(tx.execute("DELETE FROM memories WHERE key = ?1", params![key])?);
        }
        summaries.push(summary_event);
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

/// Insert the content-free `turn.deleted` row of `subject` (one per run and
/// side; a repeat keeps the first) and return its event id.
fn insert_deleted_row(
    tx: &Connection,
    target: &Target<'_>,
    session_key: &str,
    side: Side,
    subject: &str,
    payload: &serde_json::Value,
    scope: Option<&str>,
) -> anyhow::Result<String> {
    let key = format!("rf-delete:{}:{subject}", side.label());
    tx.execute(
        "INSERT INTO message_events (
            event_id, idempotency_key, workspace_id, owner_id, source, session_key, role,
            event_type, content, raw_payload_json, created_at, updated_at, ma_scope_id
         ) VALUES (?1, ?2, ?3, ?4, 'console', ?5, 'event', 'turn.deleted', 'turn.deleted', ?6, ?7, ?7, ?8)
         ON CONFLICT DO NOTHING",
        params![
            uuid::Uuid::new_v4().to_string(),
            key,
            target.workspace_id,
            target.owner_id,
            session_key,
            payload.to_string(),
            target.now,
            scope,
        ],
    )?;
    Ok(tx.query_row(
        "SELECT event_id FROM message_events WHERE workspace_id=?1 AND owner_id=?2 AND idempotency_key=?3",
        params![target.workspace_id, target.owner_id, key],
        |r| r.get(0),
    )?)
}

/// The sync header fields of one `turn.deleted` row.
struct DeletedHeader<'a> {
    session_key: &'a str,
    agent: &'a str,
    incarnation: &'a str,
    run_id: Option<&'a str>,
    event_id: &'a str,
    /// The anchor message of the deleted run.
    message_id: &'a str,
}

/// Append the sync header of a `turn.deleted` row; its source is content-free.
fn append_deleted_header(tx: &Connection, target: &Target<'_>, fields: &DeletedHeader<'_>) -> anyhow::Result<()> {
    let mut header = super::sync_journal::Header::new(
        target.owner_id,
        "turn.deleted",
        fields.session_key,
        fields.agent,
        fields.incarnation,
        format!("event:{}", fields.event_id),
    );
    header.run_id = fields.run_id.map(str::to_owned);
    header.source_ref = format!("deleted:{}:{}", fields.event_id, fields.message_id);
    super::sync_journal::append(tx, &header)?;
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
    let event_id = insert_deleted_row(
        tx,
        target,
        target.session_key,
        side,
        subject,
        &payload,
        attempt.scope.as_deref(),
    )?;
    // The delete source is content-free and names its root attempt explicitly.
    if let Some((_, incarnation)) = scope_agent(tx, attempt.scope.as_deref())? {
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
        let fields = DeletedHeader {
            session_key: target.session_key,
            agent: &attempt.agent_key,
            incarnation: &incarnation,
            run_id: attempt.run_id.as_deref(),
            event_id: &event_id,
            message_id: &message_id,
        };
        append_deleted_header(tx, target, &fields)?;
    }
    Ok(())
}

/// The `turn.deleted` record of a run spawned by the deleted run, in the
/// spawned run's own session (side as the parent delete, no message id).
fn record_child_deleted(tx: &Connection, target: &Target<'_>, side: Side, child: &Child) -> anyhow::Result<()> {
    let agent = scope_agent(tx, child.scope.as_deref())?;
    let name = match agent.as_ref().map(|(key, _)| key.as_str()) {
        Some("m") | None => "main",
        Some(key) => key.strip_prefix("n:").unwrap_or(key),
    };
    let payload = serde_json::json!({
        "side": side.label(),
        "agent": name,
        "message_id": serde_json::Value::Null,
        "run_id": child.run_id,
    });
    let event_id = insert_deleted_row(
        tx,
        target,
        &child.session_key,
        side,
        &child.run_id,
        &payload,
        child.scope.as_deref(),
    )?;
    if let Some((_, incarnation)) = &agent {
        let anchor = child.first_user.as_deref().unwrap_or(&event_id);
        let fields = DeletedHeader {
            session_key: &child.session_key,
            agent: name,
            incarnation,
            run_id: Some(&child.run_id),
            event_id: &event_id,
            message_id: anchor,
        };
        append_deleted_header(tx, target, &fields)?;
    }
    Ok(())
}

/// The session preview is taken from the newest row that is not deleted.
pub(super) fn refresh_preview(tx: &Connection, session_key: &str) -> anyhow::Result<()> {
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
pub(super) fn mark_scrub(tx: &Connection, now: &str) -> anyhow::Result<()> {
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
