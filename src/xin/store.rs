//! SQLite persistence for the xin autonomous task engine.
//!
//! DB path: `{workspace}/xin/tasks.db`
//! Pattern follows `cron/store.rs`: `with_connection()` + `rusqlite::params!`.

use crate::config::Config;
use crate::cron::{JobCreator, ScheduledContext, ScheduledDeliver};
use crate::security::agent_level::CreatorStamp;
use crate::tools::execution::JobOrigin;
use crate::xin::types::{NewXinTask, XinRun, XinTask, XinTaskEvent, XinTaskPatch};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use connection::{authoritative_now, with_connection, with_immediate_connection};
use rows::{
    SELECT_ALL_COLUMNS, TaskLineage, insert_task_event, load_task_lineage, map_task_event_row, map_task_row,
    parse_rfc3339, sql_err, stored_task_output, workspace_id,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use uuid::Uuid;

mod connection;
mod goals;
mod rows;

pub(crate) use goals::{
    XinStepLease, adopt_legacy_task, claim_step_with_lease, complete_step_with_lease, fail_step_with_lease,
    mark_step_running_with_lease, record_retired_goal, renew_step_lease_generation, save_step_checkpoint_with_lease,
};
#[cfg(test)]
pub use goals::{
    add_goal, add_goal_with_origin, claim_step, complete_step, fail_step, mark_step_running, migrate_task_to_goal,
    renew_step_lease, renew_step_lease_with_expiry,
};
pub use goals::{
    add_goal_with_creator_stamp, add_step, add_step_with_creator_stamp, cancel_goal, expired_step_leases, get_goal,
    get_step, list_goals, list_steps, mark_steps_stale, next_runnable_step, pause_goal, remove_goal, resume_goal,
    retry_step, task_revision,
};

const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const TRUNCATED_MARKER: &str = "\n...[truncated]";

#[derive(Debug, thiserror::Error)]
pub(crate) enum JobLookupError {
    #[error("Xin task '{0}' not found")]
    Task(String),
    #[error("Xin goal '{0}' not found")]
    Goal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XinTaskLease {
    pub worker_id: String,
    pub epoch: u64,
    pub expires_at: DateTime<Utc>,
}

// ── CRUD ────────────────────────────────────────────────────────────────

/// Insert a new task and return the persisted record.
///
/// The task records no creator and so runs as [`JobOrigin::Unattributed`];
/// entry points that know who created it use [`add_task_with_origin`].
#[cfg(test)]
pub fn add_task(config: &Config, new: &NewXinTask) -> Result<XinTask> {
    add_task_with_origin(config, new, &JobOrigin::Unattributed)
}

/// Insert a new task created by `origin` (CRON-ORIGIN) and return it. The
/// origin is fixed here: no later update changes whose authority its agent
/// runs execute under.
#[cfg(test)]
pub fn add_task_with_origin(config: &Config, new: &NewXinTask, origin: &JobOrigin) -> Result<XinTask> {
    add_task_with_origin_and_creator(config, new, origin, JobCreator::AgentTool)
}

/// [`add_task_with_origin`] recording which surface created the task (D-107
/// amendment): only `prx xin` passes [`JobCreator::OperatorCli`].
pub fn add_task_with_origin_and_creator(
    config: &Config,
    new: &NewXinTask,
    origin: &JobOrigin,
    created_via: JobCreator,
) -> Result<XinTask> {
    add_task_with_creator_stamp(config, new, origin, created_via, &CreatorStamp::default())
}

/// [`add_task_with_origin_and_creator`] recording the creating call's level
/// and agent (`creator_level`, `creator_agent`).
pub fn add_task_with_creator_stamp(
    config: &Config,
    new: &NewXinTask,
    origin: &JobOrigin,
    created_via: JobCreator,
    creator: &CreatorStamp,
) -> Result<XinTask> {
    add_task_with_stored_options(
        config,
        new,
        origin,
        created_via,
        creator,
        ScheduledContext::default(),
        ScheduledDeliver::default(),
    )
}

/// [`add_task_with_creator_stamp`] storing the task's `context` and `deliver` in the
/// same insert, so no task is written without the options it was created with.
pub fn add_task_with_stored_options(
    config: &Config,
    new: &NewXinTask,
    origin: &JobOrigin,
    created_via: JobCreator,
    creator: &CreatorStamp,
    context: ScheduledContext,
    deliver: ScheduledDeliver,
) -> Result<XinTask> {
    let origin = origin.to_stored()?;
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
                run_count, fail_count, max_failures, enabled, approval_grant_json, origin, created_via,
                creator_level, creator_agent, context, deliver
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, 'pending', ?9, ?10,
                ?11, ?12, ?13, ?14, ?15,
                NULL, ?16, NULL, NULL,
                0, 0, ?17, 1, ?18, ?19, ?20, ?21, ?22, ?23, ?24
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
                origin,
                created_via.as_str(),
                creator.level_column(),
                creator.agent.as_deref(),
                context.as_str(),
                deliver.as_str(),
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
        rows.next()?.map_or_else(
            || Err(JobLookupError::Task(task_id.to_owned()).into()),
            |row| map_task_row(row).map_err(Into::into),
        )
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
                    run_count, fail_count, max_failures, enabled, approval_grant_json, origin, context, deliver
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

/// Return every enabled, pending task whose `next_run_at <= now`, sorted by
/// priority descending. The runtime deliberately has no batch-size ceiling:
/// claiming and leases provide ownership without silently deferring due work.
pub fn due_tasks(config: &Config, now: DateTime<Utc>) -> Result<Vec<XinTask>> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
                    name, description, kind, status, priority, execution_mode,
                    payload, recurring, interval_secs, created_at, updated_at,
                    last_run_at, next_run_at, last_status, last_output,
                    run_count, fail_count, max_failures, enabled, approval_grant_json, origin, context, deliver
             FROM xin_tasks
             WHERE enabled = 1
               AND status IN ('pending', 'stale')
               AND next_run_at <= ?1
             ORDER BY priority DESC, next_run_at ASC",
        )?;
        let rows = stmt.query_map(params![now.to_rfc3339()], map_task_row)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    })
}

