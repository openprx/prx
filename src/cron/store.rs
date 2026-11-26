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
    CronClaim, CronJob, CronJobLineage, CronJobPatch, CronJobTerminalState, DeliveryConfig, JobCreator, Schedule,
    SessionTarget, next_run_for_schedule, schedule_cron_expression, validate_schedule,
};
use crate::security::agent_level::CreatorStamp;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

mod connection;
mod rows;
mod runs;

use connection::with_connection;
use rows::{
    JobLineage, insert_job_event, load_job_lineage, map_cron_job_row, map_job_event_row, parse_rfc3339,
    sql_conversion_error, workspace_id,
};
pub(crate) use rows::{MirrorLineage, cron_job_event_payload, decode_delivery, decode_schedule};
pub use runs::{finish_claimed_run, list_job_events, list_runs, record_terminal_manual_run};
pub(crate) use runs::{finish_claimed_run_preserving_schedule, stored_cron_output};
#[cfg(test)]
pub use runs::{record_one_shot_terminal_run, record_run};

const MAX_CRON_OUTPUT_BYTES: usize = 16 * 1024;
const TRUNCATED_OUTPUT_MARKER: &str = "\n...[truncated]";

pub(crate) fn format_claim_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Resolve the Postgres cron backend when configured, else `None` (SQLite path).
/// Centralizes backend selection so each public dispatcher is a one-line guard.
fn pg_store(config: &Config) -> Result<Option<std::sync::Arc<crate::cron::postgres::PostgresCronStore>>> {
    crate::cron::postgres::resolve(config)
}

/// Whether the configured cron store is Postgres, which keeps only `deliver = note`
/// (a patch with any other `deliver` is refused there).
pub fn store_keeps_only_note_deliver(config: &Config) -> Result<bool> {
    Ok(pg_store(config)?.is_some())
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
    let delivery_principal = lineage.delivery_principal;
    let origin = lineage.origin.to_stored()?;
    let created_via = lineage.created_via.as_str();
    let creator = lineage.creator;

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, owner_id, topic_id, parent_task_id, source_message_event_id,
                expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, created_at, next_run, approval_grant_json,
                creator_sender, creator_channel, creator_chat_type, origin, created_via,
                creator_level, creator_agent
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'shell', NULL, ?9, 'isolated', NULL, 1, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
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
                delivery_principal.sender.as_deref(),
                delivery_principal.channel.as_deref(),
                delivery_principal.chat_type.as_deref(),
                origin.as_str(),
                created_via,
                creator.level_column(),
                creator.agent.as_deref(),
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
    let delivery_principal = lineage.delivery_principal;
    let origin = lineage.origin.to_stored()?;
    let creator = lineage.creator;

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, owner_id, topic_id, parent_task_id, source_message_event_id,
                expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, created_at, next_run, approval_grant_json,
                creator_sender, creator_channel, creator_chat_type, origin, creator_level, creator_agent
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', ?7, 'agent', ?8, ?9, ?10, ?11, 1, ?12, ?13, ?14, ?15, NULL,
                       ?16, ?17, ?18, ?19, ?20, ?21)",
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
                delivery_principal.sender.as_deref(),
                delivery_principal.channel.as_deref(),
                delivery_principal.chat_type.as_deref(),
                origin.as_str(),
                creator.level_column(),
                creator.agent.as_deref(),
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
                    terminal_state, approval_grant_json, claim_owner, attempt_id, claimed_at, claim_expires_at,
                    creator_sender, creator_channel, creator_chat_type, origin, context, deliver
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
                    terminal_state, approval_grant_json, claim_owner, attempt_id, claimed_at, claim_expires_at,
                    creator_sender, creator_channel, creator_chat_type, origin, context, deliver
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

/// Which surface created the job and the stored creator stamp; NULL
/// `created_via` is `agent_tool`, and an unreadable stamp reads as empty.
pub fn job_creator(config: &Config, job_id: &str) -> Result<(JobCreator, CreatorStamp)> {
    if let Some(store) = pg_store(config)? {
        return store.job_creator(job_id);
    }
    // No job store means no stored creator: the run is capped, and looking it
    // up creates nothing.
    if !config.workspace_dir.join("cron").join("jobs.db").exists() {
        return Ok((JobCreator::AgentTool, CreatorStamp::default()));
    }
    with_connection(config, |conn| {
        let (created_via, level, agent) = conn
            .query_row(
                "SELECT created_via, creator_level, creator_agent FROM cron_jobs WHERE id = ?1",
                params![job_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .with_context(|| format!("Cron job '{job_id}' not found"))?;
        Ok((
            JobCreator::from_stored(created_via.as_deref()),
            CreatorStamp::from_stored(level.as_deref(), agent),
        ))
    })
}

/// Record that the operator vouches for a job as their own (`prx cron
/// mark-operator`): its runs then use the operator level.
pub fn mark_job_operator(config: &Config, job_id: &str) -> Result<()> {
    if let Some(store) = pg_store(config)? {
        return store.mark_job_operator(job_id);
    }
    let changed = with_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET version=version+1, created_via = 'operator_cli' WHERE id = ?1",
            params![job_id],
        )
        .context("Failed to mark cron job as the operator's")
    })?;
    if changed == 0 {
        anyhow::bail!("Cron job '{job_id}' not found");
    }
    Ok(())
}

