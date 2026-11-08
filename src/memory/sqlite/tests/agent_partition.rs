//! MA-01b2: transcript and event reads are partitioned by agent scope.
use super::*;
use crate::config::agent_name::AgentName;
use crate::memory::sqlite::events::conversation_turns_statement;
use crate::memory::{AgentPartition, ConversationTurn, PartitionKey};
use rusqlite::{Connection, params};

const W: &str = "prx_workspace";
const O: &str = "prx_owner";
const O2: &str = "other_owner";
const SESSION_A: &str = "session-a";
const SESSION_B: &str = "session-b";
const REV: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const UX: &str = "00000000-0000-4000-8000-0000000000a1";
const UY: &str = "00000000-0000-4000-8000-0000000000a2";
const UM: &str = "00000000-0000-4000-8000-0000000000b1";
const UM2: &str = "00000000-0000-4000-8000-0000000000b2";
const SX: &str = "00000000-0000-4000-8000-0000000000c1";
const SY: &str = "00000000-0000-4000-8000-0000000000c2";
const SM: &str = "00000000-0000-4000-8000-0000000000d1";
const SM2: &str = "00000000-0000-4000-8000-0000000000d2";

fn open(dir: &TempDir) -> (SqliteMemory, Connection) {
    let memory = SqliteMemory::new(dir.path()).unwrap();
    let conn = Connection::open(dir.path().join("memory/brain.db")).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    (memory, conn)
}

fn incarnation(conn: &Connection, owner: &str, key: &str, id: &str) {
    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, ?3, ?4, 'active', ?5)",
        params![W, owner, key, id, REV],
    )
    .unwrap();
}

fn scope(conn: &Connection, scope_id: &str, owner: &str, session: &str, key: &str, inc: &str) {
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
        params![scope_id, W, owner, session, key, inc],
    )
    .unwrap();
}

fn session(conn: &Connection, key: &str, owner: &str) {
    conn.execute(
        "INSERT INTO sessions (session_key, channel, sender, owner_id, created_at, updated_at)
         VALUES (?1, 'console', 's', ?2, 't', 't')",
        params![key, owner],
    )
    .unwrap();
}

fn turn(conn: &Connection, label: &str, session: &str, owner: &str, scope_id: Option<&str>) {
    conn.execute(
        "INSERT INTO conversation_turns (session_key, owner_id, role, content, timestamp, ma_scope_id)
         VALUES (?1, ?2, 'user', ?3, 't', ?4)",
        params![session, owner, label, scope_id],
    )
    .unwrap();
}

fn event(conn: &Connection, id: &str, session: &str, owner: &str, scope_id: Option<&str>) {
    conn.execute(
        "INSERT INTO message_events (event_id, workspace_id, owner_id, source, session_key, role,
                                     event_type, content, visibility, created_at, updated_at, ma_scope_id)
         VALUES (?1, ?2, ?3, 'console', ?4, 'user', 'message.created', ?1, 'session', 't', 't', ?5)",
        params![id, W, owner, session, scope_id],
    )
    .unwrap();
}

fn principal(session: &str, partition: Option<AgentPartition>) -> MemoryPrincipal {
    MemoryPrincipal {
        workspace_id: W.to_owned(),
        agent_id: Some("system".to_owned()),
        persona_id: None,
        session_key: Some(session.to_owned()),
        channel: Some("console".to_owned()),
        sender: Some("s".to_owned()),
        owner_id: Some("system:*".to_owned()),
        require_owner_match: false,
        legacy_session_key: None,
        agent_partition: partition,
    }
}

fn named(name: &str, incarnation: &str) -> AgentPartition {
    AgentPartition {
        workspace_id: W.to_owned(),
        owner_id: O.to_owned(),
        key: PartitionKey::Named {
            name: AgentName::parse(name).unwrap(),
            incarnation: incarnation.parse().unwrap(),
        },
    }
}

fn main_partition() -> AgentPartition {
    AgentPartition {
        workspace_id: W.to_owned(),
        owner_id: O.to_owned(),
        key: PartitionKey::Main,
    }
}

