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
    CronClaim, CronJob, CronJobEvent, CronJobLineage, CronJobPatch, CronJobTerminalState, CronRun, DeliveryConfig,
    JobType, Schedule, SessionTarget, next_run_for_schedule, schedule_cron_expression, validate_schedule,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

const MAX_CRON_OUTPUT_BYTES: usize = 16 * 1024;
const TRUNCATED_OUTPUT_MARKER: &str = "\n...[truncated]";

pub(crate) fn format_claim_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

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
                    terminal_state, approval_grant_json, claim_owner, attempt_id, claimed_at, claim_expires_at
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
                    terminal_state, approval_grant_json, claim_owner, attempt_id, claimed_at, claim_expires_at
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
#[cfg(test)]
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

pub fn claim_job_if_current(
    config: &Config,
    job: &CronJob,
    worker_id: &str,
    now: DateTime<Utc>,
    lease_duration: ChronoDuration,
) -> Result<Option<CronClaim>> {
    if let Some(store) = pg_store(config)? {
        return store.claim_job_if_current(&workspace_id(config), job, worker_id, now, lease_duration, true);
    }
    claim_sqlite_job_snapshot(config, job, worker_id, now, lease_duration, true)
}

pub fn claim_job_if_current_for_manual_run(
    config: &Config,
    job: &CronJob,
    worker_id: &str,
    now: DateTime<Utc>,
    lease_duration: ChronoDuration,
) -> Result<Option<CronClaim>> {
    if let Some(store) = pg_store(config)? {
        return store.claim_job_if_current(&workspace_id(config), job, worker_id, now, lease_duration, false);
    }
    claim_sqlite_job_snapshot(config, job, worker_id, now, lease_duration, false)
}

pub fn claim_terminal_job_for_manual_rerun(
    config: &Config,
    job: &CronJob,
    worker_id: &str,
    now: DateTime<Utc>,
    lease_duration: ChronoDuration,
) -> Result<Option<CronClaim>> {
    if !matches!(job.schedule, Schedule::At { .. }) || job.terminal_state.is_none() {
        anyhow::bail!("terminal manual rerun claim requires an already terminal Schedule::At job");
    }
    if let Some(store) = pg_store(config)? {
        return store.claim_terminal_job_for_manual_rerun(&workspace_id(config), job, worker_id, now, lease_duration);
    }
    if worker_id.trim().is_empty() || lease_duration <= ChronoDuration::zero() {
        anyhow::bail!("terminal cron claim requires a worker_id and positive lease duration");
    }
    let schedule_json = serde_json::to_string(&job.schedule)?;
    let attempt_id = Uuid::new_v4().to_string();
    let expires_at = now + lease_duration;
    with_connection(config, |conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE cron_jobs SET claim_owner = ?1, attempt_id = ?2, claimed_at = ?3, claim_expires_at = ?4
             WHERE id = ?5 AND terminal_state IS NOT NULL AND next_run = ?6 AND schedule = ?7
               AND ((claim_owner IS NULL AND attempt_id IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL)
                    OR (claim_owner IS NOT NULL AND attempt_id IS NOT NULL AND claimed_at IS NOT NULL
                        AND claim_expires_at IS NOT NULL AND claim_expires_at <= ?3))",
            params![
                worker_id,
                attempt_id,
                format_claim_time(now),
                format_claim_time(expires_at),
                job.id,
                job.next_run.to_rfc3339(),
                schedule_json,
            ],
        )?;
        if changed == 0 {
            tx.rollback()?;
            return Ok(None);
        }
        if let Some(lineage) = load_job_lineage(&tx, &job.id)? {
            let payload = serde_json::json!({
                "worker_id": worker_id,
                "attempt_id": attempt_id,
                "claimed_at": now.to_rfc3339(),
                "expires_at": expires_at.to_rfc3339(),
            })
            .to_string();
            insert_job_event(
                &tx,
                &workspace_id(config),
                &job.id,
                lineage,
                "cron.job.manual_rerun_claimed",
                Some("running"),
                Some(&payload),
            )?;
        }
        tx.commit()?;
        Ok(Some(CronClaim {
            worker_id: worker_id.to_string(),
            attempt_id,
            claimed_at: now,
            expires_at,
        }))
    })
}

