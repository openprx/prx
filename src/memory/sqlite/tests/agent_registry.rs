//! MA-01b1a: the multi-agent registry and erase schema constraints.
use super::*;

const W: &str = "prx_workspace";
const O: &str = "prx_owner";
const REV: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const U1: &str = "00000000-0000-4000-8000-000000000001";
const U2: &str = "00000000-0000-4000-8000-000000000002";
const U3: &str = "00000000-0000-4000-8000-000000000003";
const SCOPE: &str = "00000000-0000-4000-8000-0000000000a1";
const ERASE: &str = "00000000-0000-4000-8000-0000000000b1";
const KEY: &str = "idempotency-key-0001";

const STORES: [&str; 33] = [
    "conversation_turns",
    "message_events",
    "memories",
    "documents",
    "document_chunks",
    "embedding_cache",
    "memory_events",
    "memory_drafts",
    "memory_trash",
    "memory_links",
    "retrieval_traces",
    "compaction_runs",
    "topics",
    "topic_participants",
    "topic_aliases",
    "router_history_v1",
    "router_learning_state",
    "router_learning_receipts",
    "chat_profiles",
    "chat_sessions",
    "chat_assignments",
    "chat_results",
    "decisions",
    "outcomes",
    "child_outcomes",
    "engine_outcomes",
    "access_audit_log",
    "approval_grants",
    "approval_grant_events",
    "approval_grant_revocations",
    "ingress_recovery",
    "recovery_attestations",
    "ramflux_blobs",
];

fn open(dir: &TempDir) -> (SqliteMemory, Connection) {
    let memory = SqliteMemory::new(dir.path()).unwrap();
    let conn = Connection::open(dir.path().join("memory/brain.db")).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    (memory, conn)
}

fn incarnation(conn: &Connection, key: &str, id: &str, state: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![W, O, key, id, state, REV],
    )
}

fn scope(conn: &Connection, scope_id: &str, session: Option<&str>, key: &str, inc: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
        params![scope_id, W, O, session, key, inc],
    )
}

fn receipt(
    conn: &Connection,
    erase: &str,
    state: &str,
    remaining: Option<i64>,
    done: Option<i64>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO agent_erase_receipts VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8)",
        params![W, O, erase, SCOPE, U1, state, remaining, done],
    )
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn seed(conn: &Connection) {
    incarnation(conn, "n:alpha", U1, "active").unwrap();
    scope(conn, SCOPE, None, "n:alpha", U1).unwrap();
}

#[test]
fn registry_unique_active_incarnation() {
    let dir = TempDir::new().unwrap();
    let (memory, conn) = open(&dir);
    incarnation(&conn, "n:alpha", U1, "active").unwrap();
    let error = incarnation(&conn, "n:alpha", U2, "active").unwrap_err().to_string();
    assert_eq!(
        error,
        "UNIQUE constraint failed: agent_incarnations.workspace_id, agent_incarnations.owner_id, agent_incarnations.agent_key"
    );
    conn.execute(
        "UPDATE agent_incarnations SET state = 'retired' WHERE incarnation = ?1",
        [U1],
    )
    .unwrap();
    incarnation(&conn, "n:alpha", U3, "active").unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM agent_incarnations"), 2);
    drop(memory);
    for _ in 0..2 {
        drop(SqliteMemory::new(dir.path()).unwrap());
    }
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM memory_schema_migrations WHERE version = 18"
        ),
        1
    );
}

