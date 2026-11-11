//! Attribute writes at the single SQLite writer boundary, including blocking
//! workers. TEMP triggers update the durable index in the data transaction.

use crate::memory::erasure::{ErasureOrigin, ErasureStore};
use crate::runtime::sqlite_pool::{SqliteConnectionPool, SqliteWriteGuard};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Instant,
};

const TABLES: &[&str] = &[
    "memories",
    "embedding_cache",
    "conversation_turns",
    "message_events",
    "ingress_recovery",
    "recovery_attestations",
    "memory_events",
    "memory_drafts",
    "memory_trash",
    "chat_profiles",
    "memory_links",
    "retrieval_traces",
    "compaction_runs",
    "ramflux_blobs",
];

pub(super) fn init_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ramflux_erase_index (
        account TEXT NOT NULL, conversation TEXT NOT NULL, message TEXT NOT NULL,
        table_name TEXT NOT NULL, record_rowid INTEGER NOT NULL,
        PRIMARY KEY(account,conversation,message,table_name,record_rowid));
     CREATE INDEX IF NOT EXISTS ramflux_erase_origin ON ramflux_erase_index(account,conversation,message);
     CREATE INDEX IF NOT EXISTS ramflux_erase_record ON ramflux_erase_index(table_name,record_rowid);
     CREATE TABLE IF NOT EXISTS ramflux_tombstones (
        account TEXT NOT NULL, conversation TEXT NOT NULL, message TEXT NOT NULL,
        sealed INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(account,conversation,message));
     CREATE TABLE IF NOT EXISTS ramflux_blobs (
        id INTEGER PRIMARY KEY, account TEXT NOT NULL, conversation TEXT NOT NULL,
        kind TEXT NOT NULL, object_key TEXT NOT NULL, bytes BLOB NOT NULL,
        UNIQUE(account,conversation,kind,object_key));",
    )?;
    super::agent_registry::add_scope_column(conn, "ramflux_blobs")?;
    Ok(())
}

pub(super) struct MemoryPool {
    raw: Arc<SqliteConnectionPool>,
    active: Arc<Mutex<Option<ErasureOrigin>>>,
    origin: Option<ErasureOrigin>,
}

impl MemoryPool {
    pub fn new(raw: Arc<SqliteConnectionPool>) -> anyhow::Result<Self> {
        let active = Arc::new(Mutex::new(None::<ErasureOrigin>));
        {
            let conn = raw.write();
            let context = Arc::clone(&active);
            conn.create_scalar_function(
                "ramflux_origin",
                1,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8 | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
                move |ctx| {
                    let origin = context.lock();
                    Ok(origin.as_ref().map(|origin| match ctx.get::<i64>(0) {
                        Ok(0) => origin.account.clone(),
                        Ok(1) => origin.conversation.clone(),
                        _ => origin.message.clone(),
                    }))
                },
            )?;
            for table in TABLES {
                install_table_triggers(&conn, table)?;
            }
        }
        Ok(Self {
            raw,
            active,
            origin: None,
        })
    }

    pub fn with_origin(&self, origin: ErasureOrigin) -> Self {
        Self {
            raw: Arc::clone(&self.raw),
            active: Arc::clone(&self.active),
            origin: Some(origin),
        }
    }

    pub fn write(&self) -> WriteGuard<'_> {
        let guard = self.raw.write();
        *self.active.lock() = self.origin.clone();
        WriteGuard {
            guard,
            active: &self.active,
        }
    }

    pub fn write_until(&self, deadline: Instant) -> anyhow::Result<WriteGuard<'_>> {
        let guard = self.raw.write_until(deadline)?;
        *self.active.lock() = self.origin.clone();
        Ok(WriteGuard {
            guard,
            active: &self.active,
        })
    }
}

