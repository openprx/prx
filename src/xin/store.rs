//! SQLite persistence for the xin (心) autonomous task engine.
//!
//! DB path: `{workspace}/xin/tasks.db`
//! Pattern follows `cron/store.rs`: `with_connection()` + `rusqlite::params!`.

use crate::config::Config;
use crate::xin::types::{ExecutionMode, NewXinTask, TaskKind, TaskPriority, TaskStatus, XinTask, XinTaskPatch};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use uuid::Uuid;

const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const TRUNCATED_MARKER: &str = "\n...[truncated]";

// ── CRUD ────────────────────────────────────────────────────────────────

/// Insert a new task and return the persisted record.
pub fn add_task(config: &Config, new: &NewXinTask) -> Result<XinTask> {
    // Enforce max_tasks capacity
    let max = config.xin.max_tasks;
    if max > 0 {
        let current = with_connection(config, |conn| {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM xin_tasks", [], |row| row.get(0))?;
            Ok(count)
        })?;
        if current >= max as i64 {
            anyhow::bail!("Xin task limit reached ({max}). Remove completed/disabled tasks first.");
        }
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let next_run = if new.recurring && new.interval_secs > 0 {
        now + chrono::Duration::seconds(i64::min(new.interval_secs as i64, i64::MAX))
    } else {
        now
    };

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO xin_tasks (
                id, name, description, kind, status, priority, execution_mode,
                payload, recurring, interval_secs, created_at, updated_at,
                last_run_at, next_run_at, last_status, last_output,
                run_count, fail_count, max_failures, enabled
             ) VALUES (
                ?1, ?2, ?3, ?4, 'pending', ?5, ?6,
                ?7, ?8, ?9, ?10, ?11,
                NULL, ?12, NULL, NULL,
                0, 0, ?13, 1
             )",
            params![
                id,
                new.name,
                new.description,
                new.kind.as_str(),
                new.priority.as_i32(),
                new.execution_mode.as_str(),
                new.payload,
                if new.recurring { 1 } else { 0 },
                i64::try_from(new.interval_secs).unwrap_or(i64::MAX),
                now.to_rfc3339(),
                now.to_rfc3339(),
                next_run.to_rfc3339(),
                i64::from(new.max_failures),
            ],
        )
        .context("Failed to insert xin task")?;
        Ok(())
    })?;

    get_task(config, &id)
}

/// Retrieve a single task by ID.
pub fn get_task(config: &Config, task_id: &str) -> Result<XinTask> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(SELECT_ALL_COLUMNS)?;
        let mut rows = stmt.query(params![task_id])?;
        if let Some(row) = rows.next()? {
            map_task_row(row).map_err(Into::into)
        } else {
            anyhow::bail!("Xin task '{task_id}' not found")
        }
    })
}

/// List all tasks, ordered by priority DESC then next_run ASC.
pub fn list_tasks(config: &Config) -> Result<Vec<XinTask>> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, kind, status, priority, execution_mode,
                    payload, recurring, interval_secs, created_at, updated_at,
                    last_run_at, next_run_at, last_status, last_output,
                    run_count, fail_count, max_failures, enabled
             FROM xin_tasks
             ORDER BY priority DESC, next_run_at ASC",
        )?;
        let rows = stmt.query_map([], map_task_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
}

/// Return enabled, pending tasks whose `next_run_at <= now`, sorted by priority DESC.
pub fn due_tasks(config: &Config, now: DateTime<Utc>, limit: usize) -> Result<Vec<XinTask>> {
    let lim = i64::try_from(limit.max(1)).context("due_tasks limit overflows i64")?;
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, kind, status, priority, execution_mode,
                    payload, recurring, interval_secs, created_at, updated_at,
                    last_run_at, next_run_at, last_status, last_output,
                    run_count, fail_count, max_failures, enabled
             FROM xin_tasks
             WHERE enabled = 1
               AND status IN ('pending', 'stale')
               AND next_run_at <= ?1
             ORDER BY priority DESC, next_run_at ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now.to_rfc3339(), lim], map_task_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
}

