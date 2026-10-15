#![cfg(test)]
use super::*;

#[test]
fn core_markdown_backup_upserts_and_deduplicates_generated_keys() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    SqliteMemory::append_backup_entry(&db_path, "fitness/day-1", "old", &MemoryCategory::Core);
    SqliteMemory::append_backup_entry(&db_path, "preference", "keep", &MemoryCategory::Core);
    SqliteMemory::append_backup_entry(&db_path, "fitness/day-1", "new", &MemoryCategory::Core);

    let backup = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
    assert_eq!(backup.matches("**fitness/day-1**:").count(), 1);
    assert_eq!(backup.matches("**preference**:").count(), 1);
    assert!(backup.contains("**fitness/day-1**: new"));
    assert!(!backup.contains("**fitness/day-1**: old"));
}

#[tokio::test]
async fn chat_profile_metadata_upsert_preserves_self_maintained_fields() {
    let (_tmp, mem) = temp_sqlite();
    mem.upsert_chat_profile_metadata("telegram", "group-a", "group", Some("Initial"))
        .await
        .unwrap();
    let profile = mem
        .update_chat_profile(
            "telegram",
            "group-a",
            "group",
            Some("Release coordination"),
            Some("Keep deploy notes here"),
            Some(&["release".to_string()]),
            "agent",
        )
        .await
        .unwrap();
    assert_eq!(profile.updated_by, "agent");

    mem.upsert_chat_profile_metadata("telegram", "group-a", "thread", Some("Renamed"))
        .await
        .unwrap();
    let profile = mem.get_chat_profile("telegram", "group-a").await.unwrap().unwrap();
    assert_eq!(profile.chat_kind, "thread");
    assert_eq!(profile.title.as_deref(), Some("Renamed"));
    assert_eq!(profile.purpose.as_deref(), Some("Release coordination"));
    assert_eq!(profile.notes.as_deref(), Some("Keep deploy notes here"));
    assert_eq!(profile.tags, vec!["release"]);
    assert_eq!(profile.updated_by, "agent");

    mem.upsert_chat_profile_metadata("telegram", "group-a", "group", None)
        .await
        .unwrap();
    let profile = mem.get_chat_profile("telegram", "group-a").await.unwrap().unwrap();
    assert_eq!(profile.title.as_deref(), Some("Renamed"));
}

#[tokio::test]
async fn sqlite_new_with_custom_path_creates_db() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("workers").join("w1").join("brain.db");

    let mem = SqliteMemory::new_with_path(db_path.clone()).unwrap();
    mem.store("k1", "v1", MemoryCategory::Core, None).await.unwrap();

    assert!(db_path.exists());
    assert_eq!(mem.name(), "sqlite");
}

#[tokio::test]
async fn sqlite_local_embeddings_persist_and_drive_vector_recall() {
    let tmp = TempDir::new().unwrap();
    let embedder = Arc::new(crate::memory::embeddings::LocalHashEmbedding::new(
        "prx-local-hash-v1",
        384,
    ));
    let mem = SqliteMemory::with_embedder(tmp.path(), embedder, 0.7, 0.3, 100, None).unwrap();

    mem.store(
        "release_failure",
        "release deployment failed",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store("garden", "garden flowers", MemoryCategory::Core, None)
        .await
        .unwrap();

    let results = mem.recall("deployment failure during release", 2, None).await.unwrap();
    assert_eq!(results.first().map(|entry| entry.key.as_str()), Some("release_failure"));

    let conn = mem.pool.write();
    let embedded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL AND embedding_provider = 'local'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(embedded, 2);
}

#[tokio::test]
async fn sqlite_acl_enabled_skips_markdown_backup() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("memory").join("brain.db");
    let mem = SqliteMemory::new_with_path_and_acl(db_path, true).unwrap();

    mem.store("k1", "sensitive", MemoryCategory::Core, None).await.unwrap();

    assert!(mem.get("k1").await.unwrap().is_some());
    assert!(!tmp.path().join("MEMORY.md").exists());
}

#[tokio::test]
async fn sqlite_name() {
    let (_tmp, mem) = temp_sqlite();
    assert_eq!(mem.name(), "sqlite");
}

#[tokio::test]
async fn sqlite_health() {
    let (_tmp, mem) = temp_sqlite();
    assert!(mem.health_check().await);
}

