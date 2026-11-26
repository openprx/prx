use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::config::Config;
use crate::cron::{
    CronClaim, CronJob, CronJobEvent, CronJobTerminalState, DeliveryConfig, DeliveryPrincipal, JobOrigin, JobType,
    Schedule, ScheduledContext, ScheduledDeliver, SessionTarget,
};
use crate::runtime::sqlite_pool::in_write_scope;

use super::stored_cron_output;

pub(super) fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed =
        DateTime::parse_from_rfc3339(raw).with_context(|| format!("Invalid RFC3339 timestamp in cron DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

pub(super) fn sql_conversion_error(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.into())
}

pub(super) fn map_cron_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob> {
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
        delivery_principal: DeliveryPrincipal::new(row.get(27)?, row.get(28)?, row.get(29)?),
        origin: JobOrigin::from_stored(row.get::<_, Option<String>>(30)?.as_deref()),
        context: ScheduledContext::parse(&row.get::<_, String>(31)?),
        deliver: ScheduledDeliver::parse(&row.get::<_, String>(32)?),
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
pub(super) struct JobLineage {
    pub(super) owner_id: Option<String>,
    pub(super) topic_id: Option<String>,
    pub(super) parent_task_id: Option<String>,
    pub(super) source_message_event_id: Option<String>,
    pub(super) status: Option<String>,
}

pub(super) fn workspace_id(config: &Config) -> String {
    config.workspace_dir.to_string_lossy().to_string()
}

pub(super) fn load_job_lineage(conn: &Connection, job_id: &str) -> Result<Option<JobLineage>> {
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

pub(super) fn insert_job_event(
    conn: &Connection,
    workspace_id: &str,
    job_id: &str,
    lineage: JobLineage,
    event_type: &str,
    status: Option<&str>,
    payload_json: Option<&str>,
) -> Result<()> {
    crate::cron::agent_jobs::remember_cron(conn, job_id)?;
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
    in_write_scope(conn, || {
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
    })
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
pub(super) fn deliver_pending_cron_event_outbox(conn: &Connection) -> Result<usize> {
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
                    params![stored_cron_output(&error.to_string()), event_id],
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

pub(super) fn map_job_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJobEvent> {
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
