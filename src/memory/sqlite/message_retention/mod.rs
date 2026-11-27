//! Age-based deletion of the message log, run by run. Each batch reads its
//! candidate units on a reader connection; one IMMEDIATE transaction re-checks
//! them, appends their rf retention notices and deletes them with their
//! derived copies, so a crash leaves all or nothing. The per-pass copies, the state row and the WAL scrub follow.
mod copies;

use super::{SqliteMemory, rf_delete, rf_messages::RESTART_INTERRUPTED, sync_journal, sync_retained};
use crate::memory::sync::{MessageTrim, TrimMode, TrimSkipped};
use crate::runtime::sqlite_pool::begin_immediate;
use rusqlite::{Connection, OptionalExtension, named_params, params};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A start-up pass does nothing when a pass completed less than this many seconds ago.
const THROTTLE_SECS: i64 = 55 * 60;
/// A start-up pass does nothing when any pass started less than this many seconds ago,
/// so a pass that keeps failing is retried at most every 15 minutes, not at every start.
const BACKOFF_SECS: i64 = 15 * 60;
/// Effective minimum retention: the rf journal keeps 14 days, plus one day of margin.
const MIN_RETENTION_DAYS: u64 = 15;
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
        ON message_events(workspace_id, parent_run_id) WHERE parent_run_id IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_message_events_retention_task_run
        ON message_events(workspace_id, (CASE WHEN json_valid(raw_payload_json)
            THEN json_extract(raw_payload_json, '$.task_run_id') END))
        WHERE role = 'user';";

/// Task-echo keep rule of a run unit, evaluated once per unit: a `user` row
/// outside the unit restates one of the unit's `user` rows and names the unit's
/// run as its `task_run_id` (the conditions of `TASK_ECHO_PREDICATE_E` with
/// `p` in the unit). The `task_run_id` expression is the exact text of
/// `idx_message_events_retention_task_run`, so the lookup uses that index; the
/// `json_valid` guard makes a malformed payload a non-echo instead of an error.
/// Mutual references between two units would keep both forever; an echo is
/// written after the row it restates (`e.id > p.id`), so this does not occur.
const ECHO_KEEP: &str = "EXISTS(SELECT 1 FROM message_events p
                 JOIN message_events e
                   ON e.workspace_id = :workspace AND e.role = 'user'
                  AND (CASE WHEN json_valid(e.raw_payload_json)
                       THEN json_extract(e.raw_payload_json, '$.task_run_id') END) = :run
                 WHERE p.workspace_id = :workspace AND p.run_id = :run AND p.role = 'user'
                   AND e.owner_id = p.owner_id AND e.session_key = p.session_key
                   AND e.content_hash = p.content_hash AND e.id > p.id
                   AND COALESCE(e.run_id, '') <> :run)";

/// Keep rules for the rows `r` of a unit (E1, E2, E4, E5); a run also has
/// [`ECHO_KEEP`]. A row that matches any of them keeps its whole unit. A
/// `created_at` that does not parse counts as newer (kept). E5: mutual
/// causation between two units would keep both forever; causation always
/// points to an earlier event, so this does not occur.
fn keep_row(run_unit: bool) -> String {
    let causes = if run_unit {
        "NOT (c.workspace_id = :workspace AND COALESCE(c.run_id, '') = :run)"
    } else {
        "c.id <> r.id"
    };
    format!(
        "julianday(r.created_at) IS NULL OR julianday(r.created_at) >= julianday(:cutoff)
    OR EXISTS(SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = r.ma_scope_id)
    OR (r.event_type IN ('context.switch.created', 'context.prefix.recorded')
        AND r.session_key IS NOT NULL
        AND r.id = (SELECT MAX(s.id) FROM message_events s
                    WHERE s.workspace_id = r.workspace_id AND s.session_key = r.session_key
                      AND s.event_type = r.event_type)
        AND EXISTS(SELECT 1 FROM message_events n
                   WHERE n.workspace_id = r.workspace_id AND n.session_key = r.session_key
                     AND n.id > r.id
                     AND (julianday(n.created_at) IS NULL OR julianday(n.created_at) >= julianday(:cutoff))))
    OR EXISTS(SELECT 1 FROM message_events c WHERE c.causation_event_id = r.event_id AND {causes})"
    )
}