#[tokio::test]
async fn message_events_schema_is_created() {
    let (_tmp, mem) = temp_sqlite();
    let conn = mem.pool.write();

    let message_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'message_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let memory_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'memory_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let memory_drafts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'memory_drafts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let documents: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'documents'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let document_chunks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'document_chunks'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let memory_links: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'memory_links'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let retrieval_traces: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'retrieval_traces'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let compaction_runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'compaction_runs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let approval_grants: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'approval_grants'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let approval_grant_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'approval_grant_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(message_events, 1);
    assert_eq!(memory_events, 1);
    assert_eq!(memory_drafts, 1);
    assert_eq!(documents, 1);
    assert_eq!(document_chunks, 1);
    assert_eq!(memory_links, 1);
    assert_eq!(retrieval_traces, 1);
    assert_eq!(compaction_runs, 1);
    assert_eq!(approval_grants, 1);
    assert_eq!(approval_grant_events, 1);

    for (table, column) in [
        ("memories", "workspace_id"),
        ("memories", "owner_id"),
        ("memories", "agent_id"),
        ("memories", "persona_id"),
        ("memories", "source_event_id"),
        ("memories", "source"),
        ("memories", "embedding_provider"),
        ("memories", "embedding_model"),
        ("memories", "embedding_dimensions"),
        ("conversation_turns", "message_event_id"),
        ("sessions", "owner_id"),
        ("conversation_turns", "owner_id"),
        ("conversation_turns", "agent_id"),
        ("conversation_turns", "persona_id"),
        ("conversation_turns", "visibility"),
        ("message_events", "owner_id"),
        ("message_events", "event_type"),
        ("message_events", "source_ref_json"),
        ("message_events", "subject_ref_json"),
        ("message_events", "goal_id"),
        ("message_events", "causation_event_id"),
        ("message_events", "correlation_id"),
        ("message_events", "attempt_id"),
        ("message_events", "lease_epoch"),
        ("memory_drafts", "owner_id"),
        ("memory_drafts", "parent_run_id"),
        ("memory_drafts", "source_event_id"),
        ("memory_drafts", "visibility"),
        ("memory_drafts", "payload_json"),
        ("documents", "owner_id"),
        ("documents", "topic_id"),
        ("documents", "task_id"),
        ("documents", "source_message_event_id"),
        ("document_chunks", "owner_id"),
        ("document_chunks", "topic_id"),
        ("document_chunks", "task_id"),
        ("document_chunks", "embedding"),
        ("document_chunks", "embedding_provider"),
        ("document_chunks", "embedding_model"),
        ("document_chunks", "embedding_dimensions"),
        ("memory_links", "owner_id"),
        ("memory_links", "document_id"),
        ("memory_links", "chunk_id"),
        ("retrieval_traces", "owner_id"),
        ("retrieval_traces", "selected_json"),
        ("retrieval_traces", "dropped_json"),
        ("compaction_runs", "owner_id"),
        ("compaction_runs", "summary_memory_key"),
        ("compaction_runs", "source_event_range_json"),
        ("compaction_runs", "fidelity_status"),
        ("embedding_cache", "provider"),
        ("embedding_cache", "model"),
        ("embedding_cache", "dimensions"),
        ("approval_grants", "owner_id"),
        ("approval_grants", "principal_id"),
        ("approval_grants", "capability_op_id"),
        ("approval_grants", "grant_json"),
        ("approval_grants", "revoked_at"),
        ("approval_grant_events", "grant_id"),
        ("approval_grant_events", "event_type"),
        ("approval_grant_events", "payload_json"),
    ] {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
                [column],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing {table}.{column}");
    }
}

