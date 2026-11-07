//! MA-01b1b: the scope columns and the legacy console migration to Main.
use super::super::{agent_migration, ingress_recovery, recovery_authority, router_history, router_learning};
use super::*;
use sha2::{Digest, Sha256};

const WS: &str = "ws1";
const UA: &str = "00000000-0000-4000-8000-0000000000a0";
const FENCED: &str = "00000000-0000-4000-8000-0000000000f1";
const ERASE: &str = "00000000-0000-4000-8000-0000000000e1";

fn main_revision() -> String {
    format!("{:x}", Sha256::digest(b"prx.ma.main.v1"))
}

fn open(dir: &TempDir) -> (SqliteMemory, Connection) {
    let memory = SqliteMemory::new(dir.path()).unwrap();
    let conn = Connection::open(dir.path().join("memory/brain.db")).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    (memory, conn)
}

fn session(conn: &Connection, key: &str, channel: &str, owner: &str) {
    conn.execute(
        "INSERT INTO sessions (session_key, channel, sender, owner_id, created_at, updated_at)
         VALUES (?1, ?2, 's', ?3, 't', 't')",
        params![key, channel, owner],
    )
    .unwrap();
}

fn turn(conn: &Connection, label: &str, session: &str, owner: Option<&str>, agent: Option<&str>) {
    conn.execute(
        "INSERT INTO conversation_turns (session_key, owner_id, role, content, timestamp, agent_id)
         VALUES (?1, ?2, 'user', ?3, 't', ?4)",
        params![session, owner, label, agent],
    )
    .unwrap();
}

fn event(conn: &Connection, id: &str, source: &str, session: &str, owner: Option<&str>, agent: Option<&str>) {
    conn.execute(
        "INSERT INTO message_events (event_id, workspace_id, owner_id, source, session_key, agent_id, role,
                                     event_type, content, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'user', 'message.created', ?1, 't', 't')",
        params![id, WS, owner, source, session, agent],
    )
    .unwrap();
}

