#![allow(clippy::print_stdout, clippy::print_stderr)]

//! Cron job persistence.
//!
//! FIX-P2-05 (F2-PG) — dual-backend. Cron scheduling state (`cron_jobs` /
//! `cron_runs` / `cron_job_events`) is persisted in either local SQLite (the
//! default, via [`with_connection`]) or PostgreSQL. The public free functions in
//! this module are thin dispatchers: when the workspace storage provider
//! resolves to `postgres` with a `db_url` (mirroring the memory backend's
//! `[storage.provider.config]`), they route to [`crate::cron::postgres::PostgresCronStore`];
//! otherwise they use the embedded SQLite path. Because the Postgres backend is
//! reachable from configuration it is not dead code.
//!
//! The Postgres backend gives multi-instance deployments durable, shared cron
//! state: the atomic `claim_job` guard prevents two scheduler instances polling
//! the same database from double-executing a job. Cron lifecycle events are
//! still mirrored into the shared `memory_events` fabric (FIX-P0-16/17,
//! `cron.job.*`) for cross-instance observability.

use crate::config::Config;
use crate::cron::{
    CronJob, CronJobEvent, CronJobLineage, CronJobPatch, CronJobTerminalState, CronRun, DeliveryConfig, JobType,
    Schedule, SessionTarget, next_run_for_schedule, schedule_cron_expression, validate_schedule,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use uuid::Uuid;

const MAX_CRON_OUTPUT_BYTES: usize = 16 * 1024;
const TRUNCATED_OUTPUT_MARKER: &str = "\n...[truncated]";

/// Resolve the Postgres cron backend when configured, else `None` (SQLite path).
/// Centralizes backend selection so each public dispatcher is a one-line guard.
fn pg_store(config: &Config) -> Result<Option<crate::cron::postgres::PostgresCronStore>> {
    crate::cron::postgres::resolve(config)
}

#[cfg(test)]
pub fn add_job(config: &Config, expression: &str, command: &str) -> Result<CronJob> {
    let schedule = Schedule::Cron {
        expr: expression.to_string(),
        tz: None,
    };
    add_shell_job(config, None, schedule, command)
}

pub fn add_shell_job(config: &Config, name: Option<String>, schedule: Schedule, command: &str) -> Result<CronJob> {
    add_shell_job_with_approval_grant(config, name, schedule, command, None)
}

pub fn add_shell_job_with_approval_grant(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    command: &str,
    approval_grant_json: Option<String>,
) -> Result<CronJob> {
    add_shell_job_with_lineage_and_approval_grant(
        config,
        name,
        schedule,
        command,
        approval_grant_json,
        CronJobLineage::default(),
    )
}

pub fn add_shell_job_with_lineage_and_approval_grant(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    command: &str,
    approval_grant_json: Option<String>,
    lineage: CronJobLineage,
) -> Result<CronJob> {
    add_shell_job_with_lineage_approval_and_delete(config, name, schedule, command, approval_grant_json, false, lineage)
}

#[allow(clippy::too_many_arguments)]
pub fn add_shell_job_with_lineage_approval_and_delete(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    command: &str,
    approval_grant_json: Option<String>,
    delete_after_run: bool,
    lineage: CronJobLineage,
) -> Result<CronJob> {
    if let Some(store) = pg_store(config)? {
        return store.add_shell_job_with_lineage_approval_and_delete(
            &workspace_id(config),
            name,
            schedule,
            command,
            approval_grant_json,
            delete_after_run,
            lineage,
        );
    }
    let now = Utc::now();
    validate_schedule(&schedule, now)?;
    let next_run = next_run_for_schedule(&schedule, now)?;
    let id = Uuid::new_v4().to_string();
    let expression = schedule_cron_expression(&schedule).unwrap_or_default();
    let schedule_json = serde_json::to_string(&schedule)?;
    let owner_id = lineage.owner_id.clone();
    let topic_id = lineage.topic_id.clone();
    let parent_task_id = lineage.parent_task_id.clone();
    let source_message_event_id = lineage.source_message_event_id;

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, owner_id, topic_id, parent_task_id, source_message_event_id,
                expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, created_at, next_run, approval_grant_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'shell', NULL, ?9, 'isolated', NULL, 1, ?10, ?11, ?12, ?13, ?14)",
            params![
                id,
                owner_id.as_deref(),
                topic_id.as_deref(),
                parent_task_id.as_deref(),
                source_message_event_id.as_deref(),
                expression,
                command,
                schedule_json.as_str(),
                name.as_deref(),
                serde_json::to_string(&DeliveryConfig::default())?,
                if delete_after_run { 1 } else { 0 },
                now.to_rfc3339(),
                next_run.to_rfc3339(),
                approval_grant_json,
            ],
        )
        .context("Failed to insert cron shell job")?;
        insert_job_event(
            conn,
            &workspace_id(config),
            &id,
            JobLineage {
                owner_id: owner_id.clone(),
                topic_id: topic_id.clone(),
                parent_task_id: parent_task_id.clone(),
                source_message_event_id: source_message_event_id.clone(),
                status: Some("pending".to_string()),
            },
            "cron.job.created",
            Some("pending"),
            Some(
                serde_json::json!({
                    "kind": "shell",
                    "name": name,
                    "schedule": schedule_json,
                    "source_message_event_id": source_message_event_id,
                })
                .to_string(),
            )
            .as_deref(),
        )?;
        Ok(())
    })?;

    get_job(config, &id)
}

#[allow(clippy::too_many_arguments)]
pub fn add_agent_job(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    prompt: &str,
    session_target: SessionTarget,
    model: Option<String>,
    delivery: Option<DeliveryConfig>,
    delete_after_run: bool,
) -> Result<CronJob> {
    add_agent_job_with_lineage(
        config,
        name,
        schedule,
        prompt,
        session_target,
        model,
        delivery,
        delete_after_run,
        CronJobLineage::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn add_agent_job_with_lineage(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    prompt: &str,
    session_target: SessionTarget,
    model: Option<String>,
    delivery: Option<DeliveryConfig>,
    delete_after_run: bool,
    lineage: CronJobLineage,
) -> Result<CronJob> {
    if let Some(store) = pg_store(config)? {
        return store.add_agent_job_with_lineage(
            &workspace_id(config),
            name,
            schedule,
            prompt,
            session_target,
            model,
            delivery,
            delete_after_run,
            lineage,
        );
    }
    let now = Utc::now();
    validate_schedule(&schedule, now)?;
    let next_run = next_run_for_schedule(&schedule, now)?;
    let id = Uuid::new_v4().to_string();
    let expression = schedule_cron_expression(&schedule).unwrap_or_default();
    let schedule_json = serde_json::to_string(&schedule)?;
    let delivery = delivery.unwrap_or_default();
    let owner_id = lineage.owner_id.clone();
    let topic_id = lineage.topic_id.clone();
    let parent_task_id = lineage.parent_task_id.clone();
    let source_message_event_id = lineage.source_message_event_id;

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, owner_id, topic_id, parent_task_id, source_message_event_id,
                expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, created_at, next_run, approval_grant_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', ?7, 'agent', ?8, ?9, ?10, ?11, 1, ?12, ?13, ?14, ?15, NULL)",
            params![
                id,
                owner_id.as_deref(),
                topic_id.as_deref(),
                parent_task_id.as_deref(),
                source_message_event_id.as_deref(),
                expression,
                schedule_json.as_str(),
                prompt,
                name.as_deref(),
                session_target.as_str(),
                model,
                serde_json::to_string(&delivery)?,
                if delete_after_run { 1 } else { 0 },
                now.to_rfc3339(),
                next_run.to_rfc3339(),
            ],
        )
        .context("Failed to insert cron agent job")?;
        insert_job_event(
            conn,
            &workspace_id(config),
            &id,
            JobLineage {
                owner_id: owner_id.clone(),
                topic_id: topic_id.clone(),
                parent_task_id: parent_task_id.clone(),
                source_message_event_id: source_message_event_id.clone(),
                status: Some("pending".to_string()),
            },
            "cron.job.created",
            Some("pending"),
            Some(
                serde_json::json!({
                    "kind": "agent",
                    "name": name,
                    "schedule": schedule_json,
                    "session_target": session_target.as_str(),
                    "source_message_event_id": source_message_event_id,
                })
                .to_string(),
            )
            .as_deref(),
        )?;
        Ok(())
    })?;

    get_job(config, &id)
}

