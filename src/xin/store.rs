//! SQLite persistence for the xin (心) autonomous task engine.
//!
//! DB path: `{workspace}/xin/tasks.db`
//! Pattern follows `cron/store.rs`: `with_connection()` + `rusqlite::params!`.

use crate::config::Config;
use crate::xin::types::{
    ExecutionMode, GoalStatus, NewXinGoal, NewXinStep, NewXinTask, StepStatus, TaskKind, TaskPriority, TaskStatus,
    XinGoal, XinStep, XinTask, XinTaskEvent, XinTaskPatch, default_lease_ttl_secs,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
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
                id, owner_id, topic_id, parent_task_id, source_message_event_id,
                name, description, kind, status, priority, execution_mode,
                payload, recurring, interval_secs, created_at, updated_at,
                last_run_at, next_run_at, last_status, last_output,
                run_count, fail_count, max_failures, enabled, approval_grant_json
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, 'pending', ?9, ?10,
                ?11, ?12, ?13, ?14, ?15,
                NULL, ?16, NULL, NULL,
                0, 0, ?17, 1, ?18
             )",
            params![
                id,
                new.owner_id,
                new.topic_id,
                new.parent_task_id,
                new.source_message_event_id,
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
                new.approval_grant_json,
            ],
        )
        .context("Failed to insert xin task")?;
        insert_task_event(
            conn,
            &workspace_id(config),
            &id,
            TaskLineage {
                owner_id: new.owner_id.clone(),
                topic_id: new.topic_id.clone(),
                parent_task_id: new.parent_task_id.clone(),
                source_message_event_id: new.source_message_event_id.clone(),
                status: Some("pending".to_string()),
            },
            "xin.task.created",
            Some("pending"),
            Some(
                serde_json::json!({
                    "name": new.name,
                    "kind": new.kind.as_str(),
                    "execution_mode": new.execution_mode.as_str(),
                    "source_message_event_id": new.source_message_event_id,
                })
                .to_string(),
            )
            .as_deref(),
        )?;
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
            "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
                    name, description, kind, status, priority, execution_mode,
                    payload, recurring, interval_secs, created_at, updated_at,
                    last_run_at, next_run_at, last_status, last_output,
                    run_count, fail_count, max_failures, enabled, approval_grant_json
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
            "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
                    name, description, kind, status, priority, execution_mode,
                    payload, recurring, interval_secs, created_at, updated_at,
                    last_run_at, next_run_at, last_status, last_output,
                    run_count, fail_count, max_failures, enabled, approval_grant_json
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
        task.approval_grant_json = patch.approval_grant_json.clone();
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
                 interval_secs = ?5, enabled = ?6, max_failures = ?7, updated_at = ?8,
                 approval_grant_json = ?9
             WHERE id = ?10 AND updated_at = ?11",
                params![
                    task.name,
                    task.description,
                    task.priority.as_i32(),
                    task.payload,
                    i64::try_from(task.interval_secs).unwrap_or(i64::MAX),
                    if task.enabled { 1 } else { 0 },
                    i64::from(task.max_failures),
                    now.to_rfc3339(),
                    task.approval_grant_json,
                    task_id,
                    previous_updated_at.to_rfc3339(),
                ],
            )
            .context("Failed to update xin task")?;
        if changed == 0 {
            anyhow::bail!("xin task '{task_id}' was modified by another process (optimistic concurrency conflict)");
        }
        if let Some(lineage) = load_task_lineage(conn, task_id)? {
            insert_task_event(
                conn,
                &workspace_id(config),
                task_id,
                lineage,
                "xin.task.updated",
                Some(task.status.as_str()),
                Some(
                    serde_json::json!({
                        "name": task.name,
                        "enabled": task.enabled,
                        "priority": task.priority.as_i32(),
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
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
        let changed = conn
            .execute(
                "UPDATE xin_tasks
             SET status = 'running', updated_at = ?1
             WHERE id = ?2 AND status IN ('pending', 'stale') AND enabled = 1",
                params![now.to_rfc3339(), task_id],
            )
            .context("Failed to claim xin task")?;
        if changed > 0 {
            if let Some(lineage) = load_task_lineage(conn, task_id)? {
                insert_task_event(
                    conn,
                    &workspace_id(config),
                    task_id,
                    lineage,
                    "xin.task.claimed",
                    Some("running"),
                    None,
                )?;
            }
        }
        Ok(changed)
    })?;
    Ok(changed > 0)
}

/// Mark a task as completed after successful execution.
#[cfg(test)]
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
        } else if let Some(lineage) = load_task_lineage(conn, task_id)? {
            insert_task_event(
                conn,
                &workspace_id(config),
                task_id,
                lineage,
                "xin.task.completed",
                Some("completed"),
                Some(serde_json::json!({ "output": bounded }).to_string()).as_deref(),
            )?;
        }
        Ok(())
    })
}

/// Mark a task as failed. If `fail_count >= max_failures` (and max_failures > 0), disables it.
#[cfg(test)]
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
        } else if let Some(lineage) = load_task_lineage(conn, task_id)? {
            insert_task_event(
                conn,
                &workspace_id(config),
                task_id,
                lineage,
                "xin.task.failed",
                Some("failed"),
                Some(serde_json::json!({ "output": bounded }).to_string()).as_deref(),
            )?;
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
#[cfg(test)]
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
        let changed = conn
            .execute(
                "UPDATE xin_tasks
             SET status = 'pending', next_run_at = ?1, updated_at = ?2
             WHERE id = ?3 AND recurring = 1 AND enabled = 1",
                params![next_run.to_rfc3339(), now.to_rfc3339(), task_id],
            )
            .context("Failed to reschedule xin recurring task")?;
        if changed > 0 {
            if let Some(lineage) = load_task_lineage(conn, task_id)? {
                insert_task_event(
                    conn,
                    &workspace_id(config),
                    task_id,
                    lineage,
                    "xin.task.rescheduled",
                    Some("pending"),
                    Some(serde_json::json!({ "next_run_at": next_run.to_rfc3339() }).to_string()).as_deref(),
                )?;
            }
        }
        Ok(())
    })
}

/// Delete a task by ID.
pub fn remove_task(config: &Config, task_id: &str) -> Result<()> {
    let changed = with_connection(config, |conn| {
        if let Some(lineage) = load_task_lineage(conn, task_id)? {
            insert_task_event(
                conn,
                &workspace_id(config),
                task_id,
                lineage.clone(),
                "xin.task.removed",
                lineage.status.as_deref(),
                None,
            )?;
        }
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
        let mut stmt = conn.prepare(
            "SELECT id FROM xin_tasks
             WHERE status = 'running' AND updated_at < ?1",
        )?;
        let ids = stmt
            .query_map(params![cutoff.to_rfc3339()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let changed = conn
            .execute(
                "UPDATE xin_tasks
             SET status = 'stale', updated_at = ?1
             WHERE status = 'running' AND updated_at < ?2",
                params![Utc::now().to_rfc3339(), cutoff.to_rfc3339()],
            )
            .context("Failed to mark stale xin tasks")?;
        for task_id in ids {
            if let Some(lineage) = load_task_lineage(conn, &task_id)? {
                insert_task_event(
                    conn,
                    &workspace_id(config),
                    &task_id,
                    lineage,
                    "xin.task.stale",
                    Some("stale"),
                    Some(serde_json::json!({ "stale_timeout_minutes": stale_timeout_minutes }).to_string()).as_deref(),
                )?;
            }
        }
        Ok(changed)
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

    existing.map_or_else(
        || add_task(config, new),
        |id| {
            // Update payload and interval if changed
            let patch = XinTaskPatch {
                payload: Some(new.payload.clone()),
                interval_secs: Some(new.interval_secs),
                max_failures: Some(new.max_failures),
                ..XinTaskPatch::default()
            };
            update_task(config, &id, &patch)
        },
    )
}

/// Commit one legacy Xin task execution as a single local transaction.
///
/// Result state, run history, failure disabling, recurring reschedule, local
/// lifecycle events, and their outbox rows either commit together or roll back
/// together. Cross-database delivery is recovered from the committed outbox.
pub fn commit_task_execution(
    config: &Config,
    task_id: &str,
    success: bool,
    output: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    duration_ms: i64,
) -> Result<bool> {
    let bounded = truncate_output(output);
    with_immediate_connection(config, |conn| {
        let task: Option<(bool, i64)> = conn
            .query_row(
                "SELECT recurring, interval_secs FROM xin_tasks WHERE id = ?1",
                params![task_id],
                |row| Ok((row.get::<_, i64>(0)? != 0, row.get(1)?)),
            )
            .optional()?;
        let Some((recurring, interval_secs)) = task else {
            return Ok(false);
        };

        let changed = if success {
            conn.execute(
                "UPDATE xin_tasks
                 SET status = 'completed', last_run_at = ?1, last_status = 'ok',
                     last_output = ?2, run_count = run_count + 1, updated_at = ?1
                 WHERE id = ?3 AND status = 'running'",
                params![finished_at.to_rfc3339(), bounded, task_id],
            )?
        } else {
            conn.execute(
                "UPDATE xin_tasks
                 SET status = 'failed', last_run_at = ?1, last_status = 'error',
                     last_output = ?2, run_count = run_count + 1,
                     fail_count = fail_count + 1, updated_at = ?1
                 WHERE id = ?3 AND status = 'running'",
                params![finished_at.to_rfc3339(), bounded, task_id],
            )?
        };
        if changed == 0 {
            return Ok(false);
        }

        let result_event_type = if success {
            "xin.task.completed"
        } else {
            "xin.task.failed"
        };
        let result_status = if success { "completed" } else { "failed" };
        if let Some(lineage) = load_task_lineage(conn, task_id)? {
            insert_task_event(
                conn,
                &workspace_id(config),
                task_id,
                lineage,
                result_event_type,
                Some(result_status),
                Some(serde_json::json!({ "output": bounded }).to_string()).as_deref(),
            )?;
        }

        if !success {
            conn.execute(
                "UPDATE xin_tasks
                 SET enabled = 0
                 WHERE id = ?1 AND max_failures > 0 AND fail_count >= max_failures",
                params![task_id],
            )?;
        }

        insert_run_record(
            conn,
            config,
            task_id,
            started_at,
            finished_at,
            if success { "ok" } else { "error" },
            Some(&bounded),
            duration_ms,
        )?;

        let enabled = conn.query_row("SELECT enabled FROM xin_tasks WHERE id = ?1", params![task_id], |row| {
            Ok(row.get::<_, i64>(0)? != 0)
        })?;
        if recurring && enabled {
            let next_run = finished_at + chrono::Duration::seconds(interval_secs.max(0));
            conn.execute(
                "UPDATE xin_tasks
                 SET status = 'pending', next_run_at = ?1, updated_at = ?2
                 WHERE id = ?3 AND recurring = 1 AND enabled = 1",
                params![next_run.to_rfc3339(), finished_at.to_rfc3339(), task_id],
            )?;
            if let Some(lineage) = load_task_lineage(conn, task_id)? {
                insert_task_event(
                    conn,
                    &workspace_id(config),
                    task_id,
                    lineage,
                    "xin.task.rescheduled",
                    Some("pending"),
                    Some(serde_json::json!({ "next_run_at": next_run.to_rfc3339() }).to_string()).as_deref(),
                )?;
            }
        }

        Ok(true)
    })
}

fn insert_run_record(
    conn: &Connection,
    config: &Config,
    task_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
) -> Result<()> {
    let bounded = output.map(truncate_output);

    conn.execute(
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
    if let Some(lineage) = load_task_lineage(conn, task_id)? {
        insert_task_event(
            conn,
            &workspace_id(config),
            task_id,
            lineage,
            "xin.task.run_recorded",
            Some(status),
            Some(
                serde_json::json!({
                    "started_at": started_at.to_rfc3339(),
                    "finished_at": finished_at.to_rfc3339(),
                    "duration_ms": duration_ms,
                })
                .to_string(),
            )
            .as_deref(),
        )?;
    }

    // Prune old runs — keep last 50 per task
    conn.execute(
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

    Ok(())
}

/// Record a completed run in the `xin_runs` history table.
#[cfg(test)]
pub fn record_run(
    config: &Config,
    task_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
) -> Result<()> {
    with_immediate_connection(config, |conn| {
        insert_run_record(
            conn,
            config,
            task_id,
            started_at,
            finished_at,
            status,
            output,
            duration_ms,
        )
    })
}

/// List append-only lifecycle events for a Xin task.
pub fn list_task_events(config: &Config, task_id: &str) -> Result<Vec<XinTaskEvent>> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, event_id, task_id, workspace_id, owner_id, topic_id, parent_task_id,
                    source_message_event_id, event_type, status, payload_json, created_at
             FROM xin_task_events
             WHERE task_id = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![task_id], map_task_event_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    })
}

// ── Goal / Step (FIX-P2-16, d09) ─────────────────────────────────────────

const SELECT_GOAL_COLUMNS: &str = "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
            name, description, kind, status, priority, target_completion_at,
            steps_completed, steps_total, created_at, updated_at, completed_at,
            final_output, enabled
     FROM xin_goals";

const SELECT_STEP_COLUMNS: &str = "SELECT id, goal_id, sequence, name, description, status, execution_mode,
            payload, lease_owner, lease_epoch, lease_expires_at, last_heartbeat_at, checkpoint_json,
            lease_ttl_secs, retry_count, max_retries, created_at, updated_at, started_at, completed_at,
            last_output, approval_grant_json
     FROM xin_steps";

/// Insert a goal and (optionally) its initial steps in a single transaction.
#[allow(dead_code)] // Canonical crate API; adoption currently owns the production caller.
pub fn add_goal(config: &Config, new: &NewXinGoal) -> Result<XinGoal> {
    let goal_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    with_immediate_connection(config, |conn| insert_goal_rows(conn, config, &goal_id, new, now))?;

    get_goal(config, &goal_id)
}

/// Insert a goal, its initial steps, and its lifecycle event inside the
/// caller's transaction.
fn insert_goal_rows(
    conn: &Connection,
    config: &Config,
    goal_id: &str,
    new: &NewXinGoal,
    now: DateTime<Utc>,
) -> Result<()> {
    let steps_total = u32::try_from(new.initial_steps.len()).unwrap_or(u32::MAX);
    conn.execute(
        "INSERT INTO xin_goals (
                id, owner_id, topic_id, parent_task_id, source_message_event_id,
                name, description, kind, status, priority, target_completion_at,
                steps_completed, steps_total, created_at, updated_at, completed_at,
                final_output, enabled
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, 'pending', ?9, ?10,
                0, ?11, ?12, ?13, NULL,
                NULL, 1
             )",
        params![
            goal_id,
            new.owner_id,
            new.topic_id,
            new.parent_task_id,
            new.source_message_event_id,
            new.name,
            new.description,
            new.kind.as_str(),
            new.priority.as_i32(),
            new.target_completion_at.map(|t| t.to_rfc3339()),
            i64::from(steps_total),
            now.to_rfc3339(),
            now.to_rfc3339(),
        ],
    )
    .context("Failed to insert xin goal")?;

    for step in &new.initial_steps {
        insert_step_row(conn, goal_id, step, now)?;
    }

    insert_task_event(
        conn,
        &workspace_id(config),
        goal_id,
        TaskLineage {
            owner_id: new.owner_id.clone(),
            topic_id: new.topic_id.clone(),
            parent_task_id: new.parent_task_id.clone(),
            source_message_event_id: new.source_message_event_id.clone(),
            status: Some("pending".to_string()),
        },
        "xin.goal.created",
        Some("pending"),
        Some(
            serde_json::json!({
                "name": new.name,
                "kind": new.kind.as_str(),
                "steps_total": steps_total,
            })
            .to_string(),
        )
        .as_deref(),
    )?;
    Ok(())
}

/// Insert a step row inside an existing transaction/connection.
fn insert_step_row(conn: &Connection, goal_id: &str, step: &NewXinStep, now: DateTime<Utc>) -> Result<String> {
    let step_id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO xin_steps (
            id, goal_id, sequence, name, description, status, execution_mode,
            payload, lease_ttl_secs, retry_count, max_retries, created_at, updated_at, approval_grant_json
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, 'pending', ?6,
            ?7, ?8, 0, ?9, ?10, ?11, ?12
         )",
        params![
            step_id,
            goal_id,
            i64::from(step.sequence),
            step.name,
            step.description,
            step.execution_mode.as_str(),
            step.payload,
            i64::try_from(step.lease_ttl_secs).unwrap_or(i64::MAX),
            i64::from(step.max_retries),
            now.to_rfc3339(),
            now.to_rfc3339(),
            step.approval_grant_json,
        ],
    )
    .context("Failed to insert xin step")?;
    Ok(step_id)
}

/// Append a step to an existing goal and bump its `steps_total`.
#[allow(dead_code)] // Canonical crate API for future interactive goal authoring.
pub fn add_step(config: &Config, goal_id: &str, step: &NewXinStep) -> Result<XinStep> {
    let now = Utc::now();
    let step_id = with_immediate_connection(config, |conn| {
        let id = insert_step_row(conn, goal_id, step, now)?;
        conn.execute(
            "UPDATE xin_goals SET steps_total = steps_total + 1, updated_at = ?1 WHERE id = ?2",
            params![now.to_rfc3339(), goal_id],
        )
        .context("Failed to bump goal steps_total")?;
        Ok(id)
    })?;
    get_step(config, &step_id)
}

/// Retrieve a goal by ID.
pub fn get_goal(config: &Config, goal_id: &str) -> Result<XinGoal> {
    with_connection(config, |conn| {
        let sql = format!("{SELECT_GOAL_COLUMNS} WHERE id = ?1");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![goal_id])?;
        if let Some(row) = rows.next()? {
            map_goal_row(row).map_err(Into::into)
        } else {
            anyhow::bail!("Xin goal '{goal_id}' not found")
        }
    })
}

