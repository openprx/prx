use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::config::Config;

pub const BASELINE_VERSION: &str = "PRX-2026.05-000";
pub const BASELINE_NAME: &str = "baseline_existing_schema";
const BASELINE_SQL: &str = "-- baseline: no DDL change";

#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: String,
    pub name: String,
    pub checksum_up: String,
    pub applied_at: String,
    pub applied_by: String,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct MigrationStatus {
    pub applied: Vec<AppliedMigration>,
    pub pending: Vec<PendingMigration>,
}

#[derive(Debug, Clone)]
pub struct PendingMigration {
    pub version: &'static str,
    pub name: &'static str,
}

#[derive(Debug, Clone)]
pub struct ChecksumMismatch {
    pub version: String,
    pub expected: String,
    pub actual: String,
}

pub fn memory_db_path(config: &Config) -> PathBuf {
    config.workspace_dir.join("memory").join("brain.db")
}

pub fn baseline_checksum() -> String {
    compute_sha256_two(BASELINE_SQL, BASELINE_SQL)
}

pub fn compute_sha256_two(sqlite_sql: &str, postgres_sql: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sqlite_sql.as_bytes());
    hasher.update(b"\x00");
    hasher.update(postgres_sql.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn open_sqlite_memory_db(config: &Config) -> Result<Connection> {
    let db_path = memory_db_path(config);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create memory db directory {}", parent.display()))?;
    }
    Connection::open(&db_path).with_context(|| format!("open memory db {}", db_path.display()))
}

pub fn ensure_sqlite_migrations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version       TEXT PRIMARY KEY,
            name          TEXT NOT NULL,
            checksum      TEXT NOT NULL,
            checksum_up   TEXT NOT NULL,
            checksum_down TEXT,
            applied_at    TEXT NOT NULL,
            applied_by    TEXT NOT NULL,
            duration_ms   INTEGER,
            notes         TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_migrations_applied_at
            ON schema_migrations(applied_at);",
    )?;
    add_column_if_missing(conn, "schema_migrations", "checksum", "TEXT")?;
    add_column_if_missing(conn, "schema_migrations", "checksum_up", "TEXT")?;
    add_column_if_missing(conn, "schema_migrations", "checksum_down", "TEXT")?;
    add_column_if_missing(conn, "schema_migrations", "duration_ms", "INTEGER")?;
    add_column_if_missing(conn, "schema_migrations", "notes", "TEXT")?;
    backfill_checksum_aliases(conn)?;
    Ok(())
}