/// The ingress schema is lazy; install its trigger in the creating transaction.
pub(super) fn install_table_triggers(conn: &rusqlite::Connection, table: &str) -> anyhow::Result<()> {
    anyhow::ensure!(TABLES.contains(&table), "invalid indexed table");
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?;
    let configured: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_function_list WHERE name='ramflux_origin')",
        [],
        |row| row.get(0),
    )?;
    if !exists || !configured {
        return Ok(());
    }
    let columns = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Recovery evidence and draft merges can be written by a later turn. The
    // referenced source retains ownership of its derived content and outbox.
    let parents: Vec<(&str, &str, &str, &str)> = match table {
        "message_events" => vec![(
            "message_events",
            "event_id",
            "NEW.causation_event_id",
            "AND e.rowid <> NEW.rowid",
        )],
        "recovery_attestations" => vec![
            ("message_events", "event_id", "NEW.input_event_id", ""),
            ("message_events", "event_id", "NEW.terminal_event_id", ""),
        ],
        "ingress_recovery" => vec![("message_events", "event_id", "NEW.input_event_id", "")],
        "memories" => vec![
            ("message_events", "event_id", "NEW.source_event_id", ""),
            ("memory_drafts", "key", "NEW.key", "AND e.content=NEW.content"),
            ("memory_trash", "memory_key", "NEW.key", "AND e.content=NEW.content"),
        ],
        "memory_drafts" => vec![("message_events", "event_id", "NEW.source_event_id", "")],
        "memory_trash" => vec![("memories", "key", "NEW.memory_key", "AND e.content=NEW.content")],
        "memory_events" => vec![
            (
                "message_events",
                "event_id",
                "NEW.subject_id",
                "AND NEW.subject_table='message_events'",
            ),
            ("memories", "id", "NEW.subject_id", "AND NEW.subject_table='memories'"),
            (
                "memory_drafts",
                "draft_id",
                "NEW.subject_id",
                "AND NEW.subject_table='memory_drafts'",
            ),
        ],
        _ => Vec::new(),
    };
    let inherit = parents
        .iter()
        .map(|(parent, id, reference, extra)| {
            format!(
                "INSERT OR IGNORE INTO ramflux_erase_index(account,conversation,message,table_name,record_rowid)
         SELECT i.account,i.conversation,i.message,'{table}',NEW.rowid
         FROM ramflux_erase_index i JOIN {parent} e ON e.rowid=i.record_rowid
         WHERE i.table_name='{parent}' AND e.{id}={reference} AND i.account=ramflux_origin(0) AND i.conversation=ramflux_origin(1) {extra};"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    for operation in ["INSERT", "UPDATE"] {
        let mut changed: Vec<_> = [
            "content",
            "raw_payload_json",
            "payload_json",
            "bytes",
            "summary",
            "title",
            "purpose",
            "notes",
            "tags_json",
            "chat_kind",
            "capsule_json",
            "key",
        ]
        .into_iter()
        .filter(|column| columns.iter().any(|name| name == column))
        .map(str::to_owned)
        .collect();
        if operation == "UPDATE" && table == "recovery_attestations" {
            changed.push("terminal_event_id".to_owned());
        }
        if operation == "UPDATE" && changed.is_empty() {
            continue;
        }
        let operation_sql = if operation == "UPDATE" {
            format!("UPDATE OF {}", changed.join(","))
        } else {
            operation.to_owned()
        };
        // An unscoped insert can reuse the rowid of an already-forgotten scoped
        // row. Clear stale ownership before deciding whether the new row has
        // a message origin. Keep unscoped metadata updates from stealing it.
        let when = if operation == "INSERT" {
            ""
        } else {
            "WHEN ramflux_origin(0) IS NOT NULL"
        };
        conn.execute_batch(&format!(
            "CREATE TEMP TRIGGER IF NOT EXISTS ramflux_index_{table}_{operation} AFTER {operation_sql} ON main.{table}
             {when} BEGIN
             SELECT CASE WHEN EXISTS(SELECT 1 FROM ramflux_tombstones
               WHERE account=ramflux_origin(0) AND conversation=ramflux_origin(1)
                 AND message=ramflux_origin(2) AND sealed=1)
               THEN RAISE(ABORT,'message has been erased') END;
             DELETE FROM ramflux_erase_index WHERE table_name='{table}' AND record_rowid=NEW.rowid;
             INSERT INTO ramflux_erase_index(account,conversation,message,table_name,record_rowid)
             SELECT ramflux_origin(0),ramflux_origin(1),ramflux_origin(2),'{table}',NEW.rowid
             WHERE ramflux_origin(0) IS NOT NULL;
             {inherit}
             END;"
        ))?;
    }
    Ok(())
}

impl Deref for MemoryPool {
    type Target = SqliteConnectionPool;
    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

pub(super) struct WriteGuard<'a> {
    guard: SqliteWriteGuard<'a>,
    active: &'a Mutex<Option<ErasureOrigin>>,
}
impl Deref for WriteGuard<'_> {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}
impl DerefMut for WriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}
impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        *self.active.lock() = None;
    }
}