fn event_scope(conn: &Connection, id: &str) -> Option<String> {
    conn.query_row(
        "SELECT ma_scope_id FROM message_events WHERE event_id = ?1",
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

fn turn_scope(conn: &Connection, label: &str) -> Option<String> {
    conn.query_row(
        "SELECT ma_scope_id FROM conversation_turns WHERE content = ?1",
        [label],
        |r| r.get(0),
    )
    .unwrap()
}

fn incarnation_of(conn: &Connection, scope: &str) -> String {
    conn.query_row(
        "SELECT incarnation FROM agent_scopes WHERE scope_id = ?1",
        [scope],
        |r| r.get(0),
    )
    .unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn seed_legacy(conn: &Connection) {
    session(conn, "s1", "console", "ownerA");
    turn(conn, "t1", "s1", Some("ownerA"), None);
    turn(conn, "t2", "s1", Some("ownerA"), Some("x"));
    event(conn, "e1", "console", "s1", Some("ownerA"), None);
    event(conn, "e2", "console", "s1", Some("ownerA"), Some("x"));
    event(conn, "e3", "agent", "s1", Some("ownerA"), None);
    // One console session holding NULL-agent rows from two owners.
    session(conn, "s2", "console", "ownerB");
    turn(conn, "t3", "s2", Some("ownerA"), None);
    turn(conn, "t4", "s2", Some("ownerB"), None);
    event(conn, "e4", "console", "s2", Some("ownerA"), None);
    event(conn, "e5", "console", "s2", Some("ownerB"), None);
    // A telegram session holding a console event and a NULL-agent turn.
    session(conn, "s3", "telegram", "ownerA");
    event(conn, "e6", "console", "s3", Some("ownerA"), None);
    turn(conn, "t5", "s3", Some("ownerA"), None);
    // A telegram session with a plain NULL-agent turn.
    session(conn, "s5", "telegram", "ownerA");
    turn(conn, "t6", "s5", Some("ownerA"), None);
    // A fenced Main scope for (ws1, ownerA, s6).
    session(conn, "s6", "console", "ownerA");
    event(conn, "e7", "console", "s6", Some("ownerA"), None);
    turn(conn, "t7", "s6", Some("ownerA"), None);
    event(conn, "e8", "console", "s6", None, None);
    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, 'ownerA', 'm', ?2, 'active', ?3)",
        params![WS, UA, main_revision()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, 'ownerA', 's6', 'm', ?3, 1)",
        params![FENCED, WS, UA],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_erase_fences VALUES (?1, ?2, 'draining', 2)",
        params![FENCED, ERASE],
    )
    .unwrap();
}

#[test]
fn legacy_console_history_is_main_only() {
    let dir = TempDir::new().unwrap();
    let (memory, conn) = open(&dir);
    seed_legacy(&conn);
    drop(memory);
    drop(conn);

    let (memory, conn) = open(&dir);
    // Only console NULL rows are stamped.
    for id in ["e2", "e3", "e7", "e8"] {
        assert_eq!(event_scope(&conn, id), None, "event {id} must stay NULL");
    }
    for label in ["t2", "t6", "t7"] {
        assert_eq!(turn_scope(&conn, label), None, "turn {label} must stay NULL");
    }
    // The telegram-session turn stays NULL even though its session has a console event.
    assert_eq!(turn_scope(&conn, "t5"), None, "telegram-session turn must stay NULL");

    // Each owner's rows sit under that owner's own Main incarnation.
    let owner_a: Vec<String> = ["e1", "e4", "e6"]
        .iter()
        .map(|id| event_scope(&conn, id).unwrap())
        .chain(["t1", "t3"].iter().map(|label| turn_scope(&conn, label).unwrap()))
        .collect();
    assert!(owner_a.iter().all(|scope| incarnation_of(&conn, scope) == UA));
    let b_event = event_scope(&conn, "e5").expect("owner B event");
    let b_turn = turn_scope(&conn, "t4").expect("owner B turn");
    assert_eq!(b_event, b_turn);
    let b_incarnation = incarnation_of(&conn, &b_event);
    assert_ne!(b_incarnation, UA, "owners must not share a Main incarnation");
    let b_owner: String = conn
        .query_row(
            "SELECT owner_id FROM agent_scopes WHERE scope_id = ?1",
            [&b_event],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(b_owner, "ownerB");

    // The fenced tuple: no new scope, nothing stamped, rows counted as unattributed.
    assert_eq!(
        count(&conn, "SELECT count(*) FROM agent_scopes WHERE session_key = 's6'"),
        1
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*) FROM message_events WHERE ma_scope_id = '{FENCED}'")
        ),
        0
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*) FROM conversation_turns WHERE ma_scope_id = '{FENCED}'")
        ),
        0
    );
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let left = agent_migration::migrate_legacy_console(&conn).unwrap();
    conn.execute_batch("COMMIT").unwrap();
    assert_eq!(left, 3, "e7, t7 (fenced) and e8 (no owner) are counted");

    // Main revision is the constant's hash.
    let revisions = count(
        &conn,
        &format!(
            "SELECT count(*) FROM agent_incarnations WHERE agent_key = 'm' AND config_revision = '{}'",
            main_revision()
        ),
    );
    assert_eq!(revisions, 2);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM agent_incarnations WHERE agent_key <> 'm'"),
        0
    );

    // Stable across a second reopen, with no duplicate scope.
    let scopes = count(&conn, "SELECT count(*) FROM agent_scopes");
    let incarnations = count(&conn, "SELECT count(*) FROM agent_incarnations");
    let a_scope = event_scope(&conn, "e1");
    drop(memory);
    drop(conn);
    let (_memory, conn) = open(&dir);
    assert_eq!(count(&conn, "SELECT count(*) FROM agent_scopes"), scopes);
    assert_eq!(count(&conn, "SELECT count(*) FROM agent_incarnations"), incarnations);
    assert_eq!(event_scope(&conn, "e1"), a_scope);
    assert_eq!(event_scope(&conn, "e5").as_deref(), Some(b_event.as_str()));
}

#[test]
fn every_inventory_table_has_a_scope_column() {
    let dir = TempDir::new().unwrap();
    let (_memory, conn) = open(&dir);
    // These schemas are lazy: create them first.
    ingress_recovery::schema(&conn).unwrap();
    recovery_authority::schema(&conn).unwrap();
    router_learning::schema(&conn).unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
    router_history::schema(&tx).unwrap();
    tx.commit().unwrap();
    let tables = [
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
        "chat_profiles",
        "access_audit_log",
        "approval_grants",
        "approval_grant_events",
        "approval_grant_revocations",
        "ingress_recovery",
        "recovery_attestations",
        "ramflux_blobs",
        "router_history_v1",
        "router_learning_state",
        "router_learning_receipts",
    ];
    for table in tables {
        let references = count(
            &conn,
            &format!(
                "SELECT count(*) FROM pragma_table_info('{table}') c
                 JOIN pragma_foreign_key_list('{table}') f ON f.\"from\" = c.name
                 WHERE c.name = 'ma_scope_id' AND f.\"table\" = 'agent_scopes' AND f.\"to\" = 'scope_id'"
            ),
        );
        assert_eq!(references, 1, "{table} must reference agent_scopes through ma_scope_id");
    }
}
