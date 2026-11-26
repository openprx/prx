use super::rows::deliver_pending_xin_event_outbox;
use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;

fn add_column_if_missing(conn: &Connection, table: &str, name: &str, sql_type: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        if col_name == name {
            return Ok(());
        }
    }
    drop(rows);
    drop(stmt);

    match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {sql_type}"), []) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, Some(ref msg))) if msg.contains("duplicate column name") => {
            tracing::debug!("Column {table}.{name} already exists (concurrent migration): {err}");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to add {table}.{name}")),
    }
}

pub(super) fn with_connection<T>(config: &Config, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let db_path = config.workspace_dir.join("xin").join("tasks.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create xin directory: {}", parent.display()))?;
    }

    let mut conn =
        Connection::open(&db_path).with_context(|| format!("Failed to open xin DB: {}", db_path.display()))?;

    // Concurrent task execution must queue behind the database lock, never be
    // turned into an error by it: the busy *handler* retries for as long as the
    // lock is held (warning periodically) where a `busy_timeout` would surface
    // `SQLITE_BUSY` the moment it expired.
    crate::runtime::sqlite_pool::install_busy_handler(&mut conn)?;

    conn.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA secure_delete=ON;
         CREATE TABLE IF NOT EXISTS xin_job_owners(job_id TEXT PRIMARY KEY,owner_id TEXT,creator_agent TEXT NOT NULL);

         CREATE TABLE IF NOT EXISTS xin_tasks (
            version INTEGER NOT NULL DEFAULT 1 CHECK(version > 0),
            id               TEXT PRIMARY KEY,
            owner_id         TEXT,
            topic_id         TEXT,
            parent_task_id   TEXT,
            source_message_event_id TEXT,
            name             TEXT NOT NULL,
            description      TEXT,
            kind             TEXT NOT NULL DEFAULT 'user',
            status           TEXT NOT NULL DEFAULT 'pending',
            priority         INTEGER NOT NULL DEFAULT 1,
            execution_mode   TEXT NOT NULL DEFAULT 'agent_session',
            payload          TEXT NOT NULL DEFAULT '',
            recurring        INTEGER NOT NULL DEFAULT 0,
            interval_secs    INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            updated_at       TEXT NOT NULL,
            last_run_at      TEXT,
            next_run_at      TEXT NOT NULL,
            last_status      TEXT,
            last_output      TEXT,
            run_count        INTEGER NOT NULL DEFAULT 0,
            fail_count       INTEGER NOT NULL DEFAULT 0,
            max_failures     INTEGER NOT NULL DEFAULT 0,
            enabled          INTEGER NOT NULL DEFAULT 1,
            approval_grant_json TEXT,
            lease_owner      TEXT,
            lease_epoch      INTEGER NOT NULL DEFAULT 0,
            lease_expires_at TEXT,
            last_heartbeat_at TEXT,
            origin           TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_next_run ON xin_tasks(next_run_at);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_status ON xin_tasks(status);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_kind ON xin_tasks(kind);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_xin_tasks_system_name
             ON xin_tasks(name) WHERE kind = 'system';
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_due
             ON xin_tasks(enabled, status, next_run_at, priority);

         CREATE TABLE IF NOT EXISTS xin_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id     TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            status      TEXT NOT NULL,
            output      TEXT,
            duration_ms INTEGER,
            FOREIGN KEY (task_id) REFERENCES xin_tasks(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_xin_runs_task_id ON xin_runs(task_id);
         CREATE INDEX IF NOT EXISTS idx_xin_runs_started_at ON xin_runs(started_at);

         CREATE TABLE IF NOT EXISTS xin_task_events (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id       TEXT NOT NULL UNIQUE,
            task_id        TEXT NOT NULL,
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
         CREATE INDEX IF NOT EXISTS idx_xin_task_events_task_id ON xin_task_events(task_id, id);
         CREATE INDEX IF NOT EXISTS idx_xin_task_events_owner ON xin_task_events(workspace_id, owner_id, id);
         CREATE INDEX IF NOT EXISTS idx_xin_task_events_topic ON xin_task_events(workspace_id, topic_id, id);
         CREATE INDEX IF NOT EXISTS idx_xin_task_events_type ON xin_task_events(event_type, id);

         CREATE TABLE IF NOT EXISTS xin_event_outbox (
            event_id       TEXT PRIMARY KEY,
            workspace_id   TEXT NOT NULL,
            task_id        TEXT NOT NULL,
            event_type     TEXT NOT NULL,
            payload_json   TEXT NOT NULL,
            created_at     TEXT NOT NULL,
            delivered_at   TEXT,
            attempt_count  INTEGER NOT NULL DEFAULT 0,
            last_error     TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_xin_event_outbox_pending
            ON xin_event_outbox(delivered_at, created_at);

         CREATE TABLE IF NOT EXISTS xin_goals (
            version INTEGER NOT NULL DEFAULT 1 CHECK(version > 0),
            id                      TEXT PRIMARY KEY,
            owner_id                TEXT,
            topic_id                TEXT,
            parent_task_id          TEXT,
            source_message_event_id TEXT,
            name                    TEXT NOT NULL,
            description             TEXT,
            kind                    TEXT NOT NULL DEFAULT 'user',
            status                  TEXT NOT NULL DEFAULT 'pending',
            priority                INTEGER NOT NULL DEFAULT 1,
            target_completion_at    TEXT,
            steps_completed         INTEGER NOT NULL DEFAULT 0,
            steps_total             INTEGER NOT NULL DEFAULT 0,
            created_at              TEXT NOT NULL,
            updated_at              TEXT NOT NULL,
            completed_at            TEXT,
            final_output            TEXT,
            enabled                 INTEGER NOT NULL DEFAULT 1,
            origin                  TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_xin_goals_owner  ON xin_goals(owner_id, status);
         CREATE INDEX IF NOT EXISTS idx_xin_goals_topic  ON xin_goals(topic_id, status);
         CREATE INDEX IF NOT EXISTS idx_xin_goals_status ON xin_goals(status, priority DESC);

         CREATE TABLE IF NOT EXISTS xin_steps (
            id                   TEXT PRIMARY KEY,
            goal_id              TEXT NOT NULL REFERENCES xin_goals(id) ON DELETE CASCADE,
            sequence             INTEGER NOT NULL,
            name                 TEXT NOT NULL,
            description          TEXT,
            status               TEXT NOT NULL DEFAULT 'pending',
            execution_mode       TEXT NOT NULL DEFAULT 'agent_session',
            payload              TEXT NOT NULL DEFAULT '',
            lease_owner          TEXT,
            lease_epoch          INTEGER NOT NULL DEFAULT 0,
            lease_expires_at     TEXT,
            last_heartbeat_at    TEXT,
            checkpoint_json      TEXT,
            lease_ttl_secs       INTEGER NOT NULL DEFAULT 0,
            retry_count          INTEGER NOT NULL DEFAULT 0,
            max_retries          INTEGER NOT NULL DEFAULT 3,
            created_at           TEXT NOT NULL,
            updated_at           TEXT NOT NULL,
            started_at           TEXT,
            completed_at         TEXT,
            last_output          TEXT,
            approval_grant_json  TEXT,
            UNIQUE (goal_id, sequence)
         );
         CREATE INDEX IF NOT EXISTS idx_xin_steps_goal  ON xin_steps(goal_id, sequence);
         CREATE INDEX IF NOT EXISTS idx_xin_steps_due   ON xin_steps(status, lease_expires_at);
         CREATE INDEX IF NOT EXISTS idx_xin_steps_owner ON xin_steps(lease_owner, status);

         CREATE TABLE IF NOT EXISTS xin_task_adoptions (
            legacy_task_id TEXT PRIMARY KEY REFERENCES xin_tasks(id) ON DELETE CASCADE,
            goal_id        TEXT NOT NULL UNIQUE REFERENCES xin_goals(id) ON DELETE CASCADE,
            adopted_at     TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_xin_task_adoptions_goal
            ON xin_task_adoptions(goal_id);",
    )
    .context("Failed to initialize xin schema")?;

    add_column_if_missing(&conn, "xin_tasks", "owner_id", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "topic_id", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "parent_task_id", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "source_message_event_id", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "approval_grant_json", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "lease_owner", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "lease_epoch", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(&conn, "xin_tasks", "lease_expires_at", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "last_heartbeat_at", "TEXT")?;
    add_column_if_missing(&conn, "xin_task_events", "source_message_event_id", "TEXT")?;
    // CRON-ORIGIN: nullable, no backfill; see `stored_origin` for how a row
    // written before this column existed is attributed.
    add_column_if_missing(&conn, "xin_tasks", "origin", "TEXT")?;
    // D-107 amendment: which surface created the task. Nullable with no
    // backfill: a row stored before the column existed reads as
    // `agent_tool` until the operator marks it.
    add_column_if_missing(&conn, "xin_tasks", "created_via", "TEXT")?;
    add_column_if_missing(&conn, "xin_goals", "origin", "TEXT")?;
    // The creating call's level and agent. Nullable with no backfill: a row
    // without them runs at Main's level.
    add_column_if_missing(&conn, "xin_tasks", "creator_level", "TEXT")?;
    add_column_if_missing(&conn, "xin_tasks", "creator_agent", "TEXT")?;
    // Whether a scheduled run replays its session's earlier runs. Every
    // existing row reads `task`, the behaviour before this column.
    add_column_if_missing(&conn, "xin_tasks", "context", "TEXT NOT NULL DEFAULT 'task'")?;
    add_column_if_missing(&conn, "xin_goals", "creator_level", "TEXT")?;
    add_column_if_missing(&conn, "xin_goals", "creator_agent", "TEXT")?;
    // Which surface created the goal, as on `xin_tasks`. Nullable with no
    // backfill: NULL reads as `agent_tool`.
    add_column_if_missing(&conn, "xin_goals", "created_via", "TEXT")?;
    // Forward-compat for any xin_steps table created before lease_ttl_secs existed.
    add_column_if_missing(&conn, "xin_steps", "lease_ttl_secs", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(&conn, "xin_steps", "lease_epoch", "INTEGER NOT NULL DEFAULT 0")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_xin_tasks_owner ON xin_tasks(owner_id, status, next_run_at);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_topic ON xin_tasks(topic_id, status, next_run_at);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_parent ON xin_tasks(parent_task_id, id);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_lease ON xin_tasks(status, lease_expires_at);
         CREATE INDEX IF NOT EXISTS idx_xin_tasks_lease_owner ON xin_tasks(lease_owner, status);",
    )
    .context("Failed to initialize xin lineage indexes")?;

    let result = f(&conn);
    if let Err(error) = deliver_pending_xin_event_outbox(&conn) {
        tracing::warn!("failed to drain Xin event outbox: {error}");
    }
    result
}

pub(super) fn with_immediate_connection<T>(config: &Config, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    with_connection(config, |conn| {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(conn) {
            Ok(value) => {
                conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    })
}

pub(super) fn authoritative_now(conn: &Connection) -> Result<DateTime<Utc>> {
    let raw: String = conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| row.get(0))?;
    Ok(DateTime::parse_from_rfc3339(&raw)?.with_timezone(&Utc))
}
