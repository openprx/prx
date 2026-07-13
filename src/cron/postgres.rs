//! PostgreSQL backend for cron job persistence (FIX-P2-05 / F2-PG).
//!
//! This mirrors the SQLite store in [`super::store`] so a Postgres-configured
//! deployment gets durable, cross-instance cron scheduling state. It is selected
//! when `[storage.provider.config]` resolves to `provider = "postgres"` with a
//! `db_url`, exactly like the Postgres memory backend; otherwise the SQLite path
//! is used. Because the backend is reachable from config it is not dead code.
//!
//! All SQL is parameterized (`$N` placeholders). Identifiers are never
//! interpolated. The atomic `claim_job` guard mirrors the SQLite semantics so
//! multiple scheduler instances polling the same database cannot double-execute
//! a job.

use crate::config::Config;
use crate::cron::store::{decode_delivery, decode_schedule, format_claim_time, truncate_cron_output};
use crate::cron::types::{
    CronClaim, CronJob, CronJobEvent, CronJobLineage, CronJobPatch, CronJobTerminalState, CronRun, DeliveryConfig,
    JobType, Schedule, SessionTarget,
};
use crate::cron::{next_run_for_schedule, schedule_cron_expression, validate_schedule};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use parking_lot::Mutex;
use postgres::types::ToSql;
use postgres::{Client, NoTls, Row};
use std::time::Duration;
use uuid::Uuid;

/// Cap connect timeouts so a misconfigured value cannot hang startup forever.
const CONNECT_TIMEOUT_CAP_SECS: u64 = 30;
const TERMINAL_STATE_MIGRATION_SQL: &str = "ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS terminal_state TEXT;
     UPDATE cron_jobs
     SET terminal_state = CASE WHEN last_status = 'ok' THEN 'succeeded' ELSE 'failed' END,
         enabled = FALSE
     WHERE terminal_state IS NULL AND last_run IS NOT NULL AND last_status IN ('ok', 'error')
       AND schedule LIKE '%\"kind\":\"at\"%';";
const CLAIM_LEASE_MIGRATION_SQL: &str = "ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS claim_owner TEXT;
     ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS attempt_id TEXT;
     ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS claimed_at TEXT;
     ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS claim_expires_at TEXT;
     ALTER TABLE cron_runs ADD COLUMN IF NOT EXISTS attempt_id TEXT;
     ALTER TABLE cron_runs ADD COLUMN IF NOT EXISTS worker_id TEXT;
     CREATE UNIQUE INDEX IF NOT EXISTS idx_cron_runs_job_attempt
        ON cron_runs(job_id, attempt_id) WHERE attempt_id IS NOT NULL;";

/// PostgreSQL-backed cron store. One instance owns a single pooled connection
/// guarded by a `parking_lot::Mutex` (no poison, no unwrap), matching the
/// memory backend's `PostgresClientSlot` pattern.
pub struct PostgresCronStore {
    client: Mutex<Client>,
}

impl PostgresCronStore {
    /// Connect, ensure the schema, and return a ready store. The connection is
    /// established on a dedicated thread so a slow/hung TCP connect cannot block
    /// an async runtime worker (the caller may be inside a Tokio context).
    pub fn connect(db_url: &str, connect_timeout_secs: Option<u64>) -> Result<Self> {
        let db_url = db_url.to_string();
        let handle = std::thread::Builder::new()
            .name("cron-postgres-init".to_string())
            .spawn(move || -> Result<Client> {
                let mut config: postgres::Config = db_url
                    .parse()
                    .context("invalid PostgreSQL connection URL for cron store")?;
                if let Some(secs) = connect_timeout_secs {
                    config.connect_timeout(Duration::from_secs(secs.min(CONNECT_TIMEOUT_CAP_SECS)));
                }
                let mut client = config
                    .connect(NoTls)
                    .context("failed to connect to PostgreSQL cron backend")?;
                init_schema(&mut client)?;
                Ok(client)
            })
            .context("failed to spawn cron postgres initializer thread")?;

        let client = handle
            .join()
            .map_err(|_| anyhow::anyhow!("cron postgres initializer thread panicked"))??;

        Ok(Self {
            client: Mutex::new(client),
        })
    }

