//! Age-based deletion of the message log, run by run. Each batch evaluates its
//! units, appends their rf retention notices and deletes them with their
//! derived copies in one IMMEDIATE transaction, so a crash leaves all or
//! nothing. The per-pass copies, the state row and the WAL scrub follow.
mod copies;

use super::{SqliteMemory, rf_delete, sync_journal, sync_retained};
use crate::memory::sync::{MessageTrim, TrimSkipped};
use crate::runtime::sqlite_pool::begin_immediate;
use rusqlite::{Connection, OptionalExtension, named_params, params};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A pass that started less than this many seconds ago does nothing.
const THROTTLE_SECS: i64 = 55 * 60;
/// Rows scanned per batch.
const SCAN_ROWS: i64 = 2_000;
/// Eligible units deleted per batch.
const BATCH_UNITS: usize = 200;

const INDEXES: &str = "
    CREATE INDEX IF NOT EXISTS idx_message_events_retention_run
        ON message_events(workspace_id, run_id) WHERE run_id IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_message_events_retention_cause
        ON message_events(causation_event_id) WHERE causation_event_id IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_message_events_retention_parent
        ON message_events(workspace_id, parent_run_id) WHERE parent_run_id IS NOT NULL;";

/// Rules E1, E2, E4 and E5 for the rows `r` of a unit. A row that matches any
/// of them keeps its whole unit.
const KEEP_ROW: &str = "julianday(r.created_at) >= julianday(:cutoff)
    OR EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = r.ma_scope_id)
    OR (r.event_type IN ('context.switch.created', 'context.prefix.recorded')
        AND r.session_key IS NOT NULL
        AND r.id = (SELECT MAX(s.id) FROM message_events s
                    WHERE s.workspace_id = r.workspace_id AND s.session_key = r.session_key
                      AND s.event_type = r.event_type)
        AND EXISTS(SELECT 1 FROM message_events n
                   WHERE n.workspace_id = r.workspace_id AND n.session_key = r.session_key
                     AND n.id > r.id AND julianday(n.created_at) >= julianday(:cutoff)))
    OR EXISTS(SELECT 1 FROM message_events c
              WHERE c.causation_event_id = r.event_id AND julianday(c.created_at) >= julianday(:cutoff))";

/// One deletion unit: every row of a run, or a runless row.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Unit {
    Run { workspace: String, run: String },
    Row { id: i64, workspace: String },
}

impl Unit {
    fn workspace(&self) -> &str {
        match self {
            Self::Run { workspace, .. } | Self::Row { workspace, .. } => workspace,
        }
    }
}

/// The ids to delete in one workspace, with its runs.
#[derive(Default)]
struct WorkspaceSet {
    runs: Vec<String>,
    events: Vec<String>,
}

/// Run a retention pass: state checks, one batch transaction per step, then the
/// per-pass copies. `cutoff` is the oldest kept instant.
pub(super) fn trim(
    memory: &SqliteMemory,
    cutoff: chrono::DateTime<chrono::Utc>,
    max_batches: u32,
) -> anyhow::Result<MessageTrim> {
    let cutoff_text = cutoff.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let now = chrono::Utc::now().timestamp();
    let mut trim = MessageTrim::default();
    {
        let writer = memory.pool.write();
        let tx = begin_immediate(&writer)?;
        let last: Option<i64> = tx
            .query_row(
                "SELECT last_run FROM message_retention_state WHERE singleton = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if last.is_some_and(|last| now - last < THROTTLE_SECS) {
            trim.skipped = TrimSkipped::Throttled;
            return Ok(trim);
        }
        let open: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_snapshots WHERE expires >= ?1)",
            params![now],
            |r| r.get(0),
        )?;
        if open {
            trim.skipped = TrimSkipped::SnapshotOpen;
            return Ok(trim);
        }
        tx.execute_batch(INDEXES)?;
        sync_journal::prune(&tx)?;
        tx.execute(
            "INSERT INTO message_retention_state(singleton, last_run) VALUES(1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET last_run = excluded.last_run",
            params![now],
        )?;
        tx.commit()?;
    }
    let mut cursor = 0_i64;
    for _ in 0..max_batches {
        if !batch(memory, &cutoff_text, &mut cursor, &mut trim)? {
            break;
        }
    }
    let rf_now = chrono::Utc::now().to_rfc3339();
    let passed = {
        let writer = memory.pool.write();
        let tx = begin_immediate(&writer)?;
        let counts = copies::pass(&tx, &cutoff_text)?;
        let deleted = counts.documents + counts.traces + counts.compactions;
        if deleted > 0 {
            rf_delete::mark_scrub(&tx, &rf_now)?;
            record_deleted(&tx, &cutoff_text, now, deleted)?;
        }
        tx.commit()?;
        counts
    };
    trim.documents = u64::try_from(passed.documents)?;
    trim.traces = u64::try_from(passed.traces)?;
    trim.compactions = u64::try_from(passed.compactions)?;
    if trim.events + trim.documents + trim.traces + trim.compactions > 0 {
        match rf_delete::scrub(&memory.pool) {
            Ok(true) => {}
            Ok(false) => tracing::debug!("message log retention WAL scrub waits for a reader"),
            Err(error) => tracing::warn!(
                error = %crate::agent::diagnostics::error_class(&error),
                "message log retention WAL scrub failed"
            ),
        }
    }
    Ok(trim)
}