/// Apply a partial update to an existing task.
pub fn update_task(config: &Config, task_id: &str, patch: &XinTaskPatch) -> Result<XinTask> {
    let mut task = get_task(config, task_id)?;
    let now = Utc::now();

    if let Some(ref name) = patch.name {
        task.name = name.clone();
    }
    if let Some(ref desc) = patch.description {
        task.description = Some(desc.clone());
    }
    if let Some(priority) = patch.priority {
        task.priority = priority;
    }
    if let Some(ref payload) = patch.payload {
        task.payload = payload.clone();
    }
    if let Some(interval) = patch.interval_secs {
        task.interval_secs = interval;
    }
    if let Some(enabled) = patch.enabled {
        task.enabled = enabled;
    }
    if let Some(max_failures) = patch.max_failures {
        task.max_failures = max_failures;
    }
    // Optimistic concurrency: capture the DB-read updated_at BEFORE overwriting.
    let previous_updated_at = task.updated_at;
    task.updated_at = now;
    with_connection(config, |conn| {
        let changed = conn
            .execute(
                "UPDATE xin_tasks
             SET name = ?1, description = ?2, priority = ?3, payload = ?4,
                 interval_secs = ?5, enabled = ?6, max_failures = ?7, updated_at = ?8
             WHERE id = ?9 AND updated_at = ?10",
                params![
                    task.name,
                    task.description,
                    task.priority.as_i32(),
                    task.payload,
                    i64::try_from(task.interval_secs).unwrap_or(i64::MAX),
                    if task.enabled { 1 } else { 0 },
                    i64::from(task.max_failures),
                    now.to_rfc3339(),
                    task_id,
                    previous_updated_at.to_rfc3339(),
                ],
            )
            .context("Failed to update xin task")?;
        if changed == 0 {
            anyhow::bail!("xin task '{task_id}' was modified by another process (optimistic concurrency conflict)");
        }
        Ok(())
    })?;

    get_task(config, task_id)
}

/// Atomically claim a task for execution.
///
/// Only transitions tasks that are still in a claimable state (`pending` or `stale`)
/// AND enabled. Returns `true` if the claim succeeded, `false` if another worker
/// already claimed it or it was disabled in the meantime.
pub fn claim_task(config: &Config, task_id: &str) -> Result<bool> {
    let now = Utc::now();
    let changed = with_connection(config, |conn| {
        conn.execute(
            "UPDATE xin_tasks
             SET status = 'running', updated_at = ?1
             WHERE id = ?2 AND status IN ('pending', 'stale') AND enabled = 1",
            params![now.to_rfc3339(), task_id],
        )
        .context("Failed to claim xin task")
    })?;
    Ok(changed > 0)
}

/// Mark a task as completed after successful execution.
pub fn mark_completed(config: &Config, task_id: &str, output: &str) -> Result<()> {
    let now = Utc::now();
    let bounded = truncate_output(output);
    with_connection(config, |conn| {
        let changed = conn
            .execute(
                "UPDATE xin_tasks
             SET status = 'completed', last_run_at = ?1, last_status = 'ok',
                 last_output = ?2, run_count = run_count + 1, updated_at = ?3
             WHERE id = ?4",
                params![now.to_rfc3339(), bounded, now.to_rfc3339(), task_id],
            )
            .context("Failed to mark xin task completed")?;
        if changed == 0 {
            tracing::warn!(task_id = %task_id, "mark_completed: no rows affected (task may have been deleted)");
        }
        Ok(())
    })
}

