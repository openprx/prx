#[cfg(test)]
use super::get_task;
use super::rows::{
    SELECT_ALL_COLUMNS, TaskLineage, insert_task_event, map_task_row, parse_rfc3339, sql_err, stored_origin,
    stored_task_output, workspace_id,
};
use super::{JobLookupError, authoritative_now, stored_creator, with_connection, with_immediate_connection};
use crate::config::Config;
use crate::cron::JobCreator;
use crate::security::agent_level::CreatorStamp;
use crate::tools::execution::JobOrigin;
use crate::xin::types::{
    ExecutionMode, GoalStatus, NewXinGoal, NewXinStep, StepStatus, TaskKind, TaskPriority, TaskStatus, XinGoal,
    XinStep, default_lease_ttl_secs,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

// ── Goal / Step (FIX-P2-16, d09) ─────────────────────────────────────────

const SELECT_GOAL_COLUMNS: &str = "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
            name, description, kind, status, priority, target_completion_at,
            steps_completed, steps_total, created_at, updated_at, completed_at,
            final_output, enabled, origin
     FROM xin_goals";

const SELECT_STEP_COLUMNS: &str = "SELECT id, goal_id, sequence, name, description, status, execution_mode,
            payload, lease_owner, lease_epoch, lease_expires_at, last_heartbeat_at, checkpoint_json,
            lease_ttl_secs, retry_count, max_retries, created_at, updated_at, started_at, completed_at,
            last_output, approval_grant_json
     FROM xin_steps";

/// Insert a goal and (optionally) its initial steps in a single transaction.
///
/// The goal records no creator and so runs as [`JobOrigin::Unattributed`];
/// entry points that know who created it use [`add_goal_with_origin`].
#[cfg(test)]
pub fn add_goal(config: &Config, new: &NewXinGoal) -> Result<XinGoal> {
    add_goal_with_origin(config, new, &JobOrigin::Unattributed)
}

/// Insert a goal created by `origin` (CRON-ORIGIN); every step of it runs
/// under that origin. It records no operator creator.
#[cfg(test)]
pub fn add_goal_with_origin(config: &Config, new: &NewXinGoal, origin: &JobOrigin) -> Result<XinGoal> {
    add_goal_with_creator_stamp(config, new, origin, JobCreator::AgentTool, &CreatorStamp::default())
}

/// Insert a goal created by `origin`, recording which surface created it
/// (`created_via`: only `prx xin` passes [`JobCreator::OperatorCli`]) and the
/// creating call's level and agent.
pub fn add_goal_with_creator_stamp(
    config: &Config,
    new: &NewXinGoal,
    origin: &JobOrigin,
    created_via: JobCreator,
    creator: &CreatorStamp,
) -> Result<XinGoal> {
    let goal_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    with_immediate_connection(config, |conn| {
        insert_goal_rows(conn, config, &goal_id, new, origin, (created_via, creator), now)
    })?;

    get_goal(config, &goal_id)
}

/// Insert a goal, its initial steps, and its lifecycle event inside the
/// caller's transaction.
fn insert_goal_rows(
    conn: &Connection,
    config: &Config,
    goal_id: &str,
    new: &NewXinGoal,
    origin: &JobOrigin,
    (created_via, creator): (JobCreator, &CreatorStamp),
    now: DateTime<Utc>,
) -> Result<()> {
    let steps_total = u32::try_from(new.initial_steps.len()).unwrap_or(u32::MAX);
    let origin = origin.to_stored()?;
    conn.execute(
        "INSERT INTO xin_goals (
                id, owner_id, topic_id, parent_task_id, source_message_event_id,
                name, description, kind, status, priority, target_completion_at,
                steps_completed, steps_total, created_at, updated_at, completed_at,
                final_output, enabled, origin, creator_level, creator_agent, created_via
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, 'pending', ?9, ?10,
                0, ?11, ?12, ?13, NULL,
                NULL, 1, ?14, ?15, ?16, ?17
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
            origin,
            creator.level_column(),
            creator.agent.as_deref(),
            created_via.as_str(),
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
/// Append a step as the operator (`prx xin`): the goal is re-recorded as the
/// operator's (`created_via = operator_cli`, no stored level).
pub fn add_step(config: &Config, goal_id: &str, step: &NewXinStep) -> Result<XinStep> {
    add_step_with_creator_stamp(config, goal_id, step, JobCreator::OperatorCli, &CreatorStamp::default())
}

/// [`add_step`] re-recording the adding surface and the call's level and
/// agent on the goal: a new step rewrites what the goal runs (V7).
pub fn add_step_with_creator_stamp(
    config: &Config,
    goal_id: &str,
    step: &NewXinStep,
    created_via: JobCreator,
    creator: &CreatorStamp,
) -> Result<XinStep> {
    let now = Utc::now();
    let step_id = with_immediate_connection(config, |conn| {
        let id = insert_step_row(conn, goal_id, step, now)?;
        let changed = conn
            .execute(
                "UPDATE xin_goals
                 SET version = version + 1, steps_total = steps_total + 1,
                     status = CASE WHEN status = 'completed' THEN 'running' ELSE status END,
                     completed_at = CASE WHEN status = 'completed' THEN NULL ELSE completed_at END,
                     final_output = CASE WHEN status = 'completed' THEN NULL ELSE final_output END,
                     updated_at = ?1, creator_level = CASE WHEN substr(creator_agent,1,2)='n:' THEN creator_level ELSE ?3 END, creator_agent = CASE WHEN substr(creator_agent,1,2)='n:' THEN creator_agent ELSE ?4 END, created_via = ?5
                 WHERE id = ?2",
                params![
                    now.to_rfc3339(),
                    goal_id,
                    creator.level_column(),
                    creator.agent.as_deref(),
                    created_via.as_str()
                ],
            )
            .context("Failed to bump goal steps_total")?;
        if changed == 0 {
            return Err(JobLookupError::Goal(goal_id.to_owned()).into());
        }
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
        rows.next()?.map_or_else(
            || Err(JobLookupError::Goal(goal_id.to_owned()).into()),
            |row| map_goal_row(row).map_err(Into::into),
        )
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

pub fn pause_goal(config: &Config, goal_id: &str) -> Result<XinGoal> {
    set_goal_control_state(config, goal_id, "pause")
}

pub fn resume_goal(config: &Config, goal_id: &str) -> Result<XinGoal> {
    set_goal_control_state(config, goal_id, "resume")
}

pub fn cancel_goal(config: &Config, goal_id: &str) -> Result<XinGoal> {
    set_goal_control_state(config, goal_id, "cancel")
}

fn set_goal_control_state(config: &Config, goal_id: &str, action: &str) -> Result<XinGoal> {
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let exists = conn
            .query_row("SELECT 1 FROM xin_goals WHERE id = ?1", params![goal_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            return Err(JobLookupError::Goal(goal_id.to_owned()).into());
        }
        match action {
            "pause" => {
                conn.execute(
                    "UPDATE xin_goals SET version = version + 1, enabled = 0, updated_at = ?1 WHERE id = ?2",
                    params![now.to_rfc3339(), goal_id],
                )?;
            }
            "resume" => {
                conn.execute(
                    "UPDATE xin_goals
                     SET version = version + 1, enabled = 1,
                         status = CASE
                             WHEN status = 'cancelled' AND steps_completed = 0 THEN 'pending'
                             WHEN status = 'cancelled' THEN 'running'
                             ELSE status
                         END,
                         completed_at = CASE WHEN status = 'cancelled' THEN NULL ELSE completed_at END,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![now.to_rfc3339(), goal_id],
                )?;
            }
            "cancel" => {
                conn.execute(
                    "UPDATE xin_goals
                     SET version = version + 1, enabled = 0, status = 'cancelled', completed_at = ?1, updated_at = ?1
                     WHERE id = ?2",
                    params![now.to_rfc3339(), goal_id],
                )?;
                conn.execute(
                    "UPDATE xin_steps
                     SET status = CASE WHEN status IN ('claimed', 'running') THEN 'stale' ELSE status END,
                         lease_owner = NULL, lease_expires_at = NULL, last_heartbeat_at = NULL, updated_at = ?1
                     WHERE goal_id = ?2 AND status IN ('claimed', 'running')",
                    params![now.to_rfc3339(), goal_id],
                )?;
            }
            _ => anyhow::bail!("Unsupported Xin goal control action '{action}'"),
        }
        let event_type = match action {
            "pause" => "xin.goal.paused",
            "resume" => "xin.goal.resumed",
            "cancel" => "xin.goal.cancelled",
            _ => anyhow::bail!("Unsupported Xin goal control action '{action}'"),
        };
        let status: String = conn.query_row("SELECT status FROM xin_goals WHERE id = ?1", params![goal_id], |row| {
            row.get(0)
        })?;
        emit_goal_event(conn, config, goal_id, event_type, Some(&status))?;
        Ok(())
    })?;
    get_goal(config, goal_id)
}

pub fn remove_goal(config: &Config, goal_id: &str) -> Result<()> {
    let changed = with_immediate_connection(config, |conn| {
        if conn
            .query_row("SELECT 1 FROM xin_goals WHERE id = ?1", params![goal_id], |_| Ok(()))
            .optional()?
            .is_none()
        {
            return Ok(0);
        }
        emit_goal_event(conn, config, goal_id, "xin.goal.removed", None)?;
        conn.execute("DELETE FROM xin_goals WHERE id = ?1", params![goal_id])
            .context("Failed to delete xin goal")
    })?;
    if changed == 0 {
        return Err(JobLookupError::Goal(goal_id.to_owned()).into());
    }
    Ok(())
}

/// Re-run a step and every later step so ordered workflow outputs cannot become
/// inconsistent with an earlier retried result.
pub fn retry_step(config: &Config, step_id: &str) -> Result<XinStep> {
    with_immediate_connection(config, |conn| {
        let (goal_id, sequence): (String, i64) = conn
            .query_row(
                "SELECT goal_id, sequence FROM xin_steps WHERE id = ?1",
                params![step_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("Xin step '{step_id}' not found"))?;
        let running: i64 = conn.query_row(
            "SELECT COUNT(*) FROM xin_steps
             WHERE goal_id = ?1 AND sequence >= ?2 AND status IN ('claimed', 'running')",
            params![goal_id, sequence],
            |row| row.get(0),
        )?;
        if running > 0 {
            anyhow::bail!("Cannot retry step '{step_id}' while it or a later step is running; cancel the goal first");
        }
        let now = authoritative_now(conn)?;
        conn.execute(
            "UPDATE xin_steps
             SET status = 'pending', lease_owner = NULL, lease_expires_at = NULL,
                 last_heartbeat_at = NULL, checkpoint_json = NULL, completed_at = NULL,
                 last_output = NULL, updated_at = ?1
             WHERE goal_id = ?2 AND sequence >= ?3",
            params![now.to_rfc3339(), goal_id, sequence],
        )?;
        let completed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM xin_steps WHERE goal_id = ?1 AND status = 'completed'",
            params![goal_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "UPDATE xin_goals
             SET version=version+1, enabled = 1, status = CASE WHEN ?1 = 0 THEN 'pending' ELSE 'running' END,
                 steps_completed = ?1, completed_at = NULL, final_output = NULL, updated_at = ?2
             WHERE id = ?3",
            params![completed, now.to_rfc3339(), goal_id],
        )?;
        emit_step_event(conn, config, step_id, "xin.step.retry_requested", Some("pending"), None)?;
        Ok(())
    })?;
    get_step(config, step_id)
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
    let bounded = stored_task_output(output);
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

/// Retry only the exact lease generation that performed the work.
///
/// Xin goals are durable intents: an execution failure records the error and
/// releases the lease, but does not impose a hidden retry ceiling or terminally
/// fail the goal. Operators can explicitly cancel a goal when retrying is no
/// longer desired.
pub(crate) fn fail_step_with_lease(config: &Config, step_id: &str, lease: &XinStepLease, output: &str) -> Result<bool> {
    let bounded = stored_task_output(output);
    with_immediate_connection(config, |conn| {
        let now = authoritative_now(conn)?;
        let exists = conn
            .query_row(
                "SELECT 1 FROM xin_steps
                 WHERE id = ?1 AND lease_owner = ?2 AND lease_epoch = ?3
                   AND lease_expires_at >= ?4 AND status IN ('claimed', 'running')",
                params![
                    step_id,
                    lease.worker_id,
                    i64::try_from(lease.epoch).unwrap_or(i64::MAX),
                    now.to_rfc3339()
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Ok(false);
        }
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
        Ok(true)
    })
}

/// Mark a step completed, recompute the goal's progress and roll the goal up to
/// `completed` once every step is done.
#[cfg(test)]
pub fn complete_step(config: &Config, step_id: &str, output: &str) -> Result<()> {
    let now = Utc::now();
    let bounded = stored_task_output(output);
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

/// Record a failed attempt and reset the step to `pending` for re-execution.
#[cfg(test)]
pub fn fail_step(config: &Config, step_id: &str, output: &str) -> Result<()> {
    let now = Utc::now();
    let bounded = stored_task_output(output);
    with_connection(config, |conn| {
        let exists = conn
            .query_row("SELECT 1 FROM xin_steps WHERE id = ?1", params![step_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            tracing::warn!(step_id = %step_id, "fail_step: step not found");
            return Ok(());
        }
        conn.execute(
            "UPDATE xin_steps
             SET status = 'pending', retry_count = retry_count + 1, last_output = ?1,
                 lease_owner = NULL, lease_expires_at = NULL, updated_at = ?2
             WHERE id = ?3",
            params![bounded, now.to_rfc3339(), step_id],
        )
        .context("Failed to reset xin step for retry")?;
        emit_step_event(conn, config, step_id, "xin.step.retry", Some("pending"), None)?;
        Ok(())
    })
}

/// Reset steps whose lease expired (while claimed/running) back to `stale` so
/// they can be re-claimed. Returns the affected step ids.
pub fn mark_steps_stale(config: &Config, now: DateTime<Utc>) -> Result<Vec<String>> {
    with_immediate_connection(config, |conn| {
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
            max_retries: 0,
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
        // The adopted goal runs under the legacy task's own origin and creator.
        let (created_via, creator) = stored_creator(conn, &task.id)?;
        insert_goal_rows(
            conn,
            config,
            &goal_id,
            &goal,
            &task.origin,
            (created_via, &creator),
            now,
        )?;
        conn.execute(
            "INSERT INTO xin_task_adoptions (legacy_task_id, goal_id, adopted_at)
             VALUES (?1, ?2, ?3)",
            params![task_id, goal_id, now.to_rfc3339()],
        )
        .context("Failed to link adopted Xin task to goal")?;
        conn.execute(
            "UPDATE xin_tasks SET version=version+1, enabled = 0, updated_at = ?1 WHERE id = ?2",
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
        max_retries: 0,
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
    add_goal_with_origin(config, &new_goal, &task.origin)
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

/// Retain the retirement reason in the goal's existing lifecycle stream.
pub(crate) fn record_retired_goal(conn: &Connection, config: &Config, goal_id: &str) -> Result<()> {
    let lineage = load_goal_lineage(conn, goal_id)?.context("retired goal missing")?;
    insert_task_event(
        conn,
        &workspace_id(config),
        goal_id,
        lineage,
        "goal.paused",
        Some("retired"),
        Some(r#"{"reason":"retired"}"#),
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
    let kind = TaskKind::from_str_lossy(&row.get::<_, String>(7)?);
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
        kind: kind.clone(),
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
        origin: stored_origin(row.get::<_, Option<String>>(18)?.as_deref(), &kind),
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

/// A step runs against the goal revision; edits to steps advance that goal.
pub fn task_revision(config: &Config, id: &str) -> Result<u64> {
    with_connection(config, |conn| {
        Ok(u64::try_from(conn.query_row("SELECT version FROM xin_tasks WHERE id=?1 UNION ALL SELECT g.version FROM xin_steps s JOIN xin_goals g ON g.id=s.goal_id WHERE s.id=?1 UNION ALL SELECT version FROM xin_goals WHERE id=?1 LIMIT 1", [id], |r| r.get::<_, i64>(0))?)?)
    })
}
