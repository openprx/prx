//! Task-event mirror and approval-grant schema stored beside the memory database.

use anyhow::Context;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use uuid::Uuid;

pub struct SqliteTaskEventMirror<'a> {
    pub workspace_id: &'a str,
    pub task_id: &'a str,
    pub event_type: &'a str,
    pub session_key: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub persona_id: Option<&'a str>,
    pub payload_json: Option<&'a str>,
}

pub fn append_task_event_mirror(workspace_dir: &Path, input: SqliteTaskEventMirror<'_>) -> anyhow::Result<i64> {
    append_task_event_mirror_idempotent(workspace_dir, &Uuid::new_v4().to_string(), input)
}

/// Append a task event mirror with a caller-owned stable event id. Replaying
/// the same outbox row returns the existing mirror instead of duplicating it.
pub fn append_task_event_mirror_idempotent(
    workspace_dir: &Path,
    event_id: &str,
    input: SqliteTaskEventMirror<'_>,
) -> anyhow::Result<i64> {
    let db_path = workspace_dir.join("memory").join("brain.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create memory directory: {}", parent.display()))?;
    }
    let conn =
        Connection::open(&db_path).with_context(|| format!("Failed to open memory DB: {}", db_path.display()))?;
    // Same policy as the pooled connections onto this file: a lock held by
    // another process makes this call wait (and warn), never fail.
    crate::runtime::sqlite_pool::install_busy_handler(&conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id      TEXT NOT NULL UNIQUE,
            workspace_id  TEXT NOT NULL,
            owner_id      TEXT,
            event_type    TEXT NOT NULL,
            subject_table TEXT NOT NULL,
            subject_id    TEXT NOT NULL,
            session_key   TEXT,
            agent_id      TEXT,
            persona_id    TEXT,
            visibility    TEXT NOT NULL DEFAULT 'workspace',
            payload_json  TEXT,
            created_at    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_memory_events_workspace_id
            ON memory_events(workspace_id, id);
        CREATE INDEX IF NOT EXISTS idx_memory_events_type
            ON memory_events(workspace_id, event_type, id);
        CREATE INDEX IF NOT EXISTS idx_memory_events_session
            ON memory_events(workspace_id, session_key, id);",
    )?;

    conn.execute(
        "INSERT OR IGNORE INTO memory_events (
            event_id, workspace_id, event_type, subject_table, subject_id,
            session_key, agent_id, persona_id, visibility, payload_json, created_at
         )
         VALUES (?1, ?2, ?3, 'tasks', ?4, ?5, ?6, ?7, 'workspace', ?8, ?9)",
        params![
            event_id,
            input.workspace_id,
            input.event_type,
            input.task_id,
            input.session_key,
            input.agent_id,
            input.persona_id,
            input.payload_json,
            Utc::now().to_rfc3339(),
        ],
    )?;
    conn.query_row(
        "SELECT id FROM memory_events WHERE event_id = ?1",
        params![event_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

pub fn init_approval_grant_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS approval_grants (
            grant_id                 TEXT PRIMARY KEY,
            version                  INTEGER NOT NULL,
            owner_id                 TEXT NOT NULL,
            principal_id             TEXT NOT NULL,
            workspace_id             TEXT NOT NULL,
            agent_id                 TEXT NOT NULL,
            session_key              TEXT,
            issuer_authority         TEXT NOT NULL,
            issuer_authority_id      TEXT NOT NULL,
            issuer_public_key_id     TEXT NOT NULL,
            capability_op_id         TEXT NOT NULL,
            capability_op_id_match   TEXT NOT NULL,
            capability_risk_level    TEXT NOT NULL CHECK (capability_risk_level IN ('low','medium','high','critical')),
            resource_constraints_json TEXT NOT NULL DEFAULT '{}',
            grant_json               TEXT NOT NULL,
            signature_alg            TEXT NOT NULL,
            signed_payload_sha256    TEXT NOT NULL,
            issued_at                TEXT NOT NULL,
            not_before               TEXT NOT NULL,
            expires_at               TEXT NOT NULL,
            max_uses                 INTEGER NOT NULL,
            uses_consumed            INTEGER NOT NULL DEFAULT 0,
            related_task_id          TEXT,
            related_message_event_id INTEGER,
            revoked_at               TEXT,
            revocation_reason        TEXT,
            created_at               TEXT NOT NULL,
            updated_at               TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_approval_grants_owner
            ON approval_grants(workspace_id, owner_id, issued_at DESC);
        CREATE INDEX IF NOT EXISTS idx_approval_grants_principal
            ON approval_grants(workspace_id, principal_id, issued_at DESC);
        CREATE INDEX IF NOT EXISTS idx_approval_grants_capability
            ON approval_grants(workspace_id, capability_op_id, expires_at);
        CREATE INDEX IF NOT EXISTS idx_approval_grants_active
            ON approval_grants(workspace_id, expires_at, revoked_at);

        CREATE TABLE IF NOT EXISTS approval_grant_events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id      TEXT NOT NULL UNIQUE,
            grant_id      TEXT NOT NULL,
            event_type    TEXT NOT NULL CHECK (event_type IN (
                'grant.issued','grant.verified','grant.consumed','grant.revoked',
                'grant.rejected','grant.expired'
            )),
            actor         TEXT NOT NULL,
            occurred_at   TEXT NOT NULL,
            payload_json  TEXT,
            FOREIGN KEY (grant_id) REFERENCES approval_grants(grant_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_approval_grant_events_grant
            ON approval_grant_events(grant_id, occurred_at);
        CREATE INDEX IF NOT EXISTS idx_approval_grant_events_type
            ON approval_grant_events(event_type, occurred_at);

        CREATE TABLE IF NOT EXISTS approval_grant_revocations (
            grant_id     TEXT PRIMARY KEY,
            revoked_at   TEXT NOT NULL,
            reason       TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_approval_grant_revocations_revoked_at
            ON approval_grant_revocations(revoked_at);",
    )?;
    Ok(())
}

/// FIX-P3-06: record an out-of-band revocation for `grant_id`.
///
/// Idempotent: a second revocation of the same grant overwrites the previous
/// `revoked_at` / `reason` rather than failing on the primary-key conflict, so
/// re-revoking is always safe. The signed grant payload is never mutated; the
/// revocation lives only in this side-table and is consulted by
/// [`is_approval_grant_revoked`].
pub fn revoke_approval_grant(
    conn: &Connection,
    grant_id: &str,
    revoked_at: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO approval_grant_revocations (grant_id, revoked_at, reason) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(grant_id) DO UPDATE SET revoked_at = excluded.revoked_at, reason = excluded.reason",
        params![grant_id, revoked_at, reason],
    )?;
    Ok(())
}

/// FIX-P3-06: report whether `grant_id` has an out-of-band revocation row.
///
/// The gate path (to be wired up under separate coordination — the gate
/// verification currently lives outside this module's boundary) MUST treat a
/// `true` result as fail-closed and deny the operation, in addition to the
/// in-grant `revoked_at` check performed by
/// [`crate::acl::approval_grant::ApprovalGrantV2::verify_for_operation`].
pub fn is_approval_grant_revoked(conn: &Connection, grant_id: &str) -> anyhow::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM approval_grant_revocations WHERE grant_id = ?1",
            params![grant_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}
