//! Child-session blobs never enter the operator's memories/picker namespace.

use super::SqliteMemory;
use crate::memory::child_sessions::ChildSessionStore;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

pub(super) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS child_sessions (
            key TEXT PRIMARY KEY NOT NULL,
            record TEXT NOT NULL CHECK(json_valid(record))
        ) STRICT;",
    )?;
    Ok(())
}

impl ChildSessionStore for SqliteMemory {
    fn load(&self, key: &str) -> anyhow::Result<Option<String>> {
        let reader = self.pool.read()?;
        Ok(reader
            .connection()?
            .query_row("SELECT record FROM child_sessions WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    fn update(
        &self,
        key: &str,
        update: &mut dyn FnMut(Option<&str>) -> anyhow::Result<String>,
    ) -> anyhow::Result<String> {
        let writer = self.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row("SELECT record FROM child_sessions WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        let record = update(old.as_deref())?;
        tx.execute(
            "INSERT INTO child_sessions(key, record) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET record = excluded.record",
            params![key, record],
        )?;
        tx.commit()?;
        Ok(record)
    }
}