/// Retrieve a single step by ID.
pub fn get_step(config: &Config, step_id: &str) -> Result<XinStep> {
    with_connection(config, |conn| {
        let sql = format!("{SELECT_STEP_COLUMNS} WHERE id = ?1");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![step_id])?;
        if let Some(row) = rows.next()? {
            map_step_row(row).map_err(Into::into)
        } else {
            anyhow::bail!("Xin step '{step_id}' not found")
        }
    })
}

/// List a goal's steps ordered by sequence.
pub fn list_steps(config: &Config, goal_id: &str) -> Result<Vec<XinStep>> {
    with_connection(config, |conn| {
        let sql = format!("{SELECT_STEP_COLUMNS} WHERE goal_id = ?1 ORDER BY sequence ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![goal_id], map_step_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    })
}

/// List all goals ordered by priority DESC then creation time.
pub fn list_goals(config: &Config) -> Result<Vec<XinGoal>> {
    with_connection(config, |conn| {
        let sql = format!("{SELECT_GOAL_COLUMNS} ORDER BY priority DESC, created_at ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_goal_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    })
}

/// Return the next claimable step of a goal: the lowest-sequence step that is
/// pending or stale (lease expired). Returns `None` when nothing is runnable.
pub fn next_runnable_step(config: &Config, goal_id: &str) -> Result<Option<XinStep>> {
    with_connection(config, |conn| {
        let sql = format!(
            "{SELECT_STEP_COLUMNS} WHERE goal_id = ?1 AND status IN ('pending', 'stale') \
             AND NOT EXISTS (\
                 SELECT 1 FROM xin_steps AS prior \
                 WHERE prior.goal_id = xin_steps.goal_id \
                   AND prior.sequence < xin_steps.sequence \
                   AND prior.status != 'completed'\
             ) \
             ORDER BY sequence ASC LIMIT 1"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![goal_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_step_row(row)?)),
            None => Ok(None),
        }
    })
}

/// Resolve the effective lease TTL for a step, in priority order:
/// 1. the caller-supplied `ttl_secs` (when non-zero),
/// 2. the per-step persisted `lease_ttl_secs` (when non-zero),
/// 3. the per-execution-mode default.
const fn effective_lease_ttl(mode: &ExecutionMode, persisted_ttl: u64, ttl_secs: u64) -> u64 {
    if ttl_secs != 0 {
        ttl_secs
    } else if persisted_ttl != 0 {
        persisted_ttl
    } else {
        default_lease_ttl_secs(mode)
    }
}

/// Exact Xin lease generation. Reusing the same worker id after expiry creates
/// a distinct epoch; renewal extends expiry without changing that generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XinStepLease {
    pub(crate) worker_id: String,
    pub(crate) epoch: u64,
    pub(crate) expires_at: DateTime<Utc>,
}

/// Read a step's `(execution_mode, lease_ttl_secs)` for TTL resolution.
fn step_lease_params(conn: &Connection, step_id: &str) -> Option<(ExecutionMode, u64)> {
    conn.query_row(
        "SELECT execution_mode, lease_ttl_secs FROM xin_steps WHERE id = ?1",
        params![step_id],
        |row| {
            let mode: String = row.get(0)?;
            let ttl: i64 = row.get(1)?;
            Ok((mode, ttl))
        },
    )
    .ok()
    .map(|(mode, ttl)| (ExecutionMode::from_str_lossy(&mode), u64::try_from(ttl).unwrap_or(0)))
}

/// Atomically claim a step for a worker.
///
/// CAS semantics: a step is claimable when it is `pending`/`stale` (a free
/// step), or when it is `claimed`/`running` with an expired (or null) lease. On
/// success the step becomes `claimed` with a fresh lease and heartbeat. Returns
/// `true` iff this worker won the claim.
pub(crate) fn claim_step_with_lease(
    config: &Config,
    step_id: &str,
    worker_id: &str,
    ttl_secs: u64,
) -> Result<Option<XinStepLease>> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let Some((mode, persisted)) = step_lease_params(conn, step_id) else {
            return Ok(None);
        };
        let ttl = effective_lease_ttl(&mode, persisted, ttl_secs);
        let expires = now + chrono::Duration::seconds(i64::try_from(ttl).unwrap_or(i64::MAX));

        let changed = conn
            .execute(
                "UPDATE xin_steps
                 SET status = 'claimed', lease_owner = ?1, lease_expires_at = ?2,
                     lease_epoch = lease_epoch + 1,
                     last_heartbeat_at = ?3, updated_at = ?3
                 WHERE id = ?4
                   AND lease_epoch < 9223372036854775807
                   AND NOT EXISTS (
                     SELECT 1 FROM xin_steps AS prior
                     WHERE prior.goal_id = xin_steps.goal_id
                       AND prior.sequence < xin_steps.sequence
                       AND prior.status != 'completed'
                   )
                   AND (
                     -- A free step (never started, or already reaped) is always claimable.
                     status IN ('pending', 'stale')
                     -- A still-held step is only claimable once its lease has lapsed.
                     OR (status IN ('claimed', 'running')
                         AND (lease_expires_at IS NULL OR lease_expires_at < ?3))
                   )",
                params![worker_id, expires.to_rfc3339(), now.to_rfc3339(), step_id],
            )
            .context("Failed to claim xin step")?;

        if changed == 0 {
            return Ok(None);
        }
        let epoch = conn.query_row(
            "SELECT lease_epoch FROM xin_steps WHERE id = ?1 AND lease_owner = ?2",
            params![step_id, worker_id],
            |row| row.get::<_, i64>(0),
        )?;
        let lease = XinStepLease {
            worker_id: worker_id.to_string(),
            epoch: u64::try_from(epoch).unwrap_or(0),
            expires_at: expires,
        };
        emit_step_event(conn, config, step_id, "xin.step.claimed", Some("claimed"), Some(&lease))?;
        Ok(Some(lease))
    })
}

#[cfg(test)]
pub fn claim_step(config: &Config, step_id: &str, worker_id: &str, ttl_secs: u64) -> Result<bool> {
    Ok(claim_step_with_lease(config, step_id, worker_id, ttl_secs)?.is_some())
}

/// Transition a claimed step to `running` (sets `started_at` on first run).
pub(crate) fn mark_step_running_with_lease(config: &Config, step_id: &str, lease: &XinStepLease) -> Result<bool> {
    let changed = with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let changed = conn
            .execute(
                "UPDATE xin_steps
                 SET status = 'running',
                     started_at = COALESCE(started_at, ?1),
                     last_heartbeat_at = ?1,
                     updated_at = ?1
                 WHERE id = ?2 AND lease_owner = ?3 AND lease_epoch = ?4
                   AND status IN ('claimed', 'running') AND lease_expires_at >= ?1",
                params![
                    now.to_rfc3339(),
                    step_id,
                    lease.worker_id,
                    i64::try_from(lease.epoch).unwrap_or(i64::MAX)
                ],
            )
            .context("Failed to mark xin step running")?;
        if changed > 0 {
            emit_step_event(conn, config, step_id, "xin.step.running", Some("running"), Some(lease))?;
        }
        Ok(changed)
    })?;
    Ok(changed > 0)
}

