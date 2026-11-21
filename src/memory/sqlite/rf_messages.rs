//! SQL for `rf_message_attempts` (AC-1b2a): one row per admitted attempt of
//! an rf message key, written in the transaction of the attempt's input
//! event. No expiry runs here: rows are kept at least 7 days by having no
//! deletion path but the incarnation purge (MA-01), which reaches them
//! through `ma_scope_id`. Growth is one small row per rf message attempt.

use crate::memory::MessageEvent;
use crate::memory::rf_messages::{RfAdmission, RfAdmissionRefused, RfAttempt, RfAttemptLookup, RfRun, run_terminal};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

/// Idempotent DDL. Called from schema init.
pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS rf_message_attempts (
            workspace_id TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            session_key TEXT NOT NULL,
            agent_key TEXT NOT NULL,
            incarnation TEXT NOT NULL,
            client_message_id TEXT NOT NULL,
            attempt INTEGER NOT NULL CHECK (attempt >= 1),
            request_hash TEXT NOT NULL,
            run_id TEXT,
            job_id TEXT,
            message_event_id TEXT NOT NULL,
            lifetime TEXT,
            interrupted INTEGER NOT NULL DEFAULT 0 CHECK (interrupted IN (0, 1)),
            ma_scope_id TEXT REFERENCES agent_scopes(scope_id),
            created_at TEXT NOT NULL,
            PRIMARY KEY (workspace_id, owner_id, session_key, agent_key, incarnation, client_message_id, attempt),
            CHECK ((run_id IS NULL) = (job_id IS NULL))
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_rf_message_attempts_run
            ON rf_message_attempts(workspace_id, run_id) WHERE run_id IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_rf_message_attempts_job
            ON rf_message_attempts(workspace_id, job_id) WHERE job_id IS NOT NULL;",
    )?;
    Ok(())
}

/// Whether the run has a terminal row: its `turn.finalized` or its
/// `turn.admission.failed` event.
fn has_terminal(conn: &Connection, workspace_id: &str, owner_id: &str, run_id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM message_events
             WHERE workspace_id = ?1 AND owner_id = ?2 AND idempotency_key IN (?3, ?4))",
        params![
            workspace_id,
            owner_id,
            format!("turn:{run_id}:final"),
            format!("turn:{run_id}:admission-failed")
        ],
        |row| row.get(0),
    )
}

/// Mark the run `interrupted` while it has no terminal row; `false` when it
/// has one (or no attempt holds the run).
fn interrupt(conn: &Connection, workspace_id: &str, owner_id: &str, run_id: &str) -> anyhow::Result<bool> {
    if has_terminal(conn, workspace_id, owner_id, run_id)? {
        return Ok(false);
    }
    let changed = conn.execute(
        "UPDATE rf_message_attempts SET interrupted = 1
         WHERE workspace_id = ?1 AND owner_id = ?2 AND run_id = ?3",
        params![workspace_id, owner_id, run_id],
    )?;
    Ok(changed == 1)
}

