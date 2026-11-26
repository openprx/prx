use super::{MAX_OUTPUT_BYTES, TRUNCATED_MARKER};
use crate::config::Config;
use crate::cron::ScheduledContext;
use crate::tools::execution::JobOrigin;
use crate::xin::types::{ExecutionMode, TaskKind, TaskPriority, TaskStatus, XinTask, XinTaskEvent};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use uuid::Uuid;

// ── Helpers ─────────────────────────────────────────────────────────────

pub(super) const SELECT_ALL_COLUMNS: &str = "SELECT id, owner_id, topic_id, parent_task_id, source_message_event_id,
            name, description, kind, status, priority, execution_mode,
            payload, recurring, interval_secs, created_at, updated_at,
            last_run_at, next_run_at, last_status, last_output,
            run_count, fail_count, max_failures, enabled, approval_grant_json, origin, context
     FROM xin_tasks WHERE id = ?1";

/// Redact and bound a task's output before the store persists it.
///
/// An agent task's output is the agent turn's reply (or its error), so it is
/// redacted like the terminal commit (D-053) before it is capped.
pub(super) fn stored_task_output(output: &str) -> String {
    let redacted = crate::agent::terminal::redact_persisted_text(output);
    let output = redacted.as_str();
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

pub(super) fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed =
        DateTime::parse_from_rfc3339(raw).with_context(|| format!("Invalid RFC3339 timestamp in xin DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

pub(super) fn sql_err(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.into())
}

#[derive(Debug, Clone)]
pub(super) struct TaskLineage {
    pub(super) owner_id: Option<String>,
    pub(super) topic_id: Option<String>,
    pub(super) parent_task_id: Option<String>,
    pub(super) source_message_event_id: Option<String>,
    pub(super) status: Option<String>,
}

pub(super) fn workspace_id(config: &Config) -> String {
    config.workspace_dir.to_string_lossy().to_string()
}

pub(super) fn load_task_lineage(conn: &Connection, task_id: &str) -> Result<Option<TaskLineage>> {
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

pub(super) fn insert_task_event(
    conn: &Connection,
    workspace_id: &str,
    task_id: &str,
    lineage: TaskLineage,
    event_type: &str,
    status: Option<&str>,
    payload_json: Option<&str>,
) -> Result<()> {
    crate::cron::agent_jobs::remember_xin(conn, task_id)?;
    let event_id = Uuid::new_v4().to_string();
    let created_at = Utc::now().to_rfc3339();
    crate::runtime::sqlite_pool::in_write_scope(conn, || {
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
    })
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
pub(super) fn deliver_pending_xin_event_outbox(conn: &Connection) -> Result<usize> {
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
                let error = stored_task_output(&error.to_string());
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

pub(super) fn map_task_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<XinTaskEvent> {
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

pub(super) fn map_task_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<XinTask> {
    let kind = TaskKind::from_str_lossy(&row.get::<_, String>(7)?);
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
        kind: kind.clone(),
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
        origin: stored_origin(row.get::<_, Option<String>>(25)?.as_deref(), &kind),
        context: ScheduledContext::parse(&row.get::<_, String>(26)?),
    })
}

/// Decode a stored task or goal origin (CRON-ORIGIN migration rule).
///
/// A row written before the `origin` column existed is attributed to the local
/// operator only when it is a `system` task or goal: those are written solely
/// by `ensure_system_task` from the built-in definitions and `HEARTBEAT.md`
/// (and by adopting such a task), never by the `xin` tool or the `prx xin` CLI,
/// which both hard-code `user`. Every other legacy row reads as
/// [`JobOrigin::Unattributed`] and never runs with operator authority.
pub(super) fn stored_origin(raw: Option<&str>, kind: &TaskKind) -> JobOrigin {
    if raw.is_none() && *kind == TaskKind::System {
        return JobOrigin::LocalOperator;
    }
    JobOrigin::from_stored(raw)
}