#[cfg(test)]
pub fn mark_step_running(config: &Config, step_id: &str, worker_id: &str) -> Result<bool> {
    let Some(step) = get_step(config, step_id).ok() else {
        return Ok(false);
    };
    let Some(expires_at) = step.lease_expires_at else {
        return Ok(false);
    };
    mark_step_running_with_lease(
        config,
        step_id,
        &XinStepLease {
            worker_id: worker_id.to_string(),
            epoch: step.lease_epoch,
            expires_at,
        },
    )
}

/// Atomically renew a lease. Succeeds only when the caller still owns a
/// non-expired lease — this is what keeps long agent runs from being marked
/// stale mid-flight. Returns `true` iff the lease was extended.
#[cfg(test)]
pub fn renew_step_lease(config: &Config, step_id: &str, worker_id: &str, ttl_secs: u64) -> Result<bool> {
    Ok(renew_step_lease_with_expiry(config, step_id, worker_id, ttl_secs)?.is_some())
}

/// Renew a lease and return the exact persisted expiry. The claim epoch stays
/// unchanged and remains the mutation fence.
#[cfg(test)]
pub fn renew_step_lease_with_expiry(
    config: &Config,
    step_id: &str,
    worker_id: &str,
    ttl_secs: u64,
) -> Result<Option<DateTime<Utc>>> {
    let Some(step) = get_step(config, step_id).ok() else {
        return Ok(None);
    };
    let Some(expires_at) = step.lease_expires_at else {
        return Ok(None);
    };
    Ok(renew_step_lease_generation(
        config,
        step_id,
        &XinStepLease {
            worker_id: worker_id.to_string(),
            epoch: step.lease_epoch,
            expires_at,
        },
        ttl_secs,
    )?
    .map(|lease| lease.expires_at))
}

pub(crate) fn renew_step_lease_generation(
    config: &Config,
    step_id: &str,
    lease: &XinStepLease,
    ttl_secs: u64,
) -> Result<Option<XinStepLease>> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let Some((mode, persisted)) = step_lease_params(conn, step_id) else {
            return Ok(None);
        };
        let ttl = effective_lease_ttl(&mode, persisted, ttl_secs);
        let expires = now + chrono::Duration::seconds(i64::try_from(ttl).unwrap_or(i64::MAX));

        let changed = conn
            .execute(
                "UPDATE xin_steps
             SET lease_expires_at = ?1, last_heartbeat_at = ?2, updated_at = ?2
             WHERE id = ?3 AND lease_owner = ?4
               AND status IN ('claimed', 'running')
               AND lease_epoch = ?5 AND lease_expires_at >= ?2",
                params![
                    expires.to_rfc3339(),
                    now.to_rfc3339(),
                    step_id,
                    lease.worker_id,
                    i64::try_from(lease.epoch).unwrap_or(i64::MAX)
                ],
            )
            .context("Failed to renew xin step lease")?;
        Ok((changed > 0).then(|| XinStepLease {
            worker_id: lease.worker_id.clone(),
            epoch: lease.epoch,
            expires_at: expires,
        }))
    })
}

/// Persist a checkpoint only if the exact lease generation is still current.
pub(crate) fn save_step_checkpoint_with_lease(
    config: &Config,
    step_id: &str,
    lease: &XinStepLease,
    checkpoint_json: &str,
) -> Result<bool> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let changed = conn.execute(
            "UPDATE xin_steps SET checkpoint_json = ?1, updated_at = ?2
             WHERE id = ?3 AND lease_owner = ?4 AND lease_epoch = ?5
               AND lease_expires_at >= ?2 AND status IN ('claimed', 'running')",
            params![
                checkpoint_json,
                now.to_rfc3339(),
                step_id,
                lease.worker_id,
                i64::try_from(lease.epoch).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(changed > 0)
    })
}

/// Complete only the exact lease generation that performed the work.
pub(crate) fn complete_step_with_lease(
    config: &Config,
    step_id: &str,
    lease: &XinStepLease,
    output: &str,
) -> Result<bool> {
    let bounded = truncate_output(output);
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let goal_id: Option<String> = conn
            .query_row("SELECT goal_id FROM xin_steps WHERE id = ?1", params![step_id], |row| {
                row.get(0)
            })
            .ok();
        let changed = conn.execute(
            "UPDATE xin_steps
             SET status = 'completed', completed_at = ?1, last_output = ?2,
                 lease_owner = NULL, lease_expires_at = NULL, updated_at = ?1
             WHERE id = ?3 AND lease_owner = ?4 AND lease_epoch = ?5
               AND lease_expires_at >= ?1 AND status IN ('claimed', 'running')",
            params![
                now.to_rfc3339(),
                bounded,
                step_id,
                lease.worker_id,
                i64::try_from(lease.epoch).unwrap_or(i64::MAX)
            ],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        emit_step_event(
            conn,
            config,
            step_id,
            "xin.step.completed",
            Some("completed"),
            Some(lease),
        )?;
        if let Some(goal_id) = goal_id {
            recompute_goal_progress(conn, config, &goal_id, now)?;
        }
        Ok(true)
    })
}

/// Fail/retry only the exact lease generation that performed the work.
pub(crate) fn fail_step_with_lease(config: &Config, step_id: &str, lease: &XinStepLease, output: &str) -> Result<bool> {
    let bounded = truncate_output(output);
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let row: Option<(String, i64, i64)> = conn
            .query_row(
                "SELECT goal_id, retry_count, max_retries FROM xin_steps
                 WHERE id = ?1 AND lease_owner = ?2 AND lease_epoch = ?3
                   AND lease_expires_at >= ?4 AND status IN ('claimed', 'running')",
                params![
                    step_id,
                    lease.worker_id,
                    i64::try_from(lease.epoch).unwrap_or(i64::MAX),
                    now.to_rfc3339()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();
        let Some((goal_id, retry_count, max_retries)) = row else {
            return Ok(false);
        };
        if retry_count < max_retries {
            conn.execute(
                "UPDATE xin_steps
                 SET status = 'pending', retry_count = retry_count + 1, last_output = ?1,
                     lease_owner = NULL, lease_expires_at = NULL, updated_at = ?2
                 WHERE id = ?3 AND lease_owner = ?4 AND lease_epoch = ?5",
                params![
                    bounded,
                    now.to_rfc3339(),
                    step_id,
                    lease.worker_id,
                    i64::try_from(lease.epoch).unwrap_or(i64::MAX)
                ],
            )?;
            emit_step_event(conn, config, step_id, "xin.step.retry", Some("pending"), Some(lease))?;
        } else {
            conn.execute(
                "UPDATE xin_steps
                 SET status = 'failed', retry_count = retry_count + 1, last_output = ?1,
                     completed_at = ?2, lease_owner = NULL, lease_expires_at = NULL, updated_at = ?2
                 WHERE id = ?3 AND lease_owner = ?4 AND lease_epoch = ?5",
                params![
                    bounded,
                    now.to_rfc3339(),
                    step_id,
                    lease.worker_id,
                    i64::try_from(lease.epoch).unwrap_or(i64::MAX)
                ],
            )?;
            emit_step_event(conn, config, step_id, "xin.step.failed", Some("failed"), Some(lease))?;
            conn.execute(
                "UPDATE xin_goals SET status = 'failed', updated_at = ?1 WHERE id = ?2",
                params![now.to_rfc3339(), goal_id],
            )?;
        }
        Ok(true)
    })
}

/// Mark a step completed, recompute the goal's progress and roll the goal up to
/// `completed` once every step is done.
#[cfg(test)]
pub fn complete_step(config: &Config, step_id: &str, output: &str) -> Result<()> {
    let now = Utc::now();
    let bounded = truncate_output(output);
    with_connection(config, |conn| {
        let goal_id: Option<String> = conn
            .query_row("SELECT goal_id FROM xin_steps WHERE id = ?1", params![step_id], |row| {
                row.get(0)
            })
            .ok();
        let changed = conn
            .execute(
                "UPDATE xin_steps
                 SET status = 'completed', completed_at = ?1, last_output = ?2,
                     lease_owner = NULL, lease_expires_at = NULL, updated_at = ?1
                 WHERE id = ?3",
                params![now.to_rfc3339(), bounded, step_id],
            )
            .context("Failed to complete xin step")?;
        if changed == 0 {
            tracing::warn!(step_id = %step_id, "complete_step: no rows affected");
            return Ok(());
        }
        emit_step_event(conn, config, step_id, "xin.step.completed", Some("completed"), None)?;
        if let Some(goal_id) = goal_id {
            recompute_goal_progress(conn, config, &goal_id, now)?;
        }
        Ok(())
    })
}

/// Mark a step failed. If retries remain the step is reset to `pending` (lease
/// released) for re-execution; otherwise it is terminally `failed` and the goal
/// rolls up to `failed`.
#[cfg(test)]
pub fn fail_step(config: &Config, step_id: &str, output: &str) -> Result<()> {
    let now = Utc::now();
    let bounded = truncate_output(output);
    with_connection(config, |conn| {
        let row: Option<(String, i64, i64)> = conn
            .query_row(
                "SELECT goal_id, retry_count, max_retries FROM xin_steps WHERE id = ?1",
                params![step_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();
        let Some((goal_id, retry_count, max_retries)) = row else {
            tracing::warn!(step_id = %step_id, "fail_step: step not found");
            return Ok(());
        };

        let retries_left = retry_count < max_retries;
        if retries_left {
            conn.execute(
                "UPDATE xin_steps
                 SET status = 'pending', retry_count = retry_count + 1, last_output = ?1,
                     lease_owner = NULL, lease_expires_at = NULL, updated_at = ?2
                 WHERE id = ?3",
                params![bounded, now.to_rfc3339(), step_id],
            )
            .context("Failed to reset xin step for retry")?;
            emit_step_event(conn, config, step_id, "xin.step.retry", Some("pending"), None)?;
        } else {
            conn.execute(
                "UPDATE xin_steps
                 SET status = 'failed', retry_count = retry_count + 1, last_output = ?1,
                     completed_at = ?2, lease_owner = NULL, lease_expires_at = NULL, updated_at = ?2
                 WHERE id = ?3",
                params![bounded, now.to_rfc3339(), step_id],
            )
            .context("Failed to fail xin step")?;
            emit_step_event(conn, config, step_id, "xin.step.failed", Some("failed"), None)?;
            conn.execute(
                "UPDATE xin_goals SET status = 'failed', updated_at = ?1 WHERE id = ?2",
                params![now.to_rfc3339(), goal_id],
            )
            .context("Failed to mark goal failed")?;
        }
        Ok(())
    })
}

/// Reset steps whose lease expired (while claimed/running) back to `stale` so
/// they can be re-claimed. Returns the affected step ids.
pub fn mark_steps_stale(config: &Config, now: DateTime<Utc>) -> Result<Vec<String>> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id FROM xin_steps
             WHERE status IN ('claimed', 'running')
               AND lease_expires_at IS NOT NULL AND lease_expires_at < ?1",
        )?;
        let ids = stmt
            .query_map(params![now.to_rfc3339()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        if ids.is_empty() {
            return Ok(ids);
        }
        conn.execute(
            "UPDATE xin_steps
             SET status = 'stale', lease_owner = NULL, lease_expires_at = NULL, updated_at = ?1
             WHERE status IN ('claimed', 'running')
               AND lease_expires_at IS NOT NULL AND lease_expires_at < ?1",
            params![now.to_rfc3339()],
        )
        .context("Failed to mark xin steps stale")?;
        for id in &ids {
            emit_step_event(conn, config, id, "xin.step.stale", Some("stale"), None)?;
        }
        Ok(ids)
    })
}

