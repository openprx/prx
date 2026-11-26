use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::Connection;

use crate::config::Config;

use super::rows::deliver_pending_cron_event_outbox;

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

/// Database files whose schema batch has succeeded in this process, keyed by
/// canonical path. The batch is idempotent, so two first openers racing only
/// repeat work; a batch that fails is never recorded.
static MIGRATED_PATHS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
static WAL_SWITCH_WARNED: AtomicBool = AtomicBool::new(false);

fn migrated_paths() -> &'static Mutex<BTreeSet<PathBuf>> {
    MIGRATED_PATHS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn is_migrated(path: &Path) -> bool {
    migrated_paths().lock().contains(path)
}

fn mark_migrated(path: PathBuf) {
    migrated_paths().lock().insert(path);
}

/// `journal_mode` is persistent per file, so this is a no-op once the file is
/// WAL. A failed switch must not fail the call: the file keeps its journal mode
/// and the failure is logged once per process.
fn switch_to_wal(conn: &Connection, db_path: &Path) {
    let detail = match conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0)) {
        Ok(mode) if mode.eq_ignore_ascii_case("wal") => return,
        Ok(mode) => format!("journal_mode is {mode}"),
        Err(error) => error.to_string(),
    };
    if !WAL_SWITCH_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(path = %db_path.display(), "cron DB stays in its current journal mode: {detail}");
    }
}

pub(super) fn with_connection<T>(config: &Config, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
    let db_path = config.workspace_dir.join("cron").join("jobs.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cron directory: {}", parent.display()))?;
    }

    let mut conn =
        Connection::open(&db_path).with_context(|| format!("Failed to open cron DB: {}", db_path.display()))?;

    // Concurrent scheduler + CLI access must queue behind the database lock,
    // never be turned into an error by it: the busy *handler* retries for as
    // long as the lock is held (warning periodically) where a `busy_timeout`
    // would surface `SQLITE_BUSY` the moment it expired.
    crate::runtime::sqlite_pool::install_busy_handler(&mut conn)?;

    switch_to_wal(&conn, &db_path);
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAGMA secure_delete=ON;")
        .context("Failed to set cron connection pragmas")?;

    let migrated_key = std::fs::canonicalize(&db_path)
        .with_context(|| format!("Failed to resolve cron DB path: {}", db_path.display()))?;
    if !is_migrated(&migrated_key) {
        migrate_schema(&conn)?;
        mark_migrated(migrated_key);
    }

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

fn migrate_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cron_job_owners(job_id TEXT PRIMARY KEY,owner_id TEXT,creator_agent TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS cron_jobs (
            id               TEXT PRIMARY KEY,
            version          INTEGER NOT NULL DEFAULT 1 CHECK(version > 0),
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
            claim_expires_at TEXT,
            creator_sender   TEXT,
            creator_channel  TEXT,
            creator_chat_type TEXT,
            origin           TEXT
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

    add_column_if_missing(conn, "cron_jobs", "owner_id", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "topic_id", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "parent_task_id", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "source_message_event_id", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "schedule", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "job_type", "TEXT NOT NULL DEFAULT 'shell'")?;
    add_column_if_missing(conn, "cron_jobs", "prompt", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "name", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "session_target", "TEXT NOT NULL DEFAULT 'isolated'")?;
    add_column_if_missing(conn, "cron_jobs", "model", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "enabled", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(conn, "cron_jobs", "delivery", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "delete_after_run", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "cron_jobs", "last_run", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "last_status", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "last_output", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "terminal_state", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "approval_grant_json", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "claim_owner", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "attempt_id", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "claimed_at", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "claim_expires_at", "TEXT")?;
    // Outbound-authorization identity (T17). Nullable on purpose: a row written
    // before these columns existed, or by anything outside the tool path, keeps
    // NULL and is authorized as the least privileged caller — see
    // `DeliveryPrincipal`. Nothing is backfilled, because there is no honest
    // value to invent for a job whose creator was never recorded.
    add_column_if_missing(conn, "cron_jobs", "creator_sender", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "creator_channel", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "creator_chat_type", "TEXT")?;
    // CRON-ORIGIN: nullable with no backfill. A row stored before this column
    // existed has no provable creator and reads as `JobOrigin::Unattributed`,
    // so it never runs with operator authority; no path wrote agent jobs that
    // only a local operator could reach (the `prx cron` CLI writes shell jobs).
    add_column_if_missing(conn, "cron_jobs", "origin", "TEXT")?;
    // D-107 amendment: which surface created the job. Nullable with no
    // backfill: a row stored before the column existed reads as
    // `agent_tool` until the operator marks it.
    add_column_if_missing(conn, "cron_jobs", "created_via", "TEXT")?;
    // The creating call's level and agent. Nullable with no backfill: a row
    // without them runs at Main's level.
    add_column_if_missing(conn, "cron_jobs", "creator_level", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "creator_agent", "TEXT")?;
    // Whether a scheduled run replays its session's earlier runs. Every
    // existing row reads `task`, the behaviour before this column.
    add_column_if_missing(conn, "cron_jobs", "context", "TEXT NOT NULL DEFAULT 'task'")?;
    add_column_if_missing(conn, "cron_runs", "attempt_id", "TEXT")?;
    add_column_if_missing(conn, "cron_runs", "worker_id", "TEXT")?;
    add_column_if_missing(conn, "cron_job_events", "source_message_event_id", "TEXT")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_cron_jobs_owner ON cron_jobs(owner_id, enabled, next_run);
         CREATE INDEX IF NOT EXISTS idx_cron_jobs_topic ON cron_jobs(topic_id, enabled, next_run);
         CREATE INDEX IF NOT EXISTS idx_cron_jobs_parent ON cron_jobs(parent_task_id, id);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_cron_runs_job_attempt
            ON cron_runs(job_id, attempt_id) WHERE attempt_id IS NOT NULL;",
    )
    .context("Failed to initialize cron lineage indexes")?;
    Ok(())
}