/// Mark a task as failed. If `fail_count >= max_failures` (and max_failures > 0), disables it.
pub fn mark_failed(config: &Config, task_id: &str, output: &str) -> Result<()> {
    let now = Utc::now();
    let bounded = truncate_output(output);
    with_connection(config, |conn| {
        // Increment fail_count first
        let changed = conn
            .execute(
                "UPDATE xin_tasks
             SET status = 'failed', last_run_at = ?1, last_status = 'error',
                 last_output = ?2, run_count = run_count + 1, fail_count = fail_count + 1,
                 updated_at = ?3
             WHERE id = ?4",
                params![now.to_rfc3339(), bounded, now.to_rfc3339(), task_id],
            )
            .context("Failed to mark xin task failed")?;
        if changed == 0 {
            tracing::warn!(task_id = %task_id, "mark_failed: no rows affected (task may have been deleted)");
        }

        // Auto-disable if max_failures exceeded
        conn.execute(
            "UPDATE xin_tasks
             SET enabled = 0
             WHERE id = ?1 AND max_failures > 0 AND fail_count >= max_failures",
            params![task_id],
        )
        .context("Failed to auto-disable xin task")?;

        Ok(())
    })
}

/// Reschedule a recurring task after completion/failure.
pub fn reschedule_recurring(config: &Config, task_id: &str) -> Result<()> {
    let now = Utc::now();
    with_connection(config, |conn| {
        // Read interval_secs for the task
        let interval: i64 = conn.query_row(
            "SELECT interval_secs FROM xin_tasks WHERE id = ?1",
            params![task_id],
            |row| row.get(0),
        )?;

        let next_run = now + chrono::Duration::seconds(interval);
        conn.execute(
            "UPDATE xin_tasks
             SET status = 'pending', next_run_at = ?1, updated_at = ?2
             WHERE id = ?3 AND recurring = 1 AND enabled = 1",
            params![next_run.to_rfc3339(), now.to_rfc3339(), task_id],
        )
        .context("Failed to reschedule xin recurring task")?;
        Ok(())
    })
}

/// Delete a task by ID.
pub fn remove_task(config: &Config, task_id: &str) -> Result<()> {
    let changed = with_connection(config, |conn| {
        conn.execute("DELETE FROM xin_tasks WHERE id = ?1", params![task_id])
            .context("Failed to delete xin task")
    })?;

    if changed == 0 {
        anyhow::bail!("Xin task '{task_id}' not found");
    }
    Ok(())
}

/// Remove all completed (non-recurring) tasks.
pub fn remove_completed(config: &Config) -> Result<usize> {
    with_connection(config, |conn| {
        conn.execute("DELETE FROM xin_tasks WHERE status = 'completed' AND recurring = 0", [])
            .context("Failed to clean completed xin tasks")
    })
}

/// Mark running tasks as stale if they exceeded `stale_timeout_minutes`.
pub fn mark_stale(config: &Config, stale_timeout_minutes: u32) -> Result<usize> {
    let cutoff = Utc::now() - chrono::Duration::minutes(i64::from(stale_timeout_minutes));
    with_connection(config, |conn| {
        conn.execute(
            "UPDATE xin_tasks
             SET status = 'stale', updated_at = ?1
             WHERE status = 'running' AND updated_at < ?2",
            params![Utc::now().to_rfc3339(), cutoff.to_rfc3339()],
        )
        .context("Failed to mark stale xin tasks")
    })
}

/// Upsert a system task by name + kind=system. If it exists, update payload/interval; if not, insert.
pub fn ensure_system_task(config: &Config, new: &NewXinTask) -> Result<XinTask> {
    // Check if a system task with this name already exists
    let existing = with_connection(config, |conn| {
        let mut stmt = conn.prepare("SELECT id FROM xin_tasks WHERE name = ?1 AND kind = 'system'")?;
        let mut rows = stmt.query(params![new.name])?;
        if let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            Ok(Some(id))
        } else {
            Ok(None)
        }
    })?;

    match existing {
        Some(id) => {
            // Update payload and interval if changed
            let patch = XinTaskPatch {
                payload: Some(new.payload.clone()),
                interval_secs: Some(new.interval_secs),
                max_failures: Some(new.max_failures),
                ..XinTaskPatch::default()
            };
            update_task(config, &id, &patch)
        }
        None => add_task(config, new),
    }
}