pub fn list_jobs(config: &Config) -> Result<Vec<CronJob>> {
    if let Some(store) = pg_store(config)? {
        return store.list_jobs();
    }
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
                    expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
                    terminal_state, approval_grant_json
             FROM cron_jobs ORDER BY next_run ASC",
        )?;

        let rows = stmt.query_map([], map_cron_job_row)?;

        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row?);
        }
        Ok(jobs)
    })
}

pub fn get_job(config: &Config, job_id: &str) -> Result<CronJob> {
    if let Some(store) = pg_store(config)? {
        return store.get_job(job_id);
    }
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
                    expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
                    terminal_state, approval_grant_json
             FROM cron_jobs WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![job_id])?;
        if let Some(row) = rows.next()? {
            map_cron_job_row(row).map_err(Into::into)
        } else {
            anyhow::bail!("Cron job '{job_id}' not found")
        }
    })
}

pub fn remove_job(config: &Config, id: &str) -> Result<()> {
    if let Some(store) = pg_store(config)? {
        store.remove_job(&workspace_id(config), id)?;
        println!("✅ Removed cron job {id}");
        return Ok(());
    }
    let changed = with_connection(config, |conn| {
        if let Some(lineage) = load_job_lineage(conn, id)? {
            insert_job_event(
                conn,
                &workspace_id(config),
                id,
                lineage.clone(),
                "cron.job.removed",
                lineage.status.as_deref(),
                None,
            )?;
        }
        conn.execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])
            .context("Failed to delete cron job")
    })?;

    if changed == 0 {
        anyhow::bail!("Cron job '{id}' not found");
    }

    println!("✅ Removed cron job {id}");
    Ok(())
}

/// Atomically claim a cron job for execution. Returns true if claimed.
///
/// Uses an UPDATE with a WHERE guard so that only one instance can transition
/// the job from a non-running state to `running`. This prevents duplicate
/// execution when multiple scheduler instances poll the same database.
pub fn claim_job(config: &Config, job_id: &str) -> Result<bool> {
    if let Some(store) = pg_store(config)? {
        return store.claim_job(&workspace_id(config), job_id);
    }
    with_connection(config, |conn| {
        let changed = conn.execute(
            "UPDATE cron_jobs SET last_status = 'running'
             WHERE id = ?1 AND enabled = 1 AND terminal_state IS NULL
               AND (last_status IS NULL OR last_status != 'running')",
            params![job_id],
        )?;
        if changed > 0 {
            if let Some(lineage) = load_job_lineage(conn, job_id)? {
                insert_job_event(
                    conn,
                    &workspace_id(config),
                    job_id,
                    lineage,
                    "cron.job.claimed",
                    Some("running"),
                    None,
                )?;
            }
        }
        Ok(changed > 0)
    })
}

pub fn claim_job_if_current(config: &Config, job: &CronJob, now: DateTime<Utc>) -> Result<bool> {
    if let Some(store) = pg_store(config)? {
        return store.claim_job_if_current(&workspace_id(config), job, Some(now));
    }
    claim_sqlite_job_snapshot(config, job, Some(now))
}

pub fn claim_job_if_current_for_manual_run(config: &Config, job: &CronJob) -> Result<bool> {
    if let Some(store) = pg_store(config)? {
        return store.claim_job_if_current(&workspace_id(config), job, None);
    }
    claim_sqlite_job_snapshot(config, job, None)
}

fn claim_sqlite_job_snapshot(config: &Config, job: &CronJob, due_by: Option<DateTime<Utc>>) -> Result<bool> {
    let schedule_json = serde_json::to_string(&job.schedule)?;
    with_connection(config, |conn| {
        let changed = if let Some(due_by) = due_by {
            conn.execute(
                "UPDATE cron_jobs SET last_status = 'running'
                 WHERE id = ?1 AND enabled = 1 AND terminal_state IS NULL AND next_run = ?2 AND next_run <= ?3
                   AND (schedule = ?4 OR (schedule IS NULL AND expression = ?5))
                   AND (last_status IS NULL OR last_status != 'running')",
                params![
                    job.id,
                    job.next_run.to_rfc3339(),
                    due_by.to_rfc3339(),
                    schedule_json,
                    job.expression,
                ],
            )?
        } else {
            conn.execute(
                "UPDATE cron_jobs SET last_status = 'running'
                 WHERE id = ?1 AND terminal_state IS NULL AND next_run = ?2
                   AND (schedule = ?3 OR (schedule IS NULL AND expression = ?4))
                   AND (last_status IS NULL OR last_status != 'running')",
                params![job.id, job.next_run.to_rfc3339(), schedule_json, job.expression],
            )?
        };
        if changed > 0
            && let Some(lineage) = load_job_lineage(conn, &job.id)?
        {
            insert_job_event(
                conn,
                &workspace_id(config),
                &job.id,
                lineage,
                "cron.job.claimed",
                Some("running"),
                None,
            )?;
        }
        Ok(changed > 0)
    })
}

pub fn due_jobs(config: &Config, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
    if let Some(store) = pg_store(config)? {
        return store.due_jobs(now, config.scheduler.max_tasks);
    }
    let lim = i64::try_from(config.scheduler.max_tasks.max(1)).context("Scheduler max_tasks overflows i64")?;
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
                    expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
                    terminal_state, approval_grant_json
             FROM cron_jobs
             WHERE enabled = 1 AND terminal_state IS NULL AND next_run <= ?1
             ORDER BY next_run ASC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![now.to_rfc3339(), lim], map_cron_job_row)?;

        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row?);
        }
        Ok(jobs)
    })
}