impl ErasureStore for MemoryPool {
    fn put_blob(&self, origin: &ErasureOrigin, kind: &str, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let view = self.with_origin(origin.clone());
        let guard = view.write();
        let conn: &rusqlite::Connection = &guard;
        conn.execute(
            "INSERT INTO ramflux_blobs(account,conversation,kind,object_key,bytes) VALUES(?1,?2,?3,?4,?5)
                      ON CONFLICT(account,conversation,kind,object_key) DO UPDATE SET bytes=excluded.bytes",
            params![origin.account, origin.conversation, kind, key, bytes],
        )?;
        Ok(())
    }
    fn get_blob(&self, account: &str, conversation: &str, kind: &str, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self
            .read()?
            .connection()?
            .query_row(
                "SELECT bytes FROM ramflux_blobs WHERE account=?1 AND conversation=?2 AND kind=?3 AND object_key=?4",
                params![account, conversation, kind, key],
                |row| row.get(0),
            )
            .optional()?)
    }
    fn list_blobs(&self, account: &str, conversation: &str, kind: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let reader = self.read()?;
        let conn = reader.connection()?;
        let mut stmt = conn.prepare(
            "SELECT object_key,bytes FROM ramflux_blobs WHERE account=?1 AND conversation=?2 AND kind=?3 ORDER BY id",
        )?;
        Ok(stmt
            .query_map(params![account, conversation, kind], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    fn remove_blob(&self, account: &str, conversation: &str, kind: &str, key: &str) -> anyhow::Result<()> {
        let guard = self.write();
        let conn: &rusqlite::Connection = &guard;
        conn.execute(
            "DELETE FROM ramflux_blobs WHERE account=?1 AND conversation=?2 AND kind=?3 AND object_key=?4",
            params![account, conversation, kind, key],
        )?;
        Ok(())
    }
    fn begin_delete(&self, origin: &ErasureOrigin) -> anyhow::Result<()> {
        let guard = self.write();
        let conn: &rusqlite::Connection = &guard;
        conn.execute(
            "INSERT OR IGNORE INTO ramflux_tombstones(account,conversation,message) VALUES(?1,?2,?3)",
            params![origin.account, origin.conversation, origin.message],
        )?;
        Ok(())
    }
    fn is_deleted(&self, origin: &ErasureOrigin) -> anyhow::Result<bool> {
        Ok(self.read()?.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM ramflux_tombstones WHERE account=?1 AND conversation=?2 AND message=?3)",
            params![origin.account, origin.conversation, origin.message],
            |row| row.get(0),
        )?)
    }
    fn origins(&self, account: &str, conversation: Option<&str>) -> anyhow::Result<Vec<ErasureOrigin>> {
        let reader = self.read()?;
        let conn = reader.connection()?;
        let mut stmt = conn.prepare("SELECT DISTINCT conversation,message FROM ramflux_erase_index WHERE account=?1 AND (?2 IS NULL OR conversation=?2)")?;
        Ok(stmt
            .query_map(params![account, conversation], |row| {
                Ok(ErasureOrigin {
                    account: account.to_owned(),
                    conversation: row.get(0)?,
                    message: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    fn run_ids(&self, origin: &ErasureOrigin) -> anyhow::Result<Vec<String>> {
        let reader = self.read()?;
        let conn = reader.connection()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT e.run_id FROM message_events e
            JOIN ramflux_erase_index i ON i.table_name='message_events' AND i.record_rowid=e.rowid
            WHERE i.account=?1 AND i.conversation=?2 AND i.message=?3 AND e.run_id IS NOT NULL",
        )?;
        Ok(stmt
            .query_map(params![origin.account, origin.conversation, origin.message], |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    fn erase_rows(&self, origin: &ErasureOrigin) -> anyhow::Result<()> {
        let mut conn = self.write();
        let tx = conn.transaction()?;
        tx.execute_batch("PRAGMA defer_foreign_keys=ON")?;
        tx.execute(
            "UPDATE ramflux_tombstones SET sealed=1 WHERE account=?1 AND conversation=?2 AND message=?3",
            params![origin.account, origin.conversation, origin.message],
        )?;
        let rows = {
            let mut stmt = tx.prepare("SELECT table_name,record_rowid FROM ramflux_erase_index WHERE account=?1 AND conversation=?2 AND message=?3")?;
            stmt.query_map(params![origin.account, origin.conversation, origin.message], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let rebuild_fts = rows.iter().any(|(table, _)| table == "memories");
        for (table, rowid) in rows {
            anyhow::ensure!(TABLES.contains(&table.as_str()), "invalid erase index table");
            tx.execute(&format!("DELETE FROM {table} WHERE rowid=?1"), [rowid])?;
        }
        if rebuild_fts {
            // Drop obsolete FTS segments as well as their logical tombstones.
            tx.execute("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')", [])?;
        }
        tx.commit()?;
        Ok(())
    }
    fn finish_delete(&self, origin: &ErasureOrigin) -> anyhow::Result<()> {
        let mut conn = self.write();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE ramflux_tombstones SET sealed=1 WHERE account=?1 AND conversation=?2 AND message=?3",
            params![origin.account, origin.conversation, origin.message],
        )?;
        tx.execute(
            "DELETE FROM ramflux_erase_index WHERE account=?1 AND conversation=?2 AND message=?3",
            params![origin.account, origin.conversation, origin.message],
        )?;
        tx.commit()?;
        Ok(())
    }
}