/// Record a completed run in the `xin_runs` history table.
pub fn record_run(
    config: &Config,
    task_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
) -> Result<()> {
    let bounded = output.map(truncate_output);
    with_connection(config, |conn| {
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "INSERT INTO xin_runs (task_id, started_at, finished_at, status, output, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                task_id,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
                status,
                bounded.as_deref(),
                duration_ms,
            ],
        )
        .context("Failed to insert xin run")?;

        // Prune old runs — keep last 50 per task
        tx.execute(
            "DELETE FROM xin_runs
             WHERE task_id = ?1
               AND id NOT IN (
                 SELECT id FROM xin_runs
                 WHERE task_id = ?1
                 ORDER BY started_at DESC, id DESC
                 LIMIT 50
               )",
            params![task_id],
        )
        .context("Failed to prune xin run history")?;

        tx.commit().context("Failed to commit xin run transaction")?;
        Ok(())
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────

const SELECT_ALL_COLUMNS: &str = "SELECT id, name, description, kind, status, priority, execution_mode,
            payload, recurring, interval_secs, created_at, updated_at,
            last_run_at, next_run_at, last_status, last_output,
            run_count, fail_count, max_failures, enabled
     FROM xin_tasks WHERE id = ?1";

fn truncate_output(output: &str) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output.to_string();
    }
    if MAX_OUTPUT_BYTES <= TRUNCATED_MARKER.len() {
        return TRUNCATED_MARKER.to_string();
    }
    let mut cutoff = MAX_OUTPUT_BYTES - TRUNCATED_MARKER.len();
    while cutoff > 0 && !output.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    let mut truncated = output[..cutoff].to_string();
    truncated.push_str(TRUNCATED_MARKER);
    truncated
}

fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed =
        DateTime::parse_from_rfc3339(raw).with_context(|| format!("Invalid RFC3339 timestamp in xin DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

fn sql_err(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.into())
}

fn map_task_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<XinTask> {
    let created_at_raw: String = row.get(10)?;
    let updated_at_raw: String = row.get(11)?;
    let last_run_raw: Option<String> = row.get(12)?;
    let next_run_raw: String = row.get(13)?;

    Ok(XinTask {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        kind: TaskKind::from_str_lossy(&row.get::<_, String>(3)?),
        status: TaskStatus::from_str_lossy(&row.get::<_, String>(4)?),
        priority: TaskPriority::from_i32(row.get(5)?),
        execution_mode: ExecutionMode::from_str_lossy(&row.get::<_, String>(6)?),
        payload: row.get(7)?,
        recurring: row.get::<_, i64>(8)? != 0,
        interval_secs: u64::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_err)?,
        updated_at: parse_rfc3339(&updated_at_raw).map_err(sql_err)?,
        last_run_at: match last_run_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_err)?),
            None => None,
        },
        next_run_at: parse_rfc3339(&next_run_raw).map_err(sql_err)?,
        last_status: row.get(14)?,
        last_output: row.get(15)?,
        run_count: u64::try_from(row.get::<_, i64>(16)?).unwrap_or(0),
        fail_count: u64::try_from(row.get::<_, i64>(17)?).unwrap_or(0),
        max_failures: u32::try_from(row.get::<_, i64>(18)?).unwrap_or(0),
        enabled: row.get::<_, i64>(19)? != 0,
    })
}

