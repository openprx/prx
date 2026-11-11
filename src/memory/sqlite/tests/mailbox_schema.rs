//! Storage upgrades exercise real init, including a migration-22 database.
use super::*;
use crate::memory::sqlite::agent_registry::{MANIFEST_STORES, manifest_schema_sql};

#[test]
fn migration_22_upgrade_accepts_all_messaging_manifest_stores() {
    let dir = TempDir::new().unwrap();
    let memory = SqliteMemory::new(dir.path()).unwrap();
    drop(memory);
    let path = dir.path().join("memory/brain.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut old = manifest_schema_sql("agent_erase_manifest_old");
    for store in MANIFEST_STORES.into_iter().skip(33) {
        old = old.replace(&format!(",'{store}'"), "");
    }
    conn.execute_batch(&old).unwrap();
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
    ).unwrap();
    let erase = "00000000-0000-4000-8000-000000000001";
    conn.execute(
        "INSERT INTO agent_erase_manifest VALUES (?1,'memories','preserved',7)",
        [erase],
    )
    .unwrap();
    let record = r#"{ "title": "preserved", "usage": 3, "tool_calls": [1] }"#;
    conn.execute("INSERT INTO child_sessions VALUES ('legacy',?1)", [record])
        .unwrap();
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);

    for _ in 0..2 {
        let memory = SqliteMemory::new(dir.path()).unwrap();
        drop(memory);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT fts_rowid FROM agent_erase_manifest WHERE record_key='preserved'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            7
        );
        assert_eq!(
            conn.query_row(
                "SELECT record FROM child_sessions WHERE key='legacy' AND workspace_id IS NULL",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            record
        );
        for store in MANIFEST_STORES.into_iter().skip(33) {
            conn.execute(
                "INSERT OR IGNORE INTO agent_erase_manifest VALUES (?1,?2,'new',NULL)",
                params![erase, store],
            )
            .unwrap();
        }
        assert!(
            conn.execute(
                "INSERT INTO agent_erase_manifest VALUES (?1,'unknown','new',NULL)",
                [erase]
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "INSERT INTO agent_erase_manifest VALUES ('invalid','memories','new',NULL)",
                []
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "INSERT INTO agent_erase_manifest VALUES (?1,'memories',?2,NULL)",
                params![erase, "bad\0key"]
            )
            .is_err()
        );
        let foreign_keys: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 0);
        let migrations: i64 = conn
            .query_row(
                "SELECT count(*) FROM memory_schema_migrations WHERE version IN (23,24,25)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(migrations, 3);
    }
}

#[test]
fn mailbox_schema_rejects_unknown_enum() {
    let dir = TempDir::new().unwrap();
    let _memory = SqliteMemory::new(dir.path()).unwrap();
    let conn = Connection::open(dir.path().join("memory/brain.db")).unwrap();
    // Nullable roles make this an isolated enum test; the store rejects an
    // unbound role before inserting it through the mailbox API.
    let sql = "INSERT INTO prx_mailbox_seq(workspace_id,owner_id,from_addr,to_addr,wire_stream_id,state,progress,updated_at) VALUES ('w','o','{}','{}',x'00',9,'{}',0)";
    assert!(
        conn.execute(sql, [])
            .unwrap_err()
            .to_string()
            .contains("CHECK constraint failed")
    );
}

#[test]
fn mailbox_storage_stays_within_owner_budget() {
    let dir = TempDir::new().unwrap();
    let _memory = SqliteMemory::new(dir.path()).unwrap();
    let conn = Connection::open(dir.path().join("memory/brain.db")).unwrap();
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
            .unwrap();
        assert_eq!(count, columns, "{table}");
        let indexes: i64 = conn
            .query_row("SELECT count(*) FROM pragma_index_list(?1)", [table], |r| r.get(0))
            .unwrap();
        assert_eq!(indexes + 1, trees, "{table}");
    }
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name LIKE 'prx_mailbox_%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
