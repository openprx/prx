#![cfg(test)]
//! Initialization has one commit and never leaves a partial schema.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn count_commits(connection: &Connection) -> Arc<AtomicUsize> {
    let commits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&commits);
    connection
        .commit_hook(Some(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            false
        }))
        .unwrap();
    commits
}

#[test]
fn fresh_and_reopened_schemas_each_commit_only_once() {
    let connection = Connection::open_in_memory().unwrap();
    let commits = count_commits(&connection);
    SqliteMemory::init_schema(&connection).unwrap();
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(connection.is_autocommit());
    let count: i64 = connection
        .query_row("SELECT count(*) FROM memory_schema_migrations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        usize::try_from(count).unwrap(),
        SqliteMemory::memory_schema_migration_registry().len()
    );
    SqliteMemory::init_schema(&connection).unwrap();
    assert_eq!(commits.load(Ordering::SeqCst), 2);
    assert!(connection.is_autocommit());
}

#[test]
fn a_late_migration_conflict_rolls_back_every_schema_change() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE memory_schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL);").unwrap();
    let (version, name, descriptor) = SqliteMemory::memory_schema_migration_registry().last().unwrap();
    connection
        .execute(
            "INSERT INTO memory_schema_migrations VALUES (?1,?2,'prx_invalid_checksum','prx_date')",
            params![version, name],
        )
        .unwrap();
    let commits = count_commits(&connection);
    let error = SqliteMemory::init_schema(&connection).unwrap_err();
    assert!(error.to_string().contains("migration checksum mismatch"));
    assert!(connection.is_autocommit());
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    let tables: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(tables, ["memory_schema_migrations"]);
    let count: i64 = connection
        .query_row("SELECT count(*) FROM memory_schema_migrations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    connection
        .execute(
            "UPDATE memory_schema_migrations SET checksum=?1 WHERE version=?2",
            params![SqliteMemory::schema_migration_checksum(descriptor), version],
        )
        .unwrap();
    commits.store(0, Ordering::SeqCst);
    SqliteMemory::init_schema(&connection).unwrap();
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(connection.is_autocommit());
}