/// List steps whose lease has expired while claimed/running.
pub fn expired_step_leases(config: &Config, now: DateTime<Utc>) -> Result<Vec<XinStep>> {
    with_connection(config, |conn| {
        let sql = format!(
            "{SELECT_STEP_COLUMNS} WHERE status IN ('claimed', 'running') \
             AND lease_expires_at IS NOT NULL AND lease_expires_at < ?1 ORDER BY sequence ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![now.to_rfc3339()], map_step_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    })
}

/// Result of attempting to adopt one stale legacy task.
pub(crate) struct LegacyTaskAdoption {
    pub(crate) goal: XinGoal,
    pub(crate) newly_adopted: bool,
}

/// Atomically adopt one enabled, stale, non-recurring legacy task into an
/// ordered two-step goal. Replays return the existing linked goal.
pub(crate) fn adopt_legacy_task(config: &Config, task_id: &str) -> Result<Option<LegacyTaskAdoption>> {
    let outcome = with_immediate_connection(config, |conn| {
        let existing_goal_id = conn
            .query_row(
                "SELECT goal_id FROM xin_task_adoptions WHERE legacy_task_id = ?1",
                params![task_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(goal_id) = existing_goal_id {
            conn.execute(
                "UPDATE xin_tasks SET enabled = 0 WHERE id = ?1 AND enabled != 0",
                params![task_id],
            )?;
            return Ok(Some((goal_id, false)));
        }

        let task = conn
            .query_row(SELECT_ALL_COLUMNS, params![task_id], map_task_row)
            .optional()?;
        let Some(task) = task else {
            return Ok(None);
        };
        if task.recurring || !task.enabled || task.status != TaskStatus::Stale {
            return Ok(None);
        }

        let goal_id = Uuid::new_v4().to_string();
        let now = authoritative_now(conn)?;
        let migrated_step = NewXinStep {
            sequence: 1,
            name: task.name.clone(),
            description: task.description.clone(),
            execution_mode: task.execution_mode.clone(),
            payload: task.payload.clone(),
            max_retries: task.max_failures,
            approval_grant_json: task.approval_grant_json.clone(),
            lease_ttl_secs: 0,
        };
        let verification_step = NewXinStep {
            sequence: 2,
            name: format!("{}::verify", task.name),
            description: Some("post-adoption completion marker".to_string()),
            execution_mode: ExecutionMode::Internal,
            payload: "xin:health_check".to_string(),
            max_retries: 0,
            approval_grant_json: None,
            lease_ttl_secs: 0,
        };
        let goal = NewXinGoal {
            owner_id: task.owner_id.clone(),
            topic_id: task.topic_id.clone(),
            parent_task_id: task.parent_task_id.clone(),
            source_message_event_id: task.source_message_event_id.clone(),
            name: task.name.clone(),
            description: task.description.clone(),
            kind: task.kind.clone(),
            priority: task.priority,
            target_completion_at: None,
            initial_steps: vec![migrated_step, verification_step],
        };
        insert_goal_rows(conn, config, &goal_id, &goal, now)?;
        conn.execute(
            "INSERT INTO xin_task_adoptions (legacy_task_id, goal_id, adopted_at)
             VALUES (?1, ?2, ?3)",
            params![task_id, goal_id, now.to_rfc3339()],
        )
        .context("Failed to link adopted Xin task to goal")?;
        conn.execute(
            "UPDATE xin_tasks SET enabled = 0, updated_at = ?1 WHERE id = ?2",
            params![now.to_rfc3339(), task_id],
        )
        .context("Failed to disable adopted Xin task")?;
        insert_task_event(
            conn,
            &workspace_id(config),
            task_id,
            TaskLineage {
                owner_id: task.owner_id,
                topic_id: task.topic_id,
                parent_task_id: task.parent_task_id,
                source_message_event_id: task.source_message_event_id,
                status: Some(task.status.as_str().to_string()),
            },
            "xin.task.adopted",
            Some(task.status.as_str()),
            Some(
                serde_json::json!({
                    "goal_id": goal_id,
                    "legacy_task_disabled": true,
                })
                .to_string(),
            )
            .as_deref(),
        )?;
        Ok(Some((goal_id, true)))
    })?;

    let Some((goal_id, newly_adopted)) = outcome else {
        return Ok(None);
    };
    Ok(Some(LegacyTaskAdoption {
        goal: get_goal(config, &goal_id)?,
        newly_adopted,
    }))
}

/// Migrate a legacy non-recurring `XinTask` into a single-step `XinGoal`.
///
/// The original `xin_tasks` row is left untouched (zero-breakage, d09 §4.2).
#[cfg(test)]
pub fn migrate_task_to_goal(config: &Config, task_id: &str) -> Result<XinGoal> {
    let task = get_task(config, task_id)?;
    if task.recurring {
        anyhow::bail!("recurring xin task '{task_id}' cannot be migrated to a goal (keep as legacy)");
    }
    let step = NewXinStep {
        sequence: 1,
        name: task.name.clone(),
        description: task.description.clone(),
        execution_mode: task.execution_mode,
        payload: task.payload,
        max_retries: task.max_failures,
        approval_grant_json: task.approval_grant_json,
        lease_ttl_secs: 0,
    };
    let new_goal = NewXinGoal {
        owner_id: task.owner_id,
        topic_id: task.topic_id,
        parent_task_id: task.parent_task_id,
        source_message_event_id: task.source_message_event_id,
        name: task.name,
        description: task.description,
        kind: task.kind,
        priority: task.priority,
        target_completion_at: None,
        initial_steps: vec![step],
    };
    add_goal(config, &new_goal)
}

/// Recompute `steps_completed` and roll the goal status up. Called inside an
/// existing transaction after a step transition.
fn recompute_goal_progress(conn: &Connection, config: &Config, goal_id: &str, now: DateTime<Utc>) -> Result<()> {
    let completed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM xin_steps WHERE goal_id = ?1 AND status = 'completed'",
        params![goal_id],
        |row| row.get(0),
    )?;
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM xin_steps WHERE goal_id = ?1",
        params![goal_id],
        |row| row.get(0),
    )?;

    let all_done = total > 0 && completed >= total;
    if all_done {
        let final_output: Option<String> = conn
            .query_row(
                "SELECT last_output FROM xin_steps WHERE goal_id = ?1 ORDER BY sequence DESC LIMIT 1",
                params![goal_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        conn.execute(
            "UPDATE xin_goals
             SET steps_completed = ?1, status = 'completed', completed_at = ?2,
                 final_output = ?3, updated_at = ?2
             WHERE id = ?4",
            params![completed, now.to_rfc3339(), final_output, goal_id],
        )
        .context("Failed to mark goal completed")?;
        emit_goal_event(conn, config, goal_id, "xin.goal.completed", Some("completed"))?;
    } else {
        conn.execute(
            "UPDATE xin_goals
             SET steps_completed = ?1, status = 'running', updated_at = ?2
             WHERE id = ?3 AND status != 'completed'",
            params![completed, now.to_rfc3339(), goal_id],
        )
        .context("Failed to update goal progress")?;
    }
    Ok(())
}

/// Emit a lifecycle event for a step (mirrored via the goal's lineage).
fn emit_step_event(
    conn: &Connection,
    config: &Config,
    step_id: &str,
    event_type: &str,
    status: Option<&str>,
    lease: Option<&XinStepLease>,
) -> Result<()> {
    let goal_id: Option<String> = conn
        .query_row("SELECT goal_id FROM xin_steps WHERE id = ?1", params![step_id], |row| {
            row.get(0)
        })
        .ok();
    let Some(goal_id) = goal_id else {
        return Ok(());
    };
    let Some(lineage) = load_goal_lineage(conn, &goal_id)? else {
        return Ok(());
    };
    let payload = lease.map_or_else(
        || serde_json::json!({ "step_id": step_id }),
        |lease| {
            serde_json::json!({
                "step_id": step_id,
                "lease_owner": lease.worker_id,
                "lease_epoch": lease.epoch,
            })
        },
    );
    insert_task_event(
        conn,
        &workspace_id(config),
        &goal_id,
        lineage,
        event_type,
        status,
        Some(payload.to_string()).as_deref(),
    )
}

/// Emit a lifecycle event for a goal.
fn emit_goal_event(
    conn: &Connection,
    config: &Config,
    goal_id: &str,
    event_type: &str,
    status: Option<&str>,
) -> Result<()> {
    let Some(lineage) = load_goal_lineage(conn, goal_id)? else {
        return Ok(());
    };
    insert_task_event(conn, &workspace_id(config), goal_id, lineage, event_type, status, None)
}

fn load_goal_lineage(conn: &Connection, goal_id: &str) -> Result<Option<TaskLineage>> {
    let mut stmt = conn.prepare(
        "SELECT owner_id, topic_id, parent_task_id, source_message_event_id, status
         FROM xin_goals WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![goal_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(TaskLineage {
        owner_id: row.get(0)?,
        topic_id: row.get(1)?,
        parent_task_id: row.get(2)?,
        source_message_event_id: row.get(3)?,
        status: row.get(4)?,
    }))
}

fn map_goal_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<XinGoal> {
    let created_at_raw: String = row.get(13)?;
    let updated_at_raw: String = row.get(14)?;
    let target_raw: Option<String> = row.get(10)?;
    let completed_raw: Option<String> = row.get(15)?;
    Ok(XinGoal {
        id: row.get(0)?,
        owner_id: row.get(1)?,
        topic_id: row.get(2)?,
        parent_task_id: row.get(3)?,
        source_message_event_id: row.get(4)?,
        name: row.get(5)?,
        description: row.get(6)?,
        kind: TaskKind::from_str_lossy(&row.get::<_, String>(7)?),
        status: GoalStatus::from_str_lossy(&row.get::<_, String>(8)?),
        priority: TaskPriority::from_i32(row.get(9)?),
        target_completion_at: match target_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_err)?),
            None => None,
        },
        steps_completed: u32::try_from(row.get::<_, i64>(11)?).unwrap_or(0),
        steps_total: u32::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_err)?,
        updated_at: parse_rfc3339(&updated_at_raw).map_err(sql_err)?,
        completed_at: match completed_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_err)?),
            None => None,
        },
        final_output: row.get(16)?,
        enabled: row.get::<_, i64>(17)? != 0,
    })
}

fn map_step_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<XinStep> {
    let created_at_raw: String = row.get(16)?;
    let updated_at_raw: String = row.get(17)?;
    let lease_raw: Option<String> = row.get(10)?;
    let hb_raw: Option<String> = row.get(11)?;
    let started_raw: Option<String> = row.get(18)?;
    let completed_raw: Option<String> = row.get(19)?;
    Ok(XinStep {
        id: row.get(0)?,
        goal_id: row.get(1)?,
        sequence: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        name: row.get(3)?,
        description: row.get(4)?,
        status: StepStatus::from_str_lossy(&row.get::<_, String>(5)?),
        execution_mode: ExecutionMode::from_str_lossy(&row.get::<_, String>(6)?),
        payload: row.get(7)?,
        lease_owner: row.get(8)?,
        lease_epoch: u64::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
        lease_expires_at: match lease_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_err)?),
            None => None,
        },
        last_heartbeat_at: match hb_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_err)?),
            None => None,
        },
        checkpoint_json: row.get(12)?,
        lease_ttl_secs: u64::try_from(row.get::<_, i64>(13)?).unwrap_or(0),
        retry_count: u32::try_from(row.get::<_, i64>(14)?).unwrap_or(0),
        max_retries: u32::try_from(row.get::<_, i64>(15)?).unwrap_or(0),
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_err)?,
        updated_at: parse_rfc3339(&updated_at_raw).map_err(sql_err)?,
        started_at: match started_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_err)?),
            None => None,
        },
        completed_at: match completed_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_err)?),
            None => None,
        },
        last_output: row.get(20)?,
        approval_grant_json: row.get(21)?,
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────

const SELECT_ALL_COLUMNS: &str = "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
            name, description, kind, status, priority, execution_mode,
            payload, recurring, interval_secs, created_at, updated_at,
            last_run_at, next_run_at, last_status, last_output,
            run_count, fail_count, max_failures, enabled, approval_grant_json
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

