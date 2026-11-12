//! Storage upgrades exercise real init, including a migration-22 database.
use super::*;
use crate::memory::sqlite::agent_registry::{MANIFEST_STORES, manifest_schema_sql};

#[test]
fn migration_22_upgrade_accepts_all_messaging_manifest_stores() {
    let dir = TempDir::new()
        .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: fixture setup succeeds");
    let memory = SqliteMemory::new(dir.path())
        .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: SqliteMemory::new succeeds");
    drop(memory);
    let path = dir.path().join("memory/brain.db");
    let conn = Connection::open(&path)
        .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: Connection::open succeeds");
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: execute_batch succeeds");
    let mut old = manifest_schema_sql("agent_erase_manifest_old");
    for store in MANIFEST_STORES.into_iter().skip(33) {
        old = old.replace(&format!(",'{store}'"), "");
    }
    conn.execute_batch(&old)
        .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: execute_batch succeeds");
    conn.execute_batch(
        "DROP TABLE agent_erase_manifest;
         ALTER TABLE agent_erase_manifest_old RENAME TO agent_erase_manifest;
         CREATE TABLE child_sessions_old(key TEXT PRIMARY KEY NOT NULL,record TEXT NOT NULL CHECK(json_valid(record))) STRICT;
         DROP TABLE child_sessions;
         ALTER TABLE child_sessions_old RENAME TO child_sessions;
         DELETE FROM memory_schema_migrations WHERE version>22;
         DROP TABLE prx_mailbox_messages;
         DROP TABLE prx_mailbox_receipts;
         DROP TABLE prx_mailbox_outbox;
         DROP TABLE prx_mailbox_parents;
         DROP TABLE prx_mailbox_seq;",
    ).expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: fixture setup succeeds");
    let erase = "00000000-0000-4000-8000-000000000001";
    conn.execute(
        "INSERT INTO agent_erase_manifest VALUES (?1,'memories','preserved',7)",
        [erase],
    )
    .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: execute succeeds");
    let record = r#"{ "title": "preserved", "usage": 3, "tool_calls": [1] }"#;
    conn.execute("INSERT INTO child_sessions VALUES ('legacy',?1)", [record])
        .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: execute succeeds");
    conn.execute_batch("COMMIT")
        .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: execute_batch succeeds");
    drop(conn);

    for _ in 0..2 {
        let memory = SqliteMemory::new(dir.path())
            .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: SqliteMemory::new succeeds");
        drop(memory);
        let conn = Connection::open(&path)
            .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: Connection::open succeeds");
        assert_eq!(
            conn.query_row(
                "SELECT fts_rowid FROM agent_erase_manifest WHERE record_key='preserved'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: fixture setup succeeds"),
            7,
            "migration 22 upgrade accepts all messaging manifest stores: expected messaging contract changed"
        );
        assert_eq!(
            conn.query_row(
                "SELECT record FROM child_sessions WHERE key='legacy' AND workspace_id IS NULL",
                [],
                |r| r.get::<_, String>(0)
            )
            .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: fixture setup succeeds"),
            record,
            "migration 22 upgrade accepts all messaging manifest stores: expected messaging contract changed"
        );
        for store in MANIFEST_STORES.into_iter().skip(33) {
            conn.execute(
                "INSERT OR IGNORE INTO agent_erase_manifest VALUES (?1,?2,'new',NULL)",
                params![erase, store],
            )
            .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: fixture setup succeeds");
        }
        assert!(
            conn.execute(
                "INSERT INTO agent_erase_manifest VALUES (?1,'unknown','new',NULL)",
                [erase]
            )
            .is_err(),
            "migration 22 upgrade accepts all messaging manifest stores: expected messaging contract changed"
        );
        assert!(
            conn.execute(
                "INSERT INTO agent_erase_manifest VALUES ('invalid','memories','new',NULL)",
                []
            )
            .is_err(),
            "migration 22 upgrade accepts all messaging manifest stores: expected messaging contract changed"
        );
        assert!(
            conn.execute(
                "INSERT INTO agent_erase_manifest VALUES (?1,'memories',?2,NULL)",
                params![erase, "bad\0key"]
            )
            .is_err(),
            "migration 22 upgrade accepts all messaging manifest stores: expected messaging contract changed"
        );
        let foreign_keys: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))
            .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: query_row succeeds");
        assert_eq!(
            foreign_keys, 0,
            "migration 22 upgrade accepts all messaging manifest stores: expected messaging contract changed"
        );
        let migrations: i64 = conn
            .query_row(
                "SELECT count(*) FROM memory_schema_migrations WHERE version IN (23,24,25)",
                [],
                |r| r.get(0),
            )
            .expect("test: migration_22_upgrade_accepts_all_messaging_manifest_stores: query_row succeeds");
        assert_eq!(
            migrations, 3,
            "migration 22 upgrade accepts all messaging manifest stores: expected messaging contract changed"
        );
    }
}

#[test]
fn mailbox_schema_rejects_unknown_enum() {
    let dir = TempDir::new().expect("test: mailbox_schema_rejects_unknown_enum: TempDir::new succeeds");
    let _memory =
        SqliteMemory::new(dir.path()).expect("test: mailbox_schema_rejects_unknown_enum: SqliteMemory::new succeeds");
    let conn = Connection::open(dir.path().join("memory/brain.db"))
        .expect("test: mailbox_schema_rejects_unknown_enum: Connection::open succeeds");
    // Nullable roles make this an isolated enum test; the store rejects an
    // unbound role before inserting it through the mailbox API.
    let sql = "INSERT INTO prx_mailbox_seq(workspace_id,owner_id,from_addr,to_addr,wire_stream_id,state,progress,updated_at) VALUES ('w','o','{}','{}',x'00',9,'{}',0)";
    assert!(
        conn.execute(sql, [])
            .unwrap_err()
            .to_string()
            .contains("CHECK constraint failed"),
        "mailbox schema rejects unknown enum: expected messaging contract changed"
    );
}

#[test]
fn mailbox_storage_stays_within_owner_budget() {
    let dir = TempDir::new().expect("test: mailbox_storage_stays_within_owner_budget: TempDir::new succeeds");
    let _memory = SqliteMemory::new(dir.path())
        .expect("test: mailbox_storage_stays_within_owner_budget: SqliteMemory::new succeeds");
    let conn = Connection::open(dir.path().join("memory/brain.db"))
        .expect("test: mailbox_storage_stays_within_owner_budget: Connection::open succeeds");
    let tables = [
        ("prx_mailbox_seq", 15, 4),
        ("prx_mailbox_receipts", 15, 4),
        ("prx_mailbox_messages", 12, 3),
        ("prx_mailbox_parents", 11, 3),
        ("prx_mailbox_outbox", 15, 4),
    ];
    for (table, columns, trees) in tables {
        let count: i64 = conn
            .query_row("SELECT count(*) FROM pragma_table_info(?1)", [table], |r| r.get(0))
            .expect("test: mailbox_storage_stays_within_owner_budget: query_row succeeds");
        assert_eq!(count, columns, "{table}");
        let indexes: i64 = conn
            .query_row("SELECT count(*) FROM pragma_index_list(?1)", [table], |r| r.get(0))
            .expect("test: mailbox_storage_stays_within_owner_budget: query_row succeeds");
        assert_eq!(indexes + 1, trees, "{table}");
    }
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name LIKE 'prx_mailbox_%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .expect("test: mailbox_storage_stays_within_owner_budget: fixture setup succeeds"),
        0,
        "mailbox storage stays within owner budget: recipient authority or scope isolation changed"
    );
}