fn seed(conn: &Connection) {
    session(conn, SESSION_A, O);
    session(conn, SESSION_B, O);
    incarnation(conn, O, "n:x", UX);
    incarnation(conn, O, "n:y", UY);
    incarnation(conn, O, "m", UM);
    incarnation(conn, O2, "m", UM2);
    scope(conn, SX, O, SESSION_A, "n:x", UX);
    scope(conn, SY, O, SESSION_A, "n:y", UY);
    scope(conn, SM, O, SESSION_A, "m", UM);
    scope(conn, SM2, O2, SESSION_A, "m", UM2);
    turn(conn, "a-main", SESSION_A, O, Some(SM));
    turn(conn, "a-null", SESSION_A, O, None);
    turn(
        conn,
        "a-legacy-main",
        SESSION_A,
        &format!("legacy:{SESSION_A}"),
        Some(SM),
    );
    turn(conn, "a-x-1", SESSION_A, O, Some(SX));
    turn(conn, "a-x-2", SESSION_A, O, Some(SX));
    turn(conn, "a-x-3", SESSION_A, O, Some(SX));
    turn(conn, "a-y", SESSION_A, O, Some(SY));
    turn(conn, "a-other-main", SESSION_A, O2, Some(SM2));
    turn(conn, "b-null", SESSION_B, O, None);
    turn(conn, "b-main", SESSION_B, O, Some(SM));
    event(conn, "a-main", SESSION_A, O, Some(SM));
    event(conn, "a-null", SESSION_A, O, None);
    event(
        conn,
        "a-legacy-main",
        SESSION_A,
        &format!("legacy:{SESSION_A}"),
        Some(SM),
    );
    event(conn, "a-x-1", SESSION_A, O, Some(SX));
    event(conn, "a-x-2", SESSION_A, O, Some(SX));
    event(conn, "a-x-3", SESSION_A, O, Some(SX));
    event(conn, "a-y", SESSION_A, O, Some(SY));
    event(conn, "a-other-main", SESSION_A, O2, Some(SM2));
    event(conn, "b-null", SESSION_B, O, None);
    event(conn, "b-main", SESSION_B, O, Some(SM));
}

fn labels(turns: &[ConversationTurn]) -> Vec<&str> {
    turns.iter().map(|turn| turn.content.as_str()).collect()
}

fn event_ids(events: &[crate::memory::MessageEvent]) -> Vec<&str> {
    events.iter().map(|event| event.event_id.as_str()).collect()
}

fn explain(conn: &Connection, sql: &str, binds: &[&dyn rusqlite::types::ToSql]) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.query_map(binds, |row| row.get::<_, String>(row.as_ref().column_count() - 1))
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

fn extra_scans(partitioned: &[String], none: &[String]) -> Vec<String> {
    partitioned
        .iter()
        .filter(|line| line.contains("SCAN ") && !none.iter().any(|other| other == *line))
        .cloned()
        .collect()
}