#[derive(Debug, Clone)]
struct TaskLineage {
    owner_id: Option<String>,
    topic_id: Option<String>,
    parent_task_id: Option<String>,
    source_message_event_id: Option<String>,
    status: Option<String>,
}

fn workspace_id(config: &Config) -> String {
    config.workspace_dir.to_string_lossy().to_string()
}

fn load_task_lineage(conn: &Connection, task_id: &str) -> Result<Option<TaskLineage>> {
    let mut stmt = conn.prepare(
        "SELECT owner_id, topic_id, parent_task_id, source_message_event_id, status
         FROM xin_tasks
         WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![task_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(TaskLineage {
        owner_id: row.get(0)?,
        topic_id: row.get(1)?,
        parent_task_id: row.get(2)?,
        source_message_event_id: row.get(3)?,
        status: row.get(4)?,
    }))
}

fn insert_task_event(
    conn: &Connection,
    workspace_id: &str,
    task_id: &str,
    lineage: TaskLineage,
    event_type: &str,
    status: Option<&str>,
    payload_json: Option<&str>,
) -> Result<()> {
    let event_id = Uuid::new_v4().to_string();
    let created_at = Utc::now().to_rfc3339();
    conn.execute_batch("SAVEPOINT xin_task_event_append")?;
    let append = (|| -> Result<()> {
        conn.execute(
            "INSERT INTO xin_task_events (
            event_id, task_id, workspace_id, owner_id, topic_id, parent_task_id, source_message_event_id,
            event_type, status, payload_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                event_id,
                task_id,
                workspace_id,
                lineage.owner_id.as_deref(),
                lineage.topic_id.as_deref(),
                lineage.parent_task_id.as_deref(),
                lineage.source_message_event_id.as_deref(),
                event_type,
                status,
                payload_json,
                created_at,
            ],
        )
        .context("Failed to insert xin task event")?;
        let (mirror_event_type, mirror_payload_json) =
            build_xin_task_event_mirror(task_id, &lineage, event_type, status, payload_json);
        conn.execute(
            "INSERT INTO xin_event_outbox (
            event_id, workspace_id, task_id, event_type, payload_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event_id,
                workspace_id,
                task_id,
                mirror_event_type,
                mirror_payload_json,
                created_at,
            ],
        )
        .context("Failed to enqueue xin task event mirror")?;
        Ok(())
    })();
    match append {
        Ok(()) => {
            conn.execute_batch("RELEASE SAVEPOINT xin_task_event_append")?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO SAVEPOINT xin_task_event_append;
                 RELEASE SAVEPOINT xin_task_event_append;",
            );
            Err(error)
        }
    }
}

fn build_xin_task_event_mirror(
    task_id: &str,
    lineage: &TaskLineage,
    event_type: &str,
    status: Option<&str>,
    payload_json: Option<&str>,
) -> (String, String) {
    let mirrored_event_type = if event_type == "xin.task.created" {
        "xin.task.spawned"
    } else {
        event_type
    };
    let mut payload = payload_json
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    payload.insert("owner_id".to_string(), lineage.owner_id.clone().into());
    payload.insert("topic_id".to_string(), lineage.topic_id.clone().into());
    payload.insert("parent_task_id".to_string(), lineage.parent_task_id.clone().into());
    payload.insert(
        "source_message_event_id".to_string(),
        lineage.source_message_event_id.clone().into(),
    );
    payload.insert(
        "status".to_string(),
        status.map(str::to_string).or_else(|| lineage.status.clone()).into(),
    );
    payload.insert("task_id".to_string(), task_id.to_string().into());
    if !payload.contains_key("task") {
        if let Some(name) = payload.get("name").and_then(serde_json::Value::as_str) {
            payload.insert("task".to_string(), name.to_string().into());
        }
    }
    let payload_json = serde_json::Value::Object(payload).to_string();
    (mirrored_event_type.to_string(), payload_json)
}

/// Deliver committed Xin event-outbox rows into the shared memory event spine.
/// The outbox event id is reused as the mirror event id, so a crash after the
/// external insert but before `delivered_at` is safe to replay.
fn deliver_pending_xin_event_outbox(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT event_id, workspace_id, task_id, event_type, payload_json
         FROM xin_event_outbox
         WHERE delivered_at IS NULL
         ORDER BY created_at ASC
         LIMIT 100",
    )?;
    let pending = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut delivered = 0;
    for (event_id, workspace_id, task_id, event_type, payload_json) in pending {
        let result = crate::memory::sqlite::append_task_event_mirror_idempotent(
            std::path::Path::new(&workspace_id),
            &event_id,
            crate::memory::sqlite::SqliteTaskEventMirror {
                workspace_id: &workspace_id,
                task_id: &task_id,
                event_type: &event_type,
                session_key: None,
                agent_id: None,
                persona_id: None,
                payload_json: Some(&payload_json),
            },
        );
        match result {
            Ok(_) => {
                conn.execute(
                    "UPDATE xin_event_outbox
                     SET delivered_at = ?1, attempt_count = attempt_count + 1, last_error = NULL
                     WHERE event_id = ?2 AND delivered_at IS NULL",
                    params![Utc::now().to_rfc3339(), event_id],
                )?;
                delivered += 1;
            }
            Err(error) => {
                let error = truncate_output(&error.to_string());
                conn.execute(
                    "UPDATE xin_event_outbox
                     SET attempt_count = attempt_count + 1, last_error = ?1
                     WHERE event_id = ?2 AND delivered_at IS NULL",
                    params![error, event_id],
                )?;
                tracing::warn!(task_id, event_type, "failed to deliver Xin event outbox row: {error}");
            }
        }
    }
    Ok(delivered)
}

fn map_task_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<XinTaskEvent> {
    let created_at_raw: String = row.get(11)?;
    Ok(XinTaskEvent {
        id: row.get(0)?,
        event_id: row.get(1)?,
        task_id: row.get(2)?,
        workspace_id: row.get(3)?,
        owner_id: row.get(4)?,
        topic_id: row.get(5)?,
        parent_task_id: row.get(6)?,
        source_message_event_id: row.get(7)?,
        event_type: row.get(8)?,
        status: row.get(9)?,
        payload_json: row.get(10)?,
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_err)?,
    })
}

fn map_task_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<XinTask> {
    let created_at_raw: String = row.get(14)?;
    let updated_at_raw: String = row.get(15)?;
    let last_run_raw: Option<String> = row.get(16)?;
    let next_run_raw: String = row.get(17)?;

    Ok(XinTask {
        id: row.get(0)?,
        owner_id: row.get(1)?,
        topic_id: row.get(2)?,
        parent_task_id: row.get(3)?,
        source_message_event_id: row.get(4)?,
        name: row.get(5)?,
        description: row.get(6)?,
        kind: TaskKind::from_str_lossy(&row.get::<_, String>(7)?),
        status: TaskStatus::from_str_lossy(&row.get::<_, String>(8)?),
        priority: TaskPriority::from_i32(row.get(9)?),
        execution_mode: ExecutionMode::from_str_lossy(&row.get::<_, String>(10)?),
        payload: row.get(11)?,
        recurring: row.get::<_, i64>(12)? != 0,
        interval_secs: u64::try_from(row.get::<_, i64>(13)?).unwrap_or(0),
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_err)?,
        updated_at: parse_rfc3339(&updated_at_raw).map_err(sql_err)?,
        last_run_at: match last_run_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_err)?),
            None => None,
        },
        next_run_at: parse_rfc3339(&next_run_raw).map_err(sql_err)?,
        last_status: row.get(18)?,
        last_output: row.get(19)?,
        run_count: u64::try_from(row.get::<_, i64>(20)?).unwrap_or(0),
        fail_count: u64::try_from(row.get::<_, i64>(21)?).unwrap_or(0),
        max_failures: u32::try_from(row.get::<_, i64>(22)?).unwrap_or(0),
        enabled: row.get::<_, i64>(23)? != 0,
        approval_grant_json: row.get(24)?,
    })
}