/// Shell jobs stored before the creator was recorded (NULL `created_via`),
/// as `(id, command)`: they count as `agent_tool` until marked.
pub fn unmarked_shell_jobs(config: &Config) -> Result<Vec<(String, String)>> {
    if let Some(store) = pg_store(config)? {
        return store.unmarked_shell_jobs();
    }
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, command FROM cron_jobs WHERE job_type = 'shell' AND created_via IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
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

/// Publish the model outcome before its session terminal becomes visible.
/// Keep the lease: the scheduler still owns fenced history/schedule settlement.
pub(crate) fn mark_run_finalizing(config: &Config, job_id: &str, attempt: &str, status: &str) -> Result<()> {
    if let Some(store) = pg_store(config)? {
        return store.mark_run_finalizing(job_id, attempt, status);
    }
    let changed = with_connection(config, |conn| {
        Ok(conn.execute(
            "UPDATE cron_jobs SET last_status=?3 WHERE id=?1 AND attempt_id=?2 AND claim_expires_at>?4",
            params![job_id, attempt, status, format_claim_time(Utc::now())],
        )?)
    })?;
    anyhow::ensure!(changed == 1, "cron finalization claim lost");
    Ok(())
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

/// Record that a job's `announce` delivery was refused by the outbound ACL.
///
/// A withheld delivery is a policy decision, not an execution failure, so it
/// must not be inferred from a missing message: without a row of its own the
/// only difference between "the operator's rules stopped this" and "the channel
/// never fired" is a log line. This puts it on the job's own event stream, where
/// `cron events <job_id>` shows it next to the run that produced the output.
///
/// `destination_ref` is the audit fingerprint of `channel:recipient`
/// ([`crate::security::op_id::ref_for_channel_recipient`]), never the plaintext
/// recipient — the same rule the `message_send` rejection path follows.
pub fn record_delivery_withheld(
    config: &Config,
    job_id: &str,
    dst_channel: &str,
    destination_ref: &str,
    principal_channel: &str,
) -> Result<()> {
    if let Some(store) = pg_store(config)? {
        return store.record_delivery_withheld(
            &workspace_id(config),
            job_id,
            dst_channel,
            destination_ref,
            principal_channel,
        );
    }
    with_connection(config, |conn| {
        if let Some(lineage) = load_job_lineage(conn, job_id)? {
            let payload = serde_json::json!({
                "dst_channel": dst_channel,
                "destination_ref": destination_ref,
                "src_channel": principal_channel,
                "reason": "outbound scope rules",
            })
            .to_string();
            insert_job_event(
                conn,
                &workspace_id(config),
                job_id,
                lineage,
                "cron.job.delivery_withheld",
                Some("delivery_withheld"),
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
                    terminal_state, approval_grant_json, claim_owner, attempt_id, claimed_at, claim_expires_at,
                    creator_sender, creator_channel, creator_chat_type, origin, context, deliver
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
    let command_created_via = patch
        .command
        .as_ref()
        .map(|_| patch.created_via.unwrap_or_default().as_str());
    // A rewrite of what the job runs re-records the rewriter (V7).
    let rewritten_creator =
        (patch.command.is_some() || patch.prompt.is_some()).then(|| patch.creator.unwrap_or_default());
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
    // Re-anchoring is unconditional when the runtime asks for it: whoever
    // rewrites `delivery` becomes the principal that delivery is authorized as.
    // Keeping the original creator's identity would let one caller aim another
    // caller's privileges at a destination of its choosing.
    if let Some(delivery_principal) = patch.delivery_principal {
        job.delivery_principal = delivery_principal;
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
    if let Some(context) = patch.context {
        job.context = context;
    }
    if let Some(deliver) = patch.deliver {
        job.deliver = deliver;
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
             SET version = version + 1, expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                 session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11,
                 next_run = ?12, last_run = ?13, last_status = ?14, last_output = ?15,
                 terminal_state = ?16, approval_grant_json = ?17,
                 claim_owner = ?18, attempt_id = ?19, claimed_at = ?20, claim_expires_at = ?21,
                 creator_sender = ?31, creator_channel = ?32, creator_chat_type = ?33,
                 created_via = COALESCE(?34, created_via),
                 creator_level = CASE WHEN ?35 AND COALESCE(substr(creator_agent,1,2),'')<>'n:' THEN ?36 ELSE creator_level END,
                 creator_agent = CASE WHEN ?35 AND COALESCE(substr(creator_agent,1,2),'')<>'n:' THEN ?37 ELSE creator_agent END,
                 context = ?38, deliver = ?39
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
                    job.delivery_principal.sender.as_deref(),
                    job.delivery_principal.channel.as_deref(),
                    job.delivery_principal.chat_type.as_deref(),
                    command_created_via,
                    rewritten_creator.is_some(),
                    rewritten_creator.as_ref().and_then(CreatorStamp::level_column),
                    rewritten_creator.as_ref().and_then(|creator| creator.agent.as_deref()),
                    job.context.as_str(),
                    job.deliver.as_str(),
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
    let status = super::agent_jobs::run_status(success, output);
    let bounded_output = stored_cron_output(output);

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

/// The persisted job revision, independent of claims and execution attempts.
pub fn job_revision(config: &Config, id: &str) -> Result<u64> {
    if let Some(store) = pg_store(config)? {
        return store.job_revision(id);
    }
    with_connection(config, |conn| {
        Ok(u64::try_from(conn.query_row(
            "SELECT version FROM cron_jobs WHERE id=?1",
            [id],
            |r| r.get::<_, i64>(0),
        )?)?)
    })
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests;
