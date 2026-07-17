use anyhow::{Context, Result, bail};
use postgres::NoTls;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;
use crate::memory::{
    MemoryBackendKind, PostgresMemory, SqliteMemory, classify_memory_backend, effective_memory_backend_name,
};

#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: String,
    pub name: String,
    pub checksum: String,
    pub applied_at: String,
}

#[derive(Debug, Clone)]
pub struct PendingMigration {
    pub version: i64,
    pub name: &'static str,
}

#[derive(Debug, Clone)]
pub struct LegacyAppliedMigration {
    pub version: String,
    pub name: String,
    pub checksum: String,
    pub applied_at: String,
    pub applied_by: String,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct MigrationStatus {
    pub applied: Vec<AppliedMigration>,
    pub pending: Vec<PendingMigration>,
    pub legacy_applied: Vec<LegacyAppliedMigration>,
    pub legacy_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub backend: String,
    pub target: String,
    pub status: MigrationStatus,
}

pub fn memory_db_path(config: &Config) -> PathBuf {
    config.workspace_dir.join("memory").join("brain.db")
}

/// Inspect the configured backend's authoritative memory migration ledger.
///
/// This function is deliberately read-only: it does not create a workspace,
/// database, schema, table, baseline, or migration row.
pub fn inspect_configured_backend(config: &Config) -> Result<MigrationReport> {
    let backend = effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    match classify_memory_backend(&backend) {
        MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid => inspect_sqlite_path(memory_db_path(config), backend),
        MemoryBackendKind::Postgres => inspect_postgres(config, backend),
        MemoryBackendKind::Markdown | MemoryBackendKind::None | MemoryBackendKind::Unknown => {
            bail!("schema migration inspection is unsupported for configured memory backend '{backend}'")
        }
    }
}

fn inspect_sqlite_path(db_path: PathBuf, backend: String) -> Result<MigrationReport> {
    if !db_path.is_file() {
        bail!(
            "authoritative memory database is missing at {}; inspection did not create it",
            db_path.display()
        );
    }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open memory database read-only {}", db_path.display()))?;
    let status = status_sqlite(&conn)?;
    Ok(MigrationReport {
        backend,
        target: db_path.display().to_string(),
        status,
    })
}

pub fn status_sqlite(conn: &Connection) -> Result<MigrationStatus> {
    if !sqlite_table_exists(conn, "memory_schema_migrations")? {
        if let Ok(legacy) = read_legacy_sqlite_evidence(conn)
            && !legacy.is_empty()
        {
            bail!(
                "authoritative memory_schema_migrations ledger is missing; found {} legacy schema_migrations row(s) as compatibility evidence only",
                legacy.len()
            );
        }
        bail!("authoritative memory_schema_migrations ledger is missing");
    }

    let mut stmt = conn.prepare(
        "SELECT version, name, checksum, applied_at
           FROM memory_schema_migrations
          ORDER BY version ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let applied = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let (legacy_applied, legacy_warning) = match read_legacy_sqlite_evidence(conn) {
        Ok(rows) => (rows, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    build_status(
        applied,
        SqliteMemory::memory_schema_migration_registry(),
        SqliteMemory::schema_migration_checksum,
        legacy_applied,
        legacy_warning,
    )
}

fn inspect_postgres(config: &Config, backend: String) -> Result<MigrationReport> {
    let storage = &config.storage.provider.config;
    let db_url = storage
        .db_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("postgres migration inspection requires storage.provider.config.db_url")?;
    let schema = storage.schema.trim();
    validate_postgres_identifier(schema, "storage schema")?;

    let mut postgres_config = db_url
        .parse::<postgres::Config>()
        .context("invalid postgres connection URL for migration inspection")?;
    if let Some(timeout_secs) = storage.connect_timeout_secs {
        postgres_config.connect_timeout(Duration::from_secs(timeout_secs.min(30)));
    }
    let mut client = postgres_config
        .connect(NoTls)
        .context("connect to postgres for read-only migration inspection")?;
    let mut tx = client
        .build_transaction()
        .read_only(true)
        .start()
        .context("start read-only postgres migration inspection")?;
    let exists: bool = tx
        .query_one(
            "SELECT EXISTS(
                SELECT 1 FROM information_schema.tables
                 WHERE table_schema = $1 AND table_name = 'memory_schema_migrations'
             )",
            &[&schema],
        )?
        .get(0);
    if !exists {
        bail!("authoritative {schema}.memory_schema_migrations ledger is missing");
    }

    let table = format!("\"{schema}\".memory_schema_migrations");
    let query = format!("SELECT version, name, checksum, applied_at FROM {table} ORDER BY version ASC");
    let rows = tx.query(&query, &[])?;
    let applied = rows
        .into_iter()
        .map(|row| {
            (
                row.get::<_, i64>(0),
                row.get::<_, String>(1),
                row.get::<_, String>(2),
                row.get::<_, String>(3),
            )
        })
        .collect();
    let status = build_status(
        applied,
        PostgresMemory::memory_schema_migration_registry(),
        PostgresMemory::schema_migration_checksum,
        Vec::new(),
        None,
    )?;
    tx.rollback().context("close read-only postgres migration inspection")?;

    Ok(MigrationReport {
        backend,
        target: format!("postgres schema {schema}"),
        status,
    })
}

fn build_status(
    rows: Vec<(i64, String, String, String)>,
    registry: &'static [(i64, &'static str, &'static str)],
    checksum: fn(&str) -> String,
    legacy_applied: Vec<LegacyAppliedMigration>,
    legacy_warning: Option<String>,
) -> Result<MigrationStatus> {
    let mut applied = Vec::with_capacity(rows.len());
    let mut applied_versions = HashSet::with_capacity(rows.len());

    for (version, recorded_name, recorded_checksum, applied_at) in rows {
        let Some((_, expected_name, descriptor)) =
            registry.iter().find(|(known_version, _, _)| *known_version == version)
        else {
            bail!("authoritative migration ledger contains unknown version {version}");
        };
        if recorded_name != *expected_name {
            bail!(
                "memory schema migration name mismatch for version {version}: expected {expected_name}, found {recorded_name}"
            );
        }
        let expected_checksum = checksum(descriptor);
        if recorded_checksum != expected_checksum {
            bail!(
                "memory schema migration checksum mismatch for version {version} ({expected_name}): expected {expected_checksum}, found {recorded_checksum}"
            );
        }
        applied_versions.insert(version);
        applied.push(AppliedMigration {
            version: version.to_string(),
            name: recorded_name,
            checksum: recorded_checksum,
            applied_at,
        });
    }

    let pending = registry
        .iter()
        .filter(|(version, _, _)| !applied_versions.contains(version))
        .map(|(version, name, _)| PendingMigration {
            version: *version,
            name,
        })
        .collect();

    Ok(MigrationStatus {
        applied,
        pending,
        legacy_applied,
        legacy_warning,
    })
}

fn read_legacy_sqlite_evidence(conn: &Connection) -> Result<Vec<LegacyAppliedMigration>> {
    if !sqlite_table_exists(conn, "schema_migrations")? {
        return Ok(Vec::new());
    }

    let columns = sqlite_table_columns(conn, "schema_migrations")?;
    for required in ["version", "name", "applied_at", "applied_by"] {
        if !columns.contains(required) {
            bail!("legacy schema_migrations table is missing required column '{required}'");
        }
    }
    let checksum_column = if columns.contains("checksum_up") {
        "checksum_up"
    } else if columns.contains("checksum") {
        "checksum"
    } else {
        bail!("legacy schema_migrations table has no checksum column");
    };
    let duration_expr = if columns.contains("duration_ms") {
        "duration_ms"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT version, name, {checksum_column}, applied_at, applied_by, {duration_expr} FROM schema_migrations ORDER BY version ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(LegacyAppliedMigration {
            version: row.get(0)?,
            name: row.get(1)?,
            checksum: row.get(2)?,
            applied_at: row.get(3)?,
            applied_by: row.get(4)?,
            duration_ms: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn sqlite_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
         )",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
    .map_err(Into::into)
}

fn sqlite_table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    columns
        .collect::<rusqlite::Result<Vec<_>>>()
        .map(|columns| columns.into_iter().collect())
        .map_err(Into::into)
}

fn validate_postgres_identifier(value: &str, label: &str) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        bail!("{label} must not be empty");
    };
    if !(first == '_' || first.is_ascii_alphabetic()) || !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        bail!("invalid {label} '{value}'");
    }
    Ok(())
}

pub fn known_target_version(report: &MigrationReport, target: &str) -> Result<i64> {
    let target = target
        .trim()
        .parse::<i64>()
        .with_context(|| format!("target migration version '{target}' must be an integer"))?;
    let registry = match classify_memory_backend(&report.backend) {
        MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid => SqliteMemory::memory_schema_migration_registry(),
        MemoryBackendKind::Postgres => PostgresMemory::memory_schema_migration_registry(),
        _ => bail!(
            "schema migration planning is unsupported for backend '{}'",
            report.backend
        ),
    };
    if registry.iter().all(|(version, _, _)| *version != target) {
        bail!(
            "unknown target migration version {target} for backend '{}'",
            report.backend
        );
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rusqlite::params;
    use tempfile::TempDir;

    fn create_authoritative_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE memory_schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL
            )",
        )
        .unwrap();
    }

    fn insert_sqlite_registry_row(conn: &Connection, version: i64) {
        let (_, name, descriptor) = SqliteMemory::memory_schema_migration_registry()
            .iter()
            .find(|(candidate, _, _)| *candidate == version)
            .copied()
            .unwrap();
        conn.execute(
            "INSERT INTO memory_schema_migrations (version, name, checksum, applied_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                version,
                name,
                SqliteMemory::schema_migration_checksum(descriptor),
                Utc::now().to_rfc3339()
            ],
        )
        .unwrap();
    }

    #[test]
    fn status_probe_does_not_create_legacy_ledger() {
        let conn = Connection::open_in_memory().unwrap();
        create_authoritative_table(&conn);
        insert_sqlite_registry_row(&conn, 1);

        let status = status_sqlite(&conn).unwrap();
        assert_eq!(status.applied.len(), 1);
        assert!(!sqlite_table_exists(&conn, "schema_migrations").unwrap());
    }

    #[test]
    fn verify_missing_authoritative_ledger_is_non_success() {
        let conn = Connection::open_in_memory().unwrap();
        let error = status_sqlite(&conn).expect_err("missing authoritative ledger must fail");
        assert!(error.to_string().contains("memory_schema_migrations ledger is missing"));
        assert!(!sqlite_table_exists(&conn, "schema_migrations").unwrap());
    }

    #[test]
    fn authoritative_checksum_mismatch_is_non_success() {
        let conn = Connection::open_in_memory().unwrap();
        create_authoritative_table(&conn);
        let (_, name, _) = SqliteMemory::memory_schema_migration_registry()
            .first()
            .copied()
            .expect("migration registry must contain its baseline");
        conn.execute(
            "INSERT INTO memory_schema_migrations (version, name, checksum, applied_at) VALUES (1, ?1, 'bad', ?2)",
            params![name, Utc::now().to_rfc3339()],
        )
        .unwrap();

        let error = status_sqlite(&conn).expect_err("checksum mismatch must fail");
        assert!(error.to_string().contains("checksum mismatch for version 1"));
    }

    #[test]
    fn authoritative_unknown_version_is_non_success() {
        let conn = Connection::open_in_memory().unwrap();
        create_authoritative_table(&conn);
        conn.execute(
            "INSERT INTO memory_schema_migrations (version, name, checksum, applied_at) VALUES (999, 'future', 'unknown', ?1)",
            [Utc::now().to_rfc3339()],
        )
        .unwrap();

        let error = status_sqlite(&conn).expect_err("unknown version must fail");
        assert!(error.to_string().contains("unknown version 999"));
    }

    #[test]
    fn legacy_synthetic_ledger_is_compatibility_evidence_only() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                version TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                checksum_up TEXT NOT NULL,
                applied_at TEXT NOT NULL,
                applied_by TEXT NOT NULL
            );
             INSERT INTO schema_migrations VALUES (
                'PRX-2026.05-000', 'baseline_existing_schema', 'legacy',
                '2026-01-01T00:00:00Z', 'old-cli'
             );",
        )
        .unwrap();

        let error = status_sqlite(&conn).expect_err("legacy evidence cannot replace the authoritative ledger");
        assert!(error.to_string().contains("authoritative memory_schema_migrations"));

        create_authoritative_table(&conn);
        insert_sqlite_registry_row(&conn, 1);
        let status = status_sqlite(&conn).unwrap();
        assert_eq!(status.legacy_applied.len(), 1);
        assert_eq!(
            status
                .legacy_applied
                .first()
                .map(|migration| migration.version.as_str()),
            Some("PRX-2026.05-000")
        );
    }

    #[test]
    fn invalid_postgres_schema_identifier_is_rejected() {
        let error = validate_postgres_identifier("public;DROP", "storage schema").unwrap_err();
        assert!(error.to_string().contains("invalid storage schema"));
    }

    #[test]
    fn configured_sqlite_missing_database_is_not_created() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("missing-workspace");
        let mut config = Config::default();
        config.workspace_dir = workspace.clone();
        config.memory.backend = "sqlite".to_string();

        let error = inspect_configured_backend(&config).expect_err("missing database must fail");
        assert!(error.to_string().contains("authoritative memory database is missing"));
        assert!(!workspace.exists(), "inspection must not create the workspace");
    }

    #[test]
    fn configured_sqlite_inspection_is_byte_for_byte_read_only() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        let db_path = workspace.join("memory").join("brain.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&db_path).unwrap();
            create_authoritative_table(&conn);
            insert_sqlite_registry_row(&conn, 1);
        }
        let before = std::fs::read(&db_path).unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace;
        config.memory.backend = "sqlite".to_string();

        let report = inspect_configured_backend(&config).unwrap();
        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(report.status.applied.len(), 1);
        assert_eq!(before, after, "inspection must not mutate the database file");
    }

    #[test]
    fn configured_unsupported_backend_is_explicit_non_success() {
        let mut config = Config::default();
        config.memory.backend = "markdown".to_string();

        let error = inspect_configured_backend(&config).expect_err("unsupported backend must fail");
        assert!(error.to_string().contains("unsupported"));
        assert!(error.to_string().contains("markdown"));
    }
}