/// The stored creator of a task, or of the goal a step belongs to: which
/// surface created it (NULL is `agent_tool`) and the creating call's level
/// and agent (an unreadable stamp reads as empty).
pub fn task_creator(config: &Config, task_or_step_id: &str) -> Result<(JobCreator, CreatorStamp)> {
    // No task store means no stored creator: the run is capped, and looking
    // it up creates nothing.
    if !config.workspace_dir.join("xin").join("tasks.db").exists() {
        return Ok((JobCreator::AgentTool, CreatorStamp::default()));
    }
    with_connection(config, |conn| stored_creator(conn, task_or_step_id))
}

fn stored_creator(conn: &Connection, task_or_step_id: &str) -> Result<(JobCreator, CreatorStamp)> {
    let (created_via, level, agent) = conn
        .query_row(
            "SELECT created_via, creator_level, creator_agent FROM xin_tasks WHERE id = ?1
             UNION ALL
             SELECT g.created_via, g.creator_level, g.creator_agent
               FROM xin_steps s JOIN xin_goals g ON g.id = s.goal_id
              WHERE s.id = ?1
             UNION ALL SELECT created_via,creator_level,creator_agent FROM xin_goals WHERE id=?1
             LIMIT 1",
            params![task_or_step_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .with_context(|| format!("xin task or step '{task_or_step_id}' not found"))?;
    Ok((
        JobCreator::from_stored(created_via.as_deref()),
        CreatorStamp::from_stored(level.as_deref(), agent),
    ))
}

/// Record that the operator vouches for a task as their own (`prx xin
/// mark-operator`): its runs then use the operator level.
pub fn mark_task_operator(config: &Config, task_id: &str) -> Result<()> {
    let changed = with_connection(config, |conn| {
        conn.execute(
            "UPDATE xin_tasks SET version=version+1, created_via = 'operator_cli' WHERE id = ?1",
            params![task_id],
        )
        .context("Failed to mark xin task as the operator's")
    })?;
    if changed == 0 {
        anyhow::bail!("xin task '{task_id}' not found");
    }
    Ok(())
}

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
    if let Some(context) = patch.context {
        task.context = context;
    }
    if let Some(deliver) = patch.deliver {
        task.deliver = deliver;
    }
    // A payload rewrite re-records the rewriter (V7).
    let rewritten_creator = patch
        .payload
        .as_ref()
        .map(|_| patch.creator.clone().unwrap_or_default());
    // Optimistic concurrency: capture the DB-read updated_at BEFORE overwriting.
    let previous_updated_at = task.updated_at;
    task.updated_at = now;
    with_connection(config, |conn| {
        let changed = conn
            .execute(
                "UPDATE xin_tasks
             SET version = version + 1, name = ?1, description = ?2, priority = ?3, payload = ?4,
                 interval_secs = ?5, enabled = ?6, max_failures = ?7, updated_at = ?8,
                 approval_grant_json = ?9,
                 created_via = COALESCE(?12, created_via),
                 creator_level = CASE WHEN ?13 AND COALESCE(substr(creator_agent,1,2),'')<>'n:' THEN ?14 ELSE creator_level END,
                 creator_agent = CASE WHEN ?13 AND COALESCE(substr(creator_agent,1,2),'')<>'n:' THEN ?15 ELSE creator_agent END,
                 context = ?16, deliver = ?17
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
                    patch
                        .payload
                        .as_ref()
                        .map(|_| patch.created_via.unwrap_or_default().as_str()),
                    rewritten_creator.is_some(),
                    rewritten_creator.as_ref().and_then(CreatorStamp::level_column),
                    rewritten_creator.as_ref().and_then(|creator| creator.agent.as_deref()),
                    task.context.as_str(),
                    task.deliver.as_str(),
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

/// Atomically claim a legacy task with a monotonically increasing lease epoch.
///
/// The returned token is required by renew and terminal commit operations, so
/// an expired prior owner cannot commit after another worker reclaims the task.
pub fn claim_task_with_lease(
    config: &Config,
    task_id: &str,
    worker_id: &str,
    lease_ttl_secs: u64,
) -> Result<Option<XinTaskLease>> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let expires_at = now + chrono::Duration::seconds(i64::try_from(lease_ttl_secs.max(1)).unwrap_or(i64::MAX));
        let changed = conn
            .execute(
                "UPDATE xin_tasks
                 SET status = 'running',
                     lease_owner = ?1,
                     lease_epoch = lease_epoch + 1,
                     lease_expires_at = ?2,
                     last_heartbeat_at = ?3,
                     updated_at = ?3
                 WHERE id = ?4 AND status IN ('pending', 'stale') AND enabled = 1",
                params![worker_id, expires_at.to_rfc3339(), now.to_rfc3339(), task_id],
            )
            .context("Failed to claim xin task")?;
        if changed == 0 {
            return Ok(None);
        }
        let epoch_i64: i64 = conn.query_row(
            "SELECT lease_epoch FROM xin_tasks WHERE id = ?1",
            params![task_id],
            |row| row.get(0),
        )?;
        let epoch = u64::try_from(epoch_i64).context("xin task lease epoch is negative")?;
        if let Some(lineage) = load_task_lineage(conn, task_id)? {
            insert_task_event(
                conn,
                &workspace_id(config),
                task_id,
                lineage,
                "xin.task.claimed",
                Some("running"),
                Some(
                    serde_json::json!({
                        "lease_owner": worker_id,
                        "lease_epoch": epoch,
                        "lease_expires_at": expires_at.to_rfc3339(),
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
        }
        Ok(Some(XinTaskLease {
            worker_id: worker_id.to_string(),
            epoch,
            expires_at,
        }))
    })
}

/// Claim a task for an explicit operator-requested run. Paused tasks may be
/// run once without changing their paused state; cancelled and already-running
/// tasks remain ineligible.
pub fn claim_task_for_manual_run(
    config: &Config,
    task_id: &str,
    worker_id: &str,
    lease_ttl_secs: u64,
) -> Result<Option<XinTaskLease>> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let expires_at = now + chrono::Duration::seconds(i64::try_from(lease_ttl_secs.max(1)).unwrap_or(i64::MAX));
        let changed = conn.execute(
            "UPDATE xin_tasks
             SET status = 'running', lease_owner = ?1, lease_epoch = lease_epoch + 1,
                 lease_expires_at = ?2, last_heartbeat_at = ?3, updated_at = ?3
             WHERE id = ?4 AND status != 'running' AND status != 'cancelled'",
            params![worker_id, expires_at.to_rfc3339(), now.to_rfc3339(), task_id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let epoch_i64: i64 = conn.query_row(
            "SELECT lease_epoch FROM xin_tasks WHERE id = ?1",
            params![task_id],
            |row| row.get(0),
        )?;
        let epoch = u64::try_from(epoch_i64).context("xin task lease epoch is negative")?;
        if let Some(lineage) = load_task_lineage(conn, task_id)? {
            insert_task_event(
                conn,
                &workspace_id(config),
                task_id,
                lineage,
                "xin.task.manual_run_claimed",
                Some("running"),
                Some(
                    serde_json::json!({
                        "lease_owner": worker_id,
                        "lease_epoch": epoch,
                        "lease_expires_at": expires_at.to_rfc3339(),
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
        }
        Ok(Some(XinTaskLease {
            worker_id: worker_id.to_string(),
            epoch,
            expires_at,
        }))
    })
}

/// Compatibility claim for callers that do not commit a production execution.
#[cfg(test)]
pub fn claim_task(config: &Config, task_id: &str) -> Result<bool> {
    Ok(claim_task_with_lease(config, task_id, "legacy-compat", 3600)?.is_some())
}

pub fn renew_task_lease_generation(
    config: &Config,
    task_id: &str,
    lease: &XinTaskLease,
    lease_ttl_secs: u64,
) -> Result<Option<XinTaskLease>> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let expires_at = now + chrono::Duration::seconds(i64::try_from(lease_ttl_secs.max(1)).unwrap_or(i64::MAX));
        let changed = conn.execute(
            "UPDATE xin_tasks
             SET lease_expires_at = ?1, last_heartbeat_at = ?2, updated_at = ?2
             WHERE id = ?3 AND status = 'running'
               AND lease_owner = ?4 AND lease_epoch = ?5
               AND lease_expires_at > ?2",
            params![
                expires_at.to_rfc3339(),
                now.to_rfc3339(),
                task_id,
                lease.worker_id,
                i64::try_from(lease.epoch).unwrap_or(i64::MAX),
            ],
        )?;
        Ok((changed > 0).then(|| XinTaskLease {
            worker_id: lease.worker_id.clone(),
            epoch: lease.epoch,
            expires_at,
        }))
    })
}

pub fn task_lease_is_current(config: &Config, task_id: &str, lease: &XinTaskLease, now: DateTime<Utc>) -> Result<bool> {
    with_connection(config, |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM xin_tasks
             WHERE id = ?1 AND status = 'running'
               AND lease_owner = ?2 AND lease_epoch = ?3
               AND lease_expires_at > ?4",
            params![
                task_id,
                lease.worker_id,
                i64::try_from(lease.epoch).unwrap_or(i64::MAX),
                now.to_rfc3339(),
            ],
            |row| row.get(0),
        )?;
        Ok(count == 1)
    })
}

pub fn step_lease_is_current(config: &Config, step_id: &str, lease: &XinStepLease, now: DateTime<Utc>) -> Result<bool> {
    with_connection(config, |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM xin_steps
             WHERE id = ?1 AND status IN ('claimed', 'running')
               AND lease_owner = ?2 AND lease_epoch = ?3
               AND lease_expires_at > ?4",
            params![
                step_id,
                lease.worker_id,
                i64::try_from(lease.epoch).unwrap_or(i64::MAX),
                now.to_rfc3339(),
            ],
            |row| row.get(0),
        )?;
        Ok(count == 1)
    })
}

/// Mark a task as completed after successful execution.
#[cfg(test)]
pub fn mark_completed(config: &Config, task_id: &str, output: &str) -> Result<()> {
    let now = Utc::now();
    let bounded = stored_task_output(output);
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

/// Mark a task as failed while preserving its operator-controlled enabled state.
#[cfg(test)]
pub fn mark_failed(config: &Config, task_id: &str, output: &str) -> Result<()> {
    let now = Utc::now();
    let bounded = stored_task_output(output);
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

/// Pause future scheduling without interrupting an execution that already owns
/// a valid lease.
pub fn pause_task(config: &Config, task_id: &str) -> Result<XinTask> {
    set_task_control_state(config, task_id, "pause")
}

/// Resume future scheduling. A previously cancelled task is reset to pending.
pub fn resume_task(config: &Config, task_id: &str) -> Result<XinTask> {
    set_task_control_state(config, task_id, "resume")
}

/// Cancel a task and revoke any active lease. Runners observe the revoked lease
/// and cooperatively stop the active execution.
pub fn cancel_task(config: &Config, task_id: &str) -> Result<XinTask> {
    set_task_control_state(config, task_id, "cancel")
}

fn set_task_control_state(config: &Config, task_id: &str, action: &str) -> Result<XinTask> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let exists = load_task_lineage(conn, task_id)?.is_some();
        if !exists {
            return Err(JobLookupError::Task(task_id.to_owned()).into());
        }
        match action {
            "pause" => {
                conn.execute(
                    "UPDATE xin_tasks SET version=version+1, enabled = 0, updated_at = ?1 WHERE id = ?2",
                    params![now.to_rfc3339(), task_id],
                )?;
            }
            "resume" => {
                conn.execute(
                    "UPDATE xin_tasks
                     SET version = version + 1, enabled = 1,
                         status = CASE WHEN status = 'cancelled' THEN 'pending' ELSE status END,
                         next_run_at = CASE WHEN status = 'cancelled' THEN ?1 ELSE next_run_at END,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![now.to_rfc3339(), task_id],
                )?;
            }
            "cancel" => {
                conn.execute(
                    "UPDATE xin_tasks
                     SET version=version+1, enabled = 0, status = 'cancelled', lease_owner = NULL,
                         lease_expires_at = NULL, last_heartbeat_at = NULL, updated_at = ?1
                     WHERE id = ?2",
                    params![now.to_rfc3339(), task_id],
                )?;
            }
            _ => anyhow::bail!("Unsupported Xin task control action '{action}'"),
        }
        let lineage = load_task_lineage(conn, task_id)?
            .ok_or_else(|| anyhow::anyhow!("Xin task '{task_id}' disappeared during {action}"))?;
        let event_type = match action {
            "pause" => "xin.task.paused",
            "resume" => "xin.task.resumed",
            "cancel" => "xin.task.cancelled",
            _ => anyhow::bail!("Unsupported Xin task control action '{action}'"),
        };
        insert_task_event(
            conn,
            &workspace_id(config),
            task_id,
            lineage.clone(),
            event_type,
            lineage.status.as_deref(),
            None,
        )?;
        Ok(())
    })?;
    get_task(config, task_id)
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
        return Err(JobLookupError::Task(task_id.to_owned()).into());
    }
    Ok(())
}

