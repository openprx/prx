//! Review r1 findings: H1 cache decline, M2 unique unfenced, M3 parent, M5 W/O.
use super::*;
use crate::config::agent_name::AgentName;
use crate::memory::{
    AgentPartition, AgentScopeRefused, DocumentIngestInput, Memory, MemoryCategory, MemoryPrincipal, MemoryVisibility,
    MemoryWriteContext, PartitionKey,
};
use rusqlite::{Connection, params};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

const W: &str = "prx_workspace";
const O: &str = "prx_owner";
const SESSION: &str = "prx-session";
const REV: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const UX: &str = "00000000-0000-4000-8000-0000000000a1";
const SX: &str = "00000000-0000-4000-8000-0000000000c1";
const SX2: &str = "00000000-0000-4000-8000-0000000000c2";

fn named() -> AgentPartition {
    AgentPartition {
        workspace_id: W.to_owned(),
        owner_id: O.to_owned(),
        key: PartitionKey::Named {
            name: AgentName::parse("x").unwrap(),
            incarnation: UX.parse().unwrap(),
        },
    }
}

fn seed_named(conn: &Connection) {
    conn.execute(
        "INSERT INTO agent_incarnations VALUES (?1, ?2, 'n:x', ?3, 'active', ?4)",
        params![W, O, UX, REV],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, 'n:x', ?5, 1)",
        params![SX, W, O, SESSION, UX],
    )
    .unwrap();
}

fn write_ctx(owner: &str) -> MemoryWriteContext {
    MemoryWriteContext {
        workspace_id: Some(W.to_owned()),
        owner_id: Some(owner.to_owned()),
        session_key: Some(SESSION.to_owned()),
        agent_partition: Some(named()),
        ..MemoryWriteContext::default()
    }
}

#[tokio::test]
async fn named_document_search_declines_unscoped_query_cache() {
    let tmp = TempDir::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = SqliteMemory::with_embedder(
        tmp.path(),
        Arc::new(CountingEmbedding {
            calls: Arc::clone(&calls),
        }),
        1.0,
        0.0,
        100,
        None,
    )
    .unwrap();
    let conn = Connection::open(tmp.path().join("memory/brain.db")).unwrap();
    seed_named(&conn);
    mem.ingest_document(DocumentIngestInput {
        agent_partition: Some(named()),
        document_id: Some("doc-h1".into()),
        workspace_id: W.to_owned(),
        owner_id: Some(O.to_owned()),
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        source_kind: "test".into(),
        source_uri: None,
        title: Some("h1".into()),
        content: "alpha beta source evidence for document retrieval".into(),
        mime_type: Some("text/plain".into()),
        visibility: MemoryVisibility::Workspace,
        metadata_json: None,
        session_key: Some(SESSION.to_owned()),
        agent_id: None,
        persona_id: None,
    })
    .await
    .unwrap();
    let unscoped_before: i64 = conn
        .query_row(
            "SELECT count(*) FROM embedding_cache WHERE length(content_hash) = 32",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let principal = MemoryPrincipal {
        workspace_id: W.to_owned(),
        agent_id: None,
        persona_id: None,
        session_key: Some(SESSION.to_owned()),
        channel: None,
        sender: None,
        owner_id: Some(O.to_owned()),
        require_owner_match: false,
        legacy_session_key: None,
        agent_partition: Some(named()),
    };
    let _ = mem
        .search_document_chunks(&principal, "source evidence", 5)
        .await
        .unwrap();
    let unscoped_after: i64 = conn
        .query_row(
            "SELECT count(*) FROM embedding_cache WHERE length(content_hash) = 32",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        unscoped_after, unscoped_before,
        "Named document search must not persist an unscoped query cache row"
    );
}

#[test]
fn lookup_write_scope_refuses_multiple_unfenced_scopes() {
    let tmp = TempDir::new().unwrap();
    let _mem = SqliteMemory::new(tmp.path()).unwrap();
    let conn = Connection::open(tmp.path().join("memory/brain.db")).unwrap();
    seed_named(&conn);
    conn.execute(
        "INSERT INTO agent_scopes VALUES (?1, ?2, ?3, ?4, 'n:x', ?5, 2)",
        params![SX2, W, O, SESSION, UX],
    )
    .unwrap();
    let error = super::super::agent_migration::lookup_write_scope(&conn, &named(), Some(SESSION)).unwrap_err();
    assert!(
        error.downcast_ref::<AgentScopeRefused>().is_some(),
        "non-unique unfenced: {error:#}"
    );
}

#[tokio::test]
async fn append_conversation_turn_inherits_parent_stamp_inside_the_writer_tx() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    let conn = Connection::open(tmp.path().join("memory/brain.db")).unwrap();
    seed_named(&conn);
    conn.execute(
        "INSERT INTO message_events (event_id, workspace_id, owner_id, source, session_key, role,
                                     event_type, content, visibility, created_at, updated_at, ma_scope_id)
         VALUES ('evt-x', ?1, ?2, 'console', ?3, 'user', 'message.created', 'hello', 'session', 't', 't', ?4)",
        params![W, O, SESSION, SX],
    )
    .unwrap();
    mem.append_conversation_turn(
        SESSION,
        "console",
        "sender",
        "user",
        "hello",
        None,
        Some("evt-x"),
        Some(O),
    )
    .await
    .unwrap();
    let stamp: Option<String> = conn
        .query_row(
            "SELECT ma_scope_id FROM conversation_turns WHERE message_id = 'evt-x'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stamp.as_deref(),
        Some(SX),
        "W4 inherits the parent stamp inside BEGIN IMMEDIATE"
    );
}