#[tokio::test]
async fn legacy_message_events_schema_adds_typed_lineage_columns() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("brain.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL UNIQUE,
                idempotency_key TEXT UNIQUE,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                source TEXT NOT NULL,
                channel TEXT,
                session_key TEXT,
                parent_session_key TEXT,
                run_id TEXT,
                parent_run_id TEXT,
                agent_id TEXT,
                persona_id TEXT,
                sender TEXT,
                recipient TEXT,
                role TEXT NOT NULL,
                event_type TEXT,
                content TEXT NOT NULL,
                content_hash TEXT,
                raw_payload_json TEXT,
                visibility TEXT NOT NULL DEFAULT 'workspace',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );
             INSERT INTO message_events (
                event_id, idempotency_key, workspace_id, source, role, content,
                visibility, created_at, updated_at
             ) VALUES (
                'legacy-event-1', 'shared-legacy-key', 'workspace-a',
                'legacy-adapter', 'user', 'legacy content',
                'workspace', '2026-07-01T00:00:00Z', '2026-07-01T00:00:00Z'
             );",
        )
        .unwrap();
    }

    let mem = SqliteMemory::new_with_path(db_path).unwrap();
    let events = mem
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace-a".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    if let Some(event) = events.first() {
        assert_eq!(event.source, "legacy-adapter");
        assert_eq!(event.event_type, "message.legacy");
        assert!(event.subject.is_none());
        assert!(event.correlation_id.is_none());
    }

    let mut second_workspace = message_input(
        "workspace-b",
        "same external key in another workspace",
        MemoryVisibility::Workspace,
        None,
        Some("chat:b"),
        Some("bob"),
    );
    second_workspace.idempotency_key = Some("shared-legacy-key".to_string());
    let second_workspace_event = mem.append_message_event(second_workspace).await.unwrap();
    assert_eq!(second_workspace_event.workspace_id, "workspace-b");
    assert_ne!(second_workspace_event.event_id, "legacy-event-1");

    let conn = mem.pool.write();
    for column in [
        "source_ref_json",
        "subject_ref_json",
        "goal_id",
        "causation_event_id",
        "correlation_id",
        "attempt_id",
        "lease_epoch",
        "config_generation_id",
        "config_source_revision",
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('message_events') WHERE name = ?1",
                [column],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing upgraded message_events.{column}");
    }
    let generation_index_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_index_list('message_events')
             WHERE name = 'idx_message_events_config_generation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(generation_index_count, 1, "missing generation lookup index");
}

#[tokio::test]
async fn legacy_compaction_runs_schema_adds_source_event_range() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("brain.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE compaction_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL UNIQUE,
                workspace_id TEXT NOT NULL,
                owner_id TEXT,
                session_key TEXT,
                agent_id TEXT,
                persona_id TEXT,
                trigger TEXT NOT NULL,
                mode TEXT NOT NULL,
                source_message_count INTEGER NOT NULL,
                source_token_estimate INTEGER NOT NULL,
                summary TEXT NOT NULL,
                summary_memory_key TEXT,
                source_event_ids_json TEXT,
                source_document_refs_json TEXT,
                fidelity_status TEXT NOT NULL,
                payload_json TEXT,
                created_at TEXT NOT NULL
             );",
        )
        .unwrap();
    }

    let mem = SqliteMemory::new_with_path(db_path).unwrap();
    let conn = mem.pool.write();
    let column_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('compaction_runs') WHERE name = 'source_event_range_json'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(column_count, 1);
    let migration_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_schema_migrations WHERE version = 12 AND name = 'compaction_source_event_range'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(migration_count, 1);
}

#[tokio::test]
async fn legacy_conversation_turns_schema_upgrades_owner_id_and_backfills() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("brain.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                session_key          TEXT PRIMARY KEY,
                channel              TEXT NOT NULL,
                sender               TEXT NOT NULL,
                created_at           TEXT NOT NULL,
                updated_at           TEXT NOT NULL,
                message_count        INTEGER NOT NULL DEFAULT 0,
                last_message_preview TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE conversation_turns (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key TEXT NOT NULL,
                role        TEXT NOT NULL,
                content     TEXT NOT NULL,
                timestamp   TEXT NOT NULL,
                message_id  TEXT,
                FOREIGN KEY (session_key) REFERENCES sessions(session_key) ON DELETE CASCADE
             );",
        )
        .unwrap();
        for idx in 0..85 {
            let session_key = format!("legacy-session-{}", idx % 5);
            conn.execute(
                "INSERT OR IGNORE INTO sessions (
                    session_key, channel, sender, created_at, updated_at, message_count, last_message_preview
                 ) VALUES (?1, 'signal', 'legacy-user', '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z', 0, '')",
                params![session_key],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO conversation_turns (session_key, role, content, timestamp, message_id)
                 VALUES (?1, 'user', ?2, '2026-05-01T00:00:00Z', ?3)",
                params![session_key, format!("turn-{idx}"), format!("msg-{idx}")],
            )
            .unwrap();
        }
    }

    let mem = SqliteMemory::new_with_path(db_path).unwrap();
    let conn = mem.pool.write();
    for (table, column) in [("sessions", "owner_id"), ("conversation_turns", "owner_id")] {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
                [column],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing upgraded {table}.{column}");
    }
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM conversation_turns", [], |row| row.get(0))
        .unwrap();
    let non_null: i64 = conn
        .query_row(
            "SELECT COUNT(owner_id) FROM conversation_turns WHERE owner_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let null_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_turns WHERE owner_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(total, 85);
    assert_eq!(non_null, 85);
    assert_eq!(null_count, 0);
}

