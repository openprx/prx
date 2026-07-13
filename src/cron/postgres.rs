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
use crate::cron::store::{decode_delivery, decode_schedule, truncate_cron_output};
use crate::cron::types::{
    CronJob, CronJobEvent, CronJobLineage, CronJobPatch, CronJobTerminalState, CronRun, DeliveryConfig, JobType,
    Schedule, SessionTarget,
};
use crate::cron::{next_run_for_schedule, schedule_cron_expression, validate_schedule};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
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
        due_by: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let schedule_json = serde_json::to_string(&job.schedule)?;
        self.with_client(|client| {
            let mut tx = client
                .transaction()
                .context("failed to open cron snapshot claim transaction")?;
            let changed = if let Some(due_by) = due_by {
                tx.execute(
                    "UPDATE cron_jobs SET last_status = 'running'
                     WHERE id = $1 AND enabled = TRUE AND terminal_state IS NULL AND next_run = $2 AND next_run <= $3
                       AND (schedule = $4 OR (schedule IS NULL AND expression = $5))
                       AND (last_status IS NULL OR last_status <> 'running')",
                    &[
                        &job.id,
                        &job.next_run.to_rfc3339(),
                        &due_by.to_rfc3339(),
                        &schedule_json,
                        &job.expression,
                    ],
                )
            } else {
                tx.execute(
                    "UPDATE cron_jobs SET last_status = 'running'
                     WHERE id = $1 AND terminal_state IS NULL AND next_run = $2
                       AND (schedule = $3 OR (schedule IS NULL AND expression = $4))
                       AND (last_status IS NULL OR last_status <> 'running')",
                    &[&job.id, &job.next_run.to_rfc3339(), &schedule_json, &job.expression],
                )
            }
            .context("Failed to claim current cron job snapshot")?;
            if changed > 0
                && let Some(lineage) = load_job_lineage(&mut tx, &job.id)?
            {
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    &job.id,
                    &lineage,
                    "cron.job.claimed",
                    Some("running"),
                    None,
                )?;
            }
            tx.commit().context("failed to commit cron snapshot claim")?;
            Ok(changed > 0)
        })
    }

    pub fn due_jobs(&self, now: DateTime<Utc>, max_tasks: usize) -> Result<Vec<CronJob>> {
        let lim = i64::try_from(max_tasks.max(1)).context("Scheduler max_tasks overflows i64")?;
        self.with_client(|client| {
            let rows = client
                .query(
                    &format!(
                        "{SELECT_JOB_COLUMNS} WHERE enabled = TRUE AND terminal_state IS NULL
                         AND next_run <= $1 ORDER BY next_run ASC LIMIT $2"
                    ),
                    &[&now.to_rfc3339(), &lim],
                )
                .context("Failed to query due cron jobs")?;
            rows.iter().map(map_cron_job_row).collect()
        })
    }

    pub fn update_job(&self, workspace_id: &str, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
        let mut job = self.get_job(job_id)?;
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
                         terminal_state = $16, approval_grant_json = $17
                     WHERE id = $18 AND next_run = $19
                       AND (schedule = $20 OR (schedule IS NULL AND expression = $21))
                       AND last_status IS NOT DISTINCT FROM $22",
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
                        &job.id,
                        &expected_next_run,
                        &expected_schedule_json,
                        &expected_expression,
                        &expected_last_status,
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

    pub fn record_last_run(
        &self,
        workspace_id: &str,
        job_id: &str,
        finished_at: DateTime<Utc>,
        success: bool,
        output: &str,
    ) -> Result<()> {
        let status = if success { "ok" } else { "error" };
        let bounded_output = truncate_cron_output(output);
        self.with_client(|client| {
            let mut tx = client
                .transaction()
                .context("failed to open cron last-run transaction")?;
            tx.execute(
                "UPDATE cron_jobs SET last_run = $1, last_status = $2, last_output = $3 WHERE id = $4",
                &[&finished_at.to_rfc3339(), &status, &bounded_output, &job_id],
            )
            .context("Failed to update cron last run fields")?;
            if let Some(lineage) = load_job_lineage(&mut tx, job_id)? {
                insert_job_event(
                    &mut tx,
                    workspace_id,
                    job_id,
                    &lineage,
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
            tx.commit().context("failed to commit cron last-run")?;
            Ok(())
        })
    }

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
    pub fn record_one_shot_terminal_run(
        &self,
        workspace_id: &str,
        job: &CronJob,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
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
            tx.execute(
                "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &job.id,
                    &started_at.to_rfc3339(),
                    &finished_at.to_rfc3339(),
                    &status,
                    &bounded_output,
                    &duration_ms,
                ],
            )
            .context("Failed to insert terminal cron run")?;
            tx.execute(
                "DELETE FROM cron_runs
                 WHERE job_id = $1
                   AND id NOT IN (
                     SELECT id FROM cron_runs
                     WHERE job_id = $1
                     ORDER BY started_at DESC, id DESC
                     LIMIT $2
                   )",
                &[&job.id, &keep],
            )
            .context("Failed to prune terminal cron run history")?;
            let changed = tx
                .execute(
                    "UPDATE cron_jobs
                     SET last_run = $1, last_status = $2, last_output = $3, terminal_state = $4
                     WHERE id = $5 AND terminal_state IS NULL AND next_run = $6 AND schedule = $7
                       AND last_status = 'running'",
                    &[
                        &finished_at.to_rfc3339(),
                        &status,
                        &bounded_output,
                        &terminal_state.as_str(),
                        &job.id,
                        &job.next_run.to_rfc3339(),
                        &schedule_json,
                    ],
                )
                .context("Failed to persist terminal cron job state")?;
            if changed == 0 {
                anyhow::bail!("cron one-shot '{}' was deleted or already terminal", job.id);
            }
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
                    "SELECT id, job_id, started_at, finished_at, status, output, duration_ms
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
            terminal_state, approval_grant_json
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
    })
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
                approval_grant_json TEXT
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
            );
            CREATE INDEX IF NOT EXISTS idx_cron_runs_job_id ON cron_runs(job_id);
            CREATE INDEX IF NOT EXISTS idx_cron_runs_job_started ON cron_runs(job_id, started_at);

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
        assert!(TERMINAL_STATE_MIGRATION_SQL.contains("ADD COLUMN IF NOT EXISTS terminal_state"));
        assert!(TERMINAL_STATE_MIGRATION_SQL.contains("last_run IS NOT NULL"));
        assert!(TERMINAL_STATE_MIGRATION_SQL.contains("last_status IN ('ok', 'error')"));
        assert!(TERMINAL_STATE_MIGRATION_SQL.contains("enabled = FALSE"));
        assert_eq!(
            CronJobTerminalState::parse(CronJobTerminalState::Succeeded.as_str()).unwrap(),
            CronJobTerminalState::Succeeded
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
            !store
                .claim_job_if_current(ws, &retained_one_shot, Some(Utc::now()))
                .expect("test: future one-shot is not scheduler-due")
        );
        assert!(
            store
                .claim_job_if_current(ws, &retained_one_shot, None)
                .expect("test: manual claim one-shot")
        );
        let started = Utc::now();
        store
            .record_one_shot_terminal_run(
                ws,
                &retained_one_shot,
                started,
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
            !store
                .claim_job_if_current(ws, &stale, Some(due_by))
                .expect("test: stale snapshot claim")
        );
        assert!(
            store
                .claim_job_if_current(ws, &current, Some(due_by))
                .expect("test: current snapshot claim")
        );
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
                .contains("in-flight Schedule::At")
        );
        let started = Utc::now();
        store
            .record_one_shot_terminal_run(ws, &current, started, started, true, "done", 0, 50)
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
        assert!(
            store
                .claim_job_if_current(ws, &toggle_job, None)
                .expect("test: claim retention toggle fixture")
        );
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
                .record_one_shot_terminal_run(ws, &toggle_job, started, started, true, "done", 0, 50)
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
            !store
                .claim_job_if_current(
                    ws,
                    &delete_stale,
                    Some(delete_rearmed_at + chrono::Duration::seconds(1))
                )
                .expect("test: stale delete claim")
        );
        assert!(
            store
                .claim_job_if_current(ws, &delete_current, None)
                .expect("test: manual current delete claim")
        );
        let started = Utc::now();
        assert!(
            store
                .record_one_shot_terminal_run(ws, &delete_current, started, started, true, "done", 0, 50)
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

        // Remove cascades the run history.
        store.remove_job(ws, &job.id).expect("test: remove");
        assert!(store.list_jobs().expect("test: list after remove").is_empty());
        assert!(
            store
                .list_runs(&job.id, 10)
                .expect("test: runs after remove")
                .is_empty()
        );
    }
}