#[tokio::test]
async fn store_refuses_workspace_owner_mismatch_before_embed() {
    let tmp = TempDir::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = SqliteMemory::with_embedder(
        tmp.path(),
        Arc::new(CountingEmbedding {
            calls: Arc::clone(&calls),
        }),
        0.7,
        0.3,
        100,
        None,
    )
    .unwrap();
    let conn = Connection::open(tmp.path().join("memory/brain.db")).unwrap();
    seed_named(&conn);
    let error = mem
        .store_with_context(
            "core-key",
            "durable fact for embedding",
            MemoryCategory::Core,
            None,
            Some(&write_ctx("other-owner")),
        )
        .await
        .expect_err("test: W/O mismatch");
    assert!(
        error.downcast_ref::<AgentScopeRefused>().is_some(),
        "W/O mismatch: {error:#}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "embedder must not run before W/O match"
    );
}

#[tokio::test]
async fn ingest_refuses_workspace_owner_mismatch_before_embed() {
    let tmp = TempDir::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = SqliteMemory::with_embedder(
        tmp.path(),
        Arc::new(CountingEmbedding {
            calls: Arc::clone(&calls),
        }),
        0.7,
        0.3,
        100,
        None,
    )
    .unwrap();
    let conn = Connection::open(tmp.path().join("memory/brain.db")).unwrap();
    seed_named(&conn);
    let error = mem
        .ingest_document(DocumentIngestInput {
            agent_partition: Some(named()),
            document_id: Some("doc-m5".into()),
            workspace_id: W.to_owned(),
            owner_id: Some("other-owner".into()),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "test".into(),
            source_uri: None,
            title: Some("m5".into()),
            content: "mismatch body that would otherwise embed".into(),
            mime_type: Some("text/plain".into()),
            visibility: MemoryVisibility::Workspace,
            metadata_json: None,
            session_key: Some(SESSION.to_owned()),
            agent_id: None,
            persona_id: None,
        })
        .await
        .expect_err("test: ingest W/O mismatch");
    assert!(
        error.downcast_ref::<AgentScopeRefused>().is_some(),
        "ingest W/O mismatch: {error:#}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "ingest embedder must not run before W/O match"
    );
}