#[test]
fn registry_rejects_unknown_enum() {
    let dir = TempDir::new().unwrap();
    let (_memory, conn) = open(&dir);
    seed(&conn);
    let mut cases: Vec<(&str, rusqlite::Result<usize>)> = Vec::new();
    // Positive controls, one per table.
    receipt(&conn, ERASE, "pending", None, None).unwrap();
    incarnation(&conn, "m", U3, "active").unwrap();
    conn.execute(
        "INSERT INTO agent_erase_fences VALUES (?1, ?2, 'draining', 2)",
        params![SCOPE, ERASE],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_erase_keys VALUES (?1, ?2, ?3, 'DELETE', '/p', ?4, ?5, ?6, 1, NULL)",
        params![W, O, KEY, SCOPE, REV, ERASE],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_purge_progress VALUES (?1, ?2, 'n:alpha', ?3, ?4, 'draining', NULL, NULL)",
        params![W, O, U1, U2],
    )
    .unwrap();
    scope(&conn, U2, Some("s"), "n:alpha", U1).unwrap();
    for store in STORES {
        conn.execute(
            "INSERT INTO agent_erase_manifest VALUES (?1, ?2, '{}', NULL)",
            params![ERASE, store],
        )
        .unwrap();
    }
    assert_eq!(count(&conn, "SELECT count(*) FROM agent_erase_manifest"), 33);

    let long = "x".repeat(4097);
    let long_o = "x".repeat(513);
    let manifest = |store: &str, key: String, fts: Option<i64>| {
        conn.execute(
            "INSERT INTO agent_erase_manifest VALUES (?1, ?2, ?3, ?4)",
            params![ERASE, store, key, fts],
        )
    };
    let inc = |w: &str, o: &str, key: &str, id: &str, state: &str, rev: &str| {
        conn.execute(
            "INSERT INTO agent_incarnations VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![w, o, key, id, state, rev],
        )
    };
    let u4 = "00000000-0000-4000-8000-000000000004";
    cases.push(("incarnation state", inc(W, O, "n:b", u4, "bogus", REV)));
    cases.push(("workspace too long", inc(&long, O, "n:b", u4, "active", REV)));
    cases.push(("owner too long", inc(W, &long_o, "n:b", u4, "active", REV)));
    cases.push(("key n:main", inc(W, O, "n:main", u4, "active", REV)));
    cases.push(("key bad grammar", inc(W, O, "n:Bad", u4, "active", REV)));
    cases.push((
        "key too long",
        inc(W, O, &format!("n:a{}", "a".repeat(65)), u4, "active", REV),
    ));
    cases.push(("key unprefixed", inc(W, O, "alpha", u4, "active", REV)));
    cases.push((
        "uuid uppercase",
        inc(W, O, "n:b", "0000000A-0000-4000-8000-000000000004", "active", REV),
    ));
    cases.push(("uuid short", inc(W, O, "n:b", "abc", "active", REV)));
    cases.push(("revision not hex", inc(W, O, "n:b", u4, "active", &"G".repeat(64))));
    cases.push((
        "revision uppercase",
        inc(W, O, "n:b", u4, "active", &REV.to_uppercase()),
    ));
    cases.push(("main without session", scope(&conn, u4, None, "m", U3)));
    cases.push(("scope without incarnation", scope(&conn, u4, None, "n:zzz", u4)));
    cases.push(("scope session too long", scope(&conn, u4, Some(&long_o), "n:alpha", U1)));
    cases.push((
        "scope generation zero",
        conn.execute(
            "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, NULL, 'n:alpha', ?4, 0)",
            params![u4, W, O, U1],
        ),
    ));
    cases.push((
        "scope generation text",
        conn.execute(
            "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, NULL, 'n:alpha', ?4, 'abc')",
            params![u4, W, O, U1],
        ),
    ));
    cases.push(("private scope duplicate", scope(&conn, u4, None, "n:alpha", U1)));
    cases.push(("session scope duplicate", scope(&conn, u4, Some("s"), "n:alpha", U1)));
    cases.push((
        "fence state",
        conn.execute(
            "INSERT INTO agent_erase_fences VALUES (?1, ?2, 'bogus', 1)",
            params![U2, ERASE],
        ),
    ));
    cases.push((
        "fence generation zero",
        conn.execute(
            "INSERT INTO agent_erase_fences VALUES (?1, ?2, 'draining', 0)",
            params![U2, ERASE],
        ),
    ));
    cases.push((
        "fence unknown scope",
        conn.execute(
            "INSERT INTO agent_erase_fences VALUES (?1, ?2, 'draining', 1)",
            params![u4, ERASE],
        ),
    ));
    cases.push(("receipt state", receipt(&conn, u4, "bogus", None, None)));
    cases.push((
        "receipt pending with remaining",
        receipt(&conn, u4, "pending", Some(1), None),
    ));
    cases.push((
        "receipt erased without time",
        receipt(&conn, u4, "erased", Some(0), None),
    ));
    cases.push((
        "receipt erased remaining",
        receipt(&conn, u4, "erased", Some(3), Some(5)),
    ));
    cases.push((
        "receipt cross-owner scope",
        conn.execute(
            "INSERT INTO agent_erase_receipts VALUES (?1, 'other', ?2, ?3, ?4, 1, 'pending', NULL, NULL)",
            params![W, u4, SCOPE, U1],
        ),
    ));
    let key = |idem: &str, hash: &str, owner: &str| {
        conn.execute(
            "INSERT INTO agent_erase_keys VALUES (?1, ?2, ?3, 'DELETE', '/p', ?4, ?5, ?6, 1, NULL)",
            params![W, owner, idem, SCOPE, hash, ERASE],
        )
    };
    cases.push(("key too short", key("short", REV, O)));
    cases.push(("key too long", key(&"k".repeat(129), REV, O)));
    cases.push(("key control byte", key("idempotency-key-\u{7f}x", REV, O)));
    cases.push(("key hash", key("idempotency-key-0002", "abc", O)));
    cases.push(("key owner mismatch", key("idempotency-key-0003", REV, "other")));
    cases.push(("manifest store", manifest("bogus", "{}".into(), None)));
    cases.push(("manifest record too long", manifest("memories", "x".repeat(1025), None)));
    cases.push(("manifest fts zero", manifest("memories", "{\"a\":1}".into(), Some(0))));
    let purge = |key: &str, id: &str, state: &str, after: Option<&str>| {
        conn.execute(
            "INSERT INTO agent_purge_progress VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![W, O, key, id, U2, state, after],
        )
    };
    cases.push(("purge state", purge("n:alpha", U3, "bogus", None)));
    cases.push(("purge main key", purge("m", U3, "draining", None)));
    cases.push(("purge cursor", purge("n:alpha", U3, "draining", Some("nope"))));
    cases.push(("purge unknown incarnation", purge("n:alpha", U3, "draining", None)));
    for (name, result) in cases {
        let error = result.expect_err(name);
        assert!(
            matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ConstraintViolation)
            ),
            "{name}: expected a constraint error, got {error}"
        );
    }

    // NULL arms: NOT NULL columns refuse NULL.
    for sql in [
        "INSERT INTO agent_incarnations VALUES ('w', 'o', 'n:b', '00000000-0000-4000-8000-000000000004', 'active', NULL)",
        "INSERT INTO agent_scopes VALUES ('00000000-0000-4000-8000-000000000004', 'w', 'o', NULL, 'n:alpha', NULL, 1)",
        "INSERT INTO agent_erase_fences VALUES ('00000000-0000-4000-8000-000000000002', NULL, 'draining', 1)",
        "INSERT INTO agent_erase_receipts VALUES ('w', 'o', '00000000-0000-4000-8000-000000000004', NULL, NULL, 1, 'pending', NULL, NULL)",
        "INSERT INTO agent_erase_keys VALUES ('w', 'o', 'idempotency-key-0009', 'DELETE', '/p', NULL, NULL, NULL, 1, NULL)",
        "INSERT INTO agent_erase_manifest VALUES ('00000000-0000-4000-8000-0000000000b1', NULL, '{}', NULL)",
        "INSERT INTO agent_purge_progress VALUES ('w', 'o', 'n:alpha', NULL, NULL, 'draining', NULL, NULL)",
    ] {
        let error = conn.execute(sql, []).unwrap_err().to_string();
        assert!(error.contains("NOT NULL constraint failed"), "{sql}: {error}");
    }

    // Immutability triggers.
    conn.execute(
        "UPDATE agent_erase_receipts SET state = 'erased', remaining = 0, completed_at = 9",
        [],
    )
    .unwrap();
    for sql in [
        "UPDATE agent_erase_receipts SET completed_at = 10",
        "UPDATE agent_erase_receipts SET state = 'pending', remaining = NULL, completed_at = NULL",
        "DELETE FROM agent_erase_receipts",
    ] {
        let error = conn.execute(sql, []).unwrap_err().to_string();
        assert!(error.contains("erased receipt is immutable"), "{sql}: {error}");
    }
}

