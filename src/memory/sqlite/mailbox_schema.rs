//! Compact dormant mailbox DDL and upgrades. The outer init owns the transaction.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use super::agent_registry::{MANIFEST_STORES, manifest_schema_sql};

pub(super) const MAILBOX_DDL: &str = include_str!("mailbox_schema.sql");
const CHILD_UPGRADE: &str = include_str!("mailbox_child_schema.sql");

pub(super) fn init_schema(conn: &Connection) -> Result<()> {
    upgrade_manifest(conn)?;
    conn.execute_batch(MAILBOX_DDL)
        .context("initialize dormant mailbox schema")?;
    upgrade_children(conn)
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut query = conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    Ok(query
        .query_map([table], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn require_absent(conn: &Connection, table: &str) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
        [table],
        |row| row.get(0),
    )?;
    if exists {
        bail!("mailbox_schema_migration: unexpected replacement table {table}");
    }
    Ok(())
}

fn upgrade_manifest(conn: &Connection) -> Result<()> {
    if columns(conn, "agent_erase_manifest")? != ["erase_id", "store", "record_key", "fts_rowid"] {
        bail!("mailbox_schema_migration: unexpected manifest columns");
    }
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='agent_erase_manifest'",
        [],
        |row| row.get(0),
    )?;
    // RENAME may quote the table name. Only the closed enum and columns are
    // the idempotency guard; comparing complete SQL would reject valid upgrades.
    let values = sql
        .split("store IN (")
        .nth(1)
        .and_then(|tail| tail.split(')').next())
        .context("mailbox_schema_migration: missing manifest store CHECK")?;
    let stores: BTreeSet<&str> = values.split(',').map(|s| s.trim().trim_matches('\'')).collect();
    let expected: BTreeSet<&str> = MANIFEST_STORES.into_iter().collect();
    if stores == expected {
        return Ok(());
    }
    let old: BTreeSet<&str> = MANIFEST_STORES.into_iter().take(33).collect();
    if stores != old {
        bail!("mailbox_schema_migration: unexpected manifest inventory");
    }
    require_absent(conn, "agent_erase_manifest_extended_new")?;
    conn.execute_batch(&manifest_schema_sql("agent_erase_manifest_extended_new"))?;
    conn.execute_batch(
        "INSERT INTO agent_erase_manifest_extended_new(erase_id,store,record_key,fts_rowid)
         SELECT erase_id,store,record_key,fts_rowid FROM agent_erase_manifest;
         DROP TABLE agent_erase_manifest;
         ALTER TABLE agent_erase_manifest_extended_new RENAME TO agent_erase_manifest;",
    )?;
    Ok(())
}

fn upgrade_children(conn: &Connection) -> Result<()> {
    let found = columns(conn, "child_sessions")?;
    if found == ["key", "record"] {
        require_absent(conn, "child_sessions_linked_new")?;
        conn.execute_batch(CHILD_UPGRADE)?;
    } else if found
        != [
            "key",
            "record",
            "workspace_id",
            "owner_id",
            "instance_id",
            "session_id",
            "agent_key",
            "ma_scope_id",
            "incarnation",
            "generation",
        ]
    {
        bail!("mailbox_schema_migration: unexpected child registry columns");
    }
    let fk_columns: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_foreign_key_list('child_sessions') WHERE \"table\"='agent_scopes'",
        [],
        |row| row.get(0),
    )?;
    let indexes: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_index_list('child_sessions') WHERE name IN ('child_sessions_address','child_sessions_scope')",
        [], |row| row.get(0),
    )?;
    if fk_columns != 5 || indexes != 2 {
        bail!("mailbox_schema_migration: incomplete child linkage constraints");
    }
    Ok(())
}