/// Remove completed one-shot system tasks. User/agent tasks are retained until
/// explicit removal so their result and `xin_runs` history remain queryable.
pub fn remove_completed(config: &Config) -> Result<usize> {
    with_connection(config, |conn| {
        conn.execute(
            "DELETE FROM xin_tasks
             WHERE status = 'completed' AND recurring = 0 AND kind = 'system'",
            [],
        )
        .context("Failed to clean completed xin tasks")
    })
}

/// Mark running tasks as stale if they exceeded `stale_timeout_minutes`.
pub fn mark_stale(config: &Config, stale_timeout_minutes: u32) -> Result<usize> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let cutoff = now - chrono::Duration::minutes(i64::from(stale_timeout_minutes));
        let mut stmt = conn.prepare(
            "SELECT id FROM xin_tasks
             WHERE status = 'running'
               AND (
                    (lease_expires_at IS NOT NULL AND lease_expires_at <= ?1)
                    OR (lease_expires_at IS NULL AND updated_at < ?2)
               )",
        )?;
        let ids = stmt
            .query_map(params![now.to_rfc3339(), cutoff.to_rfc3339()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let changed = conn
            .execute(
                "UPDATE xin_tasks
             SET status = 'stale', lease_owner = NULL, lease_expires_at = NULL, updated_at = ?1
             WHERE status = 'running'
               AND (
                    (lease_expires_at IS NOT NULL AND lease_expires_at <= ?1)
                    OR (lease_expires_at IS NULL AND updated_at < ?2)
               )",
                params![now.to_rfc3339(), cutoff.to_rfc3339()],
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
        // System tasks come only from the built-in definitions and the
        // operator's `HEARTBEAT.md` (CRON-ORIGIN). They are created with
        // `deliver = none`: a recurring system task leaves no result row per
        // run. An operator can still set another value with an update.
        || {
            add_task_with_stored_options(
                config,
                new,
                &JobOrigin::LocalOperator,
                JobCreator::AgentTool,
                &CreatorStamp::default(),
                ScheduledContext::default(),
                ScheduledDeliver::None,
            )
        },
        |id| {
            let current = get_task(config, &id)?;
            if current.payload == new.payload
                && current.interval_secs == new.interval_secs
                && current.max_failures == new.max_failures
            {
                return Ok(current);
            }
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

/// Make a newly materialized recurring task immediately claimable. Repeated
/// reconciliation is a no-op after the first run or once the due time passes.
pub(crate) fn make_task_due_if_never_run(config: &Config, task_id: &str) -> Result<()> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        conn.execute(
            "UPDATE xin_tasks SET next_run_at = ?1, updated_at = ?1
             WHERE id = ?2 AND run_count = 0 AND last_run_at IS NULL
               AND next_run_at > ?1",
            params![now.to_rfc3339(), task_id],
        )?;
        Ok(())
    })
}

/// Disable enabled system tasks in a reserved namespace that are absent from
/// the latest materialized source. Reconciliation is idempotent and converges
/// after a partial prior pass.
pub(crate) fn disable_system_tasks_except(
    config: &Config,
    namespace_prefix: &str,
    keep_names: &HashSet<String>,
) -> Result<usize> {
    with_immediate_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, name FROM xin_tasks
             WHERE kind = 'system' AND enabled = 1",
        )?;
        let candidates = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let now = authoritative_now(conn)?;
        let mut disabled = 0usize;
        for (task_id, name) in candidates {
            if !name.starts_with(namespace_prefix) || keep_names.contains(&name) {
                continue;
            }
            let changed = conn.execute(
                "UPDATE xin_tasks SET version=version+1, enabled = 0, updated_at = ?1
                 WHERE id = ?2 AND enabled = 1",
                params![now.to_rfc3339(), task_id],
            )?;
            if changed > 0 {
                disabled += 1;
                if let Some(lineage) = load_task_lineage(conn, &task_id)? {
                    insert_task_event(
                        conn,
                        &workspace_id(config),
                        &task_id,
                        lineage,
                        "xin.task.dematerialized",
                        Some("disabled"),
                        Some(serde_json::json!({ "name": name }).to_string()).as_deref(),
                    )?;
                }
            }
        }
        Ok(disabled)
    })
}

