#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]
use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

mod message_events;
mod mirror_and_guards;
mod owner_isolation;
mod pool_and_vector_cap;
mod recall;
mod schema_and_documents;
mod visibility_events;

fn temp_sqlite() -> (TempDir, SqliteMemory) {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    (tmp, mem)
}

fn test_conversation_principal(session_key: &str, owner_id: Option<&str>) -> MemoryPrincipal {
    MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "/tmp/test-workspace".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some(session_key.to_string()),
        channel: Some("test".to_string()),
        sender: Some("tester".to_string()),
        owner_id: owner_id.map(str::to_string),
        legacy_session_key: None,
    }
}

fn message_input(
    workspace_id: &str,
    content: &str,
    visibility: MemoryVisibility,
    agent_id: Option<&str>,
    session_key: Option<&str>,
    sender: Option<&str>,
) -> MessageEventInput {
    MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: workspace_id.to_string(),
        owner_id: None,
        source: "test".into(),
        channel: Some("terminal".to_string()),
        session_key: session_key.map(str::to_string),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: agent_id.map(str::to_string),
        persona_id: None,
        sender: sender.map(str::to_string),
        recipient: None,
        role: "user".to_string(),
        event_type: "message.created".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: Some(0),
        config_source_revision: None,
        content: content.to_string(),
        raw_payload_json: None,
        visibility,
    }
}

fn memory_event_input(
    workspace_id: &str,
    event_type: &str,
    visibility: MemoryVisibility,
    agent_id: Option<&str>,
    session_key: Option<&str>,
) -> MemoryEventInput {
    MemoryEventInput {
        owner_id: None,
        event_id: None,
        workspace_id: workspace_id.to_string(),
        event_type: event_type.to_string(),
        subject_table: "message_events".to_string(),
        subject_id: format!("{event_type}:subject"),
        session_key: session_key.map(str::to_string),
        run_id: None,
        parent_run_id: None,
        agent_id: agent_id.map(str::to_string),
        persona_id: None,
        visibility,
        payload_json: None,
    }
}