#[tokio::test]
async fn retrieval_trace_persists_context_pack_audit() {
    let (_tmp, mem) = temp_sqlite();

    let trace = mem
        .append_retrieval_trace(RetrievalTraceInput {
            trace_id: Some("trace-sqlite-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            session_key: Some("chat:session".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            source: "agent_context.document_evidence".to_string(),
            query: "stable source anchors".to_string(),
            candidate_count: 2,
            selected_count: 1,
            dropped_count: 1,
            budget_tokens: Some(512),
            selected_json: Some(r#"[{"chunk_id":"doc-1:chunk:0"}]"#.to_string()),
            dropped_json: Some(r#"[{"chunk_id":"doc-1:chunk:1"}]"#.to_string()),
            payload_json: Some(r#"{"phase":"test"}"#.to_string()),
        })
        .await
        .unwrap();

    assert_eq!(trace.trace_id, "trace-sqlite-1");
    assert_eq!(trace.selected_count, 1);
    assert_eq!(trace.dropped_count, 1);
    assert!(trace.selected_json.unwrap().contains("doc-1:chunk:0"));
}

#[tokio::test]
async fn compaction_run_persists_summary_audit() {
    let (_tmp, mem) = temp_sqlite();

    let run = mem
        .append_compaction_run(CompactionRunInput {
            run_id: Some("compact-sqlite-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            session_key: Some("chat:session".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            trigger: "pre_turn".to_string(),
            mode: "safeguard".to_string(),
            source_message_count: 4,
            source_token_estimate: 256,
            summary: "## Decisions\n- keep source anchors".to_string(),
            summary_memory_key: Some("compaction_summary_1".to_string()),
            source_event_ids_json: Some(r#"["event-1"]"#.to_string()),
            source_event_range_json: Some(
                r#"{"first_event_id":"event-1","last_event_id":"event-1","first_row_id":1,"last_row_id":1,"source_event_count":1}"#.to_string(),
            ),
            source_document_refs_json: Some(r#"[{"chunk_id":"doc-1:chunk:0"}]"#.to_string()),
            fidelity_status: "accepted".to_string(),
            payload_json: Some(r#"{"phase":"test"}"#.to_string()),
        })
        .await
        .unwrap();

    assert_eq!(run.run_id, "compact-sqlite-1");
    assert_eq!(run.source_message_count, 4);
    assert_eq!(run.summary_memory_key.as_deref(), Some("compaction_summary_1"));
    assert!(
        run.source_event_range_json
            .as_deref()
            .is_some_and(|json| json.contains("event-1"))
    );
    assert_eq!(run.fidelity_status, "accepted");
}

// FIX-P1-20: a semantic memory promoted through the fabric whose content
// carries a `[document_ingest_ref]` marker must record a back-reference into
// the (previously dead) `memory_links` table.
#[tokio::test]
async fn fabric_semantic_memory_records_memory_link_for_ingest_ref() {
    let (tmp, mem) = temp_sqlite();
    let pool = mem.pool.clone();
    let fabric = crate::memory::fabric::MemoryFabric::new(Arc::new(mem), "workspace-a");

    let content = "Promoted fact.\n\n[document_ingest_ref]\n\
document_id: doc-prov-1\n\
source: tool_output\n\
[/document_ingest_ref]";
    fabric
        .record_semantic_memory("prov-key", content, MemoryCategory::Conversation, Some("chat:session"))
        .await
        .expect("test: record_semantic_memory should succeed");

    let (document_id, link_type, memory_key): (String, String, Option<String>) = pool
        .write()
        .query_row(
            "SELECT document_id, link_type, memory_key FROM memory_links WHERE memory_key = 'prov-key'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("test: a memory_links row should exist");
    assert_eq!(document_id, "doc-prov-1");
    assert_eq!(link_type, "derived_from");
    assert_eq!(memory_key.as_deref(), Some("prov-key"));
    drop(tmp);
}

#[tokio::test]
async fn store_with_metadata_persists_fabric_source_fields() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_metadata(
        "semantic-key",
        "semantic value",
        MemoryCategory::Core,
        Some("chat:session"),
        MemoryStoreMetadata {
            workspace_id: Some("workspace-a".to_string()),
            owner_id: Some("owner-a".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            source_event_id: Some("event-123".to_string()),
            source: Some("semantic_promotion".to_string()),
            topic_id: None,
            channel: None,
        },
    )
    .await
    .unwrap();

    let row: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = mem
        .pool
        .write()
        .query_row(
            "SELECT workspace_id, owner_id, agent_id, persona_id, source_event_id, source
             FROM memories WHERE key = 'semantic-key'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();

    assert_eq!(row.0.as_deref(), Some("workspace-a"));
    assert_eq!(row.1.as_deref(), Some("owner-a"));
    assert_eq!(row.2.as_deref(), Some("agent-a"));
    assert_eq!(row.3.as_deref(), Some("persona-a"));
    assert_eq!(row.4.as_deref(), Some("event-123"));
    assert_eq!(row.5.as_deref(), Some("semantic_promotion"));
}

// FIX-P1-08: the metadata-only store path (used by compaction summaries,
// which carry no MemoryWriteContext) must persist the originating channel so
// anonymous principals can resolve channel scope on a later recall.
#[tokio::test]
async fn store_with_metadata_persists_channel_for_anonymous_recall() {
    let (_tmp, mem) = temp_sqlite();

    mem.store_with_metadata(
        "compaction-key",
        "compaction summary value",
        MemoryCategory::Conversation,
        Some("chat:session"),
        MemoryStoreMetadata {
            workspace_id: Some("workspace-a".to_string()),
            owner_id: Some("owner-a".to_string()),
            agent_id: None,
            persona_id: None,
            source_event_id: None,
            source: Some("compaction_summary".to_string()),
            topic_id: None,
            channel: Some("telegram".to_string()),
        },
    )
    .await
    .expect("test: store_with_metadata should succeed");

    let row_channel: Option<String> = mem
        .pool
        .write()
        .query_row("SELECT channel FROM memories WHERE key = 'compaction-key'", [], |row| {
            row.get(0)
        })
        .expect("test: stored row should exist");

    assert_eq!(
        row_channel.as_deref(),
        Some("telegram"),
        "metadata.channel must be persisted to the channel column"
    );
}

#[tokio::test]
async fn document_ingest_chunks_searches_and_links_sources() {
    let (_tmp, mem) = temp_sqlite();
    let document = mem
        .ingest_document(DocumentIngestInput {
            document_id: Some("doc-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner:workspace-a:telegram:alice".to_string()),
            topic_id: Some("topic-a".to_string()),
            task_id: Some("task-a".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
            source_kind: "tool_output".to_string(),
            source_uri: Some("tool:file_read".to_string()),
            title: Some("Research Notes".to_string()),
            content: "# Notes\n\nDurable document fact about vector retrieval and source anchors.".to_string(),
            mime_type: Some("text/markdown".to_string()),
            visibility: MemoryVisibility::Workspace,
            metadata_json: Some(serde_json::json!({"tool": "file_read"}).to_string()),
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .unwrap();

    assert_eq!(document.document_id, "doc-1");
    assert_eq!(document.owner_id.as_deref(), Some("owner:workspace-a:telegram:alice"));
    assert_eq!(document.topic_id.as_deref(), Some("topic-a"));
    assert_eq!(document.task_id.as_deref(), Some("task-a"));
    assert_eq!(document.chunk_count, 1);
    assert_eq!(document.content_sha256.len(), 64);

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("telegram:chat-1:alice".to_string()),
        channel: Some("telegram".to_string()),
        sender: Some("alice".to_string()),
        owner_id: None,
        legacy_session_key: None,
    };
    let results = mem
        .search_document_chunks(&principal, "vector retrieval", 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].chunk.document_id, "doc-1");
    assert_eq!(
        results[0].chunk.owner_id.as_deref(),
        Some("owner:workspace-a:telegram:alice")
    );
    assert_eq!(results[0].chunk.source_anchor, "doc-1#chunk-0");

    let chunk = mem
        .get_document_chunk(&results[0].chunk.chunk_id)
        .await
        .unwrap()
        .expect("chunk should exist");
    assert!(chunk.content.contains("source anchors"));

    let link = mem
        .link_memory_source(MemoryLinkInput {
            link_id: Some("link-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner:workspace-a:telegram:alice".to_string()),
            memory_key: Some("summary-key".to_string()),
            memory_event_id: None,
            message_event_id: Some("msg-a".to_string()),
            document_id: document.document_id.clone(),
            chunk_id: Some(chunk.chunk_id.clone()),
            link_type: "evidence".to_string(),
            payload_json: None,
        })
        .await
        .unwrap();
    assert_eq!(link.link_id, "link-1");
    assert_eq!(link.chunk_id.as_deref(), Some(chunk.chunk_id.as_str()));
}

#[tokio::test]
async fn document_chunk_vector_search_uses_current_embedding_metadata() {
    let tmp = TempDir::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = SqliteMemory::with_embedder(
        tmp.path(),
        Arc::new(CountingEmbedding {
            calls: Arc::clone(&calls),
        }),
        1.0,
        0.0,
        10,
        None,
    )
    .unwrap();
    mem.ingest_document(DocumentIngestInput {
        document_id: Some("doc-vector-1".to_string()),
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        source_kind: "test".to_string(),
        source_uri: None,
        title: Some("Vector Doc".to_string()),
        content: "alpha beta gamma".to_string(),
        mime_type: Some("text/plain".to_string()),
        visibility: MemoryVisibility::Workspace,
        metadata_json: None,
        session_key: None,
        agent_id: None,
        persona_id: None,
    })
    .await
    .unwrap();

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("chat:vector".to_string()),
        channel: Some("terminal".to_string()),
        sender: Some("local-user".to_string()),
        owner_id: None,
        legacy_session_key: None,
    };
    let vector_results = mem
        .search_document_chunks(&principal, "no keyword overlap", 10)
        .await
        .unwrap();
    assert_eq!(vector_results.len(), 1);
    assert_eq!(vector_results[0].chunk.document_id, "doc-vector-1");

    {
        let conn = mem.pool.write();
        let (provider, model, dimensions): (String, String, i64) = conn
            .query_row(
                "SELECT embedding_provider, embedding_model, embedding_dimensions
                 FROM document_chunks WHERE document_id = 'doc-vector-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(provider, "counting");
        assert_eq!(model, "counting-v1");
        assert_eq!(dimensions, 3);
        conn.execute(
            "UPDATE document_chunks
             SET embedding_provider = 'stale-provider',
                 embedding_model = 'stale-model',
                 embedding_dimensions = 999
             WHERE document_id = 'doc-vector-1'",
            [],
        )
        .unwrap();
    }

    let stale_results = mem
        .search_document_chunks(&principal, "no keyword overlap", 10)
        .await
        .unwrap();
    assert!(stale_results.is_empty());
}

#[tokio::test]
async fn selected_document_read_preserves_owner_workspace_and_deadline_bounds() {
    let (_tmp, mem) = temp_sqlite();
    seed_document_visibility_fixture(&mem, "prx_workspace").await;
    let deadline = || std::time::Instant::now() + std::time::Duration::from_secs(5);
    let owner = isolated_document_principal("prx_workspace", Some("owner-second"));
    let hits = mem
        .retrieve_decision_documents("kestrel", 5, &owner, deadline())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document_id, "document-second-owner");
    assert!(!format!("{:?}", hits[0]).contains("kestrel"));
    for principal in [
        isolated_document_principal("prx_workspace", None),
        isolated_document_principal("prx_workspace", Some("")),
        isolated_document_principal("prx_other_workspace", Some("owner-second")),
    ] {
        assert!(
            mem.retrieve_decision_documents("kestrel", 5, &principal, deadline())
                .await
                .unwrap()
                .is_empty(),
            "expected an empty value"
        );
    }
    let shared = shared_document_principal("prx_workspace");
    assert_eq!(
        mem.retrieve_decision_documents("kestrel", 5, &shared, deadline())
            .await
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        mem.retrieve_decision_documents("kestrel", 1, &shared, deadline())
            .await
            .unwrap()
            .len(),
        1
    );
    for limit in [0, 6] {
        assert!(
            mem.retrieve_decision_documents("kestrel", limit, &owner, deadline())
                .await
                .is_err()
        );
    }
    assert!(
        mem.retrieve_decision_documents(&"x".repeat(1025), 5, &owner, deadline())
            .await
            .is_err()
    );
    assert!(
        mem.retrieve_decision_documents("kestrel", 5, &owner, std::time::Instant::now())
            .await
            .is_err()
    );
}

/// V02-R8: the guard the chunk search appends to its visibility disjunct.
///
/// It suppresses the disjunct for an owner-isolated principal and for
/// nobody else, and it binds no parameter in either case, so the
/// placeholder accounting of the statements it lands in is unchanged.
///
/// MUTATION GUARD: return the empty string unconditionally and the isolated
/// rows go red; return `" AND FALSE"` unconditionally and the first row
/// does.
#[test]
fn document_visibility_guard_suppresses_the_arm_only_for_an_isolated_principal() {
    let shared = SqliteMemory::document_visibility_guard(&shared_document_principal("workspace-a"));
    assert_eq!(
        shared, "",
        "a non-isolated principal must leave the statement byte-identical"
    );

    for owner in [None, Some("owner-second"), Some("   ")] {
        let guard = SqliteMemory::document_visibility_guard(&isolated_document_principal("workspace-a", owner));
        assert_eq!(
            guard, " AND FALSE",
            "an owner-isolated principal is never admitted by visibility alone (owner: {owner:?})"
        );
        assert!(
            !guard.contains('?'),
            "the guard binds no parameter, so no placeholder index moves: {guard}"
        );
    }
}

/// V02-R8, keyword arm: the FTS chunk search admits a chunk to an
/// owner-isolated principal only when the chunk's owner matches.
///
/// This is the statement `document_search` answers from for any query with
/// usable keywords. It used to admit every `workspace` and `global`
/// document to every authenticated caller, which is the disclosure.
///
/// MUTATION GUARD: drop `{visibility_guard}` from the FTS statement and
/// every isolated assertion below goes red.
#[tokio::test]
async fn document_chunk_keyword_search_is_owner_qualified_for_an_isolated_principal() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    seed_document_visibility_fixture(&mem, "workspace-a").await;

    let shared = mem
        .search_document_chunks(&shared_document_principal("workspace-a"), "kestrel", 10)
        .await
        .unwrap();
    assert_eq!(
        shared.len(),
        4,
        "a non-isolated principal keeps the workspace-shared view of all four documents"
    );

    let isolated = mem
        .search_document_chunks(
            &isolated_document_principal("workspace-a", Some("owner-second")),
            "kestrel",
            10,
        )
        .await
        .unwrap();
    let reached = isolated
        .iter()
        .map(|result| result.chunk.document_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        reached,
        vec!["document-second-owner"],
        "an isolated principal reads its own chunk and no other: another owner's workspace \
         document, an unattributed document and another owner's global document are all hidden"
    );

    for blank in [None, Some("   ")] {
        let nothing = mem
            .search_document_chunks(&isolated_document_principal("workspace-a", blank), "kestrel", 10)
            .await
            .unwrap();
        assert!(
            nothing.is_empty(),
            "an isolated principal with no usable owner id reads nothing (owner: {blank:?}): {nothing:?}"
        );
    }
}

/// V02-R8, vector arm: the second chunk search statement carries the same
/// guard.
///
/// The keyword arm cannot answer this query, so the rows can only come from
/// the vector statement — which is why the fixture embedder scores every
/// chunk alike.
///
/// MUTATION GUARD: drop `{visibility_guard}` from the vector statement and
/// the isolated assertion goes red.
#[tokio::test]
async fn document_chunk_vector_search_is_owner_qualified_for_an_isolated_principal() {
    let tmp = TempDir::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = SqliteMemory::with_embedder(
        tmp.path(),
        Arc::new(CountingEmbedding {
            calls: Arc::clone(&calls),
        }),
        1.0,
        0.0,
        10,
        None,
    )
    .unwrap();
    seed_document_visibility_fixture(&mem, "workspace-a").await;

    let shared = mem
        .search_document_chunks(&shared_document_principal("workspace-a"), "no keyword overlap", 10)
        .await
        .unwrap();
    assert_eq!(
        shared.len(),
        4,
        "sanity: the vector arm is the only one that can answer this query, and it answers"
    );

    let isolated = mem
        .search_document_chunks(
            &isolated_document_principal("workspace-a", Some("owner-second")),
            "no keyword overlap",
            10,
        )
        .await
        .unwrap();
    let reached = isolated
        .iter()
        .map(|result| result.chunk.document_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        reached,
        vec!["document-second-owner"],
        "the vector arm is owner-qualified for an isolated principal too"
    );
}

#[tokio::test]
async fn reindex_backfills_stale_document_chunk_embeddings() {
    let tmp = TempDir::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = SqliteMemory::with_embedder(
        tmp.path(),
        Arc::new(CountingEmbedding {
            calls: Arc::clone(&calls),
        }),
        1.0,
        0.0,
        10,
        None,
    )
    .unwrap();
    mem.ingest_document(DocumentIngestInput {
        document_id: Some("doc-reindex-1".to_string()),
        workspace_id: "workspace-a".to_string(),
        owner_id: None,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        source_kind: "test".to_string(),
        source_uri: None,
        title: Some("Reindex Doc".to_string()),
        content: "delta epsilon zeta".to_string(),
        mime_type: Some("text/plain".to_string()),
        visibility: MemoryVisibility::Workspace,
        metadata_json: None,
        session_key: None,
        agent_id: None,
        persona_id: None,
    })
    .await
    .unwrap();
    calls.store(0, Ordering::SeqCst);
    {
        let conn = mem.pool.write();
        conn.execute(
            "UPDATE document_chunks
             SET embedding = NULL,
                 embedding_provider = 'stale-provider',
                 embedding_model = 'stale-model',
                 embedding_dimensions = 999
             WHERE document_id = 'doc-reindex-1'",
            [],
        )
        .unwrap();
    }

    let reindexed = mem.reindex().await.unwrap();
    assert_eq!(reindexed, 1);

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("chat:reindex".to_string()),
        channel: Some("terminal".to_string()),
        sender: Some("local-user".to_string()),
        owner_id: None,
        legacy_session_key: None,
    };
    let vector_results = mem
        .search_document_chunks(&principal, "no keyword overlap", 10)
        .await
        .unwrap();
    assert_eq!(vector_results.len(), 1);
    assert_eq!(vector_results[0].chunk.document_id, "doc-reindex-1");

    let conn = mem.pool.write();
    let (provider, model, dimensions): (String, String, i64) = conn
        .query_row(
            "SELECT embedding_provider, embedding_model, embedding_dimensions
             FROM document_chunks WHERE document_id = 'doc-reindex-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(provider, "counting");
    assert_eq!(model, "counting-v1");
    assert_eq!(dimensions, 3);
}

#[tokio::test]
async fn memory_draft_lifecycle_creates_merges_and_rejects_with_outbox() {
    let (_tmp, mem) = temp_sqlite();

    let draft = mem
        .create_memory_draft(MemoryDraftInput {
            draft_id: Some("draft-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            worker_run_id: "run-worker".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            session_key: Some("session-a".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            key: "draft-key".to_string(),
            content: "draft memory".to_string(),
            category: MemoryCategory::Conversation,
            source_event_id: Some("event-worker-result".to_string()),
            visibility: MemoryVisibility::Workspace,
            payload_json: Some("{\"kind\":\"worker_result\"}".to_string()),
        })
        .await
        .unwrap();
    assert_eq!(draft.status, "pending");
    assert_eq!(draft.owner_id.as_deref(), Some("owner-a"));

    let test_principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace".to_string(),
        agent_id: Some("system".to_string()),
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: None,
        legacy_session_key: None,
    };
    let drafts = mem
        .list_memory_drafts_for_run(&test_principal, "run-worker")
        .await
        .unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].draft_id, "draft-1");

    let merged = mem
        .merge_memory_draft(&test_principal, "draft-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.status, "merged");
    let memory = mem.get("draft-key").await.unwrap().unwrap();
    assert_eq!(memory.content, "draft memory");

    let rejected = mem
        .create_memory_draft(MemoryDraftInput {
            draft_id: Some("draft-2".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            worker_run_id: "run-worker".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            session_key: Some("session-a".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            key: "rejected-key".to_string(),
            content: "do not merge".to_string(),
            category: MemoryCategory::Conversation,
            source_event_id: None,
            visibility: MemoryVisibility::Workspace,
            payload_json: None,
        })
        .await
        .unwrap();
    assert_eq!(rejected.status, "pending");
    let rejected = mem
        .reject_memory_draft(&test_principal, "draft-2", Some("duplicate"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rejected.status, "rejected");
    assert!(mem.get("rejected-key").await.unwrap().is_none());

    let event_types: Vec<String> = mem
        .pool
        .write()
        .prepare("SELECT event_type FROM memory_events ORDER BY id ASC")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(event_types.contains(&"memory.draft.created".to_string()));
    assert!(event_types.contains(&"memory.draft.merged".to_string()));
    assert!(event_types.contains(&"memory.draft.rejected".to_string()));
    assert!(event_types.contains(&"memory.stored".to_string()));
}