/// E8 for a run unit: a pending recovery attestation names one of its rows.
const PENDING_ATTESTATION_RUN: &str = "
                 AND NOT EXISTS(SELECT 1 FROM message_events r
                     JOIN recovery_attestations a ON (a.input_event_id = r.event_id OR a.terminal_event_id = r.event_id)
                     WHERE r.workspace_id = :workspace AND r.run_id = :run AND a.state = 'pending')";

/// E8 for a runless unit.
const PENDING_ATTESTATION_ROW: &str = "
                 AND NOT EXISTS(SELECT 1 FROM message_events r
                     JOIN recovery_attestations a ON (a.input_event_id = r.event_id OR a.terminal_event_id = r.event_id)
                     WHERE r.id = :id AND a.state = 'pending')";

/// Which optional tables exist in this store, checked once per batch.
#[derive(Clone, Copy)]
struct Tables {
    ingress: bool,
    attestations: bool,
}

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
    mode: TrimMode,
) -> anyhow::Result<MessageTrim> {
    // Store-side floor: a cutoff later than now minus the 15-day sync window is clamped.
    let floor = chrono::Utc::now()
        .checked_sub_days(chrono::Days::new(MIN_RETENTION_DAYS))
        .ok_or_else(|| anyhow::anyhow!("retention floor out of range"))?;
    let cutoff = cutoff.min(floor);
    let cutoff_text = cutoff.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let now = chrono::Utc::now().timestamp();
    let mut trim = MessageTrim::default();
    {
        let writer = memory.pool.write();
        let tx = begin_immediate(&writer)?;
        ensure_attempt_column(&tx)?;
        let state: Option<(i64, Option<i64>)> = tx
            .query_row(
                "SELECT last_run, last_attempt FROM message_retention_state WHERE singleton = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if mode == TrimMode::Startup
            && state.is_some_and(|(last_run, last_attempt)| {
                now - last_run < THROTTLE_SECS || last_attempt.is_some_and(|at| now - at < BACKOFF_SECS)
            })
        {
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
        // Recorded before any delete, so a pass that fails later still starts the back-off.
        tx.execute(
            "INSERT INTO message_retention_state(singleton, last_run, last_attempt) VALUES(1, 0, ?1)
             ON CONFLICT(singleton) DO UPDATE SET last_attempt = excluded.last_attempt",
            params![now],
        )?;
        tx.execute_batch(INDEXES)?;
        sync_journal::prune(&tx)?;
        tx.commit()?;
    }
    let mut cursor = 0_i64;
    let mut resweep = true;
    for _ in 0..max_batches {
        if batch(memory, &cutoff_text, &mut cursor, &mut trim)? {
            continue;
        }
        // A unit kept only because a later row named it as its cause (E5) is
        // eligible once that row is gone: a sweep that deleted rows is
        // followed by one more sweep from the start.
        if resweep && trim.events > 0 {
            resweep = false;
            cursor = 0;
            continue;
        }
        break;
    }
    let rf_now = chrono::Utc::now().to_rfc3339();
    let passed = {
        let writer = memory.pool.write();
        let tx = begin_immediate(&writer)?;
        let counts = copies::pass(&tx, &cutoff_text)?;
        if counts.documents + counts.traces + counts.compactions > 0 {
            rf_delete::mark_scrub(&tx, &rf_now)?;
        }
        // Only a pass that got this far counts for the start-up throttle; a
        // failed pass leaves `last_run` as it was, so the next start retries.
        tx.execute(
            "INSERT INTO message_retention_state(singleton, last_run) VALUES(1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET last_run = excluded.last_run",
            params![now],
        )?;
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

/// Candidate units of one batch, read on a reader connection.
struct Scan {
    candidates: Vec<Unit>,
    more: bool,
    next: i64,
}

/// Reads the rows older than the cutoff after `cursor` (at most `SCAN_ROWS`) and
/// decides each unit with [`eligible`] on this reader, up to `BATCH_UNITS`
/// candidates. The decision is provisional: the writer re-checks every candidate.
/// Returns `None` when no row is left (the sweep is complete).
fn scan(conn: &Connection, cutoff: &str, cursor: i64) -> anyhow::Result<Option<Scan>> {
    let tables = tables(conn)?;
    let rows = {
        let mut statement = conn.prepare(
            "SELECT e.id, e.workspace_id, e.run_id FROM message_events e
             WHERE e.id > ?1 AND julianday(e.created_at) < julianday(?2)
             ORDER BY e.id LIMIT ?3",
        )?;
        statement
            .query_map(params![cursor, cutoff, SCAN_ROWS], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if rows.is_empty() {
        return Ok(None);
    }
    let mut more = i64::try_from(rows.len())? >= SCAN_ROWS;
    let mut candidates = Vec::new();
    let mut decided: HashMap<Unit, bool> = HashMap::new();
    let mut next = cursor;
    for (id, workspace, run) in rows {
        let unit = match run {
            Some(run) => Unit::Run { workspace, run },
            None => Unit::Row { id, workspace },
        };
        if !decided.contains_key(&unit) {
            let keep = eligible(conn, &unit, cutoff, tables)?;
            if keep && candidates.len() >= BATCH_UNITS {
                more = true;
                break;
            }
            decided.insert(unit.clone(), keep);
            if keep {
                candidates.push(unit);
            }
        }
        next = id;
    }
    Ok(Some(Scan { candidates, more, next }))
}

/// The optional tables of this store, read on the given connection.
fn tables(conn: &Connection) -> anyhow::Result<Tables> {
    Ok(Tables {
        ingress: copies::has_table(conn, "ingress_recovery")?,
        attestations: copies::has_table(conn, "recovery_attestations")?,
    })
}

/// One batch. The candidates are read without the write lock; then one IMMEDIATE
/// transaction re-checks each candidate (its rows must still exist and E1-E8 must
/// still hold) and deletes those that pass, so the delete decision is made under
/// the writer. Returns `false` when the scan reached the last row older than the
/// cutoff (the sweep is complete).
fn batch(memory: &SqliteMemory, cutoff: &str, cursor: &mut i64, trim: &mut MessageTrim) -> anyhow::Result<bool> {
    let scanned = {
        let reader = memory.pool.read()?;
        scan(reader.connection()?, cutoff, *cursor)?
    };
    let Some(Scan { candidates, more, next }) = scanned else {
        return Ok(false);
    };
    trim.batches += 1;
    let writer = memory.pool.write();
    let tx = begin_immediate(&writer)?;
    let tables = tables(&tx)?;
    let mut units = Vec::new();
    for unit in candidates {
        if !members(&tx, &unit)?.is_empty() && eligible(&tx, &unit, cutoff, tables)? {
            units.push(unit);
        }
    }
    if units.is_empty() {
        tx.commit()?;
        *cursor = next;
        return Ok(more);
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
    let ids_json = serde_json::to_string(&ids)?;
    if tables.ingress {
        copies::drop_ingress(&tx, &events_json, &ids_json)?;
    }
    let deleted = tx.execute(
        "DELETE FROM message_events WHERE id IN (SELECT value FROM json_each(?1))",
        params![ids_json],
    )?;
    trim.events += u64::try_from(deleted)?;
    trim.runs += u64::try_from(units.len())?;
    let now = chrono::Utc::now();
    if deleted > 0 {
        rf_delete::mark_scrub(&tx, &now.to_rfc3339())?;
        record_deleted(&tx, cutoff, deleted)?;
    }
    tx.commit()?;
    *cursor = next;
    Ok(more)
}

/// Rules E1-E8 for one unit, evaluated by the scan and again in the batch
/// transaction, which alone decides the delete. A run
/// also needs E3 (no unsettled attempt) and E7 (no descendant run). When the
/// lazily created `ingress_recovery` table exists (`ingress`), E3 also covers
/// ingress recovery: a unit whose input or evidence row still has an
/// unreconciled recovery record is kept, so a restart can still recover it.
/// E8: when `recovery_attestations` exists (`attestations`), a unit with a
/// pending attestation naming one of its rows is kept; the attestation's
/// foreign keys to `message_events` cascade on delete.
fn eligible(tx: &Connection, unit: &Unit, cutoff: &str, tables: Tables) -> anyhow::Result<bool> {
    let keep = match unit {
        Unit::Run { workspace, run } => {
            let keep = keep_row(true);
            let mut recovering = if tables.ingress {
                "AND NOT EXISTS(SELECT 1 FROM message_events r
                     JOIN ingress_recovery i ON (i.input_event_id = r.event_id OR i.evidence_event_id = r.event_id)
                     WHERE r.workspace_id = :workspace AND r.run_id = :run AND i.state <> 'reconciled')"
                    .to_owned()
            } else {
                String::new()
            };
            if tables.attestations {
                recovering.push_str(PENDING_ATTESTATION_RUN);
            }
            let sql = format!(
                "SELECT NOT EXISTS(SELECT 1 FROM message_events r
                     WHERE r.workspace_id = :workspace AND r.run_id = :run AND ({keep}))
                 AND NOT {ECHO_KEEP}
                 AND NOT EXISTS(SELECT 1 FROM rf_message_attempts a
                     WHERE a.workspace_id = :workspace AND a.run_id = :run AND (a.erase_pending = 1
                        OR (a.interrupted IN (0, {RESTART_INTERRUPTED}) AND NOT EXISTS(SELECT 1 FROM message_events t
                              WHERE t.workspace_id = a.workspace_id AND t.owner_id = a.owner_id
                                AND t.idempotency_key IN ('turn:' || a.run_id || ':final',
                                                          'turn:' || a.run_id || ':admission-failed')))))
                 AND NOT EXISTS(SELECT 1 FROM message_events d
                     WHERE d.workspace_id = :workspace AND d.parent_run_id = :run
                       AND COALESCE(d.run_id, '') <> :run)
                 {recovering}"
            );
            tx.query_row(
                &sql,
                named_params! {":workspace": workspace, ":run": run, ":cutoff": cutoff},
                |r| r.get::<_, bool>(0),
            )?
        }
        Unit::Row { id, .. } => {
            let keep = keep_row(false);
            let mut recovering = if tables.ingress {
                "AND NOT EXISTS(SELECT 1 FROM message_events r
                     JOIN ingress_recovery i ON (i.input_event_id = r.event_id OR i.evidence_event_id = r.event_id)
                     WHERE r.id = :id AND i.state <> 'reconciled')"
                    .to_owned()
            } else {
                String::new()
            };
            if tables.attestations {
                recovering.push_str(PENDING_ATTESTATION_ROW);
            }
            let sql =
                format!("SELECT NOT EXISTS(SELECT 1 FROM message_events r WHERE r.id = :id AND ({keep})) {recovering}");
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

/// Adds the `last_attempt` column to a state row created before it existed.
/// Literals only; no request data reaches the DDL.
fn ensure_attempt_column(tx: &Connection) -> anyhow::Result<()> {
    let present: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('message_retention_state') WHERE name = 'last_attempt')",
        [],
        |row| row.get(0),
    )?;
    if !present {
        tx.execute_batch("ALTER TABLE message_retention_state ADD COLUMN last_attempt INTEGER")?;
    }
    Ok(())
}

/// Writes the state row: `cutoff` only ever rises (fixed-width text, so the
/// comparison is chronological) and `deleted_total` accumulates. Callers invoke
/// it only when rows were deleted. `last_run` is left to the end of the pass; a
/// first row starts at 0 (no completed pass), so a pass that fails after this
/// batch is not throttled by the 55-minute rule. `last_attempt` is written at
/// the start of every pass and is not touched here.
fn record_deleted(tx: &Connection, cutoff: &str, deleted: usize) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO message_retention_state(singleton, last_run, cutoff, deleted_total) VALUES(1, 0, ?1, ?2)
         ON CONFLICT(singleton) DO UPDATE SET
             cutoff = CASE WHEN cutoff IS NULL OR cutoff < excluded.cutoff THEN excluded.cutoff ELSE cutoff END,
             deleted_total = deleted_total + excluded.deleted_total",
        params![cutoff, i64::try_from(deleted)?],
    )?;
    Ok(())
}
