//! One-time scrub of the memory access audit log.
//!
//! Audit rows written before the writer hashed Ramflux scoped keys may hold a
//! clear `rf:<scope hash>:<key hash>` key in `memory_id` or `query`. This pass
//! rewrites those two columns to the SHA-256 hex digest of the stored value.
//! It is idempotent without a marker: a digest never starts with `rf:`, so a
//! second run finds nothing. A row whose clear text is a model key (not
//! `rf:`-prefixed) cannot be told apart from an ordinary key and is left alone.

use crate::memory::RAMFLUX_MEMORY_KEY_PREFIX;
use crate::security::ramflux_aead::id_hash;
use rusqlite::{Connection, params};

/// Called from the schema migration, which holds the write transaction.
pub(super) fn scrub(conn: &Connection) -> anyhow::Result<()> {
    let pattern_len = i64::try_from(RAMFLUX_MEMORY_KEY_PREFIX.len()).unwrap_or(i64::MAX);
    let rows: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT id, query, memory_id FROM access_audit_log
             WHERE substr(query, 1, ?1) = ?2 OR substr(memory_id, 1, ?1) = ?2",
        )?;
        stmt.query_map(params![pattern_len, RAMFLUX_MEMORY_KEY_PREFIX], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    for (id, query, memory_id) in rows {
        conn.execute(
            "UPDATE access_audit_log SET query = ?2, memory_id = ?3 WHERE id = ?1",
            params![id, hashed(query), hashed(memory_id)],
        )?;
    }
    Ok(())
}

fn hashed(value: Option<String>) -> Option<String> {
    value.map(|text| {
        if text.starts_with(RAMFLUX_MEMORY_KEY_PREFIX) {
            id_hash(&text)
        } else {
            text
        }
    })
}