#[test]
fn registry_null_arm_erased_receipt_rejected() {
    let dir = TempDir::new().unwrap();
    let (_memory, conn) = open(&dir);
    seed(&conn);
    let error = receipt(&conn, ERASE, "erased", None, Some(5)).unwrap_err();
    assert!(
        matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        ),
        "insert with NULL remaining: {error}"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM agent_erase_receipts"), 0);
    receipt(&conn, ERASE, "pending", None, None).unwrap();
    let error = conn
        .execute(
            "UPDATE agent_erase_receipts SET state = 'erased', remaining = NULL, completed_at = 5",
            [],
        )
        .unwrap_err();
    assert!(
        matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        ),
        "update to NULL remaining: {error}"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM agent_erase_receipts WHERE state = 'pending'"
        ),
        1
    );
}

#[test]
fn registry_rejects_embedded_nul() {
    let dir = TempDir::new().unwrap();
    let (_memory, conn) = open(&dir);
    seed(&conn);
    receipt(&conn, ERASE, "pending", None, None).unwrap();
    let u4 = "00000000-0000-4000-8000-000000000004";
    let nul_uuid = format!("{u4}\0x");
    let nul_hash = format!("{REV}\0x");
    let cases: Vec<(&str, rusqlite::Result<usize>)> = vec![
        ("agent_key n:main", incarnation(&conn, "n:main\0x", u4, "active")),
        ("agent_key n:b", incarnation(&conn, "n:b\0x", u4, "active")),
        ("agent_key m", incarnation(&conn, "m\0x", u4, "active")),
        ("incarnation uuid", incarnation(&conn, "n:b", &nul_uuid, "active")),
        (
            "incarnation uuid 36 bytes",
            incarnation(&conn, "n:b", "00000000-0000-4000-8000-0000000000\0\0", "active"),
        ),
        (
            "config revision",
            conn.execute(
                "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:b', ?3, 'active', ?4)",
                params![W, O, u4, nul_hash],
            ),
        ),
        (
            "workspace",
            conn.execute(
                "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:b', ?3, 'active', ?4)",
                params![format!("{W}\0x"), O, u4, REV],
            ),
        ),
        (
            "owner",
            conn.execute(
                "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:b', ?3, 'active', ?4)",
                params![W, format!("{O}\0x"), u4, REV],
            ),
        ),
        ("scope uuid", scope(&conn, &nul_uuid, None, "n:alpha", U1)),
        ("scope session", scope(&conn, u4, Some("s\0x"), "n:alpha", U1)),
        (
            "fence erase id",
            conn.execute(
                "INSERT INTO agent_erase_fences VALUES (?1, ?2, 'draining', 1)",
                params![SCOPE, nul_uuid],
            ),
        ),
        ("receipt erase id", receipt(&conn, &nul_uuid, "pending", None, None)),
        (
            "key idempotency",
            conn.execute(
                "INSERT INTO agent_erase_keys VALUES (?1, ?2, ?3, 'DELETE', '/p', ?4, ?5, ?6, 1, NULL)",
                params![W, O, "idempotency-key\0-0001", SCOPE, REV, ERASE],
            ),
        ),
        (
            "key request hash",
            conn.execute(
                "INSERT INTO agent_erase_keys VALUES (?1, ?2, ?3, 'DELETE', '/p', ?4, ?5, ?6, 1, NULL)",
                params![W, O, KEY, SCOPE, nul_hash, ERASE],
            ),
        ),
        (
            "key method",
            conn.execute(
                "INSERT INTO agent_erase_keys VALUES (?1, ?2, ?3, ?7, '/p', ?4, ?5, ?6, 1, NULL)",
                params![W, O, KEY, SCOPE, REV, ERASE, "DE\0LETE"],
            ),
        ),
        (
            "manifest record key",
            conn.execute(
                "INSERT INTO agent_erase_manifest VALUES (?1, 'memories', ?2, NULL)",
                params![ERASE, "a\0b"],
            ),
        ),
        (
            "purge cursor",
            conn.execute(
                "INSERT INTO agent_purge_progress VALUES (?1, ?2, 'n:alpha', ?3, ?4, 'draining', ?5, NULL)",
                params![W, O, U1, U2, nul_uuid],
            ),
        ),
        (
            "purge key",
            conn.execute(
                "INSERT INTO agent_purge_progress VALUES (?1, ?2, ?3, ?4, ?5, 'draining', NULL, NULL)",
                params![W, O, "n:alpha\0x", U1, U2],
            ),
        ),
    ];
    for (name, result) in cases {
        let error = result.expect_err(name);
        assert!(
            matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ConstraintViolation)
            ),
            "{name}: expected a constraint error, got {error}"
        );
    }
}

