//! Append-only, content-free write log (migration 28).
//!
//! One row per memory or document a run's write inserted or updated. A row
//! holds run ids, the tool call's operation id, the target's stable id
//! (`memories.id` / `documents.document_id`, never the model-chosen key), the
//! operation and the target's partition stamp: no content, no key text, no
//! hash. Rows are appended in the write's own transaction, so a write and its
//! row commit or roll back together.
//!
//! The instant the log started recording in this store is the `applied_at`
//! of version 28 in `memory_schema_migrations`.

use crate::memory::write_provenance::WriteProvenance;
use rusqlite::{Connection, params};

pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_write_log (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            root_run_id     TEXT NOT NULL,
            run_id          TEXT NOT NULL,
            parent_run_id   TEXT,
            operation_id    TEXT,
            target_kind     TEXT NOT NULL CHECK (target_kind IN ('memory', 'document')),
            target_id       TEXT NOT NULL,
            op              TEXT NOT NULL CHECK (op IN ('insert', 'update')),
            agent_partition TEXT,
            created_at      TEXT NOT NULL
        ) STRICT;
        CREATE INDEX IF NOT EXISTS memory_write_log_root ON memory_write_log(root_run_id);
        CREATE INDEX IF NOT EXISTS memory_write_log_run ON memory_write_log(run_id);
        CREATE INDEX IF NOT EXISTS memory_write_log_target ON memory_write_log(target_kind, target_id);",
    )?;
    Ok(())
}

/// The table a logged write touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteTarget {
    Memory,
    Document,
}

impl WriteTarget {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Document => "document",
        }
    }
}

/// Whether the write created the target row or changed an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOp {
    Insert,
    Update,
}

impl WriteOp {
    /// The operation of an upsert, from whether the target row existed when
    /// the write's transaction read it.
    const fn from_existing(existed: bool) -> Self {
        if existed { Self::Update } else { Self::Insert }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
        }
    }
}

/// One logged write: what it touched and how.
#[derive(Debug, Clone, Copy)]
struct LoggedWrite<'a> {
    target: WriteTarget,
    target_id: &'a str,
    op: WriteOp,
    agent_partition: Option<&'a str>,
}

/// Append the log row of `write` on `conn`, inside the caller's open
/// write transaction.
fn record(conn: &Connection, provenance: &WriteProvenance, write: LoggedWrite<'_>) -> anyhow::Result<()> {
    // A forgotten run family never writes again; the refusal rolls the
    // caller's transaction back with the write.
    if super::rf_forget::is_forgotten(conn, provenance)? {
        return Err(crate::memory::write_provenance::RunForgotten.into());
    }
    conn.execute(
        "INSERT INTO memory_write_log (
            root_run_id, run_id, parent_run_id, operation_id,
            target_kind, target_id, op, agent_partition, created_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            provenance.root_run_id(),
            provenance.run_id(),
            provenance.parent_run_id(),
            provenance.operation_id(),
            write.target.as_str(),
            write.target_id,
            write.op.as_str(),
            write.agent_partition,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Whether the row named by its upsert conflict key (a memory's key, a
/// document's id) exists: read inside the write transaction before the
/// upsert, it decides insert versus update.
pub(super) fn target_exists(conn: &Connection, target: WriteTarget, conflict_key: &str) -> anyhow::Result<bool> {
    let sql = match target {
        WriteTarget::Memory => "SELECT EXISTS(SELECT 1 FROM memories WHERE key = ?1)",
        WriteTarget::Document => "SELECT EXISTS(SELECT 1 FROM documents WHERE document_id = ?1)",
    };
    Ok(conn.query_row(sql, params![conflict_key], |row| row.get(0))?)
}

/// Log the memory upsert under `key` that just succeeded on `conn`. The
/// target is the row's stable id, read back with its partition stamp.
pub(super) fn record_memory_upsert(
    conn: &Connection,
    provenance: &WriteProvenance,
    key: &str,
    existed: bool,
) -> anyhow::Result<()> {
    let (target_id, agent_partition): (String, Option<String>) = conn.query_row(
        "SELECT id, ma_scope_id FROM memories WHERE key = ?1",
        params![key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    record(
        conn,
        provenance,
        LoggedWrite {
            target: WriteTarget::Memory,
            target_id: &target_id,
            op: WriteOp::from_existing(existed),
            agent_partition: agent_partition.as_deref(),
        },
    )
}

/// Log the document upsert of `document_id` that just succeeded on `conn`,
/// with the document's partition stamp.
pub(super) fn record_document_upsert(
    conn: &Connection,
    provenance: &WriteProvenance,
    document_id: &str,
    existed: bool,
) -> anyhow::Result<()> {
    let agent_partition: Option<String> = conn.query_row(
        "SELECT ma_scope_id FROM documents WHERE document_id = ?1",
        params![document_id],
        |row| row.get(0),
    )?;
    record(
        conn,
        provenance,
        LoggedWrite {
            target: WriteTarget::Document,
            target_id: document_id,
            op: WriteOp::from_existing(existed),
            agent_partition: agent_partition.as_deref(),
        },
    )
}