    fn with_client<T>(&self, f: impl FnOnce(&mut Client) -> Result<T>) -> Result<T> {
        let mut guard = self.client.lock();
        f(&mut guard)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_shell_job_with_lineage_approval_and_delete(
        &self,
        workspace_id: &str,
        name: Option<String>,
        schedule: Schedule,
        command: &str,
        approval_grant_json: Option<String>,
        delete_after_run: bool,
        lineage: CronJobLineage,
    ) -> Result<CronJob> {
        let now = Utc::now();
        validate_schedule(&schedule, now)?;
        let next_run = next_run_for_schedule(&schedule, now)?;
        let id = Uuid::new_v4().to_string();
        let expression = schedule_cron_expression(&schedule).unwrap_or_default();
        let schedule_json = serde_json::to_string(&schedule)?;
        let delivery_json = serde_json::to_string(&DeliveryConfig::default())?;

        self.with_client(|client| {
            let mut tx = client.transaction().context("failed to open cron insert transaction")?;
            tx.execute(
                "INSERT INTO cron_jobs (
                    id, owner_id, topic_id, parent_task_id, source_message_event_id,
                    expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, approval_grant_json
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'shell', NULL, $9, 'isolated', NULL,
                           TRUE, $10, $11, $12, $13, $14)",
                &[
                    &id,
                    &lineage.owner_id,
                    &lineage.topic_id,
                    &lineage.parent_task_id,
                    &lineage.source_message_event_id,
                    &expression,
                    &command,
                    &schedule_json,
                    &name,
                    &delivery_json,
                    &delete_after_run,
                    &now.to_rfc3339(),
                    &next_run.to_rfc3339(),
                    &approval_grant_json,
                ],
            )
            .context("Failed to insert cron shell job")?;
            insert_job_event(
                &mut tx,
                workspace_id,
                &id,
                &JobLineage::from_create(&lineage),
                "cron.job.created",
                Some("pending"),
                Some(
                    serde_json::json!({
                        "kind": "shell",
                        "name": name,
                        "schedule": schedule_json,
                        "source_message_event_id": lineage.source_message_event_id,
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
            tx.commit().context("failed to commit cron shell insert")?;
            Ok(())
        })?;

        self.get_job(&id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_agent_job_with_lineage(
        &self,
        workspace_id: &str,
        name: Option<String>,
        schedule: Schedule,
        prompt: &str,
        session_target: SessionTarget,
        model: Option<String>,
        delivery: Option<DeliveryConfig>,
        delete_after_run: bool,
        lineage: CronJobLineage,
    ) -> Result<CronJob> {
        let now = Utc::now();
        validate_schedule(&schedule, now)?;
        let next_run = next_run_for_schedule(&schedule, now)?;
        let id = Uuid::new_v4().to_string();
        let expression = schedule_cron_expression(&schedule).unwrap_or_default();
        let schedule_json = serde_json::to_string(&schedule)?;
        let delivery = delivery.unwrap_or_default();
        let delivery_json = serde_json::to_string(&delivery)?;
        let session_target_str = session_target.as_str();

        self.with_client(|client| {
            let mut tx = client.transaction().context("failed to open cron insert transaction")?;
            tx.execute(
                "INSERT INTO cron_jobs (
                    id, owner_id, topic_id, parent_task_id, source_message_event_id,
                    expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, approval_grant_json
                 ) VALUES ($1, $2, $3, $4, $5, $6, '', $7, 'agent', $8, $9, $10, $11,
                           TRUE, $12, $13, $14, $15, NULL)",
                &[
                    &id,
                    &lineage.owner_id,
                    &lineage.topic_id,
                    &lineage.parent_task_id,
                    &lineage.source_message_event_id,
                    &expression,
                    &schedule_json,
                    &prompt,
                    &name,
                    &session_target_str,
                    &model,
                    &delivery_json,
                    &delete_after_run,
                    &now.to_rfc3339(),
                    &next_run.to_rfc3339(),
                ],
            )
            .context("Failed to insert cron agent job")?;
            insert_job_event(
                &mut tx,
                workspace_id,
                &id,
                &JobLineage::from_create(&lineage),
                "cron.job.created",
                Some("pending"),
                Some(
                    serde_json::json!({
                        "kind": "agent",
                        "name": name,
                        "schedule": schedule_json,
                        "session_target": session_target_str,
                        "source_message_event_id": lineage.source_message_event_id,
                    })
                    .to_string(),
                )
                .as_deref(),
            )?;
            tx.commit().context("failed to commit cron agent insert")?;
            Ok(())
        })?;

        self.get_job(&id)
    }

    pub fn list_jobs(&self) -> Result<Vec<CronJob>> {
        self.with_client(|client| {
            let rows = client
                .query(&format!("{SELECT_JOB_COLUMNS} ORDER BY next_run ASC"), &[])
                .context("Failed to list cron jobs")?;
            rows.iter().map(map_cron_job_row).collect()
        })
    }

    pub fn get_job(&self, job_id: &str) -> Result<CronJob> {
        self.with_client(|client| {
            let row = client
                .query_opt(&format!("{SELECT_JOB_COLUMNS} WHERE id = $1"), &[&job_id])
                .context("Failed to query cron job")?;
            match row {
                Some(row) => map_cron_job_row(&row),
                None => anyhow::bail!("Cron job '{job_id}' not found"),
            }
        })
    }

    pub fn remove_job(&self, workspace_id: &str, id: &str) -> Result<()> {
        let changed = self.with_client(|client| {
            let mut tx = client.transaction().context("failed to open cron remove transaction")?;
            if let Some(lineage) = load_job_lineage(&mut tx, id)? {
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    id,
                    &lineage,
                    "cron.job.removed",
                    lineage.status.as_deref(),
                    None,
                )?;
            }
            let changed = tx
                .execute("DELETE FROM cron_jobs WHERE id = $1", &[&id])
                .context("Failed to delete cron job")?;
            tx.commit().context("failed to commit cron remove")?;
            Ok(changed)
        })?;

        if changed == 0 {
            anyhow::bail!("Cron job '{id}' not found");
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn claim_job(&self, workspace_id: &str, job_id: &str) -> Result<bool> {
        self.with_client(|client| {
            let mut tx = client.transaction().context("failed to open cron claim transaction")?;
            let changed = tx
                .execute(
                    "UPDATE cron_jobs SET last_status = 'running'
                     WHERE id = $1 AND enabled = TRUE AND terminal_state IS NULL
                       AND (last_status IS NULL OR last_status <> 'running')",
                    &[&job_id],
                )
                .context("Failed to claim cron job")?;
            if changed > 0
                && let Some(lineage) = load_job_lineage(&mut tx, job_id)?
            {
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    job_id,
                    &lineage,
                    "cron.job.claimed",
                    Some("running"),
                    None,
                )?;
            }
            tx.commit().context("failed to commit cron claim")?;
            Ok(changed > 0)
        })
    }

    pub fn claim_job_if_current(
        &self,
        workspace_id: &str,
        job: &CronJob,
        worker_id: &str,
        now: DateTime<Utc>,
        lease_duration: ChronoDuration,
        require_due: bool,
    ) -> Result<Option<CronClaim>> {
        if worker_id.trim().is_empty() || lease_duration <= ChronoDuration::zero() {
            anyhow::bail!("cron claim requires a worker_id and positive lease duration");
        }
        let schedule_json = serde_json::to_string(&job.schedule)?;
        let attempt_id = Uuid::new_v4().to_string();
        let expires_at = now + lease_duration;
        self.with_client(|client| {
            let mut tx = client
                .transaction()
                .context("failed to open cron snapshot claim transaction")?;
            let previous = tx.query_opt(
                "SELECT claim_owner, attempt_id, claimed_at, claim_expires_at FROM cron_jobs WHERE id = $1 FOR UPDATE",
                &[&job.id],
            )?;
            let changed = if require_due {
                tx.execute(
                    "UPDATE cron_jobs SET last_status = 'running', claim_owner = $1, attempt_id = $2,
                         claimed_at = $3, claim_expires_at = $4
                     WHERE id = $5 AND enabled = TRUE AND terminal_state IS NULL AND next_run = $6 AND next_run <= $3
                       AND (schedule = $7 OR (schedule IS NULL AND expression = $8))
                       AND ((claim_owner IS NULL AND attempt_id IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL)
                            OR (claim_owner IS NOT NULL AND attempt_id IS NOT NULL AND claimed_at IS NOT NULL
                                AND claim_expires_at IS NOT NULL AND claim_expires_at <= $3))",
                    &[&worker_id, &attempt_id, &format_claim_time(now), &format_claim_time(expires_at), &job.id,
                        &job.next_run.to_rfc3339(), &schedule_json, &job.expression],
                )
            } else {
                tx.execute(
                    "UPDATE cron_jobs SET last_status = 'running', claim_owner = $1, attempt_id = $2,
                         claimed_at = $3, claim_expires_at = $4
                     WHERE id = $5 AND terminal_state IS NULL AND next_run = $6
                       AND (schedule = $7 OR (schedule IS NULL AND expression = $8))
                       AND ((claim_owner IS NULL AND attempt_id IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL)
                            OR (claim_owner IS NOT NULL AND attempt_id IS NOT NULL AND claimed_at IS NOT NULL
                                AND claim_expires_at IS NOT NULL AND claim_expires_at <= $3))",
                    &[&worker_id, &attempt_id, &format_claim_time(now), &format_claim_time(expires_at), &job.id,
                        &job.next_run.to_rfc3339(), &schedule_json, &job.expression],
                )
            }
            .context("Failed to claim current cron job snapshot")?;
            if changed == 0 {
                tx.rollback()?;
                return Ok(None);
            }
            if let Some(lineage) = load_job_lineage(&mut tx, &job.id)? {
                let recovered = previous.as_ref().is_some_and(|row| {
                    row.try_get::<_, Option<String>>(0).ok().flatten().is_some()
                        && row.try_get::<_, Option<String>>(1).ok().flatten().is_some()
                        && row.try_get::<_, Option<String>>(2).ok().flatten().is_some()
                        && row.try_get::<_, Option<String>>(3).ok().flatten().is_some()
                });
                let previous_worker_id = previous
                    .as_ref()
                    .and_then(|row| row.try_get::<_, Option<String>>(0).ok().flatten());
                let previous_attempt_id = previous
                    .as_ref()
                    .and_then(|row| row.try_get::<_, Option<String>>(1).ok().flatten());
                let previous_expires_at = previous
                    .as_ref()
                    .and_then(|row| row.try_get::<_, Option<String>>(3).ok().flatten());
                let payload = serde_json::json!({"worker_id": worker_id, "attempt_id": attempt_id,
                    "claimed_at": now.to_rfc3339(), "expires_at": expires_at.to_rfc3339(),
                    "previous_worker_id": previous_worker_id, "previous_attempt_id": previous_attempt_id,
                    "previous_expires_at": previous_expires_at}).to_string();
                insert_job_event(&mut tx, workspace_id, &job.id, &lineage,
                    if recovered { "cron.job.claim_recovered" } else { "cron.job.claimed" },
                    Some("running"), Some(&payload))?;
            }
            tx.commit().context("failed to commit cron snapshot claim")?;
            Ok(Some(CronClaim { worker_id: worker_id.to_string(), attempt_id, claimed_at: now, expires_at }))
        })
    }

    pub fn renew_job_claim(
        &self,
        job_id: &str,
        claim: &CronClaim,
        now: DateTime<Utc>,
        lease_duration: ChronoDuration,
    ) -> Result<Option<CronClaim>> {
        if lease_duration <= ChronoDuration::zero() {
            anyhow::bail!("cron claim lease duration must be greater than zero");
        }
        let expires_at = now + lease_duration;
        self.with_client(|client| {
            let changed = client.execute(
                "UPDATE cron_jobs SET claim_expires_at = $1 WHERE id = $2 AND claim_owner = $3
                 AND attempt_id = $4 AND claimed_at = $5 AND claim_expires_at = $6 AND claim_expires_at > $7",
                &[
                    &format_claim_time(expires_at),
                    &job_id,
                    &claim.worker_id,
                    &claim.attempt_id,
                    &format_claim_time(claim.claimed_at),
                    &format_claim_time(claim.expires_at),
                    &format_claim_time(now),
                ],
            )?;
            Ok((changed > 0).then(|| CronClaim {
                expires_at,
                ..claim.clone()
            }))
        })
    }

    pub fn abandon_job_claim(
        &self,
        workspace_id: &str,
        job_id: &str,
        claim: &CronClaim,
        previous_last_status: Option<&str>,
        reason: &str,
    ) -> Result<bool> {
        self.with_client(|client| {
            let mut tx = client
                .transaction()
                .context("failed to open cron claim-abandon transaction")?;
            let changed = tx.execute(
                "UPDATE cron_jobs
                 SET last_status = $6,
                     claim_owner = NULL, attempt_id = NULL, claimed_at = NULL, claim_expires_at = NULL
                 WHERE id = $1 AND claim_owner = $2 AND attempt_id = $3
                   AND claimed_at = $4 AND claim_expires_at = $5",
                &[
                    &job_id,
                    &claim.worker_id,
                    &claim.attempt_id,
                    &format_claim_time(claim.claimed_at),
                    &format_claim_time(claim.expires_at),
                    &previous_last_status,
                ],
            )?;
            if changed > 0 {
                if let Some(lineage) = load_job_lineage(&mut tx, job_id)? {
                    let payload = serde_json::json!({
                        "worker_id": claim.worker_id,
                        "attempt_id": claim.attempt_id,
                        "claimed_at": claim.claimed_at.to_rfc3339(),
                        "expires_at": claim.expires_at.to_rfc3339(),
                        "reason": reason,
                    })
                    .to_string();
                    insert_job_event(
                        &mut tx,
                        workspace_id,
                        job_id,
                        &lineage,
                        "cron.job.claim_abandoned",
                        Some("abandoned"),
                        Some(&payload),
                    )?;
                }
            }
            tx.commit().context("failed to commit cron claim-abandon transaction")?;
            Ok(changed > 0)
        })
    }

    pub fn claim_terminal_job_for_manual_rerun(
        &self,
        workspace_id: &str,
        job: &CronJob,
        worker_id: &str,
        now: DateTime<Utc>,
        lease_duration: ChronoDuration,
    ) -> Result<Option<CronClaim>> {
        if !matches!(job.schedule, Schedule::At { .. }) || job.terminal_state.is_none() {
            anyhow::bail!("terminal manual rerun claim requires an already terminal Schedule::At job");
        }
        if worker_id.trim().is_empty() || lease_duration <= ChronoDuration::zero() {
            anyhow::bail!("terminal cron claim requires a worker_id and positive lease duration");
        }
        let attempt_id = Uuid::new_v4().to_string();
        let expires_at = now + lease_duration;
        let schedule_json = serde_json::to_string(&job.schedule)?;
        self.with_client(|client| {
            let mut tx = client.transaction()?;
            let changed = tx.execute(
                "UPDATE cron_jobs SET claim_owner = $1, attempt_id = $2, claimed_at = $3, claim_expires_at = $4
                 WHERE id = $5 AND terminal_state IS NOT NULL AND next_run = $6 AND schedule = $7
                   AND ((claim_owner IS NULL AND attempt_id IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL)
                        OR (claim_owner IS NOT NULL AND attempt_id IS NOT NULL AND claimed_at IS NOT NULL
                            AND claim_expires_at IS NOT NULL AND claim_expires_at <= $3))",
                &[
                    &worker_id,
                    &attempt_id,
                    &format_claim_time(now),
                    &format_claim_time(expires_at),
                    &job.id,
                    &job.next_run.to_rfc3339(),
                    &schedule_json,
                ],
            )?;
            if changed == 0 {
                tx.rollback()?;
                return Ok(None);
            }
            if let Some(lineage) = load_job_lineage(&mut tx, &job.id)? {
                let payload = serde_json::json!({"worker_id": worker_id, "attempt_id": attempt_id,
                    "claimed_at": now.to_rfc3339(), "expires_at": expires_at.to_rfc3339()})
                .to_string();
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    &job.id,
                    &lineage,
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

    pub fn record_claim_lost(
        &self,
        workspace_id: &str,
        job_id: &str,
        claim: &CronClaim,
        detected_at: DateTime<Utc>,
        reason: &str,
    ) -> Result<()> {
        self.with_client(|client| {
            let mut tx = client
                .transaction()
                .context("failed to open cron claim-lost transaction")?;
            if let Some(lineage) = load_job_lineage(&mut tx, job_id)? {
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
                    &mut tx,
                    workspace_id,
                    job_id,
                    &lineage,
                    "cron.job.claim_lost",
                    Some("claim_lost"),
                    Some(&payload),
                )?;
            }
            tx.commit().context("failed to commit cron claim-lost event")
        })
    }

    pub fn due_jobs(&self, now: DateTime<Utc>, max_tasks: usize) -> Result<Vec<CronJob>> {
        let lim = i64::try_from(max_tasks.max(1)).context("Scheduler max_tasks overflows i64")?;
        self.with_client(|client| {
            let rows = client
                .query(
                    &format!(
                        "{SELECT_JOB_COLUMNS} WHERE enabled = TRUE AND terminal_state IS NULL
                         AND next_run <= $1
                         AND ((claim_owner IS NULL AND attempt_id IS NULL
                               AND claimed_at IS NULL AND claim_expires_at IS NULL)
                              OR (claim_owner IS NOT NULL AND attempt_id IS NOT NULL
                                  AND claimed_at IS NOT NULL AND claim_expires_at IS NOT NULL
                                  AND claim_expires_at::timestamptz <= $1::timestamptz))
                         ORDER BY next_run ASC LIMIT $2"
                    ),
                    &[&now.to_rfc3339(), &lim],
                )
                .context("Failed to query due cron jobs")?;
            rows.iter().map(map_cron_job_row).collect()
        })
    }

    #[cfg(test)]
    pub fn update_job(&self, workspace_id: &str, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
        self.update_job_at(workspace_id, job_id, patch, Utc::now())
    }

    pub fn update_job_at(
        &self,
        workspace_id: &str,
        job_id: &str,
        patch: CronJobPatch,
        now: DateTime<Utc>,
    ) -> Result<CronJob> {
        let mut job = self.get_job(job_id)?;
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

        let schedule_json = serde_json::to_string(&job.schedule)?;
        let delivery_json = serde_json::to_string(&job.delivery)?;
        let job_type_str = job.job_type.as_str();
        let session_target_str = job.session_target.as_str();

        self.with_client(|client| {
            let mut tx = client.transaction().context("failed to open cron update transaction")?;
            let changed = tx
                .execute(
                    "UPDATE cron_jobs
                     SET expression = $1, command = $2, schedule = $3, job_type = $4, prompt = $5, name = $6,
                         session_target = $7, model = $8, enabled = $9, delivery = $10, delete_after_run = $11,
                         next_run = $12, last_run = $13, last_status = $14, last_output = $15,
                         terminal_state = $16, approval_grant_json = $17, claim_owner = $18,
                         attempt_id = $19, claimed_at = $20, claim_expires_at = $21
                     WHERE id = $22 AND next_run = $23
                       AND (schedule = $24 OR (schedule IS NULL AND expression = $25))
                       AND last_status IS NOT DISTINCT FROM $26
                       AND claim_owner IS NOT DISTINCT FROM $27 AND attempt_id IS NOT DISTINCT FROM $28
                       AND claimed_at IS NOT DISTINCT FROM $29 AND claim_expires_at IS NOT DISTINCT FROM $30",
                    &[
                        &job.expression,
                        &job.command,
                        &schedule_json,
                        &job_type_str,
                        &job.prompt,
                        &job.name,
                        &session_target_str,
                        &job.model,
                        &job.enabled,
                        &delivery_json,
                        &job.delete_after_run,
                        &job.next_run.to_rfc3339(),
                        &job.last_run.map(|value| value.to_rfc3339()),
                        &job.last_status,
                        &job.last_output,
                        &job.terminal_state.map(CronJobTerminalState::as_str),
                        &job.approval_grant_json,
                        &job.claim.as_ref().map(|claim| claim.worker_id.as_str()),
                        &job.claim.as_ref().map(|claim| claim.attempt_id.as_str()),
                        &job.claim.as_ref().map(|claim| format_claim_time(claim.claimed_at)),
                        &job.claim.as_ref().map(|claim| format_claim_time(claim.expires_at)),
                        &job.id,
                        &expected_next_run,
                        &expected_schedule_json,
                        &expected_expression,
                        &expected_last_status,
                        &expected_claim.as_ref().map(|claim| claim.worker_id.as_str()),
                        &expected_claim.as_ref().map(|claim| claim.attempt_id.as_str()),
                        &expected_claim.as_ref().map(|claim| format_claim_time(claim.claimed_at)),
                        &expected_claim.as_ref().map(|claim| format_claim_time(claim.expires_at)),
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
            if let Some(lineage) = load_job_lineage(&mut tx, &job.id)? {
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    &job.id,
                    &lineage,
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
            tx.commit().context("failed to commit cron update")?;
            Ok(())
        })?;

        self.get_job(job_id)
    }

    #[cfg(test)]
    pub fn reschedule_after_run(&self, workspace_id: &str, job: &CronJob, success: bool, output: &str) -> Result<()> {
        if matches!(job.schedule, Schedule::At { .. }) {
            anyhow::bail!("Schedule::At is terminal after its attempt and cannot be rescheduled");
        }
        let now = Utc::now();
        let next_run = next_run_for_schedule(&job.schedule, now)?;
        let status = if success { "ok" } else { "error" };
        let bounded_output = truncate_cron_output(output);
        self.with_client(|client| {
            let mut tx = client
                .transaction()
                .context("failed to open cron reschedule transaction")?;
            tx.execute(
                "UPDATE cron_jobs SET next_run = $1, last_run = $2, last_status = $3, last_output = $4 WHERE id = $5",
                &[
                    &next_run.to_rfc3339(),
                    &now.to_rfc3339(),
                    &status,
                    &bounded_output,
                    &job.id,
                ],
            )
            .context("Failed to update cron job run state")?;
            if let Some(lineage) = load_job_lineage(&mut tx, &job.id)? {
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    &job.id,
                    &lineage,
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
            tx.commit().context("failed to commit cron reschedule")?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_claimed_run(
        &self,
        workspace_id: &str,
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
        max_run_history: u32,
    ) -> Result<bool> {
        if matches!(job.schedule, Schedule::At { .. }) {
            return self.record_one_shot_terminal_run(
                workspace_id,
                job,
                claim,
                started_at,
                finished_at,
                commit_now,
                success,
                output,
                duration_ms,
                max_run_history,
            );
        }
        let status = if success { "ok" } else { "error" };
        let bounded_output = truncate_cron_output(output);
        let next_run = if advance_schedule {
            next_run_for_schedule(&job.schedule, finished_at)?
        } else {
            job.next_run
        };
        let schedule_json = serde_json::to_string(&job.schedule)?;
        let keep = i64::from(max_run_history.max(1));
        self.with_client(|client| {
            let mut tx = client.transaction()?;
            let locked = tx.query_opt("SELECT id FROM cron_jobs WHERE id = $1 FOR UPDATE", &[&job.id])?;
            if locked.is_none() {
                anyhow::bail!("cron job '{}' no longer exists", job.id);
            }
            let lease_valid: bool = tx
                .query_one(
                    "SELECT clock_timestamp() < $1::timestamptz",
                    &[&format_claim_time(claim.expires_at)],
                )?
                .get(0);
            if !lease_valid {
                anyhow::bail!("cron job '{}' claim expired before write lock was acquired", job.id);
            }
            let changed = tx.execute(
                "UPDATE cron_jobs SET last_run = $1, last_status = $2, last_output = $3,
                     next_run = $4, enabled = CASE WHEN $5 THEN FALSE ELSE enabled END,
                     claim_owner = NULL, attempt_id = NULL,
                     claimed_at = NULL, claim_expires_at = NULL
                 WHERE id = $6 AND terminal_state IS NULL AND next_run = $7 AND schedule = $8
                   AND claim_owner = $9 AND attempt_id = $10 AND claimed_at = $11
                   AND claim_expires_at = $12
                   AND clock_timestamp() < $12::timestamptz",
                &[&finished_at.to_rfc3339(), &status, &bounded_output, &next_run.to_rfc3339(),
                    &disable_after, &job.id, &job.next_run.to_rfc3339(), &schedule_json,
                    &claim.worker_id, &claim.attempt_id, &format_claim_time(claim.claimed_at),
                    &format_claim_time(claim.expires_at)],
            )?;
            if changed == 0 { anyhow::bail!("cron job '{}' claim was lost or expired", job.id); }
            tx.execute(
                "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms, attempt_id, worker_id)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
                &[&job.id, &started_at.to_rfc3339(), &finished_at.to_rfc3339(), &status,
                    &bounded_output, &duration_ms, &claim.attempt_id, &claim.worker_id],
            )?;
            tx.execute(
                "DELETE FROM cron_runs WHERE job_id = $1 AND id NOT IN
                 (SELECT id FROM cron_runs WHERE job_id = $1 ORDER BY started_at DESC, id DESC LIMIT $2)",
                &[&job.id, &keep],
            )?;
            if let Some(lineage) = load_job_lineage(&mut tx, &job.id)? {
                let run_payload = serde_json::json!({"started_at": started_at.to_rfc3339(),
                    "finished_at": finished_at.to_rfc3339(), "duration_ms": duration_ms,
                    "attempt_id": claim.attempt_id, "worker_id": claim.worker_id}).to_string();
                insert_job_event(&mut tx, workspace_id, &job.id, &lineage, "cron.job.run_recorded",
                    Some(status), Some(&run_payload))?;
                let finish_payload = serde_json::json!({"next_run": next_run.to_rfc3339(),
                    "success": success, "attempt_id": claim.attempt_id, "worker_id": claim.worker_id}).to_string();
                insert_job_event(&mut tx, workspace_id, &job.id, &lineage,
                    if disable_after { "cron.job.disabled" } else { "cron.job.rescheduled" },
                    Some(status), Some(&finish_payload))?;
            }
            tx.commit()?;
            Ok(false)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_one_shot_terminal_run(
        &self,
        workspace_id: &str,
        job: &CronJob,
        claim: &CronClaim,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        _commit_now: DateTime<Utc>,
        success: bool,
        output: &str,
        duration_ms: i64,
        max_run_history: u32,
    ) -> Result<bool> {
        if !matches!(job.schedule, Schedule::At { .. }) {
            anyhow::bail!("terminal one-shot persistence requires Schedule::At");
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
        let keep = i64::from(max_run_history.max(1));
        let schedule_json = serde_json::to_string(&job.schedule)?;
        self.with_client(|client| {
            let mut tx = client
                .transaction()
                .context("failed to open terminal cron run transaction")?;
            let locked = tx.query_opt("SELECT id FROM cron_jobs WHERE id = $1 FOR UPDATE", &[&job.id])?;
            if locked.is_none() {
                anyhow::bail!("cron job '{}' no longer exists", job.id);
            }
            let lease_valid: bool = tx
                .query_one(
                    "SELECT clock_timestamp() < $1::timestamptz",
                    &[&format_claim_time(claim.expires_at)],
                )?
                .get(0);
            if !lease_valid {
                anyhow::bail!("cron job '{}' claim expired before write lock was acquired", job.id);
            }
            let changed = tx
                .execute(
                    "UPDATE cron_jobs
                     SET last_run = $1, last_status = $2, last_output = $3, terminal_state = $4,
                         claim_owner = NULL, attempt_id = NULL, claimed_at = NULL, claim_expires_at = NULL
                     WHERE id = $5 AND terminal_state IS NULL AND next_run = $6 AND schedule = $7
                       AND claim_owner = $8 AND attempt_id = $9 AND claimed_at = $10
                       AND claim_expires_at = $11
                       AND clock_timestamp() < $11::timestamptz",
                    &[
                        &finished_at.to_rfc3339(),
                        &status,
                        &bounded_output,
                        &terminal_state.as_str(),
                        &job.id,
                        &job.next_run.to_rfc3339(),
                        &schedule_json,
                        &claim.worker_id,
                        &claim.attempt_id,
                        &format_claim_time(claim.claimed_at),
                        &format_claim_time(claim.expires_at),
                    ],
                )
                .context("Failed to persist terminal cron job state")?;
            if changed == 0 {
                anyhow::bail!("cron one-shot '{}' claim was lost, expired, or already terminal", job.id);
            }
            tx.execute(
                "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms, attempt_id, worker_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[&job.id, &started_at.to_rfc3339(), &finished_at.to_rfc3339(), &status,
                    &bounded_output, &duration_ms, &claim.attempt_id, &claim.worker_id],
            )?;
            tx.execute(
                "DELETE FROM cron_runs WHERE job_id = $1 AND id NOT IN
                 (SELECT id FROM cron_runs WHERE job_id = $1 ORDER BY started_at DESC, id DESC LIMIT $2)",
                &[&job.id, &keep],
            )?;
            if let Some(lineage) = load_job_lineage(&mut tx, &job.id)? {
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    &job.id,
                    &lineage,
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
                    &mut tx,
                    workspace_id,
                    &job.id,
                    &lineage,
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
                     WHERE id = $1 AND terminal_state = 'succeeded' AND delete_after_run = TRUE",
                &[&job.id],
            )? > 0;
            tx.commit().context("failed to commit terminal cron run")?;
            Ok(deleted)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_terminal_manual_run(
        &self,
        workspace_id: &str,
        job: &CronJob,
        claim: &CronClaim,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        success: bool,
        output: &str,
        duration_ms: i64,
        max_run_history: u32,
    ) -> Result<()> {
        if !matches!(job.schedule, Schedule::At { .. }) || job.terminal_state.is_none() {
            anyhow::bail!("manual terminal rerun requires an already terminal Schedule::At job");
        }
        let status = if success { "ok" } else { "error" };
        let bounded_output = truncate_cron_output(output);
        let schedule_json = serde_json::to_string(&job.schedule)?;
        let keep = i64::from(max_run_history.max(1));
        self.with_client(|client| {
            let mut tx = client.transaction()?;
            let locked = tx.query_opt("SELECT id FROM cron_jobs WHERE id = $1 FOR UPDATE", &[&job.id])?;
            if locked.is_none() {
                anyhow::bail!("terminal cron job '{}' no longer exists", job.id);
            }
            let lease_valid: bool = tx
                .query_one(
                    "SELECT clock_timestamp() < $1::timestamptz",
                    &[&format_claim_time(claim.expires_at)],
                )?
                .get(0);
            if !lease_valid {
                anyhow::bail!("terminal cron job '{}' rerun claim expired before write lock", job.id);
            }
            let changed = tx.execute(
                "UPDATE cron_jobs SET last_run = $1, last_status = $2, last_output = $3,
                     claim_owner = NULL, attempt_id = NULL, claimed_at = NULL, claim_expires_at = NULL
                 WHERE id = $4 AND terminal_state IS NOT NULL AND next_run = $5 AND schedule = $6
                   AND claim_owner = $7 AND attempt_id = $8 AND claimed_at = $9 AND claim_expires_at = $10
                   AND clock_timestamp() < $10::timestamptz",
                &[
                    &finished_at.to_rfc3339(),
                    &status,
                    &bounded_output,
                    &job.id,
                    &job.next_run.to_rfc3339(),
                    &schedule_json,
                    &claim.worker_id,
                    &claim.attempt_id,
                    &format_claim_time(claim.claimed_at),
                    &format_claim_time(claim.expires_at),
                ],
            )?;
            if changed == 0 {
                anyhow::bail!("terminal cron job '{}' changed before manual rerun audit", job.id);
            }
            tx.execute(
                "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms, attempt_id, worker_id)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
                &[
                    &job.id,
                    &started_at.to_rfc3339(),
                    &finished_at.to_rfc3339(),
                    &status,
                    &bounded_output,
                    &duration_ms,
                    &claim.attempt_id,
                    &claim.worker_id,
                ],
            )?;
            tx.execute(
                "DELETE FROM cron_runs WHERE job_id = $1 AND id NOT IN
                 (SELECT id FROM cron_runs WHERE job_id = $1 ORDER BY started_at DESC, id DESC LIMIT $2)",
                &[&job.id, &keep],
            )?;
            if let Some(lineage) = load_job_lineage(&mut tx, &job.id)? {
                let payload = serde_json::json!({"started_at": started_at.to_rfc3339(),
                    "finished_at": finished_at.to_rfc3339(), "duration_ms": duration_ms,
                    "success": success, "attempt_id": claim.attempt_id,
                    "worker_id": claim.worker_id})
                .to_string();
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    &job.id,
                    &lineage,
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
        &self,
        workspace_id: &str,
        job_id: &str,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        status: &str,
        output: Option<&str>,
        duration_ms: i64,
        max_run_history: u32,
    ) -> Result<()> {
        let bounded_output = output.map(truncate_cron_output);
        let keep = i64::from(max_run_history.max(1));
        self.with_client(|client| {
            let mut tx = client.transaction().context("failed to open cron run transaction")?;
            tx.execute(
                "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &job_id,
                    &started_at.to_rfc3339(),
                    &finished_at.to_rfc3339(),
                    &status,
                    &bounded_output,
                    &duration_ms,
                ],
            )
            .context("Failed to insert cron run")?;
            if let Some(lineage) = load_job_lineage(&mut tx, job_id)? {
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    job_id,
                    &lineage,
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
            tx.execute(
                "DELETE FROM cron_runs
                 WHERE job_id = $1
                   AND id NOT IN (
                     SELECT id FROM cron_runs
                     WHERE job_id = $1
                     ORDER BY started_at DESC, id DESC
                     LIMIT $2
                   )",
                &[&job_id, &keep],
            )
            .context("Failed to prune cron run history")?;
            tx.commit().context("Failed to commit cron run transaction")?;
            Ok(())
        })
    }

    pub fn list_runs(&self, job_id: &str, limit: usize) -> Result<Vec<CronRun>> {
        let lim = i64::try_from(limit.max(1)).context("Run history limit overflow")?;
        self.with_client(|client| {
            let rows = client
                .query(
                    "SELECT id, job_id, started_at, finished_at, status, output, duration_ms, attempt_id, worker_id
                     FROM cron_runs WHERE job_id = $1 ORDER BY started_at DESC, id DESC LIMIT $2",
                    &[&job_id, &lim],
                )
                .context("Failed to list cron runs")?;
            rows.iter()
                .map(|row| {
                    Ok(CronRun {
                        id: row.try_get::<_, i64>(0)?,
                        job_id: row.try_get(1)?,
                        started_at: parse_rfc3339(&row.try_get::<_, String>(2)?)?,
                        finished_at: parse_rfc3339(&row.try_get::<_, String>(3)?)?,
                        status: row.try_get(4)?,
                        output: row.try_get(5)?,
                        duration_ms: row.try_get(6)?,
                        attempt_id: row.try_get(7)?,
                        worker_id: row.try_get(8)?,
                    })
                })
                .collect()
        })
    }

    pub fn list_job_events(&self, job_id: &str) -> Result<Vec<CronJobEvent>> {
        self.with_client(|client| {
            let rows = client
                .query(
                    "SELECT id, event_id, job_id, workspace_id, owner_id, topic_id, parent_task_id,
                            source_message_event_id, event_type, status, payload_json, created_at
                     FROM cron_job_events WHERE job_id = $1 ORDER BY id ASC",
                    &[&job_id],
                )
                .context("Failed to list cron job events")?;
            rows.iter().map(map_job_event_row).collect()
        })
    }
}

const SELECT_JOB_COLUMNS: &str = "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
            expression, command, schedule, job_type, prompt, name, session_target, model,
            enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
            terminal_state, approval_grant_json, claim_owner, attempt_id, claimed_at, claim_expires_at
     FROM cron_jobs";

/// Lineage + last-known status of a job, used to enrich emitted events.
struct JobLineage {
    owner_id: Option<String>,
    topic_id: Option<String>,
    parent_task_id: Option<String>,
    source_message_event_id: Option<String>,
    status: Option<String>,
}

impl JobLineage {
    fn from_create(lineage: &CronJobLineage) -> Self {
        Self {
            owner_id: lineage.owner_id.clone(),
            topic_id: lineage.topic_id.clone(),
            parent_task_id: lineage.parent_task_id.clone(),
            source_message_event_id: lineage.source_message_event_id.clone(),
            status: Some("pending".to_string()),
        }
    }
}

fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed =
        DateTime::parse_from_rfc3339(raw).with_context(|| format!("Invalid RFC3339 timestamp in cron DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

fn map_cron_job_row(row: &Row) -> Result<CronJob> {
    let expression: String = row.try_get(5)?;
    let schedule_raw: Option<String> = row.try_get(7)?;
    let schedule = decode_schedule(schedule_raw.as_deref(), &expression)?;
    let delivery_raw: Option<String> = row.try_get(14)?;
    let delivery = decode_delivery(delivery_raw.as_deref())?;
    let created_at_raw: String = row.try_get(16)?;
    let next_run_raw: String = row.try_get(17)?;
    let last_run_raw: Option<String> = row.try_get(18)?;

    Ok(CronJob {
        id: row.try_get(0)?,
        owner_id: row.try_get(1)?,
        topic_id: row.try_get(2)?,
        parent_task_id: row.try_get(3)?,
        source_message_event_id: row.try_get(4)?,
        expression,
        schedule,
        command: row.try_get(6)?,
        job_type: JobType::parse(&row.try_get::<_, String>(8)?),
        prompt: row.try_get(9)?,
        name: row.try_get(10)?,
        session_target: SessionTarget::parse(&row.try_get::<_, String>(11)?),
        model: row.try_get(12)?,
        enabled: row.try_get(13)?,
        delivery,
        delete_after_run: row.try_get(15)?,
        created_at: parse_rfc3339(&created_at_raw)?,
        next_run: parse_rfc3339(&next_run_raw)?,
        last_run: match last_run_raw {
            Some(raw) => Some(parse_rfc3339(&raw)?),
            None => None,
        },
        last_status: row.try_get(19)?,
        last_output: row.try_get(20)?,
        terminal_state: row
            .try_get::<_, Option<String>>(21)?
            .map(|raw| CronJobTerminalState::parse(&raw))
            .transpose()?,
        approval_grant_json: row.try_get(22)?,
        claim: decode_postgres_claim(row, 23)?,
    })
}

fn decode_postgres_claim(row: &Row, start: usize) -> Result<Option<CronClaim>> {
    let owner: Option<String> = row.try_get(start)?;
    let attempt: Option<String> = row.try_get(start + 1)?;
    let claimed: Option<String> = row.try_get(start + 2)?;
    let expires: Option<String> = row.try_get(start + 3)?;
    match (owner, attempt, claimed, expires) {
        (None, None, None, None) => Ok(None),
        (Some(worker_id), Some(attempt_id), Some(claimed_at), Some(expires_at)) => Ok(Some(CronClaim {
            worker_id,
            attempt_id,
            claimed_at: parse_rfc3339(&claimed_at)?,
            expires_at: parse_rfc3339(&expires_at)?,
        })),
        _ => anyhow::bail!("partial cron claim tuple"),
    }
}

fn map_job_event_row(row: &Row) -> Result<CronJobEvent> {
    let created_at_raw: String = row.try_get(11)?;
    Ok(CronJobEvent {
        id: row.try_get(0)?,
        event_id: row.try_get(1)?,
        job_id: row.try_get(2)?,
        workspace_id: row.try_get(3)?,
        owner_id: row.try_get(4)?,
        topic_id: row.try_get(5)?,
        parent_task_id: row.try_get(6)?,
        source_message_event_id: row.try_get(7)?,
        event_type: row.try_get(8)?,
        status: row.try_get(9)?,
        payload_json: row.try_get(10)?,
        created_at: parse_rfc3339(&created_at_raw)?,
    })
}

fn load_job_lineage(tx: &mut postgres::Transaction<'_>, job_id: &str) -> Result<Option<JobLineage>> {
    let row = tx
        .query_opt(
            "SELECT owner_id, topic_id, parent_task_id, source_message_event_id, last_status
             FROM cron_jobs WHERE id = $1",
            &[&job_id],
        )
        .context("Failed to load cron job lineage")?;
    match row {
        Some(row) => Ok(Some(JobLineage {
            owner_id: row.try_get(0)?,
            topic_id: row.try_get(1)?,
            parent_task_id: row.try_get(2)?,
            source_message_event_id: row.try_get(3)?,
            status: row.try_get(4)?,
        })),
        None => Ok(None),
    }
}

fn insert_job_event(
    tx: &mut postgres::Transaction<'_>,
    workspace_id: &str,
    job_id: &str,
    lineage: &JobLineage,
    event_type: &str,
    status: Option<&str>,
    payload_json: Option<&str>,
) -> Result<()> {
    let event_id = Uuid::new_v4().to_string();
    let created_at = Utc::now().to_rfc3339();
    let params: [&(dyn ToSql + Sync); 11] = [
        &event_id,
        &job_id,
        &workspace_id,
        &lineage.owner_id,
        &lineage.topic_id,
        &lineage.parent_task_id,
        &lineage.source_message_event_id,
        &event_type,
        &status,
        &payload_json,
        &created_at,
    ];
    tx.execute(
        "INSERT INTO cron_job_events (
            event_id, job_id, workspace_id, owner_id, topic_id, parent_task_id, source_message_event_id,
            event_type, status, payload_json, created_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        &params,
    )
    .context("Failed to insert cron job event")?;
    // Mirror into the shared (workspace-file-based) memory_events fabric for
    // cross-instance observability, matching the SQLite backend's behavior.
    if let Err(error) = crate::cron::store::mirror_cron_job_event(
        workspace_id,
        job_id,
        crate::cron::store::MirrorLineage {
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

fn init_schema(client: &mut Client) -> Result<()> {
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS cron_jobs (
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
                enabled          BOOLEAN NOT NULL DEFAULT TRUE,
                delivery         TEXT,
                delete_after_run BOOLEAN NOT NULL DEFAULT FALSE,
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
            CREATE INDEX IF NOT EXISTS idx_cron_jobs_owner ON cron_jobs(owner_id, enabled, next_run);
            CREATE INDEX IF NOT EXISTS idx_cron_jobs_topic ON cron_jobs(topic_id, enabled, next_run);
            CREATE INDEX IF NOT EXISTS idx_cron_jobs_parent ON cron_jobs(parent_task_id, id);

            CREATE TABLE IF NOT EXISTS cron_runs (
                id          BIGSERIAL PRIMARY KEY,
                job_id      TEXT NOT NULL REFERENCES cron_jobs(id) ON DELETE CASCADE,
                started_at  TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                status      TEXT NOT NULL,
                output      TEXT,
                duration_ms BIGINT
                ,attempt_id TEXT
                ,worker_id TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_cron_runs_job_id ON cron_runs(job_id);
            CREATE INDEX IF NOT EXISTS idx_cron_runs_job_started ON cron_runs(job_id, started_at);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_cron_runs_job_attempt
                ON cron_runs(job_id, attempt_id) WHERE attempt_id IS NOT NULL;

            CREATE TABLE IF NOT EXISTS cron_job_events (
                id             BIGSERIAL PRIMARY KEY,
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
        .context("Failed to initialize cron postgres schema")?;
    client
        .batch_execute(TERMINAL_STATE_MIGRATION_SQL)
        .context("Failed to migrate cron postgres terminal state")?;
    client
        .batch_execute(CLAIM_LEASE_MIGRATION_SQL)
        .context("Failed to migrate cron postgres claim lease")?;
    Ok(())
}

/// Resolve a [`PostgresCronStore`] when the workspace storage provider is
/// `postgres` with a usable `db_url`; otherwise `None` (SQLite path is used).
pub fn resolve(config: &Config) -> Result<Option<PostgresCronStore>> {
    let provider = &config.storage.provider.config;
    if !provider.provider.trim().eq_ignore_ascii_case("postgres") {
        return Ok(None);
    }
    let Some(db_url) = provider
        .db_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let store = PostgresCronStore::connect(db_url, provider.connect_timeout_secs)?;
    Ok(Some(store))
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_terminal_schema_and_projection_fixture_are_aligned() {
        assert!(SELECT_JOB_COLUMNS.contains("terminal_state, approval_grant_json"));
        assert!(SELECT_JOB_COLUMNS.contains("claim_owner, attempt_id, claimed_at, claim_expires_at"));
        assert!(CLAIM_LEASE_MIGRATION_SQL.contains("ADD COLUMN IF NOT EXISTS claim_owner"));
        assert!(CLAIM_LEASE_MIGRATION_SQL.contains("ADD COLUMN IF NOT EXISTS worker_id"));
        assert!(CLAIM_LEASE_MIGRATION_SQL.contains("WHERE attempt_id IS NOT NULL"));
        assert!(TERMINAL_STATE_MIGRATION_SQL.contains("ADD COLUMN IF NOT EXISTS terminal_state"));
        assert!(TERMINAL_STATE_MIGRATION_SQL.contains("last_run IS NOT NULL"));
        assert!(TERMINAL_STATE_MIGRATION_SQL.contains("last_status IN ('ok', 'error')"));
        assert!(TERMINAL_STATE_MIGRATION_SQL.contains("enabled = FALSE"));
        assert_eq!(
            CronJobTerminalState::parse(CronJobTerminalState::Succeeded.as_str()).unwrap(),
            CronJobTerminalState::Succeeded
        );
        let source = include_str!("postgres.rs");
        for parameter in ["$12", "$11", "$10"] {
            let predicate = format!("{} < {parameter}::timestamptz", "clock_timestamp()");
            assert!(
                source.contains(&predicate),
                "fenced PostgreSQL finish UPDATE must contain atomic DB-clock predicate {predicate}"
            );
        }
        assert!(source.contains("claim_owner IS NULL AND attempt_id IS NULL"));
        assert!(source.contains("claim_expires_at::timestamptz <= $1::timestamptz"));
        assert!(
            source.contains("SET last_status = $6,"),
            "PostgreSQL abandon must restore the pre-claim last_status snapshot"
        );
        assert_eq!(
            CronJobTerminalState::parse(CronJobTerminalState::Failed.as_str()).unwrap(),
            CronJobTerminalState::Failed
        );
    }

    /// End-to-end cron lifecycle against a real PostgreSQL instance. Gated on
    /// `OPENPRX_TEST_POSTGRES_URL`; a no-op (returns early) when unset so the
    /// suite stays green without a database, mirroring the memory backend tests.
    #[test]
    fn postgres_cron_lifecycle_from_env() {
        let Ok(db_url) = std::env::var("OPENPRX_TEST_POSTGRES_URL") else {
            return;
        };
        let store = PostgresCronStore::connect(&db_url, Some(5)).expect("test: connect cron postgres");

        store
            .with_client(|client| {
                client.batch_execute(
                    "DROP TABLE IF EXISTS cron_job_events;
                     DROP TABLE IF EXISTS cron_runs;
                     DROP TABLE IF EXISTS cron_jobs;
                     CREATE TABLE cron_jobs (
                        id TEXT PRIMARY KEY,
                        owner_id TEXT,
                        topic_id TEXT,
                        parent_task_id TEXT,
                        source_message_event_id TEXT,
                        expression TEXT NOT NULL,
                        command TEXT NOT NULL,
                        schedule TEXT,
                        job_type TEXT NOT NULL DEFAULT 'shell',
                        prompt TEXT,
                        name TEXT,
                        session_target TEXT NOT NULL DEFAULT 'isolated',
                        model TEXT,
                        enabled BOOLEAN NOT NULL DEFAULT TRUE,
                        delivery TEXT,
                        delete_after_run BOOLEAN NOT NULL DEFAULT FALSE,
                        created_at TEXT NOT NULL,
                        next_run TEXT NOT NULL,
                        last_run TEXT,
                        last_status TEXT,
                        last_output TEXT,
                        approval_grant_json TEXT
                     );",
                )?;
                let at = Utc::now() - chrono::Duration::hours(1);
                let schedule_json = serde_json::to_string(&Schedule::At { at })?;
                for (id, status) in [
                    ("legacy-at-ok", Some("ok")),
                    ("legacy-at-error", Some("error")),
                    ("legacy-at-running", Some("running")),
                    ("legacy-at-unknown", Some("unknown")),
                    ("legacy-at-null", None),
                ] {
                    client.execute(
                        "INSERT INTO cron_jobs (
                            id, expression, command, schedule, created_at, next_run, last_run, last_status
                         ) VALUES ($1, '', 'echo legacy', $2, $3, $3, $3, $4)",
                        &[&id, &schedule_json, &at.to_rfc3339(), &status],
                    )?;
                }
                init_schema(client)?;
                let rows = client.query("SELECT id, terminal_state, enabled FROM cron_jobs ORDER BY id", &[])?;
                let states = rows
                    .iter()
                    .map(|row| {
                        (
                            row.get::<_, String>(0),
                            (row.get::<_, Option<String>>(1), row.get::<_, bool>(2)),
                        )
                    })
                    .collect::<std::collections::HashMap<_, _>>();
                assert_eq!(states["legacy-at-ok"], (Some("succeeded".to_string()), false));
                assert_eq!(states["legacy-at-error"], (Some("failed".to_string()), false));
                assert_eq!(states["legacy-at-running"], (None, true));
                assert_eq!(states["legacy-at-unknown"], (None, true));
                assert_eq!(states["legacy-at-null"], (None, true));
                Ok(())
            })
            .expect("test: migrate legacy terminal state");

        // Isolate from prior runs: drop and recreate the cron tables.
        store
            .with_client(|client| {
                client
                    .batch_execute(
                        "DROP TABLE IF EXISTS cron_job_events;
                         DROP TABLE IF EXISTS cron_runs;
                         DROP TABLE IF EXISTS cron_jobs;",
                    )
                    .context("test: drop cron tables")?;
                init_schema(client)
            })
            .expect("test: reset schema");

        let ws = "/tmp/prx-cron-pg-test";
        let lineage = CronJobLineage {
            owner_id: Some("owner-pg".to_string()),
            topic_id: Some("topic-pg".to_string()),
            parent_task_id: Some("parent-pg".to_string()),
            source_message_event_id: Some("msg-pg".to_string()),
        };

        // Insert a shell job and verify round-trip + created event.
        let job = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-shell".to_string()),
                Schedule::Cron {
                    expr: "*/5 * * * *".to_string(),
                    tz: None,
                },
                "echo pg",
                None,
                false,
                lineage,
            )
            .expect("test: add shell job");
        assert_eq!(job.command, "echo pg");
        assert_eq!(job.owner_id.as_deref(), Some("owner-pg"));

        let listed = store.list_jobs().expect("test: list jobs");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, job.id);

        // Atomic claim succeeds once, then the job is in `running` state.
        assert!(store.claim_job(ws, &job.id).expect("test: claim"));

        // Record a run + last-run and reschedule.
        let started = Utc::now();
        store
            .record_run(
                ws,
                &job.id,
                started,
                started + chrono::Duration::milliseconds(7),
                "ok",
                Some("done"),
                7,
                50,
            )
            .expect("test: record run");
        store
            .reschedule_after_run(ws, &job, true, "done")
            .expect("test: reschedule");

        let runs = store.list_runs(&job.id, 10).expect("test: list runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "ok");

        // Disable via patch and confirm a disabled event is emitted.
        let patched = store
            .update_job(
                ws,
                &job.id,
                CronJobPatch {
                    enabled: Some(false),
                    ..CronJobPatch::default()
                },
            )
            .expect("test: update job");
        assert!(!patched.enabled);

        let events = store.list_job_events(&job.id).expect("test: list events");
        let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&"cron.job.created"));
        assert!(types.contains(&"cron.job.claimed"));
        assert!(types.contains(&"cron.job.run_recorded"));
        assert!(types.contains(&"cron.job.rescheduled"));
        assert!(types.contains(&"cron.job.disabled"));
        assert!(events.iter().all(|e| e.owner_id.as_deref() == Some("owner-pg")));

        // due_jobs excludes a disabled job even far in the future.
        let far_future = Utc::now() + chrono::Duration::days(365);
        assert!(store.due_jobs(far_future, 10).expect("test: due").is_empty());

        let at = Utc::now() + chrono::Duration::minutes(10);
        let retained_one_shot = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-retained-one-shot".to_string()),
                Schedule::At { at },
                "echo once",
                None,
                false,
                CronJobLineage::default(),
            )
            .expect("test: add retained one-shot");
        assert!(
            store
                .claim_job_if_current(
                    ws,
                    &retained_one_shot,
                    "pg-a",
                    Utc::now(),
                    ChronoDuration::seconds(90),
                    true
                )
                .expect("test: future one-shot is not scheduler-due")
                .is_none()
        );
        let started = Utc::now();
        let retained_claim = store
            .claim_job_if_current(
                ws,
                &retained_one_shot,
                "pg-a",
                started,
                ChronoDuration::seconds(90),
                false,
            )
            .expect("test: manual claim one-shot")
            .expect("test: claim handle");
        store
            .record_one_shot_terminal_run(
                ws,
                &retained_one_shot,
                &retained_claim,
                started,
                started + chrono::Duration::milliseconds(3),
                started + chrono::Duration::milliseconds(3),
                true,
                "done once",
                3,
                50,
            )
            .expect("test: terminal one-shot");
        let terminal = store.get_job(&retained_one_shot.id).expect("test: reload one-shot");
        assert_eq!(terminal.terminal_state, Some(CronJobTerminalState::Succeeded));
        assert!(
            store
                .due_jobs(at + chrono::Duration::days(1), 10)
                .expect("test: due after terminal")
                .iter()
                .all(|job| job.id != retained_one_shot.id)
        );
        store
            .remove_job(ws, &retained_one_shot.id)
            .expect("test: remove retained one-shot");

        let stale_at = Utc::now() + chrono::Duration::minutes(20);
        let stale = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-stale-snapshot".to_string()),
                Schedule::At { at: stale_at },
                "echo stale",
                None,
                false,
                CronJobLineage::default(),
            )
            .expect("test: add stale snapshot fixture");
        let rearmed_at = Utc::now() + chrono::Duration::minutes(30);
        let current = store
            .update_job(
                ws,
                &stale.id,
                CronJobPatch {
                    schedule: Some(Schedule::At { at: rearmed_at }),
                    ..CronJobPatch::default()
                },
            )
            .expect("test: rearm snapshot fixture");
        let due_by = rearmed_at + chrono::Duration::seconds(1);
        assert!(
            store
                .claim_job_if_current(ws, &stale, "pg-a", due_by, ChronoDuration::seconds(90), true)
                .expect("test: stale snapshot claim")
                .is_none()
        );
        let current_claim = store
            .claim_job_if_current(ws, &current, "pg-a", due_by, ChronoDuration::seconds(90), true)
            .expect("test: current snapshot claim")
            .expect("test: claim handle");
        let in_flight_update = store.update_job(
            ws,
            &current.id,
            CronJobPatch {
                schedule: Some(Schedule::At {
                    at: Utc::now() + chrono::Duration::minutes(40),
                }),
                ..CronJobPatch::default()
            },
        );
        assert!(
            in_flight_update
                .expect_err("test: in-flight At update must fail")
                .to_string()
                .contains("active claim lease")
        );
        let started = Utc::now();
        store
            .record_one_shot_terminal_run(
                ws,
                &current,
                &current_claim,
                started,
                started,
                started,
                true,
                "done",
                0,
                50,
            )
            .expect("test: terminal current snapshot");
        assert_eq!(store.list_runs(&current.id, 10).expect("test: current runs").len(), 1);
        let final_at = Utc::now() + chrono::Duration::minutes(50);
        let final_plan = store
            .update_job(
                ws,
                &current.id,
                CronJobPatch {
                    schedule: Some(Schedule::At { at: final_at }),
                    ..CronJobPatch::default()
                },
            )
            .expect("test: rearm terminal snapshot");
        assert_eq!(final_plan.terminal_state, None);
        assert_eq!(final_plan.next_run, final_at);
        store
            .remove_job(ws, &current.id)
            .expect("test: remove snapshot fixture");

        let toggle_at = Utc::now() + chrono::Duration::minutes(60);
        let toggle_job = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-retention-toggle".to_string()),
                Schedule::At { at: toggle_at },
                "echo toggle",
                None,
                true,
                CronJobLineage::default(),
            )
            .expect("test: add retention toggle fixture");
        let toggle_claim = store
            .claim_job_if_current(ws, &toggle_job, "pg-a", Utc::now(), ChronoDuration::seconds(90), false)
            .expect("test: claim retention toggle fixture")
            .expect("test: claim handle");
        let toggled = store
            .update_job(
                ws,
                &toggle_job.id,
                CronJobPatch {
                    delete_after_run: Some(false),
                    ..CronJobPatch::default()
                },
            )
            .expect("test: toggle retention while running");
        assert!(!toggled.delete_after_run);
        let started = Utc::now();
        assert!(
            !store
                .record_one_shot_terminal_run(
                    ws,
                    &toggle_job,
                    &toggle_claim,
                    started,
                    started,
                    started,
                    true,
                    "done",
                    0,
                    50
                )
                .expect("test: retain toggled terminal")
        );
        assert_eq!(
            store
                .get_job(&toggle_job.id)
                .expect("test: retained toggled terminal")
                .terminal_state,
            Some(CronJobTerminalState::Succeeded)
        );
        store
            .remove_job(ws, &toggle_job.id)
            .expect("test: remove retention toggle fixture");

        let delete_at = Utc::now() + chrono::Duration::minutes(70);
        let delete_stale = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-delete-race".to_string()),
                Schedule::At { at: delete_at },
                "echo delete",
                None,
                true,
                CronJobLineage::default(),
            )
            .expect("test: add delete race fixture");
        let delete_rearmed_at = Utc::now() + chrono::Duration::minutes(80);
        let delete_current = store
            .update_job(
                ws,
                &delete_stale.id,
                CronJobPatch {
                    schedule: Some(Schedule::At { at: delete_rearmed_at }),
                    ..CronJobPatch::default()
                },
            )
            .expect("test: rearm before delete claim");
        assert!(
            store
                .claim_job_if_current(
                    ws,
                    &delete_stale,
                    "pg-a",
                    delete_rearmed_at + chrono::Duration::seconds(1),
                    ChronoDuration::seconds(90),
                    true,
                )
                .expect("test: stale delete claim")
                .is_none()
        );
        let started = Utc::now();
        let delete_claim = store
            .claim_job_if_current(ws, &delete_current, "pg-a", started, ChronoDuration::seconds(90), false)
            .expect("test: manual current delete claim")
            .expect("test: claim handle");
        assert!(
            store
                .record_one_shot_terminal_run(
                    ws,
                    &delete_current,
                    &delete_claim,
                    started,
                    started,
                    started,
                    true,
                    "done",
                    0,
                    50
                )
                .expect("test: atomic terminal delete")
        );
        assert!(store.get_job(&delete_current.id).is_err());
        assert!(
            store
                .update_job(
                    ws,
                    &delete_current.id,
                    CronJobPatch {
                        schedule: Some(Schedule::At {
                            at: Utc::now() + chrono::Duration::minutes(90),
                        }),
                        ..CronJobPatch::default()
                    }
                )
                .is_err()
        );

        // Fixed-time lease recovery and fencing against a real PostgreSQL row.
        let lease_at = Utc::now() + ChronoDuration::minutes(100);
        let lease_job = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-lease".into()),
                Schedule::At { at: lease_at },
                "echo lease",
                None,
                false,
                CronJobLineage::default(),
            )
            .expect("test: add lease fixture");
        let claim_a = store
            .claim_job_if_current(
                ws,
                &lease_job,
                "pg-worker-a",
                lease_at,
                ChronoDuration::seconds(30),
                false,
            )
            .expect("test: claim A")
            .expect("test: claim A handle");
        assert!(
            store
                .claim_job_if_current(
                    ws,
                    &lease_job,
                    "pg-worker-b",
                    claim_a.expires_at - ChronoDuration::nanoseconds(1),
                    ChronoDuration::seconds(30),
                    false
                )
                .expect("test: before expiry")
                .is_none()
        );
        let second_store = PostgresCronStore::connect(&db_url, Some(5)).expect("test: second cron postgres client");
        let claim_b = second_store
            .claim_job_if_current(
                ws,
                &lease_job,
                "pg-worker-b",
                claim_a.expires_at,
                ChronoDuration::seconds(30),
                false,
            )
            .expect("test: recover B")
            .expect("test: claim B handle");
        assert!(
            store
                .record_one_shot_terminal_run(
                    ws,
                    &lease_job,
                    &claim_a,
                    claim_a.claimed_at,
                    claim_a.expires_at,
                    claim_a.expires_at,
                    true,
                    "stale",
                    0,
                    50
                )
                .is_err()
        );
        assert!(
            store
                .record_one_shot_terminal_run(
                    ws,
                    &lease_job,
                    &claim_a,
                    claim_a.claimed_at,
                    claim_a.expires_at,
                    claim_a.expires_at,
                    false,
                    "stale failure",
                    0,
                    50
                )
                .is_err()
        );
        assert!(
            store
                .list_runs(&lease_job.id, 10)
                .expect("test: fenced runs")
                .is_empty()
        );
        let renewed_b = second_store
            .renew_job_claim(
                &lease_job.id,
                &claim_b,
                claim_b.claimed_at + ChronoDuration::seconds(10),
                ChronoDuration::seconds(30),
            )
            .expect("test: renew B")
            .expect("test: renewed B handle");
        second_store
            .record_one_shot_terminal_run(
                ws,
                &lease_job,
                &renewed_b,
                renewed_b.claimed_at,
                claim_b.expires_at + ChronoDuration::seconds(1),
                claim_b.expires_at + ChronoDuration::seconds(1),
                true,
                "fresh",
                0,
                50,
            )
            .expect("test: finish B");
        let terminal_rerun = second_store
            .get_job(&lease_job.id)
            .expect("test: terminal rerun snapshot");
        let rerun_claim = second_store
            .claim_terminal_job_for_manual_rerun(
                ws,
                &terminal_rerun,
                "pg-manual-rerun",
                Utc::now(),
                ChronoDuration::seconds(30),
            )
            .expect("test: terminal rerun claim")
            .expect("test: terminal rerun claim handle");
        assert!(
            second_store
                .update_job_at(
                    ws,
                    &terminal_rerun.id,
                    CronJobPatch {
                        schedule: Some(Schedule::At {
                            at: lease_at + ChronoDuration::minutes(10),
                        }),
                        ..CronJobPatch::default()
                    },
                    rerun_claim.claimed_at,
                )
                .is_err()
        );
        second_store
            .record_terminal_manual_run(
                ws,
                &terminal_rerun,
                &rerun_claim,
                rerun_claim.claimed_at,
                rerun_claim.claimed_at,
                true,
                "rerun done",
                0,
                50,
            )
            .expect("test: terminal rerun fenced audit");

        let recurring = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-recurring-lease".into()),
                Schedule::Cron {
                    expr: "*/5 * * * *".into(),
                    tz: None,
                },
                "echo recurring",
                None,
                false,
                CronJobLineage::default(),
            )
            .expect("test: add recurring lease fixture");
        let recurring_claim = store
            .claim_job_if_current(
                ws,
                &recurring,
                "pg-worker-a",
                lease_at,
                ChronoDuration::seconds(30),
                false,
            )
            .expect("test: recurring claim")
            .expect("test: recurring claim handle");
        store
            .finish_claimed_run(
                ws,
                &recurring,
                &recurring_claim,
                lease_at,
                lease_at + ChronoDuration::seconds(1),
                lease_at + ChronoDuration::seconds(1),
                true,
                "recurring done",
                1_000,
                false,
                true,
                50,
            )
            .expect("test: recurring fenced finish");

        let lock_wait_job = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-lock-wait".into()),
                Schedule::Cron {
                    expr: "*/5 * * * *".into(),
                    tz: None,
                },
                "echo lock-wait",
                None,
                false,
                CronJobLineage::default(),
            )
            .expect("test: add lock-wait fixture");
        let lock_claimed_at = Utc::now();
        let lock_claim = store
            .claim_job_if_current(
                ws,
                &lock_wait_job,
                "pg-lock-worker",
                lock_claimed_at,
                ChronoDuration::milliseconds(500),
                false,
            )
            .expect("test: lock-wait claim")
            .expect("test: lock-wait claim handle");
        let thread_url = db_url;
        let thread_job = lock_wait_job.clone();
        let thread_claim = lock_claim;
        let lock_wait_result = store
            .with_client(|client| {
                let mut tx = client.transaction()?;
                tx.query_one(
                    "SELECT id FROM cron_jobs WHERE id = $1 FOR UPDATE",
                    &[&lock_wait_job.id],
                )?;
                let handle = std::thread::spawn(move || {
                    let finisher = PostgresCronStore::connect(&thread_url, Some(5))?;
                    finisher.finish_claimed_run(
                        ws,
                        &thread_job,
                        &thread_claim,
                        lock_claimed_at,
                        lock_claimed_at + ChronoDuration::milliseconds(10),
                        lock_claimed_at + ChronoDuration::milliseconds(20),
                        true,
                        "must fence after pg lock wait",
                        10,
                        false,
                        false,
                        50,
                    )
                });
                std::thread::sleep(std::time::Duration::from_millis(800));
                tx.commit()?;
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("test: pg finish thread panicked"))?
            })
            .expect_err("test: DB clock after row lock must observe expiry");
        assert!(lock_wait_result.to_string().contains("expired"));

        let partial = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-partial-claim".into()),
                Schedule::Cron {
                    expr: "*/5 * * * *".into(),
                    tz: None,
                },
                "echo partial",
                None,
                false,
                CronJobLineage::default(),
            )
            .expect("test: add partial tuple fixture");
        store
            .with_client(|client| {
                client.execute(
                    "UPDATE cron_jobs SET claim_owner = 'partial-only' WHERE id = $1",
                    &[&partial.id],
                )?;
                Ok(())
            })
            .expect("test: write partial claim tuple");
        assert!(store.get_job(&partial.id).is_err());

        let due_a = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-due-claimed".into()),
                Schedule::Cron {
                    expr: "*/5 * * * *".into(),
                    tz: None,
                },
                "echo due-a",
                None,
                false,
                CronJobLineage::default(),
            )
            .expect("test: add claimed due fixture");
        let due_b = store
            .add_shell_job_with_lineage_approval_and_delete(
                ws,
                Some("pg-due-ready".into()),
                Schedule::Cron {
                    expr: "*/5 * * * *".into(),
                    tz: None,
                },
                "echo due-b",
                None,
                false,
                CronJobLineage::default(),
            )
            .expect("test: add ready due fixture");
        let due_now = Utc::now();
        store
            .with_client(|client| {
                client.execute(
                    "UPDATE cron_jobs SET next_run = CASE id
                         WHEN $1 THEN $2 WHEN $3 THEN $4 ELSE $5 END
                     WHERE id IN ($1, $3, $6)",
                    &[
                        &partial.id,
                        &(due_now - ChronoDuration::seconds(3)).to_rfc3339(),
                        &due_a.id,
                        &(due_now - ChronoDuration::seconds(2)).to_rfc3339(),
                        &(due_now - ChronoDuration::seconds(1)).to_rfc3339(),
                        &due_b.id,
                    ],
                )?;
                Ok(())
            })
            .expect("test: make fixtures due");
        let due_a = store.get_job(&due_a.id).expect("test: reload claimed due fixture");
        store
            .claim_job_if_current(ws, &due_a, "pg-due-worker", due_now, ChronoDuration::seconds(30), false)
            .expect("test: claim first due fixture")
            .expect("test: first due claim handle");
        let due = second_store
            .due_jobs(due_now, 1)
            .expect("test: due jobs without starvation");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, due_b.id);

        // Remove cascades the run history.
        store.remove_job(ws, &job.id).expect("test: remove");
        store.remove_job(ws, &lease_job.id).expect("test: remove lease fixture");
        store
            .remove_job(ws, &recurring.id)
            .expect("test: remove recurring fixture");
        store
            .remove_job(ws, &lock_wait_job.id)
            .expect("test: remove lock-wait fixture");
        store.remove_job(ws, &partial.id).expect("test: remove partial fixture");
        store
            .remove_job(ws, &due_a.id)
            .expect("test: remove claimed due fixture");
        store.remove_job(ws, &due_b.id).expect("test: remove ready due fixture");
        assert!(store.list_jobs().expect("test: list after remove").is_empty());
        assert!(
            store
                .list_runs(&job.id, 10)
                .expect("test: runs after remove")
                .is_empty()
        );
    }
}