#[test]
fn registry_replace_never_touches_erased_receipt() {
    let dir = TempDir::new().unwrap();
    let (_memory, conn) = open(&dir);
    seed(&conn);
    let other = "00000000-0000-4000-8000-0000000000b2";
    receipt(&conn, ERASE, "erased", Some(0), Some(7)).unwrap();
    receipt(&conn, other, "pending", None, None).unwrap();
    let snapshot = |conn: &Connection| -> String {
        conn.query_row(
            "SELECT group_concat(erase_id || state || ifnull(remaining, 'n') || ifnull(completed_at, 'n'), '|') \
             FROM (SELECT * FROM agent_erase_receipts ORDER BY erase_id)",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let before = snapshot(&conn);

    let error = conn
        .execute(
            "INSERT OR REPLACE INTO agent_erase_receipts VALUES (?1, ?2, ?3, ?4, ?5, 1, 'pending', NULL, NULL)",
            params![W, O, ERASE, SCOPE, U1],
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("erased receipt is immutable"),
        "insert or replace: {error}"
    );
    assert_eq!(snapshot(&conn), before);

    let error = conn
        .execute(
            "UPDATE OR REPLACE agent_erase_receipts SET erase_id = ?1 WHERE erase_id = ?2",
            params![ERASE, other],
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("erased receipt is immutable"),
        "update or replace: {error}"
    );
    assert_eq!(snapshot(&conn), before);
}