fn claim_sqlite_job_snapshot(
    config: &Config,
    job: &CronJob,
    worker_id: &str,
    now: DateTime<Utc>,
    lease_duration: ChronoDuration,
    require_due: bool,
) -> Result<Option<CronClaim>> {
    if worker_id.trim().is_empty() {
        anyhow::bail!("cron claim worker_id must not be empty");
    }
    if lease_duration <= ChronoDuration::zero() {
        anyhow::bail!("cron claim lease duration must be greater than zero");
    }
    let schedule_json = serde_json::to_string(&job.schedule)?;
    with_connection(config, |conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(Option<String>, Option<String>, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT claim_owner, attempt_id, claimed_at, claim_expires_at FROM cron_jobs WHERE id = ?1",
                params![job.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let attempt_id = Uuid::new_v4().to_string();
        let expires_at = now + lease_duration;
        let due_guard = if require_due {
            "AND enabled = 1 AND next_run <= ?8"
        } else {
            ""
        };
        let sql = format!(
            "UPDATE cron_jobs
             SET last_status = 'running', claim_owner = ?1, attempt_id = ?2, claimed_at = ?3, claim_expires_at = ?4
             WHERE id = ?5 AND terminal_state IS NULL AND next_run = ?6
               AND (schedule = ?7 OR (schedule IS NULL AND expression = ?9))
               {due_guard}
               AND (
                    (claim_owner IS NULL AND attempt_id IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL)
                    OR
                    (claim_owner IS NOT NULL AND attempt_id IS NOT NULL AND claimed_at IS NOT NULL
                     AND claim_expires_at IS NOT NULL AND claim_expires_at <= ?3)
               )"
        );
        let changed = if require_due {
            tx.execute(
                &sql,
                params![
                    worker_id,
                    attempt_id,
                    format_claim_time(now),
                    format_claim_time(expires_at),
                    job.id,
                    job.next_run.to_rfc3339(),
                    schedule_json,
                    format_claim_time(now),
                    job.expression
                ],
            )?
        } else {
            tx.execute(
                &sql,
                params![
                    worker_id,
                    attempt_id,
                    format_claim_time(now),
                    format_claim_time(expires_at),
                    job.id,
                    job.next_run.to_rfc3339(),
                    schedule_json,
                    format_claim_time(now),
                    job.expression
                ],
            )?
        };
        if changed == 0 {
            tx.rollback()?;
            return Ok(None);
        }
        if let Some(lineage) = load_job_lineage(&tx, &job.id)? {
            let recovered = matches!(previous.as_ref(), Some((Some(_), Some(_), Some(_), Some(_))));
            let (previous_worker_id, previous_attempt_id, previous_expires_at) =
                previous
                    .as_ref()
                    .map_or((None, None, None), |(worker_id, attempt_id, _, expires_at)| {
                        (worker_id.as_deref(), attempt_id.as_deref(), expires_at.as_deref())
                    });
            let payload = serde_json::json!({
                "worker_id": worker_id,
                "attempt_id": attempt_id,
                "claimed_at": now.to_rfc3339(),
                "expires_at": expires_at.to_rfc3339(),
                "previous_worker_id": previous_worker_id,
                "previous_attempt_id": previous_attempt_id,
                "previous_expires_at": previous_expires_at,
            })
            .to_string();
            insert_job_event(
                &tx,
                &workspace_id(config),
                &job.id,
                lineage,
                if recovered {
                    "cron.job.claim_recovered"
                } else {
                    "cron.job.claimed"
                },
                Some("running"),
                Some(&payload),
            )?;
        }
        tx.commit()?;
        Ok(Some(CronClaim {
            worker_id: worker_id.to_string(),
            attempt_id,
            claimed_at: now,
            expires_at,
        }))
    })
}

pub fn renew_job_claim(
    config: &Config,
    job_id: &str,
    claim: &CronClaim,
    now: DateTime<Utc>,
    lease_duration: ChronoDuration,
) -> Result<Option<CronClaim>> {
    if let Some(store) = pg_store(config)? {
        return store.renew_job_claim(job_id, claim, now, lease_duration);
    }
    if lease_duration <= ChronoDuration::zero() {
        anyhow::bail!("cron claim lease duration must be greater than zero");
    }
    let expires_at = now + lease_duration;
    with_connection(config, |conn| {
        let changed = conn.execute(
            "UPDATE cron_jobs SET claim_expires_at = ?1
             WHERE id = ?2 AND claim_owner = ?3 AND attempt_id = ?4
               AND claimed_at = ?5 AND claim_expires_at = ?6 AND claim_expires_at > ?7",
            params![
                format_claim_time(expires_at),
                job_id,
                claim.worker_id,
                claim.attempt_id,
                format_claim_time(claim.claimed_at),
                format_claim_time(claim.expires_at),
                format_claim_time(now)
            ],
        )?;
        Ok((changed > 0).then(|| CronClaim {
            expires_at,
            ..claim.clone()
        }))
    })
}

pub fn job_claim_is_current(config: &Config, job_id: &str, claim: &CronClaim, now: DateTime<Utc>) -> Result<bool> {
    if let Some(store) = pg_store(config)? {
        return store.job_claim_is_current(job_id, claim, now);
    }
    with_connection(config, |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM cron_jobs
             WHERE id = ?1 AND claim_owner = ?2 AND attempt_id = ?3
               AND claim_expires_at > ?4",
            params![job_id, claim.worker_id, claim.attempt_id, format_claim_time(now),],
            |row| row.get(0),
        )?;
        Ok(count == 1)
    })
}

/// Release a claim only while the caller still owns the exact fenced tuple.
///
/// A manual runner uses this when authorization or budget checks fail after a
/// successful claim. A stale owner cannot clear a replacement claim.
pub fn abandon_job_claim(
    config: &Config,
    job_id: &str,
    claim: &CronClaim,
    previous_last_status: Option<&str>,
    reason: &str,
) -> Result<bool> {
    if let Some(store) = pg_store(config)? {
        return store.abandon_job_claim(&workspace_id(config), job_id, claim, previous_last_status, reason);
    }
    with_connection(config, |conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE cron_jobs
             SET last_status = ?6,
                 claim_owner = NULL, attempt_id = NULL, claimed_at = NULL, claim_expires_at = NULL
             WHERE id = ?1 AND claim_owner = ?2 AND attempt_id = ?3
               AND claimed_at = ?4 AND claim_expires_at = ?5",
            params![
                job_id,
                claim.worker_id,
                claim.attempt_id,
                format_claim_time(claim.claimed_at),
                format_claim_time(claim.expires_at),
                previous_last_status,
            ],
        )?;
        if changed > 0 {
            if let Some(lineage) = load_job_lineage(&tx, job_id)? {
                let payload = serde_json::json!({
                    "worker_id": claim.worker_id,
                    "attempt_id": claim.attempt_id,
                    "claimed_at": claim.claimed_at.to_rfc3339(),
                    "expires_at": claim.expires_at.to_rfc3339(),
                    "reason": reason,
                })
                .to_string();
                insert_job_event(
                    &tx,
                    &workspace_id(config),
                    job_id,
                    lineage,
                    "cron.job.claim_abandoned",
                    Some("abandoned"),
                    Some(&payload),
                )?;
            }
        }
        tx.commit()?;
        Ok(changed > 0)
    })
}