struct CountingEmbedding {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbeddingProvider for CountingEmbedding {
    fn name(&self) -> &str {
        "counting"
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn model(&self) -> &str {
        "counting-v1"
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }
}

/// An owner-isolated MCP principal, shaped the way the gateway envelope
/// shapes one: the channel and sender are the caller-chosen external
/// subject, so only the owner id separates two issuers.
fn isolated_document_principal(workspace_id: &str, owner_id: Option<&str>) -> MemoryPrincipal {
    MemoryPrincipal {
        workspace_id: workspace_id.to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("mcp:shared-subject".to_string()),
        channel: Some("mcp".to_string()),
        sender: Some("shared-subject".to_string()),
        owner_id: owner_id.map(ToString::to_string),
        require_owner_match: true,
        legacy_session_key: None,
    }
}

/// The same principal without the isolation bit: every non-MCP origin.
fn shared_document_principal(workspace_id: &str) -> MemoryPrincipal {
    MemoryPrincipal {
        require_owner_match: false,
        ..isolated_document_principal(workspace_id, None)
    }
}

/// Seed the four cases the chunk-visibility predicate distinguishes.
async fn seed_document_visibility_fixture(mem: &SqliteMemory, workspace_id: &str) {
    for (document_id, owner_id, content, visibility) in [
        (
            "document-first-owner",
            Some("owner-first"),
            "kestrel migration notes recorded for the first owner",
            MemoryVisibility::Workspace,
        ),
        (
            "document-second-owner",
            Some("owner-second"),
            "kestrel migration notes recorded for the second owner",
            MemoryVisibility::Workspace,
        ),
        (
            "document-unowned",
            None,
            "kestrel migration notes recorded with no owner at all",
            MemoryVisibility::Workspace,
        ),
        (
            "document-global",
            Some("owner-first"),
            "kestrel migration notes published at global visibility",
            MemoryVisibility::Global,
        ),
    ] {
        mem.ingest_document(DocumentIngestInput {
            document_id: Some(document_id.to_string()),
            workspace_id: workspace_id.to_string(),
            owner_id: owner_id.map(ToString::to_string),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "conversation_page".to_string(),
            source_uri: None,
            title: None,
            content: content.to_string(),
            mime_type: Some("text/plain".to_string()),
            visibility,
            metadata_json: None,
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .unwrap();
    }
}

// ── C03 / V02-R5: owner isolation over the `memories` table ──────────
//
// Two MCP identities presenting the same external subject under different
// issuers resolve to the same channel/chat/sender triple. Only `owner_id`
// separates them, so these contexts differ in exactly that one field.

const ISOLATED_OWNER_A: &str = "owner:ws-isolation:agent:issuer-a.example:shared-subject";
const ISOLATED_OWNER_B: &str = "owner:ws-isolation:agent:issuer-b.example:shared-subject";

fn isolated_mcp_context(owner_id: Option<&str>) -> MemoryWriteContext {
    MemoryWriteContext {
        workspace_id: Some("ws-isolation".into()),
        channel: Some("mcp".into()),
        chat_type: Some("gateway".into()),
        chat_id: Some("mcp:shared-subject".into()),
        sender_id: owner_id.map(str::to_string),
        raw_sender: Some("shared-subject".into()),
        owner_id: owner_id.map(str::to_string),
        require_owner_match: true,
    }
}

/// Seed one row attributed to `owner_id`, then confirm the stored
/// attribution, so a later "B cannot see it" assertion cannot pass because
/// the row was never written the way an MCP write writes it.
async fn seed_isolated_row(mem: &SqliteMemory, owner_id: &str, key: &str, content: &str) {
    let context = isolated_mcp_context(Some(owner_id));
    mem.store_with_context(key, content, MemoryCategory::Conversation, None, Some(&context))
        .await
        .expect("test: store an owner-attributed row");
    let stored: Option<String> = mem
        .pool
        .write()
        .query_row("SELECT owner_id FROM memories WHERE key = ?1", params![key], |row| {
            row.get(0)
        })
        .expect("test: read back the stored owner");
    assert_eq!(stored.as_deref(), Some(owner_id), "test fixture must attribute the row");
}

// ── R-B1d D-1: the delete half of the owner boundary ─────────
//
// `forget_with_context` resolves its principal through
// `sqlite_read_principal`, so the shared visibility predicate already
// owner-qualifies every non-public arm for an isolated caller. The public
// arm does not, by design, which is right for a read and wrong for a
// delete — hence the top-level guard these tests pin.

/// Make the row at `key` publicly visible, which is the one visibility the
/// shared predicate admits to an isolated caller without an owner test.
fn make_public(mem: &SqliteMemory, key: &str) {
    let affected = mem
        .pool
        .write()
        .execute("UPDATE memories SET visibility = 'public' WHERE key = ?1", params![key])
        .expect("test: mark the row public");
    assert_eq!(affected, 1, "test: exactly one row must be made public");
}

// FIX-P0-25 + FIX-#59 regression tests.

fn g1_draft_input(owner: Option<&str>, worker_run_id: &str, draft_id: &str, key: &str) -> MemoryDraftInput {
    MemoryDraftInput {
        draft_id: Some(draft_id.to_string()),
        workspace_id: "ws".to_string(),
        owner_id: owner.map(str::to_string),
        worker_run_id: worker_run_id.to_string(),
        parent_run_id: None,
        session_key: None,
        agent_id: None,
        persona_id: None,
        key: key.to_string(),
        content: "content".to_string(),
        category: MemoryCategory::Core,
        source_event_id: None,
        visibility: MemoryVisibility::Workspace,
        payload_json: None,
    }
}

fn g1_owner_principal(owner: &str) -> MemoryPrincipal {
    MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "ws".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: Some(owner.to_string()),
        legacy_session_key: None,
    }
}

/// Helper: read the column names of a table via PRAGMA table_info.
fn legacy_table_columns(conn: &Connection, table: &str) -> std::collections::HashSet<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info");
    let mut names = std::collections::HashSet::new();
    for c in cols {
        names.insert(c.expect("col name"));
    }
    names
}

fn legacy_index_exists(conn: &Connection, index: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [index],
        |_| Ok(()),
    )
    .optional()
    .expect("query index")
    .is_some()
}

// ---------------------------------------------------------------------
// C03 / V02-R4: owner-qualified identity isolation.
//
// An externally authenticated MCP caller's `session_key` (`mcp:{subject}`)
// and `sender` are the external subject only, so two issuers that accept
// the same subject would share every `session`- and `private`-visibility
// row. These tests replay the audit counterexample against the production
// `SqliteMemory`.
// ---------------------------------------------------------------------

const C03_WORKSPACE: &str = "ws-c03";
const C03_SESSION_KEY: &str = "mcp:shared-subject";

fn c03_owner(issuer: &str) -> String {
    format!("owner:{C03_WORKSPACE}:agent:{issuer}:shared-subject")
}

/// One row written exactly as the MCP envelope writes it: keyed on the
/// subject, attributed to the issuer-qualified owner.
fn c03_message_input(
    workspace_id: &str,
    owner_id: Option<&str>,
    session_key: &str,
    visibility: MemoryVisibility,
    content: &str,
) -> MessageEventInput {
    MessageEventInput {
        event_id: None,
        idempotency_key: None,
        workspace_id: workspace_id.to_string(),
        owner_id: owner_id.map(str::to_string),
        source: "gateway".into(),
        channel: Some("mcp".to_string()),
        session_key: Some(session_key.to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: Some("shared-subject".to_string()),
        recipient: Some("shared-subject".to_string()),
        role: "assistant".to_string(),
        event_type: "message.assistant".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: None,
        config_source_revision: None,
        content: content.to_string(),
        raw_payload_json: None,
        visibility,
    }
}

fn c03_principal(workspace_id: &str, owner_id: Option<&str>, isolated: bool) -> MemoryPrincipal {
    MemoryPrincipal {
        workspace_id: workspace_id.to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some(C03_SESSION_KEY.to_string()),
        channel: Some("mcp".to_string()),
        sender: Some("shared-subject".to_string()),
        owner_id: owner_id.map(str::to_string),
        require_owner_match: isolated,
        legacy_session_key: None,
    }
}

async fn c03_contents(mem: &SqliteMemory, principal: &MemoryPrincipal) -> Vec<String> {
    mem.list_message_events_recent(principal, 50)
        .await
        .expect("test: recent message events")
        .into_iter()
        .map(|event| event.content)
        .collect()
}

// ---------------------------------------------------------------------
// V02-R6: `document.ingested` events carry the ingesting turn's scoping
// keys, so they are readable through the same predicate arms as every
// other memory event instead of holding unmatchable NULLs.
// ---------------------------------------------------------------------

/// One ingest written exactly as a scoped turn writes it.
fn c03_ingest_input(
    document_id: &str,
    owner_id: Option<&str>,
    session_key: Option<&str>,
    visibility: MemoryVisibility,
) -> DocumentIngestInput {
    DocumentIngestInput {
        document_id: Some(document_id.to_string()),
        workspace_id: C03_WORKSPACE.to_string(),
        owner_id: owner_id.map(str::to_string),
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        source_kind: "test".to_string(),
        source_uri: None,
        title: None,
        content: format!("document body for {document_id}"),
        mime_type: Some("text/plain".to_string()),
        visibility,
        metadata_json: None,
        session_key: session_key.map(str::to_string),
        agent_id: None,
        persona_id: None,
    }
}

async fn c03_ingest_subjects(mem: &SqliteMemory, principal: &MemoryPrincipal) -> Vec<String> {
    mem.list_memory_events_recent(principal, 200)
        .await
        .expect("test: recent memory events")
        .into_iter()
        .filter(|event| event.event_type == "document.ingested")
        .map(|event| event.subject_id)
        .collect()
}

// ── Connection pool: WAL, real concurrency, saturation, isolation ─────

fn temp_sqlite_with_read_pool(read_pool_size: usize) -> (TempDir, SqliteMemory) {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::with_embedder_with_acl(
        tmp.path(),
        Arc::new(crate::memory::embeddings::NoopEmbedding),
        0.7,
        0.3,
        10_000,
        None,
        false,
        Some(read_pool_size),
    )
    .unwrap();
    (tmp, mem)
}

// ── Vector recall candidate window ───────────────────────────

fn local_embedding_memory(tmp: &TempDir) -> SqliteMemory {
    SqliteMemory::with_embedder(
        tmp.path(),
        Arc::new(crate::memory::embeddings::LocalHashEmbedding::new(
            "prx-local-hash-v1",
            384,
        )),
        0.7,
        0.3,
        100,
        None,
    )
    .expect("test: open sqlite memory")
}

fn local_query_embedding(text: &str) -> Vec<f32> {
    use crate::memory::embeddings::EmbeddingProvider as _;
    let embedder = crate::memory::embeddings::LocalHashEmbedding::new("prx-local-hash-v1", 384);
    futures::executor::block_on(embedder.embed(&[text]))
        .expect("test: embed query")
        .into_iter()
        .next()
        .expect("test: one embedding")
}

fn vector_candidates(mem: &SqliteMemory, query: &str, limit: usize, cap: usize) -> Vec<String> {
    let embedding = local_query_embedding(query);
    let conn = mem.pool.write();
    SqliteMemory::vector_search(
        &conn,
        &embedding,
        "local",
        "prx-local-hash-v1",
        384,
        limit,
        None,
        None,
        cap,
    )
    .expect("test: vector search")
    .into_iter()
    .map(|(id, _)| id)
    .collect()
}

fn memory_key_for_id(mem: &SqliteMemory, id: &str) -> String {
    let conn = mem.pool.write();
    conn.query_row("SELECT key FROM memories WHERE id = ?1", params![id], |row| {
        row.get::<_, String>(0)
    })
    .expect("test: look up memory key")
}

// ── V02-R9: the owner-isolated `memory_store` upsert ────────────────

/// The production text of the SQLite backend: the root module plus every other
/// source file directly under `src/memory/sqlite/`, test modules excluded.
fn r9_production_source() -> String {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory/sqlite");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect();
    files.sort();
    assert!(files.len() > 1, "test: the sqlite backend source files were not found");
    files
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── OWNER-VIS: event reads (OV-C workspace arm, OV-E private arm, OV-D feed) ──

fn ov_memory_event(
    owner_id: Option<&str>,
    visibility: MemoryVisibility,
    agent_id: Option<&str>,
    marker: &str,
) -> MemoryEventInput {
    MemoryEventInput {
        owner_id: owner_id.map(str::to_string),
        event_id: None,
        workspace_id: C03_WORKSPACE.to_string(),
        event_type: "task.spawned".to_string(),
        subject_table: "tasks".to_string(),
        subject_id: marker.to_string(),
        session_key: Some(C03_SESSION_KEY.to_string()),
        run_id: Some(marker.to_string()),
        parent_run_id: None,
        agent_id: agent_id.map(str::to_string),
        persona_id: None,
        visibility,
        payload_json: Some(serde_json::json!({ "task": marker }).to_string()),
    }
}

/// Every event read an owner-isolated principal can reach, as the contents
/// (message events) and subject ids (memory events) it returned.
async fn ov_every_event_read(mem: &SqliteMemory, principal: &MemoryPrincipal) -> Vec<(&'static str, Vec<String>)> {
    let contents = |events: Vec<MessageEvent>| events.into_iter().map(|event| event.content).collect::<Vec<_>>();
    let subjects = |events: Vec<MemoryEvent>| events.into_iter().map(|event| event.subject_id).collect::<Vec<_>>();
    vec![
        (
            "message since",
            contents(
                mem.list_message_events_since(principal, 0, 50)
                    .await
                    .expect("test: since"),
            ),
        ),
        (
            "message recent",
            contents(
                mem.list_message_events_recent(principal, 50)
                    .await
                    .expect("test: recent"),
            ),
        ),
        (
            "message time range",
            contents(
                mem.list_message_events_time_range(principal, None, "0000-01-01", "9999-12-31", 50)
                    .await
                    .expect("test: time range"),
            ),
        ),
        (
            "shared context",
            contents(
                mem.load_recent_shared_context(SharedContextQuery {
                    principal: principal.clone(),
                    since_event_id: None,
                    limit: 50,
                    include_roles: Vec::new(),
                })
                .await
                .expect("test: shared context"),
            ),
        ),
        (
            "session context",
            contents(
                mem.load_recent_session_context(SessionContextQuery {
                    principal: principal.clone(),
                    since_event_id: None,
                    limit: 50,
                    include_roles: Vec::new(),
                })
                .await
                .expect("test: session context"),
            ),
        ),
        (
            "memory since",
            subjects(
                mem.list_memory_events_since(principal, 0, 50)
                    .await
                    .expect("test: memory since"),
            ),
        ),
        (
            "memory recent",
            subjects(
                mem.list_memory_events_recent(principal, 50)
                    .await
                    .expect("test: memory recent"),
            ),
        ),
    ]
}

/// The key of every generated entry line in a `MEMORY.md` projection, with
/// the text that follows it on that line.
fn mirror_entries(mirror: &str) -> Vec<(String, String)> {
    mirror
        .lines()
        .filter_map(|line| {
            let key = SqliteMemory::generated_backup_entry_key(line)?;
            let (_, rest) = line.split_once("**:")?;
            Some((key.to_string(), rest.trim().to_string()))
        })
        .collect()
}

// ── D-026 / R-MCPBfg N-1: the MEMORY_SNAPSHOT.md export ───────────────

fn n1_context(channel: &str, sender: &str, chat_id: &str, owner: Option<&str>, isolated: bool) -> MemoryWriteContext {
    MemoryWriteContext {
        channel: Some(channel.to_string()),
        raw_sender: Some(sender.to_string()),
        chat_id: Some(chat_id.to_string()),
        chat_type: Some("private".to_string()),
        sender_id: owner.map(str::to_string),
        owner_id: owner.map(str::to_string),
        require_owner_match: isolated,
        ..MemoryWriteContext::default()
    }
}

fn n1_stored(db_path: &std::path::Path, key: &str) -> Option<(String, Option<String>)> {
    Connection::open(db_path)
        .unwrap()
        .query_row(
            "SELECT content, owner_id FROM memories WHERE key = ?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .unwrap()
}

fn snapshot_owner_of(workspace: &std::path::Path, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(workspace.join("memory").join("brain.db")).unwrap();
    conn.query_row(
        "SELECT owner_id FROM memories WHERE key = ?1",
        rusqlite::params![key],
        |row| row.get(0),
    )
    .unwrap()
}