fn add_column_if_missing(conn: &Connection, table: &str, name: &str, sql_type: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        if col_name == name {
            return Ok(());
        }
    }
    drop(rows);
    drop(stmt);

    match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {sql_type}"), []) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
            tracing::debug!("Column {table}.{name} already exists (concurrent migration): {err}");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to add {table}.{name}")),
    }
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
            owner_id         TEXT,
            topic_id         TEXT,
            parent_task_id   TEXT,
            source_message_event_id TEXT,
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
            enabled          INTEGER NOT NULL DEFAULT 1,
            approval_grant_json TEXT
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
         CREATE INDEX IF NOT EXISTS idx_xin_runs_started_at ON xin_runs(started_at);

         CREATE TABLE IF NOT EXISTS xin_task_events (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id       TEXT NOT NULL UNIQUE,
            task_id        TEXT NOT NULL,
            workspace_id   TEXT NOT NULL,
            owner_id       TEXT,
            topic_id       TEXT,
            parent_task_id TEXT,
            source_message_event_id TEXT,
            event_type     TEXT NOT NULL,
            status         TEXT,
            payload_json   TEXT,
            created_at     TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_xin_task_events_task_id ON xin_task_events(task_id, id);
         CREATE INDEX IF NOT EXISTS idx_xin_task_events_owner ON xin_task_events(workspace_id, owner_id, id);
         CREATE INDEX IF NOT EXISTS idx_xin_task_events_topic ON xin_task_events(workspace_id, topic_id, id);
         CREATE INDEX IF NOT EXISTS idx_xin_task_events_type ON xin_task_events(event_type, id);

         CREATE TABLE IF NOT EXISTS xin_event_outbox (
            event_id       TEXT PRIMARY KEY,
            workspace_id   TEXT NOT NULL,
            task_id        TEXT NOT NULL,
            event_type     TEXT NOT NULL,
            payload_json   TEXT NOT NULL,
            created_at     TEXT NOT NULL,
            delivered_at   TEXT,
            attempt_count  INTEGER NOT NULL DEFAULT 0,
            last_error     TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_xin_event_outbox_pending
            ON xin_event_outbox(delivered_at, created_at);

         CREATE TABLE IF NOT EXISTS xin_goals (
            id                      TEXT PRIMARY KEY,
            owner_id                TEXT,
            topic_id                TEXT,
            parent_task_id          TEXT,
            source_message_event_id TEXT,
            name                    TEXT NOT NULL,
            description             TEXT,
            kind                    TEXT NOT NULL DEFAULT 'user',
            status                  TEXT NOT NULL DEFAULT 'pending',
            priority                INTEGER NOT NULL DEFAULT 1,
            target_completion_at    TEXT,
            steps_completed         INTEGER NOT NULL DEFAULT 0,
            steps_total             INTEGER NOT NULL DEFAULT 0,
            created_at              TEXT NOT NULL,
            updated_at              TEXT NOT NULL,
            completed_at            TEXT,
            final_output            TEXT,
            enabled                 INTEGER NOT NULL DEFAULT 1
         );
         CREATE INDEX IF NOT EXISTS idx_xin_goals_owner  ON xin_goals(owner_id, status);
         CREATE INDEX IF NOT EXISTS idx_xin_goals_topic  ON xin_goals(topic_id, status);
         CREATE INDEX IF NOT EXISTS idx_xin_goals_status ON xin_goals(status, priority DESC);

         CREATE TABLE IF NOT EXISTS xin_steps (
            id                   TEXT PRIMARY KEY,
            goal_id              TEXT NOT NULL REFERENCES xin_goals(id) ON DELETE CASCADE,
            sequence             INTEGER NOT NULL,
            name                 TEXT NOT NULL,
            description          TEXT,
            status               TEXT NOT NULL DEFAULT 'pending',
            execution_mode       TEXT NOT NULL DEFAULT 'agent_session',
            payload              TEXT NOT NULL DEFAULT '',
            lease_owner          TEXT,
            lease_epoch          INTEGER NOT NULL DEFAULT 0,
            lease_expires_at     TEXT,
            last_heartbeat_at    TEXT,
            checkpoint_json      TEXT,
            lease_ttl_secs       INTEGER NOT NULL DEFAULT 0,
            retry_count          INTEGER NOT NULL DEFAULT 0,
            max_retries          INTEGER NOT NULL DEFAULT 3,
            created_at           TEXT NOT NULL,
            updated_at           TEXT NOT NULL,
            started_at           TEXT,
            completed_at         TEXT,
            last_output          TEXT,
            approval_grant_json  TEXT,
            UNIQUE (goal_id, sequence)
         );
         CREATE INDEX IF NOT EXISTS idx_xin_steps_goal  ON xin_steps(goal_id, sequence);
         CREATE INDEX IF NOT EXISTS idx_xin_steps_due   ON xin_steps(status, lease_expires_at);
         CREATE INDEX IF NOT EXISTS idx_xin_steps_owner ON xin_steps(lease_owner, status);

         CREATE TABLE IF NOT EXISTS xin_task_adoptions (
            legacy_task_id TEXT PRIMARY KEY REFERENCES xin_tasks(id) ON DELETE CASCADE,
            goal_id        TEXT NOT NULL UNIQUE REFERENCES xin_goals(id) ON DELETE CASCADE,
            adopted_at     TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_xin_task_adoptions_goal
            ON xin_task_adoptions(goal_id);",
    )
    .context("Failed to initialize xin schema")?;

    add_column_if_missing(&conn, "xin_tasks", "owner_id", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "topic_id", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "parent_task_id", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "source_message_event_id", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "approval_grant_json", "TEXT")?;
    add_column_if_missing(&conn, "xin_task_events", "source_message_event_id", "TEXT")?;
    // Forward-compat for any xin_steps table created before lease_ttl_secs existed.
    add_column_if_missing(&conn, "xin_steps", "lease_ttl_secs", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(&conn, "xin_steps", "lease_epoch", "INTEGER NOT NULL DEFAULT 0")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_xin_tasks_owner ON xin_tasks(owner_id, status, next_run_at);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_topic ON xin_tasks(topic_id, status, next_run_at);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_parent ON xin_tasks(parent_task_id, id);",
    )
    .context("Failed to initialize xin lineage indexes")?;

    let result = f(&conn);
    if let Err(error) = deliver_pending_xin_event_outbox(&conn) {
        tracing::warn!("failed to drain Xin event outbox: {error}");
    }
    result
}

fn with_immediate_connection<T>(config: &Config, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    with_connection(config, |conn| {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(conn) {
            Ok(value) => {
                conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    })
}

fn authoritative_now(conn: &Connection) -> Result<DateTime<Utc>> {
    let raw: String = conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| row.get(0))?;
    Ok(DateTime::parse_from_rfc3339(&raw)?.with_timezone(&Utc))
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

    /// Opens the Xin repository database for tests that must hold a write lock
    /// across a concurrent repository call or deliberately expire a lease.
    fn open_xin_test_connection(config: &Config) -> Connection {
        let db_path = config.workspace_dir.join("xin").join("tasks.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
        conn
    }

    fn sample_task() -> NewXinTask {
        NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "test_task".into(),
            description: Some("A test task".into()),
            kind: TaskKind::User,
            priority: TaskPriority::Normal,
            execution_mode: ExecutionMode::AgentSession,
            payload: "hello world".into(),
            recurring: false,
            interval_secs: 0,
            max_failures: 3,
            approval_grant_json: None,
        }
    }

    fn recurring_task() -> NewXinTask {
        NewXinTask {
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            name: "recurring_task".into(),
            description: None,
            kind: TaskKind::System,
            priority: TaskPriority::High,
            execution_mode: ExecutionMode::Internal,
            payload: "health_check".into(),
            recurring: true,
            interval_secs: 300,
            max_failures: 5,
            approval_grant_json: None,
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
    fn add_task_persists_owner_topic_lineage_and_event() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut new = sample_task();
        new.owner_id = Some("owner:workspace:telegram:alice".to_string());
        new.topic_id = Some("topic-1".to_string());
        new.parent_task_id = Some("run-parent".to_string());
        new.source_message_event_id = Some("msg-1".to_string());

        let task = add_task(&config, &new).unwrap();
        assert_eq!(task.owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
        assert_eq!(task.topic_id.as_deref(), Some("topic-1"));
        assert_eq!(task.parent_task_id.as_deref(), Some("run-parent"));
        assert_eq!(task.source_message_event_id.as_deref(), Some("msg-1"));

        let listed = list_tasks(&config).unwrap();
        assert_eq!(listed[0].owner_id.as_deref(), Some("owner:workspace:telegram:alice"));

        let events = list_task_events(&config, &task.id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "xin.task.created");
        assert_eq!(events[0].owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
        assert_eq!(events[0].topic_id.as_deref(), Some("topic-1"));
        assert_eq!(events[0].parent_task_id.as_deref(), Some("run-parent"));
        assert_eq!(events[0].source_message_event_id.as_deref(), Some("msg-1"));

        let memory_conn = Connection::open(config.workspace_dir.join("memory").join("brain.db")).unwrap();
        let (event_type, subject_id, payload): (String, String, String) = memory_conn
            .query_row(
                "SELECT event_type, subject_id, payload_json
                 FROM memory_events
                 WHERE subject_table = 'tasks' AND subject_id = ?1
                 ORDER BY id DESC
                 LIMIT 1",
                params![task.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(event_type, "xin.task.spawned");
        assert_eq!(subject_id, task.id);
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["owner_id"].as_str(), Some("owner:workspace:telegram:alice"));
        assert_eq!(payload["topic_id"].as_str(), Some("topic-1"));
        assert_eq!(payload["parent_task_id"].as_str(), Some("run-parent"));
        assert_eq!(payload["source_message_event_id"].as_str(), Some("msg-1"));
        assert_eq!(payload["task"].as_str(), Some("test_task"));
    }

    #[test]
    fn legacy_xin_tasks_schema_migrates_lineage_columns_and_events_table() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let db_dir = config.workspace_dir.join("xin");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("tasks.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE xin_tasks (
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
             );",
        )
        .unwrap();
        drop(conn);

        let tasks = list_tasks(&config).unwrap();
        assert!(tasks.is_empty());

        with_connection(&config, |conn| {
            let mut stmt = conn.prepare("PRAGMA table_info(xin_tasks)")?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for name in [
                "owner_id",
                "topic_id",
                "parent_task_id",
                "source_message_event_id",
                "approval_grant_json",
            ] {
                assert!(names.iter().any(|existing| existing == name), "missing {name}");
            }
            let event_tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'xin_task_events'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(event_tables, 1);
            let mut event_stmt = conn.prepare("PRAGMA table_info(xin_task_events)")?;
            let event_names = event_stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert!(
                event_names.iter().any(|existing| existing == "source_message_event_id"),
                "missing xin_task_events.source_message_event_id"
            );
            let mut step_stmt = conn.prepare("PRAGMA table_info(xin_steps)")?;
            let step_names = step_stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert!(
                step_names.iter().any(|existing| existing == "lease_epoch"),
                "missing xin_steps.lease_epoch"
            );
            let adoption_tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'xin_task_adoptions'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(adoption_tables, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn xin_task_lifecycle_records_owner_scoped_events() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut new = recurring_task();
        new.owner_id = Some("owner-a".to_string());
        new.topic_id = Some("topic-a".to_string());
        new.source_message_event_id = Some("msg-a".to_string());
        let task = add_task(&config, &new).unwrap();

        assert!(claim_task(&config, &task.id).unwrap());
        mark_completed(&config, &task.id, "done").unwrap();
        let started = Utc::now();
        record_run(&config, &task.id, started, started, "ok", Some("done"), 10).unwrap();
        reschedule_recurring(&config, &task.id).unwrap();

        let events = list_task_events(&config, &task.id).unwrap();
        let event_types = events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>();
        assert!(event_types.contains(&"xin.task.created"));
        assert!(event_types.contains(&"xin.task.claimed"));
        assert!(event_types.contains(&"xin.task.completed"));
        assert!(event_types.contains(&"xin.task.run_recorded"));
        assert!(event_types.contains(&"xin.task.rescheduled"));
        assert!(events.iter().all(|event| event.owner_id.as_deref() == Some("owner-a")));
        assert!(events.iter().all(|event| event.topic_id.as_deref() == Some("topic-a")));
        assert!(
            events
                .iter()
                .all(|event| event.source_message_event_id.as_deref() == Some("msg-a"))
        );
    }

    #[test]
    fn recurring_execution_commits_result_run_reschedule_and_events_together() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &recurring_task()).unwrap();
        assert!(claim_task(&config, &task.id).unwrap());

        let started_at = Utc::now();
        let finished_at = started_at + chrono::Duration::seconds(2);
        assert!(commit_task_execution(&config, &task.id, true, "healthy", started_at, finished_at, 2_000,).unwrap());

        let committed = get_task(&config, &task.id).unwrap();
        assert_eq!(committed.status, TaskStatus::Pending);
        assert_eq!(committed.run_count, 1);
        assert_eq!(committed.fail_count, 0);
        assert_eq!(committed.last_output.as_deref(), Some("healthy"));
        assert_eq!(committed.next_run_at, finished_at + chrono::Duration::seconds(300));

        let conn = open_xin_test_connection(&config);
        let runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM xin_runs WHERE task_id = ?1",
                params![task.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1);
        let undelivered: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM xin_event_outbox WHERE task_id = ?1 AND delivered_at IS NULL",
                params![task.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(undelivered, 0);

        let event_types = list_task_events(&config, &task.id)
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(event_types.ends_with(&[
            "xin.task.completed".to_string(),
            "xin.task.run_recorded".to_string(),
            "xin.task.rescheduled".to_string(),
        ]));
    }

    #[test]
    fn recurring_execution_rolls_back_when_event_outbox_enqueue_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &recurring_task()).unwrap();
        assert!(claim_task(&config, &task.id).unwrap());
        let conn = open_xin_test_connection(&config);
        conn.execute_batch(
            "CREATE TRIGGER fail_xin_outbox
             BEFORE INSERT ON xin_event_outbox
             BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;",
        )
        .unwrap();
        drop(conn);

        let started_at = Utc::now();
        let result = commit_task_execution(
            &config,
            &task.id,
            true,
            "must roll back",
            started_at,
            started_at + chrono::Duration::seconds(1),
            1_000,
        );
        assert!(result.is_err());

        let rolled_back = get_task(&config, &task.id).unwrap();
        assert_eq!(rolled_back.status, TaskStatus::Running);
        assert_eq!(rolled_back.run_count, 0);
        assert_eq!(rolled_back.fail_count, 0);
        assert!(rolled_back.last_output.is_none());
        let conn = open_xin_test_connection(&config);
        let runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM xin_runs WHERE task_id = ?1",
                params![task.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(runs, 0);
        let event_types = list_task_events(&config, &task.id)
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert!(!event_types.iter().any(|event| {
            matches!(
                event.as_str(),
                "xin.task.completed" | "xin.task.run_recorded" | "xin.task.rescheduled"
            )
        }));
    }

    #[test]
    fn event_outbox_recovers_cross_database_delivery_idempotently() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let memory_path = config.workspace_dir.join("memory");
        std::fs::write(&memory_path, "block memory directory creation").unwrap();

        let task = add_task(&config, &sample_task()).unwrap();
        let conn = open_xin_test_connection(&config);
        let (event_id, attempts, delivered_at): (String, i64, Option<String>) = conn
            .query_row(
                "SELECT event_id, attempt_count, delivered_at
                 FROM xin_event_outbox WHERE task_id = ?1",
                params![task.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(attempts >= 1);
        assert!(delivered_at.is_none());
        drop(conn);

        std::fs::remove_file(&memory_path).unwrap();
        std::fs::create_dir_all(&memory_path).unwrap();
        list_task_events(&config, &task.id).unwrap();

        let conn = open_xin_test_connection(&config);
        let delivered_at: Option<String> = conn
            .query_row(
                "SELECT delivered_at FROM xin_event_outbox WHERE event_id = ?1",
                params![event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(delivered_at.is_some());
        drop(conn);

        let brain = Connection::open(memory_path.join("brain.db")).unwrap();
        let mirrored: i64 = brain
            .query_row(
                "SELECT COUNT(*) FROM memory_events WHERE event_id = ?1",
                params![event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mirrored, 1);
        drop(brain);

        list_task_events(&config, &task.id).unwrap();
        let brain = Connection::open(memory_path.join("brain.db")).unwrap();
        let mirrored_after_replay: i64 = brain
            .query_row(
                "SELECT COUNT(*) FROM memory_events WHERE event_id = ?1",
                params![event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mirrored_after_replay, 1);
    }

    #[test]
    fn local_event_and_outbox_enqueue_are_an_atomic_pair() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();
        let conn = open_xin_test_connection(&config);
        conn.execute_batch(
            "CREATE TRIGGER fail_xin_outbox
             BEFORE INSERT ON xin_event_outbox
             BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;",
        )
        .unwrap();
        drop(conn);

        let append = with_connection(&config, |conn| {
            let lineage = load_task_lineage(conn, &task.id)?.expect("task lineage");
            insert_task_event(
                conn,
                &workspace_id(&config),
                &task.id,
                lineage,
                "xin.task.injected",
                Some("test"),
                None,
            )
        });
        assert!(append.is_err());

        let conn = open_xin_test_connection(&config);
        let local_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM xin_task_events
                 WHERE task_id = ?1 AND event_type = 'xin.task.injected'",
                params![task.id],
                |row| row.get(0),
            )
            .unwrap();
        let outbox_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM xin_event_outbox
                 WHERE task_id = ?1 AND event_type = 'xin.task.injected'",
                params![task.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(local_events, 0);
        assert_eq!(outbox_events, 0);
    }

    #[test]
    fn add_task_persists_approval_grant_json() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut new = sample_task();
        new.execution_mode = ExecutionMode::Shell;
        new.payload = "touch xin-approved".into();
        new.approval_grant_json = Some(r#"{"tool":"xin_runner"}"#.into());

        let task = add_task(&config, &new).unwrap();
        let fetched = get_task(&config, &task.id).unwrap();

        assert_eq!(fetched.approval_grant_json.as_deref(), Some(r#"{"tool":"xin_runner"}"#));
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

    // ── Goal / Step (FIX-P2-16) ───────────────────────────────────────────

    fn sample_step(seq: u32) -> NewXinStep {
        NewXinStep {
            sequence: seq,
            name: format!("step-{seq}"),
            description: Some(format!("step {seq} desc")),
            execution_mode: ExecutionMode::Internal,
            payload: "noop".into(),
            max_retries: 2,
            approval_grant_json: None,
            lease_ttl_secs: 0,
        }
    }

    fn sample_goal(steps: Vec<NewXinStep>) -> NewXinGoal {
        NewXinGoal {
            owner_id: Some("owner:ws:tg:alice".into()),
            topic_id: Some("topic-7".into()),
            parent_task_id: None,
            source_message_event_id: Some("msg-9".into()),
            name: "ship_feature".into(),
            description: Some("multi-step goal".into()),
            kind: TaskKind::User,
            priority: TaskPriority::High,
            target_completion_at: Some(Utc::now() + chrono::Duration::hours(2)),
            initial_steps: steps,
        }
    }

    #[test]
    fn add_goal_with_steps_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1), sample_step(2)])).unwrap();

        assert_eq!(goal.name, "ship_feature");
        assert_eq!(goal.status, GoalStatus::Pending);
        assert_eq!(goal.steps_total, 2);
        assert_eq!(goal.steps_completed, 0);
        assert!(goal.target_completion_at.is_some());
        assert_eq!(goal.owner_id.as_deref(), Some("owner:ws:tg:alice"));

        let fetched = get_goal(&config, &goal.id).unwrap();
        assert_eq!(fetched.id, goal.id);

        let steps = list_steps(&config, &goal.id).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].sequence, 1);
        assert_eq!(steps[1].sequence, 2);
        assert_eq!(steps[0].status, StepStatus::Pending);
        assert_eq!(steps[0].lease_epoch, 0);

        let one = get_step(&config, &steps[0].id).unwrap();
        assert_eq!(one.goal_id, goal.id);

        let mut legacy_json = serde_json::to_value(&one).unwrap();
        legacy_json.as_object_mut().unwrap().remove("lease_epoch");
        let legacy: XinStep = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy.lease_epoch, 0, "pre-epoch serialized steps must remain readable");
    }

    #[test]
    fn add_step_bumps_total() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let added = add_step(&config, &goal.id, &sample_step(2)).unwrap();
        assert_eq!(added.sequence, 2);
        assert_eq!(get_goal(&config, &goal.id).unwrap().steps_total, 2);
    }

    #[test]
    fn claim_step_is_idempotent_under_contention() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);

        // First worker wins.
        assert!(claim_step(&config, &step.id, "prx:1:aaaa", 60).unwrap());
        // Second worker loses while lease is fresh.
        assert!(!claim_step(&config, &step.id, "prx:2:bbbb", 60).unwrap());

        let claimed = get_step(&config, &step.id).unwrap();
        assert_eq!(claimed.status, StepStatus::Claimed);
        assert_eq!(claimed.lease_owner.as_deref(), Some("prx:1:aaaa"));
        assert!(claimed.lease_expires_at.is_some());
    }

    #[test]
    fn claim_waiting_on_write_lock_starts_ttl_after_authoritative_lock_time() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);

        let lock = open_xin_test_connection(&config);
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let claim_config = config.clone();
        let step_id = step.id.clone();
        let claim = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            claim_step(&claim_config, &step_id, "prx:1:locked-claim", 2).unwrap()
        });
        started_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2_200));
        let released_at = Utc::now();
        lock.execute_batch("COMMIT").unwrap();

        assert!(claim.join().unwrap());
        let expiry = get_step(&config, &step.id).unwrap().lease_expires_at.unwrap();
        assert!(
            expiry >= released_at + chrono::Duration::milliseconds(1_500),
            "claim TTL must start after acquiring the write lock: expiry={expiry}, released_at={released_at}"
        );
    }

    #[test]
    fn expired_claim_cannot_transition_to_running() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);
        let worker = "prx:1:expired";
        assert!(claim_step(&config, &step.id, worker, 60).unwrap());

        let conn = open_xin_test_connection(&config);
        conn.execute(
            "UPDATE xin_steps SET lease_expires_at = ?1 WHERE id = ?2",
            params![(Utc::now() - chrono::Duration::seconds(1)).to_rfc3339(), &step.id],
        )
        .unwrap();

        assert!(!mark_step_running(&config, &step.id, worker).unwrap());
        let current = get_step(&config, &step.id).unwrap();
        assert_eq!(current.status, StepStatus::Claimed);
        assert!(current.started_at.is_none());
    }

    #[test]
    fn renew_lease_extends_then_rejects_other_owner() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);

        let lease = claim_step_with_lease(&config, &step.id, "prx:1:aaaa", 60)
            .unwrap()
            .expect("claim");

        // Owner renews successfully and pushes the expiry forward without
        // changing the claim generation.
        let renewed = renew_step_lease_generation(&config, &step.id, &lease, 120)
            .unwrap()
            .expect("renew");
        assert!(renewed.expires_at >= lease.expires_at);
        assert_eq!(renewed.epoch, lease.epoch);

        // A different owner cannot renew.
        assert!(!renew_step_lease(&config, &step.id, "prx:2:bbbb", 120).unwrap());
    }

    #[test]
    fn old_epoch_fence_rejects_same_worker_reclaim_generation() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);
        let worker = "prx:1:same";
        let old_lease = claim_step_with_lease(&config, &step.id, worker, 60)
            .unwrap()
            .expect("old claim");
        assert_eq!(old_lease.epoch, 1);
        assert!(mark_step_running_with_lease(&config, &step.id, &old_lease).unwrap());

        let future = old_lease.expires_at + chrono::Duration::seconds(1);
        assert_eq!(mark_steps_stale(&config, future).unwrap(), vec![step.id.clone()]);
        let new_lease = claim_step_with_lease(&config, &step.id, worker, 120)
            .unwrap()
            .expect("new claim");
        assert!(mark_step_running_with_lease(&config, &step.id, &new_lease).unwrap());
        assert_eq!(new_lease.epoch, old_lease.epoch + 1);
        assert!(
            save_step_checkpoint_with_lease(&config, &step.id, &new_lease, r#"{"owner":"new-generation"}"#,).unwrap()
        );

        assert!(
            !save_step_checkpoint_with_lease(&config, &step.id, &old_lease, r#"{"owner":"old-generation"}"#,).unwrap()
        );
        assert!(!complete_step_with_lease(&config, &step.id, &old_lease, "old-output").unwrap());
        assert!(!fail_step_with_lease(&config, &step.id, &old_lease, "old-failure").unwrap());
        let current = get_step(&config, &step.id).unwrap();
        assert_eq!(current.status, StepStatus::Running);
        assert_eq!(current.lease_owner.as_deref(), Some(worker));
        assert_eq!(current.lease_epoch, new_lease.epoch);
        assert_eq!(current.lease_expires_at, Some(new_lease.expires_at));
        assert_eq!(
            current.checkpoint_json.as_deref(),
            Some(r#"{"owner":"new-generation"}"#)
        );
        let events = list_task_events(&config, &goal.id).unwrap();
        assert!(
            events.iter().all(|event| !matches!(
                event.event_type.as_str(),
                "xin.step.completed" | "xin.step.failed" | "xin.step.retry"
            )),
            "stale epoch must not append a terminal/retry event: {events:?}"
        );
    }

    #[test]
    fn paused_same_worker_old_token_cannot_mark_or_renew_after_reclaim() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);
        let worker = "prx:1:same-process";
        let old_lease = claim_step_with_lease(&config, &step.id, worker, 60)
            .unwrap()
            .expect("old generation claim");

        let after_expiry = old_lease.expires_at + chrono::Duration::milliseconds(1);
        assert_eq!(mark_steps_stale(&config, after_expiry).unwrap(), vec![step.id.clone()]);
        let new_lease = claim_step_with_lease(&config, &step.id, worker, 120)
            .unwrap()
            .expect("same worker new generation claim");
        assert_ne!(old_lease.epoch, new_lease.epoch);

        let marker = config.workspace_dir.join("old-generation-marker");
        if mark_step_running_with_lease(&config, &step.id, &old_lease).unwrap() {
            std::fs::write(&marker, "stale generation executed").unwrap();
        }
        assert!(
            renew_step_lease_generation(&config, &step.id, &old_lease, 60)
                .unwrap()
                .is_none(),
            "old generation must not renew a same-worker reclaim"
        );
        assert!(!marker.exists(), "old generation must not execute its marker");
        assert!(mark_step_running_with_lease(&config, &step.id, &new_lease).unwrap());
    }

    #[test]
    fn renewal_waiting_on_write_lock_rechecks_authoritative_expiry() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);
        let worker = "prx:1:locked";
        assert!(claim_step(&config, &step.id, worker, 1).unwrap());
        let expiry = get_step(&config, &step.id).unwrap().lease_expires_at.unwrap();

        let lock = open_xin_test_connection(&config);
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let renewal_config = config;
        let step_id = step.id;
        let renewal = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            renew_step_lease_with_expiry(&renewal_config, &step_id, worker, 60).unwrap()
        });
        started_rx.recv().unwrap();
        while Utc::now() <= expiry {
            std::thread::yield_now();
        }
        lock.execute_batch("COMMIT").unwrap();

        assert!(
            renewal.join().unwrap().is_none(),
            "expired lease must not renew after lock wait"
        );
    }

    #[test]
    fn save_checkpoint_does_not_change_status() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);
        let lease = claim_step_with_lease(&config, &step.id, "prx:1:aaaa", 60)
            .unwrap()
            .expect("claim");

        assert!(save_step_checkpoint_with_lease(&config, &step.id, &lease, r#"{"cursor":42}"#).unwrap());
        let after = get_step(&config, &step.id).unwrap();
        assert_eq!(after.status, StepStatus::Claimed);
        assert_eq!(after.checkpoint_json.as_deref(), Some(r#"{"cursor":42}"#));
    }

    #[test]
    fn terminal_event_carries_the_authorized_owner_and_epoch() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);
        let lease = claim_step_with_lease(&config, &step.id, "prx:1:terminal", 60)
            .unwrap()
            .expect("claim");
        assert!(mark_step_running_with_lease(&config, &step.id, &lease).unwrap());
        assert!(complete_step_with_lease(&config, &step.id, &lease, "done").unwrap());

        let terminal = list_task_events(&config, &goal.id)
            .unwrap()
            .into_iter()
            .find(|event| event.event_type == "xin.step.completed")
            .expect("terminal step event");
        let payload: serde_json::Value = serde_json::from_str(terminal.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload.get("step_id").and_then(serde_json::Value::as_str),
            Some(step.id.as_str())
        );
        assert_eq!(
            payload.get("lease_owner").and_then(serde_json::Value::as_str),
            Some("prx:1:terminal")
        );
        assert_eq!(
            payload.get("lease_epoch").and_then(serde_json::Value::as_u64),
            Some(lease.epoch)
        );
    }

    #[test]
    fn step_completion_goal_progress_and_outbox_roll_back_together() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);
        let lease = claim_step_with_lease(&config, &step.id, "prx:1:rollback", 60)
            .unwrap()
            .expect("claim");
        assert!(mark_step_running_with_lease(&config, &step.id, &lease).unwrap());
        let goal_before_completion = get_goal(&config, &goal.id).unwrap();

        let conn = open_xin_test_connection(&config);
        conn.execute_batch(
            "CREATE TRIGGER fail_xin_outbox
             BEFORE INSERT ON xin_event_outbox
             BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;",
        )
        .unwrap();
        drop(conn);

        assert!(complete_step_with_lease(&config, &step.id, &lease, "must roll back").is_err());
        let rolled_back_step = get_step(&config, &step.id).unwrap();
        let rolled_back_goal = get_goal(&config, &goal.id).unwrap();
        assert_eq!(rolled_back_step.status, StepStatus::Running);
        assert_eq!(rolled_back_step.lease_owner.as_deref(), Some("prx:1:rollback"));
        assert_eq!(rolled_back_goal.status, goal_before_completion.status);
        assert_eq!(rolled_back_goal.steps_completed, goal_before_completion.steps_completed);
        assert!(
            list_task_events(&config, &goal.id)
                .unwrap()
                .iter()
                .all(|event| event.event_type != "xin.step.completed")
        );
    }

    #[test]
    fn long_running_step_not_reaped_when_lease_renewed() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);

        // Claim with a short lease, then renew to keep it alive.
        assert!(claim_step(&config, &step.id, "prx:1:aaaa", 1).unwrap());
        assert!(mark_step_running(&config, &step.id, "prx:1:aaaa").unwrap());
        assert!(renew_step_lease(&config, &step.id, "prx:1:aaaa", 3600).unwrap());

        // A stale sweep now must NOT reap it (lease far in the future).
        let reaped = mark_steps_stale(&config, Utc::now()).unwrap();
        assert!(reaped.is_empty());
        assert_eq!(get_step(&config, &step.id).unwrap().status, StepStatus::Running);
    }

    #[test]
    fn expired_lease_step_marked_stale_and_reclaimable() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);

        assert!(claim_step(&config, &step.id, "prx:1:aaaa", 60).unwrap());
        assert!(mark_step_running(&config, &step.id, "prx:1:aaaa").unwrap());

        // Sweep with a future "now" so the lease counts as expired.
        let future = Utc::now() + chrono::Duration::seconds(120);
        let expired = expired_step_leases(&config, future).unwrap();
        assert_eq!(expired.len(), 1);

        let reaped = mark_steps_stale(&config, future).unwrap();
        assert_eq!(reaped.len(), 1);
        let stale = get_step(&config, &step.id).unwrap();
        assert_eq!(stale.status, StepStatus::Stale);
        assert!(stale.lease_owner.is_none());

        // A new worker re-claims the SAME step (not a fresh one).
        assert!(claim_step(&config, &step.id, "prx:9:cccc", 60).unwrap());
        assert_eq!(
            get_step(&config, &step.id).unwrap().lease_owner.as_deref(),
            Some("prx:9:cccc")
        );
    }

    #[test]
    fn complete_all_steps_rolls_goal_to_completed() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1), sample_step(2)])).unwrap();
        let steps = list_steps(&config, &goal.id).unwrap();

        complete_step(&config, &steps[0].id, "out-1").unwrap();
        let mid = get_goal(&config, &goal.id).unwrap();
        assert_eq!(mid.status, GoalStatus::Running);
        assert_eq!(mid.steps_completed, 1);

        complete_step(&config, &steps[1].id, "out-2").unwrap();
        let done = get_goal(&config, &goal.id).unwrap();
        assert_eq!(done.status, GoalStatus::Completed);
        assert_eq!(done.steps_completed, 2);
        assert!(done.completed_at.is_some());
        assert_eq!(done.final_output.as_deref(), Some("out-2"));
    }

    #[test]
    fn fail_step_retries_then_fails_goal() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        // max_retries = 2 → first two failures retry, third is terminal.
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
        let step = list_steps(&config, &goal.id).unwrap().remove(0);

        fail_step(&config, &step.id, "boom-1").unwrap();
        assert_eq!(get_step(&config, &step.id).unwrap().status, StepStatus::Pending);
        fail_step(&config, &step.id, "boom-2").unwrap();
        assert_eq!(get_step(&config, &step.id).unwrap().status, StepStatus::Pending);
        fail_step(&config, &step.id, "boom-3").unwrap();
        assert_eq!(get_step(&config, &step.id).unwrap().status, StepStatus::Failed);
        assert_eq!(get_goal(&config, &goal.id).unwrap().status, GoalStatus::Failed);
    }

    #[test]
    fn next_runnable_step_returns_lowest_sequence() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1), sample_step(2)])).unwrap();
        let first = next_runnable_step(&config, &goal.id).unwrap().unwrap();
        assert_eq!(first.sequence, 1);
        complete_step(&config, &first.id, "ok").unwrap();
        let second = next_runnable_step(&config, &goal.id).unwrap().unwrap();
        assert_eq!(second.sequence, 2);
    }

    #[test]
    fn ordered_prerequisite_blocks_selection_and_direct_claim() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let goal = add_goal(&config, &sample_goal(vec![sample_step(1), sample_step(2)])).unwrap();
        let steps = list_steps(&config, &goal.id).unwrap();

        assert!(!claim_step(&config, &steps[1].id, "prx:1:early", 60).unwrap());
        assert_eq!(next_runnable_step(&config, &goal.id).unwrap().unwrap().id, steps[0].id);

        complete_step(&config, &steps[0].id, "first done").unwrap();
        assert!(claim_step(&config, &steps[1].id, "prx:1:ordered", 60).unwrap());
    }

    #[test]
    fn failed_prior_step_blocks_all_later_steps() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut first = sample_step(1);
        first.max_retries = 0;
        let goal = add_goal(&config, &sample_goal(vec![first, sample_step(2)])).unwrap();
        let steps = list_steps(&config, &goal.id).unwrap();

        fail_step(&config, &steps[0].id, "terminal failure").unwrap();
        assert_eq!(get_step(&config, &steps[0].id).unwrap().status, StepStatus::Failed);
        assert!(next_runnable_step(&config, &goal.id).unwrap().is_none());
        assert!(!claim_step(&config, &steps[1].id, "prx:1:bypass", 60).unwrap());
    }

    #[test]
    fn legacy_task_adoption_is_atomic_linked_disabled_and_idempotent() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut new = sample_task();
        new.owner_id = Some("owner:workspace:telegram:alice".to_string());
        new.topic_id = Some("topic-adoption".to_string());
        new.parent_task_id = Some("parent-task".to_string());
        new.source_message_event_id = Some("source-message".to_string());
        let task = add_task(&config, &new).unwrap();
        assert!(claim_task(&config, &task.id).unwrap());
        let conn = open_xin_test_connection(&config);
        conn.execute(
            "UPDATE xin_tasks SET updated_at = ?1 WHERE id = ?2",
            params![(Utc::now() - chrono::Duration::hours(2)).to_rfc3339(), task.id],
        )
        .unwrap();
        drop(conn);
        assert_eq!(mark_stale(&config, 60).unwrap(), 1);

        let first = adopt_legacy_task(&config, &task.id).unwrap().expect("adopted");
        assert!(first.newly_adopted);
        let replay = adopt_legacy_task(&config, &task.id)
            .unwrap()
            .expect("existing adoption");
        assert!(!replay.newly_adopted);
        assert_eq!(replay.goal.id, first.goal.id);
        assert_eq!(list_goals(&config).unwrap().len(), 1);

        let disabled = get_task(&config, &task.id).unwrap();
        assert!(!disabled.enabled);
        let steps = list_steps(&config, &first.goal.id).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].payload, task.payload);
        assert_eq!(steps[1].payload, "xin:health_check");
        assert_eq!(first.goal.owner_id, task.owner_id);
        assert_eq!(first.goal.topic_id, task.topic_id);
        assert_eq!(first.goal.parent_task_id, task.parent_task_id);
        assert_eq!(first.goal.source_message_event_id, task.source_message_event_id);

        let conn = open_xin_test_connection(&config);
        let linked_goal: String = conn
            .query_row(
                "SELECT goal_id FROM xin_task_adoptions WHERE legacy_task_id = ?1",
                params![task.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(linked_goal, first.goal.id);
        let adoption_events = list_task_events(&config, &task.id)
            .unwrap()
            .into_iter()
            .filter(|event| event.event_type == "xin.task.adopted")
            .count();
        assert_eq!(adoption_events, 1);
    }

    #[test]
    fn legacy_task_adoption_rolls_back_when_link_write_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();
        let conn = open_xin_test_connection(&config);
        conn.execute("UPDATE xin_tasks SET status = 'stale' WHERE id = ?1", params![task.id])
            .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_xin_adoption_link
             BEFORE INSERT ON xin_task_adoptions
             BEGIN SELECT RAISE(ABORT, 'injected adoption link failure'); END;",
        )
        .unwrap();
        drop(conn);

        assert!(adopt_legacy_task(&config, &task.id).is_err());
        assert!(get_task(&config, &task.id).unwrap().enabled);
        assert!(list_goals(&config).unwrap().is_empty());
        assert!(
            list_task_events(&config, &task.id)
                .unwrap()
                .iter()
                .all(|event| event.event_type != "xin.task.adopted")
        );
    }

    #[test]
    fn migrate_non_recurring_task_to_single_step_goal() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &sample_task()).unwrap();
        let goal = migrate_task_to_goal(&config, &task.id).unwrap();

        assert_eq!(goal.name, task.name);
        assert_eq!(goal.steps_total, 1);
        let steps = list_steps(&config, &goal.id).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].payload, task.payload);
        // Original task left intact (zero-breakage).
        assert!(get_task(&config, &task.id).is_ok());
    }

    #[test]
    fn migrate_recurring_task_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let task = add_task(&config, &recurring_task()).unwrap();
        assert!(migrate_task_to_goal(&config, &task.id).is_err());
    }

    #[test]
    fn per_step_lease_ttl_override_is_persisted_and_honored() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut step = sample_step(1);
        step.lease_ttl_secs = 7200; // explicit 2h override
        let goal = add_goal(&config, &sample_goal(vec![step])).unwrap();
        let persisted = list_steps(&config, &goal.id).unwrap().remove(0);
        assert_eq!(persisted.lease_ttl_secs, 7200);

        // Claim with ttl_secs=0 → must fall back to the persisted override, not
        // the per-mode default (Internal=60s).
        let before = Utc::now();
        assert!(claim_step(&config, &persisted.id, "prx:1:aaaa", 0).unwrap());
        let claimed = get_step(&config, &persisted.id).unwrap();
        let expires = claimed.lease_expires_at.unwrap();
        // Expiry should be well beyond the 60s Internal default.
        assert!(expires > before + chrono::Duration::seconds(3600));
    }

    #[test]
    fn list_goals_orders_by_priority() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let mut low = sample_goal(vec![sample_step(1)]);
        low.name = "low".into();
        low.priority = TaskPriority::Low;
        add_goal(&config, &low).unwrap();
        add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap(); // High

        let goals = list_goals(&config).unwrap();
        assert_eq!(goals.len(), 2);
        assert_eq!(goals[0].priority, TaskPriority::High);
    }
}