#[tokio::test]
async fn agent_partition_filters_transcripts_and_events() {
    assert!(serde_json::from_str::<AgentName>(r#""Bad Name""#).is_err());
    let dir = TempDir::new().unwrap();
    let (memory, conn) = open(&dir);
    seed(&conn);

    let named_x = principal(SESSION_A, Some(named("x", UX)));
    let main = principal(SESSION_A, Some(main_partition()));
    let none = principal(SESSION_A, None);

    let x_turns = memory
        .list_console_conversation_turns(&named_x, SESSION_A, 50, 0)
        .await
        .unwrap();
    assert_eq!(
        labels(&x_turns),
        ["a-x-1", "a-x-2", "a-x-3"],
        "Named X sees no Y/Main/NULL rows"
    );
    let x_page = memory
        .list_console_conversation_turns(&named_x, SESSION_A, 2, 0)
        .await
        .unwrap();
    assert_eq!(x_page.len(), 2);
    assert!(labels(&x_page).iter().all(|label| label.starts_with("a-x-")));
    let x_rest = memory
        .list_console_conversation_turns(&named_x, SESSION_A, 2, 2)
        .await
        .unwrap();
    assert_eq!(x_rest.len(), 1);
    assert!(labels(&x_rest).iter().all(|label| label.starts_with("a-x-")));
    let mut paged: Vec<&str> = labels(&x_page).into_iter().chain(labels(&x_rest)).collect();
    paged.sort_unstable();
    assert_eq!(paged, ["a-x-1", "a-x-2", "a-x-3"]);

    let main_turns = memory
        .list_console_conversation_turns(&main, SESSION_A, 50, 0)
        .await
        .unwrap();
    let main_labels = labels(&main_turns);
    for expected in ["a-main", "a-null", "a-legacy-main", "a-other-main"] {
        assert!(
            main_labels.contains(&expected),
            "{expected} missing from {main_labels:?}"
        );
    }
    for forbidden in ["a-x-1", "a-y", "b-null", "b-main"] {
        assert!(
            !main_labels.contains(&forbidden),
            "{forbidden} leaked into Main {main_labels:?}"
        );
    }

    let all_turns = memory
        .list_console_conversation_turns(&none, SESSION_A, 50, 0)
        .await
        .unwrap();
    let all_labels = labels(&all_turns);
    assert!(all_labels.contains(&"a-x-1") && all_labels.contains(&"a-y") && all_labels.contains(&"a-null"));
    assert!(
        !all_labels.iter().any(|label| label.starts_with("b-")),
        "{all_labels:?}"
    );

    let x_events = memory.list_message_events_since(&named_x, 0, 50).await.unwrap();
    assert_eq!(
        event_ids(&x_events),
        ["a-x-1", "a-x-2", "a-x-3"],
        "Named X sees no Y/Main/NULL rows"
    );
    let x_event_page = memory.list_message_events_since(&named_x, 0, 2).await.unwrap();
    assert_eq!(event_ids(&x_event_page), ["a-x-1", "a-x-2"]);

    let main_events = memory.list_message_events_since(&main, 0, 50).await.unwrap();
    let main_ids = event_ids(&main_events);
    for expected in ["a-main", "a-null", "a-legacy-main", "a-other-main"] {
        assert!(main_ids.contains(&expected), "{expected} missing from {main_ids:?}");
    }
    for forbidden in ["a-x-1", "a-y", "b-null", "b-main"] {
        assert!(
            !main_ids.contains(&forbidden),
            "{forbidden} leaked into Main events {main_ids:?}"
        );
    }

    let all_events = memory.list_message_events_since(&none, 0, 50).await.unwrap();
    let all_ids = event_ids(&all_events);
    assert!(all_ids.contains(&"a-x-1") && all_ids.contains(&"a-null"));
    assert!(!all_ids.iter().any(|id| id.starts_with("b-")), "{all_ids:?}");

    let owner = Some("system:*".to_string());
    let legacy = 1_i64;
    let limit = 50_i64;
    let offset = 0_i64;
    let after = 0_i64;
    let session = SESSION_A.to_owned();
    let none_reader = principal(SESSION_A, None);
    let (none_turn_sql, _, none_turn_legacy) = conversation_turns_statement(&none_reader, SESSION_A, true);
    let mut none_turn_binds: Vec<&dyn rusqlite::types::ToSql> = vec![&session, &limit, &offset, &owner, &legacy];
    for key in &none_turn_legacy {
        none_turn_binds.push(key);
    }
    let none_turn_plan = explain(&conn, &none_turn_sql, &none_turn_binds);
    let none_event = SqliteMemory::message_events_since_statement(&none_reader);
    let agent: Option<String> = Some("system".to_owned());
    let persona: Option<String> = None;
    let sender: Option<String> = Some("s".to_owned());
    let system_allowed = true;
    let mut none_event_binds: Vec<&dyn rusqlite::types::ToSql> =
        vec![&after, &W, &agent, &persona, &session, &sender, &system_allowed, &limit];
    for key in &none_event.legacy_session_keys {
        none_event_binds.push(key);
    }
    if none_event.bind_owner {
        none_event_binds.push(&none_reader.owner_id);
    }
    let none_event_plan = explain(&conn, &none_event.sql, &none_event_binds);
    for (label, partition) in [("named", Some(named("x", UX))), ("main", Some(main_partition()))] {
        let reader = principal(SESSION_A, partition);
        let (turn_sql, turn_values, turn_legacy) = conversation_turns_statement(&reader, SESSION_A, true);
        let mut turn_binds: Vec<&dyn rusqlite::types::ToSql> = vec![&session, &limit, &offset, &owner, &legacy];
        for key in &turn_legacy {
            turn_binds.push(key);
        }
        for value in &turn_values {
            turn_binds.push(value);
        }
        let turn_plan = explain(&conn, &turn_sql, &turn_binds);
        assert!(
            extra_scans(&turn_plan, &none_turn_plan).is_empty(),
            "{label} turns extra SCAN: {turn_plan:?} vs {none_turn_plan:?}"
        );
        assert!(
            turn_plan.iter().all(|line| !line.contains("SCAN agent_scopes")),
            "{label} turns SCAN agent_scopes: {turn_plan:?}"
        );
        let event = SqliteMemory::message_events_since_statement(&reader);
        let mut event_binds: Vec<&dyn rusqlite::types::ToSql> =
            vec![&after, &W, &agent, &persona, &session, &sender, &system_allowed, &limit];
        for key in &event.legacy_session_keys {
            event_binds.push(key);
        }
        if event.bind_owner {
            event_binds.push(&reader.owner_id);
        }
        for value in &event.partition_values {
            event_binds.push(value);
        }
        let event_plan = explain(&conn, &event.sql, &event_binds);
        assert!(
            extra_scans(&event_plan, &none_event_plan).is_empty(),
            "{label} events extra SCAN: {event_plan:?} vs {none_event_plan:?}"
        );
        assert!(
            event_plan.iter().all(|line| !line.contains("SCAN agent_scopes")),
            "{label} events SCAN agent_scopes: {event_plan:?}"
        );
        eprintln!("EXPLAIN {label} turns: {turn_plan:?}");
        eprintln!("EXPLAIN {label} events: {event_plan:?}");
    }
    eprintln!("EXPLAIN none turns: {none_turn_plan:?}");
    eprintln!("EXPLAIN none events: {none_event_plan:?}");
}