/// Write the attempt row inside the input's append transaction, after the
/// input event row. A refusal is returned as an error, so the caller's
/// transaction rolls back and the input event is not written either.
pub(super) fn admit(
    tx: &Connection,
    admission: &RfAdmission,
    event: &MessageEvent,
    inserted: bool,
    lifetime: Option<&str>,
) -> anyhow::Result<()> {
    let key = &admission.key;
    anyhow::ensure!(
        inserted && event.workspace_id == key.workspace_id && event.owner_id.as_deref() == Some(key.owner_id.as_str()),
        "an admitted rf input must be a new event of the key's workspace and owner"
    );
    if let Some(previous) = admission.interrupts.as_deref()
        && !interrupt(tx, &key.workspace_id, &key.owner_id, previous)?
    {
        return Err(RfAdmissionRefused::Stale.into());
    }
    let scope: Option<String> = tx.query_row(
        "SELECT ma_scope_id FROM message_events WHERE event_id = ?1",
        params![event.event_id],
        |row| row.get(0),
    )?;
    let written = tx.execute(
        "INSERT INTO rf_message_attempts (
            workspace_id, owner_id, session_key, agent_key, incarnation, client_message_id, attempt,
            request_hash, run_id, job_id, message_event_id, lifetime, ma_scope_id, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT DO NOTHING",
        params![
            key.workspace_id,
            key.owner_id,
            key.session_key,
            key.agent_key,
            key.incarnation,
            key.client_message_id,
            admission.attempt,
            admission.request_hash,
            admission.run.as_ref().map(|run| run.run_id.as_str()),
            admission.run.as_ref().map(|run| run.job_id.as_str()),
            event.event_id,
            lifetime,
            scope,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    if written == 0 {
        return Err(RfAdmissionRefused::Taken.into());
    }
    Ok(())
}

/// The attempt columns, then the payloads of the run's two terminal rows.
macro_rules! attempt_select {
    ($condition:literal) => {
        concat!(
            "SELECT a.owner_id, a.session_key, a.agent_key, a.attempt, a.request_hash, a.run_id, a.job_id,
                    a.message_event_id, a.lifetime, a.interrupted,
                    (SELECT e.raw_payload_json FROM message_events e WHERE e.workspace_id = a.workspace_id
                        AND e.owner_id = a.owner_id AND e.idempotency_key = 'turn:' || a.run_id || ':final'),
                    (SELECT e.raw_payload_json FROM message_events e WHERE e.workspace_id = a.workspace_id
                        AND e.owner_id = a.owner_id AND e.idempotency_key = 'turn:' || a.run_id || ':admission-failed')
             FROM rf_message_attempts a WHERE a.workspace_id = ?1 AND ",
            $condition,
            " ORDER BY a.attempt DESC LIMIT 1"
        )
    };
}

type AttemptRow = (
    RfAttempt,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
);

fn attempt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptRow> {
    Ok((
        RfAttempt {
            owner_id: row.get(0)?,
            session_key: row.get(1)?,
            agent_key: row.get(2)?,
            attempt: row.get(3)?,
            request_hash: row.get(4)?,
            run: None,
            message_event_id: row.get(7)?,
            lifetime: row.get(8)?,
            terminal: None,
        },
        row.get(5)?,
        row.get(6)?,
        row.get(10)?,
        row.get(11)?,
        row.get(9)?,
    ))
}

/// Read one attempt and its run's terminal in one statement.
pub(super) fn read(
    conn: &Connection,
    workspace_id: &str,
    lookup: &RfAttemptLookup,
) -> anyhow::Result<Option<RfAttempt>> {
    let row = match lookup {
        RfAttemptLookup::Latest(key) => conn
            .query_row(
                attempt_select!(
                    "a.owner_id = ?2 AND a.session_key = ?3 AND a.agent_key = ?4 AND a.incarnation = ?5
                     AND a.client_message_id = ?6"
                ),
                params![
                    workspace_id,
                    key.owner_id,
                    key.session_key,
                    key.agent_key,
                    key.incarnation,
                    key.client_message_id
                ],
                attempt_row,
            )
            .optional()?,
        RfAttemptLookup::Run {
            owner_id,
            session_key,
            run_id,
        } => conn
            .query_row(
                attempt_select!("a.owner_id = ?2 AND a.session_key = ?3 AND a.run_id = ?4"),
                params![workspace_id, owner_id, session_key, run_id],
                attempt_row,
            )
            .optional()?,
        RfAttemptLookup::Job(job_id) => conn
            .query_row(
                attempt_select!("a.job_id = ?2"),
                params![workspace_id, job_id],
                attempt_row,
            )
            .optional()?,
    };
    let Some((mut attempt, run_id, job_id, final_payload, admission_failure, interrupted)) = row else {
        return Ok(None);
    };
    attempt.run = match (run_id, job_id) {
        (Some(run_id), Some(job_id)) => Some(RfRun { run_id, job_id }),
        (None, None) => None,
        _ => anyhow::bail!("an rf attempt row holds a run without its job"),
    };
    if attempt.run.is_some() {
        attempt.terminal = run_terminal(final_payload.as_deref(), admission_failure.as_deref(), interrupted)?;
    }
    Ok(Some(attempt))
}

/// [`interrupt`] in its own IMMEDIATE transaction.
pub(super) fn interrupt_run(
    conn: &Connection,
    workspace_id: &str,
    owner_id: &str,
    run_id: &str,
) -> anyhow::Result<bool> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let marked = interrupt(&tx, workspace_id, owner_id, run_id)?;
    tx.commit()?;
    Ok(marked)
}