/// Append an audit event when a scheduler detects that its claim is no longer
/// authoritative. This is observability only: claim authority remains defined
/// exclusively by the fenced tuple in `cron_jobs`.
pub fn record_claim_lost(
    config: &Config,
    job_id: &str,
    claim: &CronClaim,
    detected_at: DateTime<Utc>,
    reason: &str,
) -> Result<()> {
    if let Some(store) = pg_store(config)? {
        return store.record_claim_lost(&workspace_id(config), job_id, claim, detected_at, reason);
    }
    with_connection(config, |conn| {
        if let Some(lineage) = load_job_lineage(conn, job_id)? {
            let payload = serde_json::json!({
                "worker_id": claim.worker_id,
                "attempt_id": claim.attempt_id,
                "claimed_at": claim.claimed_at.to_rfc3339(),
                "expires_at": claim.expires_at.to_rfc3339(),
                "detected_at": detected_at.to_rfc3339(),
                "reason": reason,
            })
            .to_string();
            insert_job_event(
                conn,
                &workspace_id(config),
                job_id,
                lineage,
                "cron.job.claim_lost",
                Some("claim_lost"),
                Some(&payload),
            )?;
        }
        Ok(())
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
                    terminal_state, approval_grant_json, claim_owner, attempt_id, claimed_at, claim_expires_at
             FROM cron_jobs
             WHERE enabled = 1 AND terminal_state IS NULL AND next_run <= ?1
               AND (
                    (claim_owner IS NULL AND attempt_id IS NULL
                     AND claimed_at IS NULL AND claim_expires_at IS NULL)
                    OR
                    (claim_owner IS NOT NULL AND attempt_id IS NOT NULL
                     AND claimed_at IS NOT NULL AND claim_expires_at IS NOT NULL
                     AND julianday(claim_expires_at) <= julianday(?1))
               )
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
    update_job_at(config, job_id, patch, Utc::now())
}

pub fn update_job_at(config: &Config, job_id: &str, patch: CronJobPatch, now: DateTime<Utc>) -> Result<CronJob> {
    if let Some(store) = pg_store(config)? {
        return store.update_job_at(&workspace_id(config), job_id, patch, now);
    }
    let mut job = get_job(config, job_id)?;
    let was_enabled = job.enabled;
    let expected_schedule_json = serde_json::to_string(&job.schedule)?;
    let expected_expression = job.expression.clone();
    let expected_next_run = job.next_run.to_rfc3339();
    let expected_last_status = job.last_status.clone();
    let expected_claim = job.claim.clone();
    let approval_grant_json = patch.approval_grant_json.clone();
    let schedule_changed = if let Some(schedule) = patch.schedule {
        validate_schedule(&schedule, now)?;
        if job.claim.as_ref().is_some_and(|claim| claim.expires_at > now) {
            anyhow::bail!("cannot update the schedule of a job with an active claim lease");
        }
        let rearm_terminal_at = job.terminal_state.is_some()
            && matches!(job.schedule, Schedule::At { .. })
            && matches!(schedule, Schedule::At { .. });
        if job.terminal_state.is_some() && matches!(job.schedule, Schedule::At { .. }) && !rearm_terminal_at {
            anyhow::bail!("a terminal Schedule::At job can only be re-armed with a new future Schedule::At");
        }
        job.schedule = schedule;
        job.expression = schedule_cron_expression(&job.schedule).unwrap_or_default();
        job.claim = None;
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
        job.next_run = next_run_for_schedule(&job.schedule, now)?;
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
                 terminal_state = ?16, approval_grant_json = ?17,
                 claim_owner = ?18, attempt_id = ?19, claimed_at = ?20, claim_expires_at = ?21
             WHERE id = ?22 AND next_run = ?23
               AND (schedule = ?24 OR (schedule IS NULL AND expression = ?25))
               AND last_status IS ?26
               AND claim_owner IS ?27 AND attempt_id IS ?28 AND claimed_at IS ?29 AND claim_expires_at IS ?30",
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
                    job.claim.as_ref().map(|claim| claim.worker_id.as_str()),
                    job.claim.as_ref().map(|claim| claim.attempt_id.as_str()),
                    job.claim.as_ref().map(|claim| format_claim_time(claim.claimed_at)),
                    job.claim.as_ref().map(|claim| format_claim_time(claim.expires_at)),
                    job.id,
                    expected_next_run,
                    expected_schedule_json,
                    expected_expression,
                    expected_last_status,
                    expected_claim.as_ref().map(|claim| claim.worker_id.as_str()),
                    expected_claim.as_ref().map(|claim| claim.attempt_id.as_str()),
                    expected_claim.as_ref().map(|claim| format_claim_time(claim.claimed_at)),
                    expected_claim.as_ref().map(|claim| format_claim_time(claim.expires_at)),
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

#[cfg(test)]
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
pub fn finish_claimed_run(
    config: &Config,
    job: &CronJob,
    claim: &CronClaim,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    commit_now: DateTime<Utc>,
    success: bool,
    output: &str,
    duration_ms: i64,
    disable_after: bool,
) -> Result<bool> {
    finish_claimed_run_with_schedule(
        config,
        job,
        claim,
        started_at,
        finished_at,
        commit_now,
        success,
        output,
        duration_ms,
        disable_after,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_claimed_run_preserving_schedule(
    config: &Config,
    job: &CronJob,
    claim: &CronClaim,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    commit_now: DateTime<Utc>,
    success: bool,
    output: &str,
    duration_ms: i64,
    disable_after: bool,
) -> Result<bool> {
    finish_claimed_run_with_schedule(
        config,
        job,
        claim,
        started_at,
        finished_at,
        commit_now,
        success,
        output,
        duration_ms,
        disable_after,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_claimed_run_with_schedule(
    config: &Config,
    job: &CronJob,
    claim: &CronClaim,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    commit_now: DateTime<Utc>,
    success: bool,
    output: &str,
    duration_ms: i64,
    disable_after: bool,
    advance_schedule: bool,
) -> Result<bool> {
    if !matches!(job.schedule, Schedule::At { .. }) {
        if let Some(store) = pg_store(config)? {
            return store.finish_claimed_run(
                &workspace_id(config),
                job,
                claim,
                started_at,
                finished_at,
                commit_now,
                success,
                output,
                duration_ms,
                disable_after,
                advance_schedule,
                config.cron.max_run_history,
            );
        }
        return finish_sqlite_recurring_claimed_run(
            config,
            job,
            claim,
            started_at,
            finished_at,
            commit_now,
            success,
            output,
            duration_ms,
            disable_after,
            advance_schedule,
        );
    }
    if let Some(store) = pg_store(config)? {
        return store.record_one_shot_terminal_run(
            &workspace_id(config),
            job,
            claim,
            started_at,
            finished_at,
            commit_now,
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
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authoritative_now = Utc::now();
        let changed = tx.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = ?2, last_output = ?3, terminal_state = ?4,
                 claim_owner = NULL, attempt_id = NULL, claimed_at = NULL, claim_expires_at = NULL
             WHERE id = ?5 AND terminal_state IS NULL AND next_run = ?6 AND schedule = ?7
               AND claim_owner = ?8 AND attempt_id = ?9 AND claimed_at = ?10
               AND claim_expires_at = ?11 AND claim_expires_at > ?12",
            params![
                finished_at.to_rfc3339(),
                status,
                bounded_output,
                terminal_state.as_str(),
                job.id,
                job.next_run.to_rfc3339(),
                schedule_json,
                claim.worker_id,
                claim.attempt_id,
                format_claim_time(claim.claimed_at),
                format_claim_time(claim.expires_at),
                format_claim_time(authoritative_now),
            ],
        )?;
        if changed == 0 {
            anyhow::bail!(
                "cron one-shot '{}' claim was lost, expired, or already terminal",
                job.id
            );
        }
        tx.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms, attempt_id, worker_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![job.id, started_at.to_rfc3339(), finished_at.to_rfc3339(), status, bounded_output,
                duration_ms, claim.attempt_id, claim.worker_id],
        )
        .context("Failed to insert terminal cron run")?;
        tx.execute(
            "DELETE FROM cron_runs WHERE job_id = ?1 AND id NOT IN
             (SELECT id FROM cron_runs WHERE job_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2)",
            params![job.id, keep],
        )?;
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
                        "attempt_id": claim.attempt_id,
                        "worker_id": claim.worker_id,
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
                        "attempt_id": claim.attempt_id,
                        "worker_id": claim.worker_id,
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

#[allow(clippy::too_many_arguments)]
fn finish_sqlite_recurring_claimed_run(
    config: &Config,
    job: &CronJob,
    claim: &CronClaim,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    _commit_now: DateTime<Utc>,
    success: bool,
    output: &str,
    duration_ms: i64,
    disable_after: bool,
    advance_schedule: bool,
) -> Result<bool> {
    let status = if success { "ok" } else { "error" };
    let bounded_output = truncate_cron_output(output);
    let next_run = if advance_schedule {
        next_run_for_schedule(&job.schedule, finished_at)?
    } else {
        job.next_run
    };
    let schedule_json = serde_json::to_string(&job.schedule)?;
    let keep = i64::from(config.cron.max_run_history.max(1));
    with_connection(config, |conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authoritative_now = Utc::now();
        let changed = tx.execute(
            "UPDATE cron_jobs SET last_run = ?1, last_status = ?2, last_output = ?3,
                 next_run = ?4, enabled = CASE WHEN ?5 = 1 THEN 0 ELSE enabled END,
                 claim_owner = NULL, attempt_id = NULL, claimed_at = NULL, claim_expires_at = NULL
             WHERE id = ?6 AND terminal_state IS NULL AND next_run = ?7 AND schedule = ?8
               AND claim_owner = ?9 AND attempt_id = ?10 AND claimed_at = ?11
               AND claim_expires_at = ?12 AND claim_expires_at > ?13",
            params![
                finished_at.to_rfc3339(),
                status,
                bounded_output,
                next_run.to_rfc3339(),
                i64::from(disable_after),
                job.id,
                job.next_run.to_rfc3339(),
                schedule_json,
                claim.worker_id,
                claim.attempt_id,
                format_claim_time(claim.claimed_at),
                format_claim_time(claim.expires_at),
                format_claim_time(authoritative_now)
            ],
        )?;
        if changed == 0 {
            anyhow::bail!("cron job '{}' claim was lost or expired", job.id);
        }
        tx.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms, attempt_id, worker_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![job.id, started_at.to_rfc3339(), finished_at.to_rfc3339(), status, bounded_output,
                duration_ms, claim.attempt_id, claim.worker_id],
        )?;
        tx.execute(
            "DELETE FROM cron_runs WHERE job_id = ?1 AND id NOT IN
             (SELECT id FROM cron_runs WHERE job_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2)",
            params![job.id, keep],
        )?;
        if let Some(lineage) = load_job_lineage(&tx, &job.id)? {
            let payload = serde_json::json!({"started_at": started_at.to_rfc3339(),
                "finished_at": finished_at.to_rfc3339(), "duration_ms": duration_ms,
                "attempt_id": claim.attempt_id, "worker_id": claim.worker_id})
            .to_string();
            insert_job_event(
                &tx,
                &workspace_id(config),
                &job.id,
                lineage.clone(),
                "cron.job.run_recorded",
                Some(status),
                Some(&payload),
            )?;
            let finish_payload = serde_json::json!({"next_run": next_run.to_rfc3339(),
                "success": success, "attempt_id": claim.attempt_id, "worker_id": claim.worker_id})
            .to_string();
            insert_job_event(
                &tx,
                &workspace_id(config),
                &job.id,
                lineage,
                if disable_after {
                    "cron.job.disabled"
                } else {
                    "cron.job.rescheduled"
                },
                Some(status),
                Some(&finish_payload),
            )?;
        }
        tx.commit()?;
        Ok(false)
    })
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn record_one_shot_terminal_run(
    config: &Config,
    job: &CronJob,
    claim: &CronClaim,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    success: bool,
    output: &str,
    duration_ms: i64,
) -> Result<bool> {
    if !matches!(job.schedule, Schedule::At { .. }) {
        anyhow::bail!("terminal one-shot persistence requires Schedule::At");
    }
    finish_claimed_run(
        config,
        job,
        claim,
        started_at,
        finished_at,
        finished_at,
        success,
        output,
        duration_ms,
        false,
    )
}

/// Atomically append the audit result of an explicit rerun of an already
/// terminal one-shot. This path never changes terminal state or claim fields.
#[allow(clippy::too_many_arguments)]
pub fn record_terminal_manual_run(
    config: &Config,
    job: &CronJob,
    claim: &CronClaim,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    success: bool,
    output: &str,
    duration_ms: i64,
) -> Result<()> {
    if !matches!(job.schedule, Schedule::At { .. }) || job.terminal_state.is_none() {
        anyhow::bail!("manual terminal rerun requires an already terminal Schedule::At job");
    }
    if let Some(store) = pg_store(config)? {
        return store.record_terminal_manual_run(
            &workspace_id(config),
            job,
            claim,
            started_at,
            finished_at,
            success,
            output,
            duration_ms,
            config.cron.max_run_history,
        );
    }
    let status = if success { "ok" } else { "error" };
    let bounded_output = truncate_cron_output(output);
    let keep = i64::from(config.cron.max_run_history.max(1));
    let schedule_json = serde_json::to_string(&job.schedule)?;
    with_connection(config, |conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authoritative_now = Utc::now();
        let changed = tx.execute(
            "UPDATE cron_jobs SET last_run = ?1, last_status = ?2, last_output = ?3,
                 claim_owner = NULL, attempt_id = NULL, claimed_at = NULL, claim_expires_at = NULL
             WHERE id = ?4 AND terminal_state IS NOT NULL AND next_run = ?5 AND schedule = ?6
               AND claim_owner = ?7 AND attempt_id = ?8 AND claimed_at = ?9 AND claim_expires_at = ?10
               AND claim_expires_at > ?11",
            params![
                finished_at.to_rfc3339(),
                status,
                bounded_output,
                job.id,
                job.next_run.to_rfc3339(),
                schedule_json,
                claim.worker_id,
                claim.attempt_id,
                format_claim_time(claim.claimed_at),
                format_claim_time(claim.expires_at),
                format_claim_time(authoritative_now),
            ],
        )?;
        if changed == 0 {
            anyhow::bail!("terminal cron job '{}' changed before manual rerun audit", job.id);
        }
        tx.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms, attempt_id, worker_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                job.id,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
                status,
                bounded_output,
                duration_ms,
                claim.attempt_id,
                claim.worker_id,
            ],
        )?;
        tx.execute(
            "DELETE FROM cron_runs WHERE job_id = ?1 AND id NOT IN
             (SELECT id FROM cron_runs WHERE job_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2)",
            params![job.id, keep],
        )?;
        if let Some(lineage) = load_job_lineage(&tx, &job.id)? {
            let payload = serde_json::json!({
                "started_at": started_at.to_rfc3339(),
                "finished_at": finished_at.to_rfc3339(),
                "duration_ms": duration_ms,
                "success": success,
                "attempt_id": claim.attempt_id,
                "worker_id": claim.worker_id,
            })
            .to_string();
            insert_job_event(
                &tx,
                &workspace_id(config),
                &job.id,
                lineage,
                "cron.job.manual_rerun",
                Some(status),
                Some(&payload),
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

#[cfg(test)]
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
            "SELECT id, job_id, started_at, finished_at, status, output, duration_ms, attempt_id, worker_id
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
                attempt_id: row.get(7)?,
                worker_id: row.get(8)?,
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
        claim: decode_sqlite_claim(row, 23)?,
    })
}

fn decode_sqlite_claim(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Option<CronClaim>> {
    let owner: Option<String> = row.get(start)?;
    let attempt: Option<String> = row.get(start + 1)?;
    let claimed: Option<String> = row.get(start + 2)?;
    let expires: Option<String> = row.get(start + 3)?;
    match (owner, attempt, claimed, expires) {
        (None, None, None, None) => Ok(None),
        (Some(worker_id), Some(attempt_id), Some(claimed_at), Some(expires_at)) => Ok(Some(CronClaim {
            worker_id,
            attempt_id,
            claimed_at: parse_rfc3339(&claimed_at).map_err(sql_conversion_error)?,
            expires_at: parse_rfc3339(&expires_at).map_err(sql_conversion_error)?,
        })),
        _ => Err(sql_conversion_error(anyhow::anyhow!("partial cron claim tuple"))),
    }
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
    let event_id = Uuid::new_v4().to_string();
    let mirrored_payload = cron_job_event_payload(
        job_id,
        MirrorLineage {
            owner_id: lineage.owner_id.as_deref(),
            topic_id: lineage.topic_id.as_deref(),
            parent_task_id: lineage.parent_task_id.as_deref(),
            source_message_event_id: lineage.source_message_event_id.as_deref(),
            status: lineage.status.as_deref(),
        },
        status,
        payload_json,
    );
    conn.execute_batch("SAVEPOINT cron_event_with_outbox")?;
    let result = (|| -> Result<()> {
        conn.execute(
            "INSERT INTO cron_job_events (
                event_id, job_id, workspace_id, owner_id, topic_id, parent_task_id, source_message_event_id,
                event_type, status, payload_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                event_id,
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
        conn.execute(
            "INSERT INTO cron_event_outbox (
                event_id, workspace_id, job_id, event_type, payload_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event_id,
                workspace_id,
                job_id,
                event_type,
                mirrored_payload,
                Utc::now().to_rfc3339(),
            ],
        )
        .context("Failed to insert cron event outbox row")?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("RELEASE SAVEPOINT cron_event_with_outbox")?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO SAVEPOINT cron_event_with_outbox;
                 RELEASE SAVEPOINT cron_event_with_outbox;",
            );
            Err(error)
        }
    }
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

/// Mirror a SQLite cron lifecycle event into the colocated SQLite
/// `memory_events` fabric. The Postgres store writes its configured Postgres
/// event table transactionally and only reuses the payload builder below.
fn deliver_pending_cron_event_outbox(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT event_id, workspace_id, job_id, event_type, payload_json
         FROM cron_event_outbox
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
    for (event_id, workspace_id, job_id, event_type, payload_json) in pending {
        let result = crate::memory::sqlite::append_task_event_mirror_idempotent(
            std::path::Path::new(&workspace_id),
            &event_id,
            crate::memory::sqlite::SqliteTaskEventMirror {
                workspace_id: &workspace_id,
                task_id: &job_id,
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
                    "UPDATE cron_event_outbox
                     SET delivered_at = ?1, attempt_count = attempt_count + 1, last_error = NULL
                     WHERE event_id = ?2 AND delivered_at IS NULL",
                    params![Utc::now().to_rfc3339(), event_id],
                )?;
                delivered += 1;
            }
            Err(error) => {
                conn.execute(
                    "UPDATE cron_event_outbox
                     SET attempt_count = attempt_count + 1, last_error = ?1
                     WHERE event_id = ?2 AND delivered_at IS NULL",
                    params![truncate_cron_output(&error.to_string()), event_id],
                )?;
                tracing::warn!(job_id, event_type, "failed to deliver Cron event outbox row: {error}");
            }
        }
    }
    Ok(delivered)
}

pub(crate) fn cron_job_event_payload(
    job_id: &str,
    lineage: MirrorLineage<'_>,
    status: Option<&str>,
    payload_json: Option<&str>,
) -> String {
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
    serde_json::Value::Object(payload).to_string()
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

fn with_connection<T>(config: &Config, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
    let db_path = config.workspace_dir.join("cron").join("jobs.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cron directory: {}", parent.display()))?;
    }

    let mut conn =
        Connection::open(&db_path).with_context(|| format!("Failed to open cron DB: {}", db_path.display()))?;

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
            approval_grant_json TEXT,
            claim_owner      TEXT,
            attempt_id       TEXT,
            claimed_at       TEXT,
            claim_expires_at TEXT
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
            attempt_id TEXT,
            worker_id TEXT,
            FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_id ON cron_runs(job_id);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_started_at ON cron_runs(started_at);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_started ON cron_runs(job_id, started_at);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_cron_runs_job_attempt
            ON cron_runs(job_id, attempt_id) WHERE attempt_id IS NOT NULL;

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
        CREATE INDEX IF NOT EXISTS idx_cron_job_events_type ON cron_job_events(event_type, id);

        CREATE TABLE IF NOT EXISTS cron_event_outbox (
            event_id      TEXT PRIMARY KEY,
            workspace_id  TEXT NOT NULL,
            job_id        TEXT NOT NULL,
            event_type    TEXT NOT NULL,
            payload_json  TEXT NOT NULL,
            created_at    TEXT NOT NULL,
            delivered_at  TEXT,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error    TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_cron_event_outbox_pending
            ON cron_event_outbox(delivered_at, created_at);",
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
    add_column_if_missing(&conn, "cron_jobs", "claim_owner", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "attempt_id", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "claimed_at", "TEXT")?;
    add_column_if_missing(&conn, "cron_jobs", "claim_expires_at", "TEXT")?;
    add_column_if_missing(&conn, "cron_runs", "attempt_id", "TEXT")?;
    add_column_if_missing(&conn, "cron_runs", "worker_id", "TEXT")?;
    add_column_if_missing(&conn, "cron_job_events", "source_message_event_id", "TEXT")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_cron_jobs_owner ON cron_jobs(owner_id, enabled, next_run);
         CREATE INDEX IF NOT EXISTS idx_cron_jobs_topic ON cron_jobs(topic_id, enabled, next_run);
         CREATE INDEX IF NOT EXISTS idx_cron_jobs_parent ON cron_jobs(parent_task_id, id);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_cron_runs_job_attempt
            ON cron_runs(job_id, attempt_id) WHERE attempt_id IS NOT NULL;",
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

    let result = f(&mut conn);
    if let Err(error) = deliver_pending_cron_event_outbox(&conn) {
        tracing::warn!("failed to drain Cron event outbox: {error}");
    }
    result
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use chrono::Duration as ChronoDuration;
    use tempfile::TempDir;

    fn claim_due(config: &Config, job: &CronJob, worker: &str, now: DateTime<Utc>) -> Option<CronClaim> {
        claim_job_if_current(config, job, worker, now, ChronoDuration::seconds(30)).unwrap()
    }

    fn claim_manual(config: &Config, job: &CronJob, now: DateTime<Utc>) -> CronClaim {
        claim_job_if_current_for_manual_run(config, job, "test-manual", now, ChronoDuration::seconds(90))
            .unwrap()
            .unwrap()
    }

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
    fn cron_event_outbox_retries_memory_mirror_idempotently() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let memory_dir = config.workspace_dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::create_dir(memory_dir.join("brain.db")).unwrap();

        let job = add_job(&config, "*/5 * * * *", "echo retry").unwrap();
        let cron_db = config.workspace_dir.join("cron").join("jobs.db");
        let cron_conn = Connection::open(&cron_db).unwrap();
        let (delivered_at, attempt_count): (Option<String>, i64) = cron_conn
            .query_row(
                "SELECT delivered_at, attempt_count
                 FROM cron_event_outbox
                 WHERE job_id = ?1",
                params![job.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(delivered_at.is_none());
        assert!(attempt_count > 0);
        drop(cron_conn);

        std::fs::remove_dir(memory_dir.join("brain.db")).unwrap();
        assert_eq!(list_jobs(&config).unwrap().len(), 1);

        let cron_conn = Connection::open(&cron_db).unwrap();
        let delivered_at: Option<String> = cron_conn
            .query_row(
                "SELECT delivered_at FROM cron_event_outbox WHERE job_id = ?1",
                params![job.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(delivered_at.is_some());
        let memory_conn = Connection::open(memory_dir.join("brain.db")).unwrap();
        let mirror_count: i64 = memory_conn
            .query_row(
                "SELECT COUNT(*) FROM memory_events
                 WHERE subject_table = 'tasks' AND subject_id = ?1 AND event_type = 'cron.job.created'",
                params![job.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mirror_count, 1);

        assert_eq!(list_jobs(&config).unwrap().len(), 1);
        let mirror_count_after_replay: i64 = memory_conn
            .query_row(
                "SELECT COUNT(*) FROM memory_events
                 WHERE subject_table = 'tasks' AND subject_id = ?1 AND event_type = 'cron.job.created'",
                params![job.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mirror_count_after_replay, 1);
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
    fn due_jobs_limit_skips_unexpired_active_claims_without_starving_ready_work() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.scheduler.max_tasks = 1;
        let partial = add_job(&config, "*/5 * * * *", "echo partial").unwrap();
        let claimed = add_job(&config, "*/5 * * * *", "echo claimed").unwrap();
        let ready = add_job(&config, "*/5 * * * *", "echo ready").unwrap();
        let now = Utc::now();
        with_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_jobs SET next_run = CASE id
                     WHEN ?1 THEN ?2 WHEN ?3 THEN ?4 ELSE ?5 END
                 WHERE id IN (?1, ?3, ?6)",
                params![
                    partial.id,
                    (now - ChronoDuration::seconds(3)).to_rfc3339(),
                    claimed.id,
                    (now - ChronoDuration::seconds(2)).to_rfc3339(),
                    (now - ChronoDuration::seconds(1)).to_rfc3339(),
                    ready.id,
                ],
            )?;
            conn.execute(
                "UPDATE cron_jobs SET claim_owner = 'partial-only' WHERE id = ?1",
                params![partial.id],
            )?;
            Ok(())
        })
        .unwrap();
        let claimed = get_job(&config, &claimed.id).unwrap();
        claim_job_if_current(&config, &claimed, "worker-busy", now, ChronoDuration::seconds(30))
            .unwrap()
            .unwrap();

        let due = due_jobs(&config, now).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, ready.id);
    }

    #[test]
    fn abandon_claim_is_exactly_fenced_and_audited_only_on_success() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo abandon").unwrap();
        let historical_run = Utc::now() - ChronoDuration::minutes(1);
        with_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_jobs SET last_run = ?1, last_status = 'ok', last_output = 'historical output'
                 WHERE id = ?2",
                params![historical_run.to_rfc3339(), job.id],
            )?;
            Ok(())
        })
        .unwrap();
        let before_claim = get_job(&config, &job.id).unwrap();
        let claim = claim_job_if_current_for_manual_run(
            &config,
            &before_claim,
            "abandon-owner",
            Utc::now(),
            ChronoDuration::seconds(90),
        )
        .unwrap()
        .unwrap();
        let stale = CronClaim {
            attempt_id: "stale-attempt".to_string(),
            ..claim.clone()
        };

        assert!(!abandon_job_claim(&config, &job.id, &stale, Some("ok"), "stale release").unwrap());
        let after_stale = get_job(&config, &job.id).unwrap();
        assert!(after_stale.claim.is_some());
        assert_eq!(after_stale.last_status.as_deref(), Some("running"));
        assert_eq!(after_stale.last_run, before_claim.last_run);
        assert_eq!(after_stale.last_output, before_claim.last_output);
        assert_eq!(
            list_job_events(&config, &job.id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == "cron.job.claim_abandoned")
                .count(),
            0
        );

        assert!(
            abandon_job_claim(
                &config,
                &job.id,
                &claim,
                before_claim.last_status.as_deref(),
                "authorization rejected",
            )
            .unwrap()
        );
        let restored = get_job(&config, &job.id).unwrap();
        assert!(restored.claim.is_none());
        assert_eq!(restored.last_run, before_claim.last_run);
        assert_eq!(restored.last_status, before_claim.last_status);
        assert_eq!(restored.last_output, before_claim.last_output);
        assert_eq!(
            list_job_events(&config, &job.id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == "cron.job.claim_abandoned")
                .count(),
            1
        );
    }

    #[test]
    fn sqlite_finish_rechecks_expiry_after_waiting_for_write_lock() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo lock-wait").unwrap();
        let claimed_at = Utc::now();
        let claim = claim_job_if_current_for_manual_run(
            &config,
            &job,
            "worker-lock-wait",
            claimed_at,
            ChronoDuration::milliseconds(400),
        )
        .unwrap()
        .unwrap();
        with_connection(&config, |blocker| {
            blocker.busy_timeout(std::time::Duration::from_secs(5))?;
            let blocker_tx = blocker.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let worker_config = config.clone();
            let worker_job = job.clone();
            let worker_claim = claim;
            let handle = std::thread::spawn(move || {
                finish_claimed_run_preserving_schedule(
                    &worker_config,
                    &worker_job,
                    &worker_claim,
                    claimed_at,
                    claimed_at + ChronoDuration::milliseconds(10),
                    claimed_at + ChronoDuration::milliseconds(20),
                    true,
                    "must be fenced after lock wait",
                    10,
                    false,
                )
            });

            std::thread::sleep(std::time::Duration::from_millis(650));
            blocker_tx.commit()?;
            let result = handle.join().unwrap();
            assert!(result.is_err(), "lock-acquired authoritative time must observe expiry");
            Ok(())
        })
        .unwrap();
        assert!(list_runs(&config, &job.id, 10).unwrap().is_empty());
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
        let claim = claim_manual(&config, &job, started);
        record_one_shot_terminal_run(&config, &job, &claim, started, started, true, "ok", 0).unwrap();

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
        assert!(claim_due(&config, &stale, "worker-a", due_by).is_none());
        let claim = claim_due(&config, &current, "worker-a", due_by).unwrap();

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
        assert!(in_flight_update.unwrap_err().to_string().contains("active claim lease"));

        let started = Utc::now();
        record_one_shot_terminal_run(&config, &current, &claim, started, started, true, "done", 0).unwrap();
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
        let claim = claim_manual(&config, &job, Utc::now());
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
        let deleted = record_one_shot_terminal_run(&config, &job, &claim, started, started, true, "done", 0).unwrap();
        assert!(!deleted);
        let retained = get_job(&config, &job.id).unwrap();
        assert_eq!(retained.terminal_state, Some(CronJobTerminalState::Succeeded));
    }

    #[test]
    fn terminal_manual_rerun_requires_a_dedicated_claim_before_execution() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = add_shell_job_with_lineage_approval_and_delete(
            &config,
            None,
            Schedule::At { at },
            "echo terminal-rerun",
            None,
            false,
            CronJobLineage::default(),
        )
        .unwrap();
        let now = Utc::now();
        let claim = claim_manual(&config, &job, now);
        record_one_shot_terminal_run(&config, &job, &claim, now, now, true, "done", 0).unwrap();
        let terminal = get_job(&config, &job.id).unwrap();

        let rerun_claim = claim_terminal_job_for_manual_rerun(
            &config,
            &terminal,
            "manual-rerun",
            now + ChronoDuration::seconds(1),
            ChronoDuration::seconds(30),
        )
        .unwrap();

        let rerun_claim = rerun_claim.expect("terminal rerun must be claimed before execution");
        let rearm_at = at + ChronoDuration::minutes(10);
        let rearm_while_claimed = update_job_at(
            &config,
            &terminal.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: rearm_at }),
                ..CronJobPatch::default()
            },
            rerun_claim.claimed_at,
        );
        assert!(
            rearm_while_claimed
                .unwrap_err()
                .to_string()
                .contains("active claim lease")
        );

        assert!(
            abandon_job_claim(
                &config,
                &terminal.id,
                &rerun_claim,
                terminal.last_status.as_deref(),
                "terminal authorization rejected",
            )
            .unwrap()
        );
        let terminal_after_abandon = get_job(&config, &terminal.id).unwrap();
        assert_eq!(terminal_after_abandon.terminal_state, terminal.terminal_state);
        assert_eq!(terminal_after_abandon.last_run, terminal.last_run);
        assert_eq!(terminal_after_abandon.last_status, terminal.last_status);
        assert_eq!(terminal_after_abandon.last_output, terminal.last_output);
        let rerun_claim = claim_terminal_job_for_manual_rerun(
            &config,
            &terminal_after_abandon,
            "manual-rerun-reclaimed",
            now + ChronoDuration::seconds(2),
            ChronoDuration::seconds(30),
        )
        .unwrap()
        .expect("abandoned terminal rerun must be immediately reclaimable");

        record_terminal_manual_run(
            &config,
            &terminal,
            &rerun_claim,
            rerun_claim.claimed_at,
            rerun_claim.claimed_at,
            true,
            "rerun done",
            0,
        )
        .unwrap();
        let rearmed = update_job_at(
            &config,
            &terminal.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: rearm_at }),
                ..CronJobPatch::default()
            },
            rerun_claim.claimed_at,
        )
        .unwrap();
        assert!(rearmed.claim.is_none());
        assert!(rearmed.terminal_state.is_none());
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
        assert!(claim_due(&config, &stale, "worker-a", rearmed_at + ChronoDuration::seconds(1)).is_none());

        let started = Utc::now();
        let claim = claim_manual(&config, &current, started);
        let deleted =
            record_one_shot_terminal_run(&config, &current, &claim, started, started, true, "done", 0).unwrap();
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
    fn stale_claim_can_be_recovered_after_expiry() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = add_shell_job(&config, None, Schedule::At { at }, "echo lease").unwrap();
        let first_tick = at + ChronoDuration::seconds(1);
        let first = claim_due(&config, &job, "worker-a", first_tick).unwrap();

        let before_expiry = first_tick + ChronoDuration::seconds(29);
        assert!(claim_due(&config, &job, "worker-b", before_expiry).is_none());
        let after_expiry = first.expires_at;
        let recovered = claim_due(&config, &job, "worker-b", after_expiry)
            .expect("a stale claim must be recoverable at its exact expiry");
        assert_ne!(first.attempt_id, recovered.attempt_id);
        let listed = get_job(&config, &job.id).unwrap();
        assert_eq!(listed.claim.as_ref(), Some(&recovered));
        let recovery_event = list_job_events(&config, &job.id)
            .unwrap()
            .into_iter()
            .find(|event| event.event_type == "cron.job.claim_recovered")
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(recovery_event.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload["worker_id"], "worker-b");
        assert_eq!(payload["attempt_id"], recovered.attempt_id);
        assert_eq!(payload["expires_at"], recovered.expires_at.to_rfc3339());
        assert_eq!(payload["previous_worker_id"], "worker-a");
        assert_eq!(payload["previous_attempt_id"], first.attempt_id);
        let previous_expires_at = DateTime::parse_from_rfc3339(payload["previous_expires_at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(previous_expires_at, first.expires_at);
    }

    #[test]
    fn claim_lifecycle_events_share_attempt_identity_payload() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = add_shell_job(&config, None, Schedule::At { at }, "echo observe").unwrap();
        let claimed_at = at + ChronoDuration::seconds(1);
        let claim = claim_due(&config, &job, "worker-observe", claimed_at).unwrap();
        let detected_at = claimed_at + ChronoDuration::seconds(5);

        record_claim_lost(&config, &job.id, &claim, detected_at, "renewal_rejected").unwrap();

        let events = list_job_events(&config, &job.id).unwrap();
        for event_type in ["cron.job.claimed", "cron.job.claim_lost"] {
            let event = events.iter().find(|event| event.event_type == event_type).unwrap();
            let payload: serde_json::Value = serde_json::from_str(event.payload_json.as_deref().unwrap()).unwrap();
            assert_eq!(payload["worker_id"], claim.worker_id);
            assert_eq!(payload["attempt_id"], claim.attempt_id);
            assert_eq!(payload["claimed_at"], claim.claimed_at.to_rfc3339());
            assert_eq!(payload["expires_at"], claim.expires_at.to_rfc3339());
        }
        let lost = events
            .iter()
            .find(|event| event.event_type == "cron.job.claim_lost")
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(lost.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(lost.status.as_deref(), Some("claim_lost"));
        assert_eq!(payload["detected_at"], detected_at.to_rfc3339());
        assert_eq!(payload["reason"], "renewal_rejected");
    }

    #[test]
    fn recovered_claim_fences_old_success_and_failure_without_run_rows() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = add_shell_job(&config, None, Schedule::At { at }, "echo fenced").unwrap();
        let first_tick = at + ChronoDuration::seconds(1);
        let claim_a = claim_due(&config, &job, "worker-a", first_tick).unwrap();
        let claim_b = claim_due(&config, &job, "worker-b", claim_a.expires_at).unwrap();

        for success in [true, false] {
            let result = record_one_shot_terminal_run(
                &config,
                &job,
                &claim_a,
                first_tick,
                claim_a.expires_at,
                success,
                "stale",
                0,
            );
            assert!(result.is_err());
            assert!(list_runs(&config, &job.id, 10).unwrap().is_empty());
        }

        let finish_b = claim_b.claimed_at + ChronoDuration::seconds(1);
        record_one_shot_terminal_run(&config, &job, &claim_b, claim_b.claimed_at, finish_b, true, "fresh", 0).unwrap();
        let runs = list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].attempt_id.as_deref(), Some(claim_b.attempt_id.as_str()));
        assert_eq!(runs[0].worker_id.as_deref(), Some("worker-b"));
    }

    #[test]
    fn renew_returns_updated_handle_and_old_handle_loses_authority() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = add_shell_job(&config, None, Schedule::At { at }, "echo renew").unwrap();
        let now = at + ChronoDuration::seconds(1);
        let original = claim_due(&config, &job, "worker-a", now).unwrap();
        let renew_at = now + ChronoDuration::seconds(10);
        let renewed = renew_job_claim(&config, &job.id, &original, renew_at, ChronoDuration::seconds(30))
            .unwrap()
            .unwrap();
        assert_eq!(renewed.attempt_id, original.attempt_id);
        assert_eq!(renewed.claimed_at, original.claimed_at);
        assert_eq!(renewed.expires_at, renew_at + ChronoDuration::seconds(30));
        assert!(
            renew_job_claim(&config, &job.id, &original, renew_at, ChronoDuration::seconds(30))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn caller_commit_time_is_not_authoritative_before_lock() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = add_shell_job(&config, None, Schedule::At { at }, "echo commit-clock").unwrap();
        let claim = claim_due(&config, &job, "worker-a", at + ChronoDuration::seconds(1)).unwrap();
        let finished_at = claim.expires_at - ChronoDuration::seconds(1);

        let result = finish_claimed_run(
            &config,
            &job,
            &claim,
            claim.claimed_at,
            finished_at,
            claim.expires_at,
            true,
            "stale-at-commit",
            1,
            false,
        );

        assert!(
            result.is_ok(),
            "caller-supplied future commit time must not override lock-held clock"
        );
        assert_eq!(list_runs(&config, &job.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn recurring_finish_preserves_pause_and_manual_finish_preserves_next_run() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo recurring").unwrap();
        let started = Utc::now();
        let claim = claim_manual(&config, &job, started);
        let original_next_run = job.next_run;

        let paused = update_job(
            &config,
            &job.id,
            CronJobPatch {
                enabled: Some(false),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert!(!paused.enabled);
        finish_claimed_run_preserving_schedule(
            &config,
            &job,
            &claim,
            started,
            started + ChronoDuration::seconds(1),
            started + ChronoDuration::seconds(1),
            true,
            "manual",
            1,
            false,
        )
        .unwrap();

        let stored = get_job(&config, &job.id).unwrap();
        assert!(!stored.enabled, "completion must not reopen a concurrently paused job");
        assert_eq!(
            stored.next_run, original_next_run,
            "manual runs must not advance schedule"
        );
    }

    #[test]
    fn renewed_manual_claim_commits_after_original_expiry_without_advancing_schedule() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo long-manual").unwrap();
        let claimed_at = Utc::now();
        let original = claim_job_if_current_for_manual_run(
            &config,
            &job,
            "manual-worker",
            claimed_at,
            ChronoDuration::seconds(30),
        )
        .unwrap()
        .unwrap();
        let renewed = renew_job_claim(
            &config,
            &job.id,
            &original,
            claimed_at + ChronoDuration::seconds(20),
            ChronoDuration::seconds(30),
        )
        .unwrap()
        .unwrap();
        let commit_now = original.expires_at + ChronoDuration::seconds(1);

        finish_claimed_run_preserving_schedule(
            &config,
            &job,
            &renewed,
            claimed_at,
            commit_now,
            commit_now,
            true,
            "long manual",
            31_000,
            false,
        )
        .unwrap();

        let stored = get_job(&config, &job.id).unwrap();
        assert_eq!(stored.next_run, job.next_run);
        let run = list_runs(&config, &job.id, 1).unwrap().remove(0);
        assert_eq!(run.attempt_id.as_deref(), Some(renewed.attempt_id.as_str()));
    }

    #[test]
    fn recurring_schedule_update_rejects_active_claim_but_clears_expired_claim() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo update").unwrap();
        let claimed_at = Utc::now();
        let claim = claim_manual(&config, &job, claimed_at);
        let patch = || CronJobPatch {
            schedule: Some(Schedule::Cron {
                expr: "*/10 * * * *".to_string(),
                tz: None,
            }),
            ..CronJobPatch::default()
        };

        let active = update_job_at(&config, &job.id, patch(), claim.expires_at - ChronoDuration::seconds(1));
        assert!(active.unwrap_err().to_string().contains("active claim lease"));

        let updated = update_job_at(&config, &job.id, patch(), claim.expires_at).unwrap();
        assert!(updated.claim.is_none());
        assert_eq!(updated.expression, "*/10 * * * *");
    }

    #[test]
    fn legacy_running_without_lease_is_claimable_but_partial_tuple_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let at = Utc::now() + ChronoDuration::minutes(10);
        let legacy = add_shell_job(&config, None, Schedule::At { at }, "echo legacy").unwrap();
        with_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_jobs SET last_status = 'running' WHERE id = ?1",
                params![legacy.id],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(claim_due(&config, &legacy, "worker-a", at + ChronoDuration::seconds(1)).is_some());

        let partial = add_shell_job(&config, None, Schedule::At { at }, "echo partial").unwrap();
        with_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_jobs SET claim_owner = 'orphan' WHERE id = ?1",
                params![partial.id],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(
            get_job(&config, &partial.id)
                .unwrap_err()
                .to_string()
                .contains("partial cron claim tuple")
        );
        assert!(claim_due(&config, &partial, "worker-b", at + ChronoDuration::seconds(1)).is_none());
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