/// Commit one legacy Xin task execution as a single local transaction.
///
/// Result state, run history, recurring reschedule, local
/// lifecycle events, and their outbox rows either commit together or roll back
/// together. Cross-database delivery is recovered from the committed outbox.
pub fn commit_task_execution(
    config: &Config,
    task_id: &str,
    lease: &XinTaskLease,
    success: bool,
    output: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    duration_ms: i64,
) -> Result<bool> {
    let bounded = stored_task_output(output);
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
                     last_output = ?2, run_count = run_count + 1,
                     lease_owner = NULL, lease_expires_at = NULL, updated_at = ?1
                 WHERE id = ?3 AND status = 'running'
                   AND lease_owner = ?4 AND lease_epoch = ?5
                   AND lease_expires_at > ?1",
                params![
                    finished_at.to_rfc3339(),
                    bounded,
                    task_id,
                    lease.worker_id,
                    i64::try_from(lease.epoch).unwrap_or(i64::MAX),
                ],
            )?
        } else {
            conn.execute(
                "UPDATE xin_tasks
                 SET status = 'failed', last_run_at = ?1, last_status = CASE WHEN ?2 IN ('agent_busy','agent_budget_exhausted') THEN 'skipped' ELSE 'error' END,
                     last_output = ?2, run_count = run_count + 1,
                     fail_count = fail_count + 1,
                     lease_owner = NULL, lease_expires_at = NULL, updated_at = ?1
                 WHERE id = ?3 AND status = 'running'
                   AND lease_owner = ?4 AND lease_epoch = ?5
                   AND lease_expires_at > ?1",
                params![
                    finished_at.to_rfc3339(),
                    bounded,
                    task_id,
                    lease.worker_id,
                    i64::try_from(lease.epoch).unwrap_or(i64::MAX),
                ],
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
                Some(
                    serde_json::json!({
                        "output": bounded,
                        "lease_owner": lease.worker_id,
                        "lease_epoch": lease.epoch,
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
        }

        insert_run_record(
            conn,
            config,
            task_id,
            started_at,
            finished_at,
            crate::cron::agent_jobs::run_status(success, output),
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

/// Persist an execution that was interrupted by an explicit task cancellation.
///
/// `cancel_task` revokes the active lease before the runner can commit through
/// `commit_task_execution`, so the cancelled attempt needs its own fenced
/// terminal write. Only a task that is still cancelled can accept this record;
/// lease expiry, task removal, and a subsequent resume remain authority loss.
pub fn commit_task_cancellation(
    config: &Config,
    task_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    output: &str,
) -> Result<bool> {
    let bounded = stored_task_output(output);
    let duration_ms = (finished_at - started_at).num_milliseconds();
    with_immediate_connection(config, |conn| {
        let changed = conn.execute(
            "UPDATE xin_tasks
             SET last_run_at = ?1, last_status = 'cancelled', last_output = ?2,
                 run_count = run_count + 1, updated_at = ?1
             WHERE id = ?3 AND status = 'cancelled'",
            params![finished_at.to_rfc3339(), bounded, task_id],
        )?;
        if changed == 0 {
            return Ok(false);
        }

        insert_run_record(
            conn,
            config,
            task_id,
            started_at,
            finished_at,
            "cancelled",
            Some(&bounded),
            duration_ms,
        )?;
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
    let bounded = output.map(stored_task_output);

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

/// List the newest persisted execution attempts for a task.
pub fn list_runs(config: &Config, task_id: &str, limit: usize) -> Result<Vec<XinRun>> {
    // Keep the public query bounded to the same retention window used by the
    // writer. A zero limit is treated as the useful default.
    let limit = if limit == 0 { 20 } else { limit.min(50) };
    with_connection(config, |conn| {
        let exists = conn
            .query_row("SELECT 1 FROM xin_tasks WHERE id = ?1", params![task_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            return Err(JobLookupError::Task(task_id.to_owned()).into());
        }
        let mut stmt = conn.prepare(
            "SELECT id, task_id, started_at, finished_at, status, output, duration_ms
             FROM xin_runs WHERE task_id = ?1
             ORDER BY started_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![task_id, i64::try_from(limit).unwrap_or(50)], |row| {
            let started_at: String = row.get(2)?;
            let finished_at: String = row.get(3)?;
            Ok(XinRun {
                id: row.get(0)?,
                task_id: row.get(1)?,
                started_at: parse_rfc3339(&started_at).map_err(sql_err)?,
                finished_at: parse_rfc3339(&finished_at).map_err(sql_err)?,
                status: row.get(4)?,
                output: row.get(5)?,
                duration_ms: row.get::<_, Option<i64>>(6)?.unwrap_or_default(),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests;