pub fn update_job(config: &Config, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
    if let Some(store) = pg_store(config)? {
        return store.update_job(&workspace_id(config), job_id, patch);
    }
    let mut job = get_job(config, job_id)?;
    let was_enabled = job.enabled;
    let expected_schedule_json = serde_json::to_string(&job.schedule)?;
    let expected_expression = job.expression.clone();
    let expected_next_run = job.next_run.to_rfc3339();
    let expected_last_status = job.last_status.clone();
    let approval_grant_json = patch.approval_grant_json.clone();
    let schedule_changed = if let Some(schedule) = patch.schedule {
        validate_schedule(&schedule, Utc::now())?;
        if job.terminal_state.is_none()
            && matches!(job.schedule, Schedule::At { .. })
            && job.last_status.as_deref() == Some("running")
        {
            anyhow::bail!("cannot update the schedule of an in-flight Schedule::At job");
        }
        let rearm_terminal_at = job.terminal_state.is_some()
            && matches!(job.schedule, Schedule::At { .. })
            && matches!(schedule, Schedule::At { .. });
        if job.terminal_state.is_some() && matches!(job.schedule, Schedule::At { .. }) && !rearm_terminal_at {
            anyhow::bail!("a terminal Schedule::At job can only be re-armed with a new future Schedule::At");
        }
        job.schedule = schedule;
        job.expression = schedule_cron_expression(&job.schedule).unwrap_or_default();
        Some(rearm_terminal_at)
    } else {
        None
    };
    if let Some(command) = patch.command {
        job.command = command;
        job.approval_grant_json = approval_grant_json;
    }
    if let Some(prompt) = patch.prompt {
        job.prompt = Some(prompt);
    }
    if let Some(name) = patch.name {
        job.name = Some(name);
    }
    if let Some(enabled) = patch.enabled {
        job.enabled = enabled;
    }
    if let Some(delivery) = patch.delivery {
        job.delivery = delivery;
    }
    if let Some(model) = patch.model {
        job.model = Some(model);
    }
    if let Some(target) = patch.session_target {
        job.session_target = target;
    }
    if let Some(delete_after_run) = patch.delete_after_run {
        job.delete_after_run = delete_after_run;
    }

    if let Some(rearm_terminal_at) = schedule_changed {
        job.next_run = next_run_for_schedule(&job.schedule, Utc::now())?;
        if rearm_terminal_at {
            job.last_run = None;
            job.last_status = None;
            job.last_output = None;
            job.terminal_state = None;
            job.enabled = true;
        }
    }

    with_connection(config, |conn| {
        let changed = conn
            .execute(
                "UPDATE cron_jobs
             SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                 session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11,
                 next_run = ?12, last_run = ?13, last_status = ?14, last_output = ?15,
                 terminal_state = ?16, approval_grant_json = ?17
             WHERE id = ?18 AND next_run = ?19
               AND (schedule = ?20 OR (schedule IS NULL AND expression = ?21))
               AND last_status IS ?22",
                params![
                    job.expression,
                    job.command,
                    serde_json::to_string(&job.schedule)?,
                    job.job_type.as_str(),
                    job.prompt,
                    job.name,
                    job.session_target.as_str(),
                    job.model,
                    if job.enabled { 1 } else { 0 },
                    serde_json::to_string(&job.delivery)?,
                    if job.delete_after_run { 1 } else { 0 },
                    job.next_run.to_rfc3339(),
                    job.last_run.map(|value| value.to_rfc3339()),
                    job.last_status,
                    job.last_output,
                    job.terminal_state.map(CronJobTerminalState::as_str),
                    job.approval_grant_json,
                    job.id,
                    expected_next_run,
                    expected_schedule_json,
                    expected_expression,
                    expected_last_status,
                ],
            )
            .context("Failed to update cron job")?;
        if changed == 0 {
            anyhow::bail!("cron job '{}' was deleted or modified concurrently", job.id);
        }
        let event_type = match (was_enabled, job.enabled) {
            (true, false) => "cron.job.disabled",
            (false, true) => "cron.job.enabled",
            _ => "cron.job.updated",
        };
        if let Some(lineage) = load_job_lineage(conn, &job.id)? {
            insert_job_event(
                conn,
                &workspace_id(config),
                &job.id,
                lineage,
                event_type,
                job.last_status.as_deref(),
                Some(
                    serde_json::json!({
                        "enabled": job.enabled,
                        "expression": job.expression,
                        "kind": job.job_type.as_str(),
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
        }
        Ok(())
    })?;

    get_job(config, job_id)
}

pub fn record_last_run(
    config: &Config,
    job_id: &str,
    finished_at: DateTime<Utc>,
    success: bool,
    output: &str,
) -> Result<()> {
    if let Some(store) = pg_store(config)? {
        return store.record_last_run(&workspace_id(config), job_id, finished_at, success, output);
    }
    let status = if success { "ok" } else { "error" };
    let bounded_output = truncate_cron_output(output);
    with_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = ?2, last_output = ?3
             WHERE id = ?4",
            params![finished_at.to_rfc3339(), status, bounded_output, job_id],
        )
        .context("Failed to update cron last run fields")?;
        if let Some(lineage) = load_job_lineage(conn, job_id)? {
            insert_job_event(
                conn,
                &workspace_id(config),
                job_id,
                lineage,
                "cron.job.last_run_recorded",
                Some(status),
                Some(
                    serde_json::json!({
                        "finished_at": finished_at.to_rfc3339(),
                        "success": success,
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
        }
        Ok(())
    })
}

pub fn reschedule_after_run(config: &Config, job: &CronJob, success: bool, output: &str) -> Result<()> {
    if matches!(job.schedule, Schedule::At { .. }) {
        anyhow::bail!("Schedule::At is terminal after its attempt and cannot be rescheduled");
    }
    if let Some(store) = pg_store(config)? {
        return store.reschedule_after_run(&workspace_id(config), job, success, output);
    }
    let now = Utc::now();
    let next_run = next_run_for_schedule(&job.schedule, now)?;
    let status = if success { "ok" } else { "error" };
    let bounded_output = truncate_cron_output(output);

    with_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET next_run = ?1, last_run = ?2, last_status = ?3, last_output = ?4
             WHERE id = ?5",
            params![next_run.to_rfc3339(), now.to_rfc3339(), status, bounded_output, job.id],
        )
        .context("Failed to update cron job run state")?;
        if let Some(lineage) = load_job_lineage(conn, &job.id)? {
            insert_job_event(
                conn,
                &workspace_id(config),
                &job.id,
                lineage,
                "cron.job.rescheduled",
                Some(status),
                Some(
                    serde_json::json!({
                        "next_run": next_run.to_rfc3339(),
                        "success": success,
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
        }
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
pub fn record_one_shot_terminal_run(
    config: &Config,
    job: &CronJob,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    success: bool,
    output: &str,
    duration_ms: i64,
) -> Result<bool> {
    if !matches!(job.schedule, Schedule::At { .. }) {
        anyhow::bail!("terminal one-shot persistence requires Schedule::At");
    }
    if let Some(store) = pg_store(config)? {
        return store.record_one_shot_terminal_run(
            &workspace_id(config),
            job,
            started_at,
            finished_at,
            success,
            output,
            duration_ms,
            config.cron.max_run_history,
        );
    }

    let status = if success { "ok" } else { "error" };
    let terminal_state = if success {
        CronJobTerminalState::Succeeded
    } else {
        CronJobTerminalState::Failed
    };
    let terminal_event_type = if success {
        "cron.job.completed"
    } else {
        "cron.job.failed"
    };
    let bounded_output = truncate_cron_output(output);
    let keep = i64::from(config.cron.max_run_history.max(1));
    let schedule_json = serde_json::to_string(&job.schedule)?;
    with_connection(config, |conn| {
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                job.id,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
                status,
                bounded_output,
                duration_ms,
            ],
        )
        .context("Failed to insert terminal cron run")?;
        tx.execute(
            "DELETE FROM cron_runs
             WHERE job_id = ?1
               AND id NOT IN (
                 SELECT id FROM cron_runs
                 WHERE job_id = ?1
                 ORDER BY started_at DESC, id DESC
                 LIMIT ?2
               )",
            params![job.id, keep],
        )
        .context("Failed to prune terminal cron run history")?;
        let changed = tx.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = ?2, last_output = ?3, terminal_state = ?4
             WHERE id = ?5 AND terminal_state IS NULL AND next_run = ?6 AND schedule = ?7
               AND last_status = 'running'",
            params![
                finished_at.to_rfc3339(),
                status,
                bounded_output,
                terminal_state.as_str(),
                job.id,
                job.next_run.to_rfc3339(),
                schedule_json,
            ],
        )?;
        if changed == 0 {
            anyhow::bail!("cron one-shot '{}' was deleted or already terminal", job.id);
        }
        if let Some(lineage) = load_job_lineage(&tx, &job.id)? {
            insert_job_event(
                &tx,
                &workspace_id(config),
                &job.id,
                lineage.clone(),
                "cron.job.run_recorded",
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
            insert_job_event(
                &tx,
                &workspace_id(config),
                &job.id,
                lineage,
                terminal_event_type,
                Some(status),
                Some(
                    serde_json::json!({
                        "terminal_state": terminal_state.as_str(),
                        "success": success,
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
        }
        let deleted = tx.execute(
            "DELETE FROM cron_jobs
             WHERE id = ?1 AND terminal_state = 'succeeded' AND delete_after_run = 1",
            params![job.id],
        )? > 0;
        tx.commit().context("Failed to commit terminal cron run")?;
        Ok(deleted)
    })
}

pub fn record_run(
    config: &Config,
    job_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
) -> Result<()> {
    if let Some(store) = pg_store(config)? {
        return store.record_run(
            &workspace_id(config),
            job_id,
            started_at,
            finished_at,
            status,
            output,
            duration_ms,
            config.cron.max_run_history,
        );
    }
    let bounded_output = output.map(truncate_cron_output);
    with_connection(config, |conn| {
        // Wrap INSERT + pruning DELETE in an explicit transaction so that
        // if the DELETE fails, the INSERT is rolled back and the run table
        // cannot grow unboundedly.
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                job_id,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
                status,
                bounded_output.as_deref(),
                duration_ms,
            ],
        )
        .context("Failed to insert cron run")?;
        if let Some(lineage) = load_job_lineage(&tx, job_id)? {
            insert_job_event(
                &tx,
                &workspace_id(config),
                job_id,
                lineage,
                "cron.job.run_recorded",
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

        let keep = i64::from(config.cron.max_run_history.max(1));
        tx.execute(
            "DELETE FROM cron_runs
             WHERE job_id = ?1
               AND id NOT IN (
                 SELECT id FROM cron_runs
                 WHERE job_id = ?1
                 ORDER BY started_at DESC, id DESC
                 LIMIT ?2
               )",
            params![job_id, keep],
        )
        .context("Failed to prune cron run history")?;

        tx.commit().context("Failed to commit cron run transaction")?;
        Ok(())
    })
}

pub(crate) fn truncate_cron_output(output: &str) -> String {
    if output.len() <= MAX_CRON_OUTPUT_BYTES {
        return output.to_string();
    }

    if MAX_CRON_OUTPUT_BYTES <= TRUNCATED_OUTPUT_MARKER.len() {
        return TRUNCATED_OUTPUT_MARKER.to_string();
    }

    let mut cutoff = MAX_CRON_OUTPUT_BYTES - TRUNCATED_OUTPUT_MARKER.len();
    while cutoff > 0 && !output.is_char_boundary(cutoff) {
        cutoff -= 1;
    }

    let mut truncated = output[..cutoff].to_string();
    truncated.push_str(TRUNCATED_OUTPUT_MARKER);
    truncated
}

pub fn list_runs(config: &Config, job_id: &str, limit: usize) -> Result<Vec<CronRun>> {
    if let Some(store) = pg_store(config)? {
        return store.list_runs(job_id, limit);
    }
    with_connection(config, |conn| {
        let lim = i64::try_from(limit.max(1)).context("Run history limit overflow")?;
        let mut stmt = conn.prepare(
            "SELECT id, job_id, started_at, finished_at, status, output, duration_ms
             FROM cron_runs
             WHERE job_id = ?1
             ORDER BY started_at DESC, id DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![job_id, lim], |row| {
            Ok(CronRun {
                id: row.get(0)?,
                job_id: row.get(1)?,
                started_at: parse_rfc3339(&row.get::<_, String>(2)?).map_err(sql_conversion_error)?,
                finished_at: parse_rfc3339(&row.get::<_, String>(3)?).map_err(sql_conversion_error)?,
                status: row.get(4)?,
                output: row.get(5)?,
                duration_ms: row.get(6)?,
            })
        })?;

        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
}

pub fn list_job_events(config: &Config, job_id: &str) -> Result<Vec<CronJobEvent>> {
    if let Some(store) = pg_store(config)? {
        return store.list_job_events(job_id);
    }
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, event_id, job_id, workspace_id, owner_id, topic_id, parent_task_id,
                    source_message_event_id, event_type, status, payload_json, created_at
             FROM cron_job_events
             WHERE job_id = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![job_id], map_job_event_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    })
}

fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed =
        DateTime::parse_from_rfc3339(raw).with_context(|| format!("Invalid RFC3339 timestamp in cron DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

fn sql_conversion_error(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.into())
}

fn map_cron_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob> {
    let expression: String = row.get(5)?;
    let schedule_raw: Option<String> = row.get(7)?;
    let schedule = decode_schedule(schedule_raw.as_deref(), &expression).map_err(sql_conversion_error)?;

    let delivery_raw: Option<String> = row.get(14)?;
    let delivery = decode_delivery(delivery_raw.as_deref()).map_err(sql_conversion_error)?;

    let next_run_raw: String = row.get(17)?;
    let last_run_raw: Option<String> = row.get(18)?;
    let created_at_raw: String = row.get(16)?;

    Ok(CronJob {
        id: row.get(0)?,
        owner_id: row.get(1)?,
        topic_id: row.get(2)?,
        parent_task_id: row.get(3)?,
        source_message_event_id: row.get(4)?,
        expression,
        schedule,
        command: row.get(6)?,
        job_type: JobType::parse(&row.get::<_, String>(8)?),
        prompt: row.get(9)?,
        name: row.get(10)?,
        session_target: SessionTarget::parse(&row.get::<_, String>(11)?),
        model: row.get(12)?,
        enabled: row.get::<_, i64>(13)? != 0,
        delivery,
        delete_after_run: row.get::<_, i64>(15)? != 0,
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_conversion_error)?,
        next_run: parse_rfc3339(&next_run_raw).map_err(sql_conversion_error)?,
        last_run: match last_run_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_conversion_error)?),
            None => None,
        },
        last_status: row.get(19)?,
        last_output: row.get(20)?,
        terminal_state: row
            .get::<_, Option<String>>(21)?
            .map(|raw| CronJobTerminalState::parse(&raw).map_err(sql_conversion_error))
            .transpose()?,
        approval_grant_json: row.get(22)?,
    })
}

#[derive(Debug, Clone)]
struct JobLineage {
    owner_id: Option<String>,
    topic_id: Option<String>,
    parent_task_id: Option<String>,
    source_message_event_id: Option<String>,
    status: Option<String>,
}

fn workspace_id(config: &Config) -> String {
    config.workspace_dir.to_string_lossy().to_string()
}

fn load_job_lineage(conn: &Connection, job_id: &str) -> Result<Option<JobLineage>> {
    let mut stmt = conn.prepare(
        "SELECT owner_id, topic_id, parent_task_id, source_message_event_id, last_status
         FROM cron_jobs
         WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![job_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(JobLineage {
        owner_id: row.get(0)?,
        topic_id: row.get(1)?,
        parent_task_id: row.get(2)?,
        source_message_event_id: row.get(3)?,
        status: row.get(4)?,
    }))
}

fn insert_job_event(
    conn: &Connection,
    workspace_id: &str,
    job_id: &str,
    lineage: JobLineage,
    event_type: &str,
    status: Option<&str>,
    payload_json: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO cron_job_events (
            event_id, job_id, workspace_id, owner_id, topic_id, parent_task_id, source_message_event_id,
            event_type, status, payload_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            Uuid::new_v4().to_string(),
            job_id,
            workspace_id,
            lineage.owner_id.as_deref(),
            lineage.topic_id.as_deref(),
            lineage.parent_task_id.as_deref(),
            lineage.source_message_event_id.as_deref(),
            event_type,
            status,
            payload_json,
            Utc::now().to_rfc3339(),
        ],
    )
    .context("Failed to insert cron job event")?;
    if let Err(error) = mirror_cron_job_event(
        workspace_id,
        job_id,
        MirrorLineage {
            owner_id: lineage.owner_id.as_deref(),
            topic_id: lineage.topic_id.as_deref(),
            parent_task_id: lineage.parent_task_id.as_deref(),
            source_message_event_id: lineage.source_message_event_id.as_deref(),
            status: lineage.status.as_deref(),
        },
        event_type,
        status,
        payload_json,
    ) {
        tracing::warn!(job_id = %job_id, event_type, "failed to mirror cron job event into memory_events: {error}");
    }
    Ok(())
}

/// Lineage fields used to enrich a mirrored cron event. Backend-agnostic
/// (borrowed `&str`s) so both SQLite and Postgres stores can share the mirror.
pub(crate) struct MirrorLineage<'a> {
    pub owner_id: Option<&'a str>,
    pub topic_id: Option<&'a str>,
    pub parent_task_id: Option<&'a str>,
    pub source_message_event_id: Option<&'a str>,
    pub status: Option<&'a str>,
}

/// Mirror a cron lifecycle event into the shared `memory_events` fabric. This is
/// workspace-file-based (writes `brain.db` under `workspace_id`) and therefore
/// backend-agnostic: the Postgres cron store reuses it for parity with SQLite.
pub(crate) fn mirror_cron_job_event(
    workspace_id: &str,
    job_id: &str,
    lineage: MirrorLineage<'_>,
    event_type: &str,
    status: Option<&str>,
    payload_json: Option<&str>,
) -> Result<i64> {
    let mut payload = payload_json
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    payload.insert("owner_id".to_string(), lineage.owner_id.map(str::to_string).into());
    payload.insert("topic_id".to_string(), lineage.topic_id.map(str::to_string).into());
    payload.insert(
        "parent_task_id".to_string(),
        lineage.parent_task_id.map(str::to_string).into(),
    );
    payload.insert(
        "source_message_event_id".to_string(),
        lineage.source_message_event_id.map(str::to_string).into(),
    );
    payload.insert(
        "status".to_string(),
        status.or(lineage.status).map(str::to_string).into(),
    );
    payload.insert("task_id".to_string(), job_id.to_string().into());
    if !payload.contains_key("task") {
        if let Some(name) = payload.get("name").and_then(serde_json::Value::as_str) {
            payload.insert("task".to_string(), name.to_string().into());
        }
    }
    let payload_json = serde_json::Value::Object(payload).to_string();
    crate::memory::sqlite::append_task_event_mirror(
        std::path::Path::new(workspace_id),
        crate::memory::sqlite::SqliteTaskEventMirror {
            workspace_id,
            task_id: job_id,
            event_type,
            session_key: None,
            agent_id: None,
            persona_id: None,
            payload_json: Some(&payload_json),
        },
    )
}

fn map_job_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJobEvent> {
    let created_at_raw: String = row.get(11)?;
    Ok(CronJobEvent {
        id: row.get(0)?,
        event_id: row.get(1)?,
        job_id: row.get(2)?,
        workspace_id: row.get(3)?,
        owner_id: row.get(4)?,
        topic_id: row.get(5)?,
        parent_task_id: row.get(6)?,
        source_message_event_id: row.get(7)?,
        event_type: row.get(8)?,
        status: row.get(9)?,
        payload_json: row.get(10)?,
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_conversion_error)?,
    })
}

pub(crate) fn decode_schedule(schedule_raw: Option<&str>, expression: &str) -> Result<Schedule> {
    if let Some(raw) = schedule_raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse cron schedule JSON: {trimmed}"));
        }
    }

    if expression.trim().is_empty() {
        anyhow::bail!("Missing schedule and legacy expression for cron job")
    }

    Ok(Schedule::Cron {
        expr: expression.to_string(),
        tz: None,
    })
}

pub(crate) fn decode_delivery(delivery_raw: Option<&str>) -> Result<DeliveryConfig> {
    if let Some(raw) = delivery_raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse cron delivery JSON: {trimmed}"));
        }
    }
    Ok(DeliveryConfig::default())
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
    // Drop the statement/rows before executing ALTER to release any locks
    drop(rows);
    drop(stmt);

    // Tolerate "duplicate column name" errors to handle the race where
    // another process adds the column between our PRAGMA check and ALTER.
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
    let db_path = config.workspace_dir.join("cron").join("jobs.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cron directory: {}", parent.display()))?;
    }

    let conn = Connection::open(&db_path).with_context(|| format!("Failed to open cron DB: {}", db_path.display()))?;

    // Avoid SQLITE_BUSY under concurrent scheduler + CLI access
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS cron_jobs (
            id               TEXT PRIMARY KEY,
            owner_id         TEXT,
            topic_id         TEXT,
            parent_task_id   TEXT,
            source_message_event_id TEXT,
            expression       TEXT NOT NULL,
            command          TEXT NOT NULL,
            schedule         TEXT,
            job_type         TEXT NOT NULL DEFAULT 'shell',
            prompt           TEXT,
            name             TEXT,
            session_target   TEXT NOT NULL DEFAULT 'isolated',
            model            TEXT,
            enabled          INTEGER NOT NULL DEFAULT 1,
            delivery         TEXT,
            delete_after_run INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            next_run         TEXT NOT NULL,
            last_run         TEXT,
            last_status      TEXT,
            last_output      TEXT,
            terminal_state   TEXT,
            approval_grant_json TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_cron_jobs_next_run ON cron_jobs(next_run);

        CREATE TABLE IF NOT EXISTS cron_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id      TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            status      TEXT NOT NULL,
            output      TEXT,
            duration_ms INTEGER,
            FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_id ON cron_runs(job_id);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_started_at ON cron_runs(started_at);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_started ON cron_runs(job_id, started_at);

        CREATE TABLE IF NOT EXISTS cron_job_events (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id       TEXT NOT NULL UNIQUE,
            job_id         TEXT NOT NULL,
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
        CREATE INDEX IF NOT EXISTS idx_cron_job_events_job_id ON cron_job_events(job_id, id);
        CREATE INDEX IF NOT EXISTS idx_cron_job_events_owner ON cron_job_events(workspace_id, owner_id, id);
        CREATE INDEX IF NOT EXISTS idx_cron_job_events_topic ON cron_job_events(workspace_id, topic_id, id);
        CREATE INDEX IF NOT EXISTS idx_cron_job_events_type ON cron_job_events(event_type, id);",
    )
    .context("Failed to initialize cron schema")?;

    add_column_if_missing(&conn, "cron_jobs", "owner_id", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "topic_id", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "parent_task_id", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "source_message_event_id", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "schedule", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "job_type", "TEXT NOT NULL DEFAULT 'shell'")?;
    add_column_if_missing(&conn, "cron_jobs", "prompt", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "name", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "session_target", "TEXT NOT NULL DEFAULT 'isolated'")?;
    add_column_if_missing(&conn, "cron_jobs", "model", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "enabled", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(&conn, "cron_jobs", "delivery", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "delete_after_run", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(&conn, "cron_jobs", "last_run", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "last_status", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "last_output", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "terminal_state", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "approval_grant_json", "TEXT")?;
    add_column_if_missing(&conn, "cron_job_events", "source_message_event_id", "TEXT")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_cron_jobs_owner ON cron_jobs(owner_id, enabled, next_run);
         CREATE INDEX IF NOT EXISTS idx_cron_jobs_topic ON cron_jobs(topic_id, enabled, next_run);
         CREATE INDEX IF NOT EXISTS idx_cron_jobs_parent ON cron_jobs(parent_task_id, id);",
    )
    .context("Failed to initialize cron lineage indexes")?;

    conn.execute(
        "UPDATE cron_jobs
         SET terminal_state = CASE WHEN last_status = 'ok' THEN 'succeeded' ELSE 'failed' END,
             enabled = 0
         WHERE terminal_state IS NULL AND last_run IS NOT NULL AND last_status IN ('ok', 'error')
           AND schedule LIKE '%\"kind\":\"at\"%'",
        [],
    )
    .context("Failed to backfill terminal state for historical one-shot jobs")?;

    f(&conn)
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use chrono::Duration as ChronoDuration;
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

    #[test]
    fn add_job_accepts_five_field_expression() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        assert_eq!(job.expression, "*/5 * * * *");
        assert_eq!(job.command, "echo ok");
        assert!(matches!(job.schedule, Schedule::Cron { .. }));
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "*/10 * * * *", "echo roundtrip").unwrap();
        let listed = list_jobs(&config).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, job.id);

        remove_job(&config, &job.id).unwrap();
        assert!(list_jobs(&config).unwrap().is_empty());
    }

    #[test]
    fn add_job_persists_owner_topic_lineage_and_event() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let lineage = CronJobLineage {
            owner_id: Some("owner:workspace:telegram:alice".to_string()),
            topic_id: Some("topic-1".to_string()),
            parent_task_id: Some("task-parent".to_string()),
            source_message_event_id: Some("msg-1".to_string()),
        };

        let job = add_shell_job_with_lineage_and_approval_grant(
            &config,
            Some("lineage-test".to_string()),
            Schedule::Cron {
                expr: "*/5 * * * *".to_string(),
                tz: None,
            },
            "echo lineage",
            None,
            lineage,
        )
        .unwrap();

        assert_eq!(job.owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
        assert_eq!(job.topic_id.as_deref(), Some("topic-1"));
        assert_eq!(job.parent_task_id.as_deref(), Some("task-parent"));
        assert_eq!(job.source_message_event_id.as_deref(), Some("msg-1"));

        let events = list_job_events(&config, &job.id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "cron.job.created");
        assert_eq!(events[0].owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
        assert_eq!(events[0].topic_id.as_deref(), Some("topic-1"));
        assert_eq!(events[0].parent_task_id.as_deref(), Some("task-parent"));
        assert_eq!(events[0].source_message_event_id.as_deref(), Some("msg-1"));

        let memory_conn = Connection::open(config.workspace_dir.join("memory").join("brain.db")).unwrap();
        let (event_type, subject_id, payload): (String, String, String) = memory_conn
            .query_row(
                "SELECT event_type, subject_id, payload_json
                 FROM memory_events
                 WHERE subject_table = 'tasks' AND subject_id = ?1
                 ORDER BY id DESC
                 LIMIT 1",
                params![job.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(event_type, "cron.job.created");
        assert_eq!(subject_id, job.id);
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["owner_id"].as_str(), Some("owner:workspace:telegram:alice"));
        assert_eq!(payload["topic_id"].as_str(), Some("topic-1"));
        assert_eq!(payload["parent_task_id"].as_str(), Some("task-parent"));
        assert_eq!(payload["source_message_event_id"].as_str(), Some("msg-1"));
        assert_eq!(payload["task"].as_str(), Some("lineage-test"));
    }

    #[test]
    fn cron_task_lifecycle_records_owner_scoped_events() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let lineage = CronJobLineage {
            owner_id: Some("owner-a".to_string()),
            topic_id: Some("topic-a".to_string()),
            parent_task_id: Some("task-a".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
        };
        let job = add_shell_job_with_lineage_and_approval_grant(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".to_string(),
                tz: None,
            },
            "echo lifecycle",
            None,
            lineage,
        )
        .unwrap();

        assert!(claim_job(&config, &job.id).unwrap());
        let started = Utc::now();
        record_run(
            &config,
            &job.id,
            started,
            started + ChronoDuration::milliseconds(5),
            "ok",
            Some("done"),
            5,
        )
        .unwrap();
        reschedule_after_run(&config, &job, true, "done").unwrap();
        let _ = update_job(
            &config,
            &job.id,
            CronJobPatch {
                enabled: Some(false),
                ..CronJobPatch::default()
            },
        )
        .unwrap();

        let events = list_job_events(&config, &job.id).unwrap();
        let event_types = events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>();
        assert!(event_types.contains(&"cron.job.created"));
        assert!(event_types.contains(&"cron.job.claimed"));
        assert!(event_types.contains(&"cron.job.run_recorded"));
        assert!(event_types.contains(&"cron.job.rescheduled"));
        assert!(event_types.contains(&"cron.job.disabled"));
        assert!(events.iter().all(|event| event.owner_id.as_deref() == Some("owner-a")));
        assert!(events.iter().all(|event| event.topic_id.as_deref() == Some("topic-a")));
        assert!(
            events
                .iter()
                .all(|event| event.source_message_event_id.as_deref() == Some("msg-a"))
        );
    }

    #[test]
    fn legacy_cron_schema_migrates_lineage_columns_and_events_table() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let db_dir = config.workspace_dir.join("cron");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("jobs.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE cron_jobs (
                id               TEXT PRIMARY KEY,
                expression       TEXT NOT NULL,
                command          TEXT NOT NULL,
                created_at       TEXT NOT NULL,
                next_run         TEXT NOT NULL
            );",
        )
        .unwrap();
        drop(conn);

        let jobs = list_jobs(&config).unwrap();
        assert!(jobs.is_empty());

        with_connection(&config, |conn| {
            let mut stmt = conn.prepare("PRAGMA table_info(cron_jobs)")?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for name in [
                "owner_id",
                "topic_id",
                "parent_task_id",
                "source_message_event_id",
                "terminal_state",
                "approval_grant_json",
            ] {
                assert!(names.iter().any(|existing| existing == name), "missing {name}");
            }
            let event_tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'cron_job_events'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(event_tables, 1);
            let mut event_stmt = conn.prepare("PRAGMA table_info(cron_job_events)")?;
            let event_names = event_stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert!(
                event_names.iter().any(|existing| existing == "source_message_event_id"),
                "missing cron_job_events.source_message_event_id"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn due_jobs_filters_by_timestamp_and_enabled() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "* * * * *", "echo due").unwrap();

        let due_now = due_jobs(&config, Utc::now()).unwrap();
        assert!(due_now.is_empty(), "new job should not be due immediately");

        let far_future = Utc::now() + ChronoDuration::days(365);
        let due_future = due_jobs(&config, far_future).unwrap();
        assert_eq!(due_future.len(), 1, "job should be due in far future");

        let _ = update_job(
            &config,
            &job.id,
            CronJobPatch {
                enabled: Some(false),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        let due_after_disable = due_jobs(&config, far_future).unwrap();
        assert!(due_after_disable.is_empty());
    }

    #[test]
    fn due_jobs_respects_scheduler_max_tasks_limit() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.scheduler.max_tasks = 2;

        let _ = add_job(&config, "* * * * *", "echo due-1").unwrap();
        let _ = add_job(&config, "* * * * *", "echo due-2").unwrap();
        let _ = add_job(&config, "* * * * *", "echo due-3").unwrap();

        let far_future = Utc::now() + ChronoDuration::days(365);
        let due = due_jobs(&config, far_future).unwrap();
        assert_eq!(due.len(), 2);
    }

    #[test]
    fn reschedule_after_run_persists_last_status_and_last_run() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "*/15 * * * *", "echo run").unwrap();
        reschedule_after_run(&config, &job, false, "failed output").unwrap();

        let listed = list_jobs(&config).unwrap();
        let stored = listed.iter().find(|j| j.id == job.id).unwrap();
        assert_eq!(stored.last_status.as_deref(), Some("error"));
        assert!(stored.last_run.is_some());
        assert_eq!(stored.last_output.as_deref(), Some("failed output"));
    }

    #[test]
    fn historical_completed_at_job_is_backfilled_terminal_and_disabled() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let succeeded = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-success").unwrap();
        let failed = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-failure").unwrap();
        let running = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-running").unwrap();
        let unknown = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-unknown").unwrap();
        let null_status = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-null").unwrap();

        with_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_jobs
                 SET last_run = ?1, last_status = 'ok', terminal_state = NULL, enabled = 1
                 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), succeeded.id],
            )?;
            conn.execute(
                "UPDATE cron_jobs
                 SET last_run = ?1, last_status = 'error', terminal_state = NULL, enabled = 1
                 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), failed.id],
            )?;
            conn.execute(
                "UPDATE cron_jobs
                 SET last_run = ?1, last_status = 'running', terminal_state = NULL, enabled = 1
                 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), running.id],
            )?;
            conn.execute(
                "UPDATE cron_jobs
                 SET last_run = ?1, last_status = 'unknown', terminal_state = NULL, enabled = 1
                 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), unknown.id],
            )?;
            conn.execute(
                "UPDATE cron_jobs
                 SET last_run = ?1, last_status = NULL, terminal_state = NULL, enabled = 1
                 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), null_status.id],
            )?;
            Ok(())
        })
        .unwrap();

        let migrated_success = get_job(&config, &succeeded.id).unwrap();
        assert_eq!(migrated_success.terminal_state, Some(CronJobTerminalState::Succeeded));
        assert!(!migrated_success.enabled);
        let migrated_failure = get_job(&config, &failed.id).unwrap();
        assert_eq!(migrated_failure.terminal_state, Some(CronJobTerminalState::Failed));
        assert!(!migrated_failure.enabled);
        let unresolved_running = get_job(&config, &running.id).unwrap();
        assert_eq!(unresolved_running.terminal_state, None);
        assert!(unresolved_running.enabled);
        let unresolved_unknown = get_job(&config, &unknown.id).unwrap();
        assert_eq!(unresolved_unknown.terminal_state, None);
        assert!(unresolved_unknown.enabled);
        let unresolved_null = get_job(&config, &null_status.id).unwrap();
        assert_eq!(unresolved_null.terminal_state, None);
        assert!(unresolved_null.enabled);
    }

    #[test]
    fn schedule_update_rearms_terminal_job_but_enable_alone_does_not() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = add_shell_job(&config, None, Schedule::At { at }, "echo once").unwrap();
        let started = Utc::now();
        assert!(claim_job_if_current_for_manual_run(&config, &job).unwrap());
        record_one_shot_terminal_run(&config, &job, started, started, true, "ok", 0).unwrap();

        let enabled = update_job(
            &config,
            &job.id,
            CronJobPatch {
                enabled: Some(true),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert_eq!(enabled.terminal_state, Some(CronJobTerminalState::Succeeded));
        let disabled = update_job(
            &config,
            &job.id,
            CronJobPatch {
                enabled: Some(false),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert!(!disabled.enabled);

        let rearmed_at = Utc::now() + ChronoDuration::minutes(20);
        let rearmed = update_job(
            &config,
            &job.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: rearmed_at }),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert_eq!(rearmed.terminal_state, None);
        assert_eq!(rearmed.last_run, None);
        assert_eq!(rearmed.last_status, None);
        assert_eq!(rearmed.last_output, None);
        assert_eq!(rearmed.next_run, rearmed_at);
        assert!(rearmed.enabled);
    }

    #[test]
    fn recurring_schedule_update_preserves_run_history() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/15 * * * *", "echo recurring").unwrap();
        reschedule_after_run(&config, &job, false, "previous failure").unwrap();
        let before = get_job(&config, &job.id).unwrap();

        let updated = update_job(
            &config,
            &job.id,
            CronJobPatch {
                schedule: Some(Schedule::Cron {
                    expr: "*/30 * * * *".to_string(),
                    tz: None,
                }),
                ..CronJobPatch::default()
            },
        )
        .unwrap();

        assert_eq!(updated.last_run, before.last_run);
        assert_eq!(updated.last_status, before.last_status);
        assert_eq!(updated.last_output, before.last_output);
        assert_eq!(updated.terminal_state, None);
    }

    #[test]
    fn at_snapshot_fences_claim_update_and_terminal_commit() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let original_at = Utc::now() + ChronoDuration::minutes(10);
        let stale = add_shell_job(&config, None, Schedule::At { at: original_at }, "echo stale").unwrap();
        let rearmed_at = Utc::now() + ChronoDuration::minutes(20);
        let current = update_job(
            &config,
            &stale.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: rearmed_at }),
                ..CronJobPatch::default()
            },
        )
        .unwrap();

        let due_by = rearmed_at + ChronoDuration::seconds(1);
        assert!(!claim_job_if_current(&config, &stale, due_by).unwrap());
        assert!(claim_job_if_current(&config, &current, due_by).unwrap());

        let in_flight_update = update_job(
            &config,
            &current.id,
            CronJobPatch {
                schedule: Some(Schedule::At {
                    at: Utc::now() + ChronoDuration::minutes(30),
                }),
                ..CronJobPatch::default()
            },
        );
        assert!(
            in_flight_update
                .unwrap_err()
                .to_string()
                .contains("in-flight Schedule::At")
        );

        let started = Utc::now();
        record_one_shot_terminal_run(&config, &current, started, started, true, "done", 0).unwrap();
        assert_eq!(list_runs(&config, &current.id, 10).unwrap().len(), 1);
        assert_eq!(
            list_job_events(&config, &current.id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == "cron.job.completed")
                .count(),
            1
        );

        let final_at = Utc::now() + ChronoDuration::minutes(40);
        let rearmed = update_job(
            &config,
            &current.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: final_at }),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert_eq!(rearmed.terminal_state, None);
        assert_eq!(rearmed.next_run, final_at);
        assert_eq!(rearmed.last_run, None);
    }

    #[test]
    fn one_shot_terminal_delete_uses_current_retention_flag_atomically() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = add_shell_job_with_lineage_approval_and_delete(
            &config,
            None,
            Schedule::At { at },
            "echo retained-by-toggle",
            None,
            true,
            CronJobLineage::default(),
        )
        .unwrap();
        assert!(claim_job_if_current_for_manual_run(&config, &job).unwrap());
        let toggled = update_job(
            &config,
            &job.id,
            CronJobPatch {
                delete_after_run: Some(false),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert!(!toggled.delete_after_run);

        let started = Utc::now();
        let deleted = record_one_shot_terminal_run(&config, &job, started, started, true, "done", 0).unwrap();
        assert!(!deleted);
        let retained = get_job(&config, &job.id).unwrap();
        assert_eq!(retained.terminal_state, Some(CronJobTerminalState::Succeeded));
    }

    #[test]
    fn rearm_and_terminal_auto_delete_are_serialized() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let original_at = Utc::now() + ChronoDuration::minutes(10);
        let stale = add_shell_job_with_lineage_approval_and_delete(
            &config,
            None,
            Schedule::At { at: original_at },
            "echo delete-race",
            None,
            true,
            CronJobLineage::default(),
        )
        .unwrap();
        let rearmed_at = Utc::now() + ChronoDuration::minutes(20);
        let current = update_job(
            &config,
            &stale.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: rearmed_at }),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert!(!claim_job_if_current(&config, &stale, rearmed_at + ChronoDuration::seconds(1)).unwrap());

        assert!(claim_job_if_current_for_manual_run(&config, &current).unwrap());
        let started = Utc::now();
        let deleted = record_one_shot_terminal_run(&config, &current, started, started, true, "done", 0).unwrap();
        assert!(deleted);
        assert!(get_job(&config, &current.id).is_err());
        assert!(
            update_job(
                &config,
                &current.id,
                CronJobPatch {
                    schedule: Some(Schedule::At {
                        at: Utc::now() + ChronoDuration::minutes(30),
                    }),
                    ..CronJobPatch::default()
                }
            )
            .is_err()
        );
        assert_eq!(
            list_job_events(&config, &current.id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == "cron.job.completed")
                .count(),
            1
        );
        assert!(list_runs(&config, &current.id, 10).unwrap().is_empty());
    }

    #[test]
    fn migration_falls_back_to_legacy_expression() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        with_connection(&config, |conn| {
            conn.execute(
                "INSERT INTO cron_jobs (id, expression, command, created_at, next_run)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "legacy-id",
                    "*/5 * * * *",
                    "echo legacy",
                    Utc::now().to_rfc3339(),
                    (Utc::now() + ChronoDuration::minutes(5)).to_rfc3339(),
                ],
            )?;
            conn.execute("UPDATE cron_jobs SET schedule = NULL WHERE id = 'legacy-id'", [])?;
            Ok(())
        })
        .unwrap();

        let job = get_job(&config, "legacy-id").unwrap();
        assert!(matches!(job.schedule, Schedule::Cron { .. }));
    }

    #[test]
    fn record_and_prune_runs() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.cron.max_run_history = 2;
        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let base = Utc::now();

        for idx in 0..3 {
            let start = base + ChronoDuration::seconds(idx);
            let end = start + ChronoDuration::milliseconds(100);
            record_run(&config, &job.id, start, end, "ok", Some("done"), 100).unwrap();
        }

        let runs = list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn remove_job_cascades_run_history() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let start = Utc::now();
        record_run(
            &config,
            &job.id,
            start,
            start + ChronoDuration::milliseconds(5),
            "ok",
            Some("ok"),
            5,
        )
        .unwrap();

        remove_job(&config, &job.id).unwrap();
        let runs = list_runs(&config, &job.id, 10).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn record_run_truncates_large_output() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo trunc").unwrap();
        let output = "x".repeat(MAX_CRON_OUTPUT_BYTES + 512);

        record_run(&config, &job.id, Utc::now(), Utc::now(), "ok", Some(&output), 1).unwrap();

        let runs = list_runs(&config, &job.id, 1).unwrap();
        let stored = runs[0].output.as_deref().unwrap_or_default();
        assert!(stored.ends_with(TRUNCATED_OUTPUT_MARKER));
        assert!(stored.len() <= MAX_CRON_OUTPUT_BYTES);
    }

    #[test]
    fn reschedule_after_run_truncates_last_output() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo trunc").unwrap();
        let output = "y".repeat(MAX_CRON_OUTPUT_BYTES + 1024);

        reschedule_after_run(&config, &job, false, &output).unwrap();

        let stored = get_job(&config, &job.id).unwrap();
        let last_output = stored.last_output.as_deref().unwrap_or_default();
        assert!(last_output.ends_with(TRUNCATED_OUTPUT_MARKER));
        assert!(last_output.len() <= MAX_CRON_OUTPUT_BYTES);
    }
}