fn with_connection<T>(config: &Config, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let db_path = config.workspace_dir.join("xin").join("tasks.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create xin directory: {}", parent.display()))?;
    }

    let conn = Connection::open(&db_path).with_context(|| format!("Failed to open xin DB: {}", db_path.display()))?;

    // Avoid SQLITE_BUSY under concurrent task execution
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    conn.execute_batch(
        "PRAGMA foreign_keys = ON;

         CREATE TABLE IF NOT EXISTS xin_tasks (
            id               TEXT PRIMARY KEY,
            name             TEXT NOT NULL,
            description      TEXT,
            kind             TEXT NOT NULL DEFAULT 'user',
            status           TEXT NOT NULL DEFAULT 'pending',
            priority         INTEGER NOT NULL DEFAULT 1,
            execution_mode   TEXT NOT NULL DEFAULT 'agent_session',
            payload          TEXT NOT NULL DEFAULT '',
            recurring        INTEGER NOT NULL DEFAULT 0,
            interval_secs    INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            updated_at       TEXT NOT NULL,
            last_run_at      TEXT,
            next_run_at      TEXT NOT NULL,
            last_status      TEXT,
            last_output      TEXT,
            run_count        INTEGER NOT NULL DEFAULT 0,
            fail_count       INTEGER NOT NULL DEFAULT 0,
            max_failures     INTEGER NOT NULL DEFAULT 0,
            enabled          INTEGER NOT NULL DEFAULT 1
         );
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_next_run ON xin_tasks(next_run_at);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_status ON xin_tasks(status);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_kind ON xin_tasks(kind);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_xin_tasks_system_name
             ON xin_tasks(name) WHERE kind = 'system';
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_due
             ON xin_tasks(enabled, status, next_run_at, priority);

         CREATE TABLE IF NOT EXISTS xin_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id     TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            status      TEXT NOT NULL,
            output      TEXT,
            duration_ms INTEGER,
            FOREIGN KEY (task_id) REFERENCES xin_tasks(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_xin_runs_task_id ON xin_runs(task_id);
         CREATE INDEX IF NOT EXISTS idx_xin_runs_started_at ON xin_runs(started_at);",
    )
    .context("Failed to initialize xin schema")?;

    f(&conn)
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    fn sample_task() -> NewXinTask {
        NewXinTask {
            name: "test_task".into(),
            description: Some("A test task".into()),
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::AgentSession,
            payload: "hello world".into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
        }
    }

    fn recurring_task() -> NewXinTask {
        NewXinTask {
            name: "recurring_task".into(),
            description: None,
            kind: TaskKind::System,
            priority: TaskPriority::High,
            execution_mode: ExecutionMode::Internal,
            payload: "health_check".into(),
            recurring: true,
            interval_secs: 300,
            max_failures: 5,
        }
    }

    #[test]
    fn add_and_get_task() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();

        assert_eq!(task.name, "test_task");
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.enabled);
        assert_eq!(task.run_count, 0);
        assert_eq!(task.fail_count, 0);

        let fetched = get_task(&config, &task.id).unwrap();
        assert_eq!(fetched.name, task.name);
    }

    #[test]
    fn list_tasks_ordered_by_priority() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let mut low = sample_task();
        low.name = "low_prio".into();
        low.priority = TaskPriority::Low;
        add_task(&config, &low).unwrap();

        let mut high = sample_task();
        high.name = "high_prio".into();
        high.priority = TaskPriority::High;
        add_task(&config, &high).unwrap();

        let tasks = list_tasks(&config).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "high_prio");
        assert_eq!(tasks[1].name, "low_prio");
    }

    #[test]
    fn due_tasks_filters_correctly() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        add_task(&config, &sample_task()).unwrap();

        // Task with next_run_at = now should be due
        let due = due_tasks(&config, Utc::now() + chrono::Duration::seconds(1), 10).unwrap();
        assert_eq!(due.len(), 1);

        // Far in the past — should not be due
        let due_past = due_tasks(&config, Utc::now() - chrono::Duration::days(1), 10).unwrap();
        assert!(due_past.is_empty());
    }

    #[test]
    fn update_task_applies_patch() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();

        let patch = XinTaskPatch {
            name: Some("renamed".into()),
            priority: Some(TaskPriority::Critical),
            enabled: Some(false),
            ..XinTaskPatch::default()
        };
        let updated = update_task(&config, &task.id, &patch).unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.priority, TaskPriority::Critical);
        assert!(!updated.enabled);
    }

    #[test]
    fn claim_and_completed() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();

        let claimed = claim_task(&config, &task.id).unwrap();
        assert!(claimed);
        let running = get_task(&config, &task.id).unwrap();
        assert_eq!(running.status, TaskStatus::Running);

        mark_completed(&config, &task.id, "done").unwrap();
        let completed = get_task(&config, &task.id).unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(completed.last_status.as_deref(), Some("ok"));
        assert_eq!(completed.last_output.as_deref(), Some("done"));
        assert_eq!(completed.run_count, 1);
    }

    #[test]
    fn mark_failed_auto_disables_after_max_failures() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let mut new = sample_task();
        new.max_failures = 2;
        let task = add_task(&config, &new).unwrap();

        mark_failed(&config, &task.id, "err 1").unwrap();
        let t1 = get_task(&config, &task.id).unwrap();
        assert!(t1.enabled); // still enabled, 1 failure < 2 max

        mark_failed(&config, &task.id, "err 2").unwrap();
        let t2 = get_task(&config, &task.id).unwrap();
        assert!(!t2.enabled); // auto-disabled: 2 failures >= 2 max
        assert_eq!(t2.fail_count, 2);
    }

    #[test]
    fn reschedule_recurring_resets_status() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &recurring_task()).unwrap();

        let claimed = claim_task(&config, &task.id).unwrap();
        assert!(claimed);
        mark_completed(&config, &task.id, "ok").unwrap();
        reschedule_recurring(&config, &task.id).unwrap();

        let rescheduled = get_task(&config, &task.id).unwrap();
        assert_eq!(rescheduled.status, TaskStatus::Pending);
        assert!(rescheduled.next_run_at > task.next_run_at);
    }

    #[test]
    fn remove_task_deletes_and_cascades() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();

        let start = Utc::now();
        record_run(&config, &task.id, start, start, "ok", Some("done"), 10).unwrap();

        remove_task(&config, &task.id).unwrap();
        assert!(get_task(&config, &task.id).is_err());
    }

    #[test]
    fn remove_completed_only_non_recurring() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let one_shot = add_task(&config, &sample_task()).unwrap();
        let recurring = add_task(&config, &recurring_task()).unwrap();

        mark_completed(&config, &one_shot.id, "done").unwrap();
        mark_completed(&config, &recurring.id, "done").unwrap();

        let removed = remove_completed(&config).unwrap();
        assert_eq!(removed, 1); // only one-shot removed

        assert!(get_task(&config, &one_shot.id).is_err());
        assert!(get_task(&config, &recurring.id).is_ok());
    }

    #[test]
    fn mark_stale_timeout() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();
        let claimed = claim_task(&config, &task.id).unwrap();
        assert!(claimed);

        // Backdate updated_at to simulate a long-running task
        with_connection(&config, |conn| {
            let old = (Utc::now() - chrono::Duration::minutes(120)).to_rfc3339();
            conn.execute(
                "UPDATE xin_tasks SET updated_at = ?1 WHERE id = ?2",
                params![old, task.id],
            )?;
            Ok(())
        })
        .unwrap();

        let stale_count = mark_stale(&config, 60).unwrap();
        assert_eq!(stale_count, 1);

        let stale = get_task(&config, &task.id).unwrap();
        assert_eq!(stale.status, TaskStatus::Stale);
    }

    #[test]
    fn ensure_system_task_upserts() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let new = recurring_task();

        let first = ensure_system_task(&config, &new).unwrap();
        assert_eq!(first.name, "recurring_task");

        // Second call should return the same task (upsert)
        let second = ensure_system_task(&config, &new).unwrap();
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn record_run_and_prune() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();
        let base = Utc::now();

        for i in 0..55 {
            let start = base + chrono::Duration::seconds(i);
            record_run(&config, &task.id, start, start, "ok", Some("done"), 10).unwrap();
        }

        // Verify pruning: should keep only 50
        with_connection(&config, |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM xin_runs WHERE task_id = ?1",
                params![task.id],
                |row| row.get(0),
            )?;
            assert_eq!(count, 50);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn truncate_output_handles_large_text() {
        let large = "x".repeat(MAX_OUTPUT_BYTES + 512);
        let truncated = truncate_output(&large);
        assert!(truncated.len() <= MAX_OUTPUT_BYTES);
        assert!(truncated.ends_with(TRUNCATED_MARKER));
    }

    #[test]
    fn get_nonexistent_task_errors() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        assert!(get_task(&config, "no-such-id").is_err());
    }
}