pub fn baseline_sqlite(conn: &mut Connection, applied_by: &str) -> Result<bool> {
    ensure_sqlite_migrations_table(conn)?;
    let tx = conn.transaction()?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT checksum_up FROM schema_migrations WHERE version = ?1",
            [BASELINE_VERSION],
            |row| row.get(0),
        )
        .optional()?;

    let checksum = baseline_checksum();
    if let Some(existing) = existing {
        if existing != checksum {
            bail!("checksum mismatch for {BASELINE_VERSION}: expected {checksum}, found {existing}");
        }
        tx.commit()?;
        return Ok(false);
    }

    tx.execute(
        "INSERT INTO schema_migrations (
            version, name, checksum, checksum_up, checksum_down,
            applied_at, applied_by, duration_ms, notes
         ) VALUES (?1, ?2, ?3, ?3, NULL, ?4, ?5, 0, ?6)",
        params![
            BASELINE_VERSION,
            BASELINE_NAME,
            checksum,
            Utc::now().to_rfc3339(),
            applied_by,
            "baseline: no DDL change"
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn status_sqlite(conn: &Connection) -> Result<MigrationStatus> {
    ensure_sqlite_migrations_table(conn)?;
    let applied = applied_sqlite(conn)?;
    let has_baseline = applied.iter().any(|record| record.version == BASELINE_VERSION);
    let pending = if has_baseline {
        Vec::new()
    } else {
        vec![PendingMigration {
            version: BASELINE_VERSION,
            name: BASELINE_NAME,
        }]
    };
    Ok(MigrationStatus { applied, pending })
}

pub fn dry_run_sqlite(conn: Option<&Connection>) -> Result<MigrationStatus> {
    let has_baseline = match conn {
        Some(conn) if schema_migrations_exists(conn)? => applied_sqlite(conn)?
            .iter()
            .any(|record| record.version == BASELINE_VERSION),
        _ => false,
    };
    let applied = match conn {
        Some(conn) if schema_migrations_exists(conn)? => applied_sqlite(conn)?,
        _ => Vec::new(),
    };
    let pending = if has_baseline {
        Vec::new()
    } else {
        vec![PendingMigration {
            version: BASELINE_VERSION,
            name: BASELINE_NAME,
        }]
    };
    Ok(MigrationStatus { applied, pending })
}

pub fn verify_sqlite(conn: &Connection) -> Result<Vec<ChecksumMismatch>> {
    if !schema_migrations_exists(conn)? {
        return Ok(Vec::new());
    }
    let mut mismatches = Vec::new();
    for record in applied_sqlite(conn)? {
        if record.version == BASELINE_VERSION {
            let expected = baseline_checksum();
            if record.checksum_up != expected {
                mismatches.push(ChecksumMismatch {
                    version: record.version,
                    expected,
                    actual: record.checksum_up,
                });
            }
        }
    }
    Ok(mismatches)
}

pub fn applied_sqlite(conn: &Connection) -> Result<Vec<AppliedMigration>> {
    let mut stmt = conn.prepare(
        "SELECT version, name, COALESCE(checksum_up, checksum), applied_at, applied_by, duration_ms
         FROM schema_migrations
         ORDER BY version ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AppliedMigration {
            version: row.get(0)?,
            name: row.get(1)?,
            checksum_up: row.get(2)?,
            applied_at: row.get(3)?,
            applied_by: row.get(4)?,
            duration_ms: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn schema_migrations_exists(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'schema_migrations'
        )",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
    .map_err(Into::into)
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    Ok(())
}

fn backfill_checksum_aliases(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "UPDATE schema_migrations
         SET checksum_up = COALESCE(NULLIF(checksum_up, ''), checksum)
         WHERE checksum_up IS NULL OR checksum_up = '';
         UPDATE schema_migrations
         SET checksum = COALESCE(NULLIF(checksum, ''), checksum_up)
         WHERE checksum IS NULL OR checksum = '';",
    )?;
    Ok(())
}

#[allow(dead_code)]
pub fn sqlite_db_exists(config: &Config) -> bool {
    Path::new(&memory_db_path(config)).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_records_and_verifies() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(baseline_sqlite(&mut conn, "test").unwrap());
        assert!(!baseline_sqlite(&mut conn, "test").unwrap());

        let status = status_sqlite(&conn).unwrap();
        assert_eq!(status.applied.len(), 1);
        assert_eq!(
            status
                .applied
                .first()
                .expect("baseline migration should be applied")
                .version,
            BASELINE_VERSION
        );
        assert!(status.pending.is_empty());
        assert!(verify_sqlite(&conn).unwrap().is_empty());
    }

    #[test]
    fn dry_run_does_not_create_table() {
        let conn = Connection::open_in_memory().unwrap();
        let status = dry_run_sqlite(Some(&conn)).unwrap();
        assert!(status.applied.is_empty());
        assert_eq!(status.pending.len(), 1);
        assert!(!schema_migrations_exists(&conn).unwrap());
    }

    #[test]
    fn checksum_mismatch_is_reported() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_sqlite_migrations_table(&conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (
                version, name, checksum, checksum_up, applied_at, applied_by
             ) VALUES (?1, ?2, 'bad', 'bad', ?3, 'test')",
            params![BASELINE_VERSION, BASELINE_NAME, Utc::now().to_rfc3339()],
        )
        .unwrap();

        let mismatches = verify_sqlite(&conn).unwrap();
        assert_eq!(mismatches.len(), 1);
        assert_eq!(
            mismatches.first().expect("checksum mismatch should exist").version,
            BASELINE_VERSION
        );
    }
}
