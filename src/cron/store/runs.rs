use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{TransactionBehavior, params};

use crate::config::Config;
use crate::cron::{CronClaim, CronJob, CronJobEvent, CronJobTerminalState, CronRun, Schedule, next_run_for_schedule};

use super::{
    MAX_CRON_OUTPUT_BYTES, TRUNCATED_OUTPUT_MARKER, format_claim_time, insert_job_event, load_job_lineage,
    map_job_event_row, parse_rfc3339, pg_store, sql_conversion_error, with_connection, workspace_id,
};

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

    let status = crate::cron::agent_jobs::run_status(success, output);
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
    let bounded_output = stored_cron_output(output);
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
    let status = crate::cron::agent_jobs::run_status(success, output);
    let bounded_output = stored_cron_output(output);
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
    let status = crate::cron::agent_jobs::run_status(success, output);
    let bounded_output = stored_cron_output(output);
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
    let bounded_output = output.map(stored_cron_output);
    with_connection(config, |conn| {
        // Wrap INSERT + pruning DELETE in an explicit transaction so that
        // if the DELETE fails, the INSERT is rolled back and the run table
        // cannot grow unboundedly.
        let tx = crate::runtime::sqlite_pool::begin_immediate(conn)?;

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

/// Redact and bound a job's output before a store persists it.
///
/// An agent job's output is the agent turn's reply (or its error), so it is
/// redacted like the terminal commit (D-053) before it is capped.
pub(crate) fn stored_cron_output(output: &str) -> String {
    let redacted = crate::agent::terminal::redact_persisted_text(output);
    let output = redacted.as_str();
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