/// One batch in its own IMMEDIATE transaction. Returns `false` when the scan
/// found no row past `cursor` (the pass is complete).
fn batch(memory: &SqliteMemory, cutoff: &str, cursor: &mut i64, trim: &mut MessageTrim) -> anyhow::Result<bool> {
    let writer = memory.pool.write();
    let tx = begin_immediate(&writer)?;
    let rows = {
        let mut statement = tx.prepare(
            "SELECT e.id, e.workspace_id, e.run_id FROM message_events e
             WHERE e.id > ?1 AND julianday(e.created_at) < julianday(?2)
             ORDER BY e.id LIMIT ?3",
        )?;
        statement
            .query_map(params![*cursor, cutoff, SCAN_ROWS], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if rows.is_empty() {
        return Ok(false);
    }
    trim.batches += 1;
    let mut units = Vec::new();
    let mut decided: HashMap<Unit, bool> = HashMap::new();
    let mut next = *cursor;
    for (id, workspace, run) in rows {
        let unit = match run {
            Some(run) => Unit::Run { workspace, run },
            None => Unit::Row { id, workspace },
        };
        if !decided.contains_key(&unit) {
            let keep = eligible(&tx, &unit, cutoff)?;
            if keep && units.len() >= BATCH_UNITS {
                break;
            }
            decided.insert(unit.clone(), keep);
            if keep {
                units.push(unit);
            }
        }
        next = id;
    }
    if units.is_empty() {
        tx.commit()?;
        *cursor = next;
        return Ok(true);
    }

    let mut ids: Vec<i64> = Vec::new();
    let mut all_events: Vec<String> = Vec::new();
    let mut sessions: BTreeSet<String> = BTreeSet::new();
    let mut workspaces: BTreeMap<String, WorkspaceSet> = BTreeMap::new();
    for unit in &units {
        let set = workspaces.entry(unit.workspace().to_owned()).or_default();
        if let Unit::Run { run, .. } = unit {
            set.runs.push(run.clone());
        }
        for (id, event, session) in members(&tx, unit)? {
            ids.push(id);
            all_events.push(event.clone());
            set.events.push(event);
            if let Some(session) = session {
                sessions.insert(session);
            }
        }
    }
    let events_json = serde_json::to_string(&all_events)?;
    // Notices first, while the rows and their attempts still exist; only the
    // ids of this batch's units are passed.
    let headers = sync_retained::notice_headers(&tx, &all_events)?;
    trim.notices += sync_retained::append_notices(&tx, &headers)?;
    trim.turns += u64::try_from(copies::drop_turns(&tx, &events_json)?)?;
    for session in &sessions {
        copies::refresh_session(&tx, session)?;
    }
    for (workspace, set) in &workspaces {
        let runs_json = serde_json::to_string(&set.runs)?;
        let set_events = serde_json::to_string(&set.events)?;
        trim.attempts += u64::try_from(copies::drop_attempts(&tx, workspace, &runs_json, &set_events)?)?;
    }
    trim.links += u64::try_from(copies::drop_links(&tx, &events_json)?)?;
    let deleted = tx.execute(
        "DELETE FROM message_events WHERE id IN (SELECT value FROM json_each(?1))",
        params![serde_json::to_string(&ids)?],
    )?;
    trim.events += u64::try_from(deleted)?;
    trim.runs += u64::try_from(units.len())?;
    let now = chrono::Utc::now();
    rf_delete::mark_scrub(&tx, &now.to_rfc3339())?;
    record_deleted(&tx, cutoff, now.timestamp(), deleted)?;
    tx.commit()?;
    *cursor = next;
    Ok(true)
}

/// Rules E1-E7 for one unit, evaluated inside the batch transaction. A run
/// also needs E3 (no unsettled attempt) and E7 (no descendant run).
fn eligible(tx: &Connection, unit: &Unit, cutoff: &str) -> anyhow::Result<bool> {
    let keep = match unit {
        Unit::Run { workspace, run } => {
            let sql = format!(
                "SELECT NOT EXISTS(SELECT 1 FROM message_events r
                     WHERE r.workspace_id = :workspace AND r.run_id = :run AND ({KEEP_ROW}))
                 AND NOT EXISTS(SELECT 1 FROM rf_message_attempts a
                     WHERE a.workspace_id = :workspace AND a.run_id = :run AND (a.erase_pending = 1
                        OR (a.interrupted = 0 AND NOT EXISTS(SELECT 1 FROM message_events t
                              WHERE t.workspace_id = a.workspace_id AND t.owner_id = a.owner_id
                                AND t.idempotency_key IN ('turn:' || a.run_id || ':final',
                                                          'turn:' || a.run_id || ':admission-failed')))))
                 AND NOT EXISTS(SELECT 1 FROM message_events d
                     WHERE d.workspace_id = :workspace AND d.parent_run_id = :run
                       AND COALESCE(d.run_id, '') <> :run)"
            );
            tx.query_row(
                &sql,
                named_params! {":workspace": workspace, ":run": run, ":cutoff": cutoff},
                |r| r.get::<_, bool>(0),
            )?
        }
        Unit::Row { id, .. } => {
            let sql = format!("SELECT NOT EXISTS(SELECT 1 FROM message_events r WHERE r.id = :id AND ({KEEP_ROW}))");
            tx.query_row(&sql, named_params! {":id": id, ":cutoff": cutoff}, |r| {
                r.get::<_, bool>(0)
            })?
        }
    };
    Ok(keep)
}

/// Id, event id and session key of every row of the unit.
fn members(tx: &Connection, unit: &Unit) -> anyhow::Result<Vec<(i64, String, Option<String>)>> {
    let read = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, String, Option<String>)> {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    };
    let rows = match unit {
        Unit::Run { workspace, run } => {
            let mut statement = tx.prepare(
                "SELECT id, event_id, session_key FROM message_events
                 WHERE workspace_id = ?1 AND run_id = ?2 ORDER BY id",
            )?;
            statement
                .query_map(params![workspace, run], read)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        Unit::Row { id, .. } => {
            let mut statement = tx.prepare("SELECT id, event_id, session_key FROM message_events WHERE id = ?1")?;
            statement
                .query_map(params![id], read)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
    };
    Ok(rows)
}

/// Writes the state row: `cutoff` only ever rises (fixed-width text, so the
/// comparison is chronological) and `deleted_total` accumulates. Callers invoke
/// it only when rows were deleted.
fn record_deleted(tx: &Connection, cutoff: &str, now: i64, deleted: usize) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO message_retention_state(singleton, last_run, cutoff, deleted_total) VALUES(1, ?1, ?2, ?3)
         ON CONFLICT(singleton) DO UPDATE SET
             cutoff = CASE WHEN cutoff IS NULL OR cutoff < excluded.cutoff THEN excluded.cutoff ELSE cutoff END,
             deleted_total = deleted_total + excluded.deleted_total",
        params![now, cutoff, i64::try_from(deleted)?],
    )?;
    Ok(())
}
