//! Live pool, migration, conformance and reindex tests plus shared fixtures.
use super::CountingEmbedding;
use crate::memory::embeddings;
use crate::memory::postgres::rows::quote_identifier;
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::traits::{
    CompactionRunInput, DocumentIngestInput, Memory, MemoryCategory, MemoryDraftInput, MemoryEventInput,
    MemoryLinkInput, MemoryPrincipal, MemoryVisibility, MessageEventInput, RetrievalTraceInput, SessionContextQuery,
    SharedContextQuery,
};
use anyhow::Result;
use chrono::Utc;
use postgres::NoTls;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use uuid::Uuid;

pub(super) fn fitness_message_input(workspace_id: &str, content: &str) -> MessageEventInput {
    MessageEventInput {
        event_id: Some(format!("fitness-event-{}", Uuid::new_v4())),
        idempotency_key: None,
        workspace_id: workspace_id.to_string(),
        owner_id: None,
        source: crate::memory::MessageEventSource::Other("fitness-postgres-test".to_string()),
        channel: None,
        session_key: Some("fitness-session".to_string()),
        parent_session_key: None,
        run_id: None,
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: None,
        recipient: None,
        role: "system".to_string(),
        event_type: "turn.finalized".to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: None,
        config_source_revision: None,
        content: content.to_string(),
        raw_payload_json: Some(r#"{"status":"completed"}"#.to_string()),
        visibility: MemoryVisibility::Workspace,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_time_range_query_includes_only_configured_worker_descendants_from_env() {
    let db_url = live_postgres_url();
    let schema = format!("prx_fitness_events_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    for (workspace, content) in [
        ("/workspace", "root event"),
        ("/workspace/workers/run-1", "worker event"),
        ("/workspace/workers-old/run-2", "prefix sibling hidden"),
        ("/other/workers/run-3", "other workspace hidden"),
    ] {
        mem.append_message_event(fitness_message_input(workspace, content))
            .await
            .unwrap();
    }
    let principal = MemoryPrincipal {
        workspace_id: "/workspace".to_string(),
        agent_id: Some("self_system".to_string()),
        ..MemoryPrincipal::default()
    };
    let start = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    let end = (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339();

    let visible = mem
        .list_message_events_time_range(&principal, Some("/workspace/workers"), &start, &end, 20)
        .await
        .unwrap();
    let contents = visible.iter().map(|event| event.content.as_str()).collect::<Vec<_>>();
    assert_eq!(contents, vec!["root event", "worker event"]);

    let non_system = MemoryPrincipal {
        agent_id: Some("regular-agent".to_string()),
        ..principal
    };
    assert!(
        mem.list_message_events_time_range(&non_system, Some("/workspace/workers"), &start, &end, 20)
            .await
            .is_err()
    );

    drop(mem);
    tokio::task::spawn_blocking(move || {
        let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
            .unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_fitness_prefix_migration_sql_is_exact_and_transactional_from_env() {
    let db_url = live_postgres_url();
    let schema = format!("prx_fitness_migration_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let table = mem.qualified_table.clone();
    let insert_url = db_url.clone();
    tokio::task::spawn_blocking(move || {
        let mut client = insert_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        let mut transaction = client.transaction().unwrap();
        transaction
            .execute("SELECT set_config('prx.rls_bypass', 'on', true)", &[])
            .unwrap();
        for (id, key) in [
            ("legacy-one", "self/fitness/daily/2026-09-10"),
            ("legacy-two", "self/fitness/daily/2026-09-11"),
            ("keep", "self/fitness/daily_extra/keep"),
        ] {
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {table} (id, key, content, category, created_at, updated_at) \
                             VALUES ($1, $2, 'payload', 'core', NOW(), NOW())"
                    ),
                    &[&id, &key],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
    })
    .await
    .unwrap();

    let query_url = db_url.clone();
    let query_schema = schema.clone();
    let rows = tokio::task::spawn_blocking(move || {
        PostgresMemory::read_key_prefix_read_only(&query_url, &query_schema, "memories", Some(5), "self/fitness/daily/")
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
        vec!["self/fitness/daily/2026-09-10", "self/fitness/daily/2026-09-11"]
    );
    let delete_url = db_url.clone();
    let delete_schema = schema.clone();
    let deleted = tokio::task::spawn_blocking(move || {
        PostgresMemory::delete_key_prefix_transactional(
            &delete_url,
            &delete_schema,
            "memories",
            Some(5),
            "self/fitness/daily/",
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(deleted, 2);
    let verify_url = db_url.clone();
    let verify_schema = schema.clone();
    let remaining = tokio::task::spawn_blocking(move || {
        PostgresMemory::read_key_prefix_read_only(
            &verify_url,
            &verify_schema,
            "memories",
            Some(5),
            "self/fitness/daily/",
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert!(remaining.is_empty(), "expected an empty value");
    assert!(mem.get("self/fitness/daily_extra/keep").await.unwrap().is_some());

    drop(mem);
    tokio::task::spawn_blocking(move || {
        let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
            .unwrap();
    })
    .await
    .unwrap();
}

/// Exercises the connection pool against a live PostgreSQL instance:
/// concurrent checkouts genuinely overlap, a saturated pool makes callers
/// queue instead of fail, and a connection killed server-side is replaced
/// transparently on the next use.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_connection_pool_saturates_and_recovers_from_env() {
    let db_url = live_postgres_url();
    let schema = format!("prx_pool_{}", Uuid::new_v4().simple());
    const POOL_SIZE: usize = 4;
    const CONCURRENCY: usize = 24;

    let mem = Arc::new(
        PostgresMemory::with_embedder_and_cache(
            &db_url,
            &schema,
            "memories",
            Some(5),
            Arc::new(embeddings::NoopEmbedding),
            0.35,
            0.65,
            256,
            POOL_SIZE,
        )
        .unwrap(),
    );

    // Far more concurrent work than connections: every call must complete.
    let mut tasks = Vec::new();
    for index in 0..CONCURRENCY {
        let mem = Arc::clone(&mem);
        tasks.push(tokio::spawn(async move {
            mem.store(
                &format!("pool-key-{index}"),
                "pool contention probe",
                MemoryCategory::Core,
                None,
            )
            .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }

    let stats = mem.pool_stats();
    assert_eq!(stats.max_size, POOL_SIZE, "pool must honour the configured size");
    assert!(
        stats.connects <= POOL_SIZE as u64,
        "pool must never open more connections than max_size, opened {}",
        stats.connects
    );
    assert!(stats.idle <= POOL_SIZE, "idle connections must stay within max_size");
    assert_eq!(stats.in_use, 0, "every checkout must be returned");
    assert!(
        stats.acquisitions >= CONCURRENCY as u64,
        "every operation must have checked a connection out"
    );
    assert!(
        stats.connects > 1,
        "concurrent load must open more than one connection, opened {}",
        stats.connects
    );
    assert!(
        stats.saturated_acquisitions > 0,
        "a pool of {POOL_SIZE} under {CONCURRENCY}-way load must have queued callers"
    );
    assert!(
        stats.total_wait > Duration::ZERO,
        "queued callers must record their wait time"
    );

    // Kill every pooled backend server-side, then prove the next call still
    // succeeds: stale connections are discarded and replaced, not surfaced.
    let kill_url = db_url.clone();
    let kill_schema = schema.clone();
    tokio::task::spawn_blocking(move || {
        let mut admin = kill_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        admin
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE pid <> pg_backend_pid() AND application_name IS NOT NULL \
                     AND query LIKE $1",
                &[&format!("%{kill_schema}%")],
            )
            .unwrap();
    })
    .await
    .unwrap();

    let discards_before = mem.pool_stats().discards;
    let recovered = mem.get("pool-key-0").await.unwrap();
    assert!(
        recovered.is_some(),
        "pool must transparently reconnect after the server closes connections"
    );
    let stats = mem.pool_stats();
    assert_eq!(stats.in_use, 0, "recovery must not leak a checkout slot");
    assert!(stats.discards >= discards_before, "discard counter must not regress");

    let cleanup_url = db_url.clone();
    let cleanup_schema = schema.clone();
    drop(mem);
    tokio::task::spawn_blocking(move || {
        let mut client = cleanup_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                quote_identifier(&cleanup_schema)
            ))
            .unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_memory_fabric_conformance_from_env() {
    let db_url = live_postgres_url();
    let schema = format!("prx_test_{}", Uuid::new_v4().simple());
    let table = "memories";
    let mem = PostgresMemory::new(&db_url, &schema, table, Some(5)).unwrap();

    let user = mem
        .append_message_event(MessageEventInput {
            event_id: Some("event-user-1".to_string()),
            idempotency_key: Some("idem-user-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            source: "postgres-test".into(),
            channel: Some("telegram".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            parent_session_key: None,
            run_id: Some("run-1".to_string()),
            parent_run_id: None,
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            sender: Some("sender-1".to_string()),
            recipient: Some("prx".to_string()),
            role: "user".to_string(),
            event_type: "message.created".to_string(),
            subject: Some(crate::memory::MessageEventSubject::Task("task-pg-1".to_string())),
            goal_id: Some("goal-pg-1".to_string()),
            causation_event_id: Some("event-parent-pg".to_string()),
            correlation_id: Some("correlation-pg-1".to_string()),
            attempt_id: Some("attempt-pg-2".to_string()),
            lease_epoch: Some(3),
            config_generation_id: Some(42),
            config_source_revision: Some("sha256:postgres-test".to_string()),
            content: "hello from postgres".to_string(),
            raw_payload_json: None,
            visibility: MemoryVisibility::Workspace,
        })
        .await
        .unwrap();
    let duplicate = mem
        .append_message_event(MessageEventInput {
            event_id: Some("event-user-duplicate".to_string()),
            idempotency_key: Some("idem-user-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            source: "postgres-test".into(),
            channel: Some("telegram".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            parent_session_key: None,
            run_id: Some("run-1".to_string()),
            parent_run_id: None,
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            sender: Some("sender-1".to_string()),
            recipient: Some("prx".to_string()),
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
            content: "duplicate should not replace".to_string(),
            raw_payload_json: None,
            visibility: MemoryVisibility::Workspace,
        })
        .await
        .unwrap();
    assert_eq!(duplicate.event_id, user.event_id);
    assert_eq!(duplicate.content, user.content);
    let other_workspace = mem
        .append_message_event(MessageEventInput {
            event_id: Some("event-user-workspace-b".to_string()),
            idempotency_key: Some("idem-user-1".to_string()),
            workspace_id: "workspace-b".to_string(),
            owner_id: Some("owner-b".to_string()),
            source: "postgres-test".into(),
            channel: Some("telegram".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            parent_session_key: None,
            run_id: Some("run-b".to_string()),
            parent_run_id: None,
            agent_id: Some("agent-b".to_string()),
            persona_id: Some("persona-b".to_string()),
            sender: Some("sender-b".to_string()),
            recipient: Some("prx".to_string()),
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
            content: "same idempotency key in another workspace".to_string(),
            raw_payload_json: None,
            visibility: MemoryVisibility::Workspace,
        })
        .await
        .unwrap();
    assert_eq!(other_workspace.workspace_id, "workspace-b");
    assert_ne!(other_workspace.event_id, user.event_id);
    assert_eq!(user.event_type, "message.created");
    assert_eq!(user.source, "postgres-test");
    assert_eq!(
        user.subject,
        Some(crate::memory::MessageEventSubject::Task("task-pg-1".to_string()))
    );
    assert_eq!(user.goal_id.as_deref(), Some("goal-pg-1"));
    assert_eq!(user.causation_event_id.as_deref(), Some("event-parent-pg"));
    assert_eq!(user.correlation_id.as_deref(), Some("correlation-pg-1"));
    assert_eq!(user.attempt_id.as_deref(), Some("attempt-pg-2"));
    assert_eq!(user.lease_epoch, Some(3));
    assert_eq!(user.config_generation_id, Some(42));
    assert_eq!(user.config_source_revision.as_deref(), Some("sha256:postgres-test"));

    let assistant = mem
        .append_message_event(MessageEventInput {
            event_id: Some("event-assistant-1".to_string()),
            idempotency_key: None,
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            source: "postgres-test".into(),
            channel: Some("telegram".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            parent_session_key: None,
            run_id: Some("run-1".to_string()),
            parent_run_id: None,
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            sender: Some("prx".to_string()),
            recipient: Some("sender-1".to_string()),
            role: "assistant".to_string(),
            event_type: "message.created".to_string(),
            subject: None,
            goal_id: None,
            causation_event_id: None,
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: Some(0),
            config_source_revision: None,
            content: "postgres reply".to_string(),
            raw_payload_json: None,
            visibility: MemoryVisibility::Workspace,
        })
        .await
        .unwrap();

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: Some("agent-a".to_string()),
        persona_id: Some("persona-a".to_string()),
        session_key: Some("telegram_sender-1".to_string()),
        channel: Some("telegram".to_string()),
        sender: Some("sender-1".to_string()),
        owner_id: Some("owner-a".to_string()),
        legacy_session_key: None,
    };
    let events = mem.list_message_events_since(&principal, 0, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events.first().map(|event| event.event_id.as_str()),
        Some("event-user-1")
    );
    assert_eq!(
        events.get(1).map(|event| event.event_id.as_str()),
        Some("event-assistant-1")
    );
    let recent_events = mem.list_message_events_recent(&principal, 10).await.unwrap();
    assert_eq!(recent_events.len(), 2);
    assert_eq!(
        recent_events.first().map(|event| event.event_id.as_str()),
        Some("event-assistant-1")
    );

    let shared = mem
        .load_recent_shared_context(SharedContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: 10,
            include_roles: vec!["assistant".to_string()],
        })
        .await
        .unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(
        shared.first().map(|event| event.event_id.as_str()),
        Some(assistant.event_id.as_str())
    );

    let session = mem
        .load_recent_session_context(SessionContextQuery {
            principal: principal.clone(),
            since_event_id: Some(user.id),
            limit: 10,
            include_roles: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(session.len(), 1);
    assert_eq!(
        session.first().map(|event| event.event_id.as_str()),
        Some("event-assistant-1")
    );

    let outbox_count = mem
        .list_memory_events_since(&principal, 0, 10)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.subject_table == "message_events")
        .count();
    assert_eq!(outbox_count, 2);

    let custom_event = mem
        .append_memory_event(MemoryEventInput {
            owner_id: None,
            event_id: Some("memory-event-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            event_type: "memory.custom".to_string(),
            subject_table: "memories".to_string(),
            subject_id: "subject-1".to_string(),
            session_key: Some("telegram_sender-1".to_string()),
            run_id: None,
            parent_run_id: None,
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            visibility: MemoryVisibility::Workspace,
            payload_json: Some("{\"ok\":true}".to_string()),
        })
        .await
        .unwrap();
    assert_eq!(custom_event.event_type, "memory.custom");
    let recent_outbox = mem.list_memory_events_recent(&principal, 10).await.unwrap();
    assert_eq!(
        recent_outbox.first().map(|event| event.event_id.as_str()),
        Some("memory-event-1")
    );

    let draft = mem
        .create_memory_draft(MemoryDraftInput {
            draft_id: Some("draft-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            worker_run_id: "worker-run-1".to_string(),
            parent_run_id: Some("parent-run-1".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            key: "draft-key".to_string(),
            content: "draft content".to_string(),
            category: MemoryCategory::Conversation,
            source_event_id: Some(user.event_id.clone()),
            visibility: MemoryVisibility::Private,
            payload_json: None,
        })
        .await
        .unwrap();
    assert_eq!(draft.status, "pending");
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
    let merged = mem
        .merge_memory_draft(&test_principal, "draft-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.status, "merged");
    let stored = mem.get("draft-key").await.unwrap().unwrap();
    assert_eq!(stored.content, "draft content");

    let rejected = mem
        .create_memory_draft(MemoryDraftInput {
            draft_id: Some("draft-2".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            worker_run_id: "worker-run-1".to_string(),
            parent_run_id: Some("parent-run-1".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            key: "rejected-draft-key".to_string(),
            content: "rejected draft content".to_string(),
            category: MemoryCategory::Conversation,
            source_event_id: Some(user.event_id.clone()),
            visibility: MemoryVisibility::Private,
            payload_json: None,
        })
        .await
        .unwrap();
    assert_eq!(rejected.status, "pending");
    let rejected = mem
        .reject_memory_draft(&test_principal, "draft-2", Some("not useful"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rejected.status, "rejected");

    let event_types: Vec<String> = mem
        .list_memory_events_since(&principal, 0, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert!(event_types.contains(&"memory.draft.created".to_string()));
    assert!(event_types.contains(&"memory.draft.merged".to_string()));
    assert!(event_types.contains(&"memory.draft.rejected".to_string()));
    assert!(event_types.contains(&"memory.stored".to_string()));

    let document = mem
        .ingest_document(DocumentIngestInput {
            document_id: Some("doc-postgres-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            topic_id: Some("topic-a".to_string()),
            task_id: Some("task-a".to_string()),
            source_message_event_id: Some(user.event_id.clone()),
            source_kind: "tool_output".to_string(),
            source_uri: Some("tool:file_read".to_string()),
            title: Some("Postgres Document".to_string()),
            content: "Durable postgres document fact about vector retrieval and source anchors.".to_string(),
            mime_type: Some("text/plain".to_string()),
            visibility: MemoryVisibility::Workspace,
            metadata_json: Some("{\"tool\":\"file_read\"}".to_string()),
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .unwrap();
    assert_eq!(document.document_id, "doc-postgres-1");
    assert_eq!(document.chunk_count, 1);
    assert_eq!(document.content_sha256.len(), 64);

    let document_results = mem
        .search_document_chunks(&principal, "vector retrieval", 10)
        .await
        .unwrap();
    assert_eq!(document_results.len(), 1);
    let document_result = document_results.first().expect("document result should exist");
    assert_eq!(document_result.chunk.document_id, "doc-postgres-1");
    assert_eq!(document_result.chunk.source_anchor, "doc-postgres-1#chunk-0");

    let chunk = mem
        .get_document_chunk(&document_result.chunk.chunk_id)
        .await
        .unwrap()
        .expect("document chunk should be retrievable");
    assert!(chunk.content.contains("source anchors"));

    let link = mem
        .link_memory_source(MemoryLinkInput {
            link_id: Some("link-postgres-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            memory_key: Some("draft-key".to_string()),
            memory_event_id: None,
            message_event_id: Some(user.event_id.clone()),
            document_id: document.document_id.clone(),
            chunk_id: Some(chunk.chunk_id.clone()),
            link_type: "evidence".to_string(),
            payload_json: None,
        })
        .await
        .unwrap();
    assert_eq!(link.link_id, "link-postgres-1");
    assert_eq!(link.chunk_id.as_deref(), Some(chunk.chunk_id.as_str()));

    let trace = mem
        .append_retrieval_trace(RetrievalTraceInput {
            trace_id: Some("trace-postgres-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            source: "agent_context.document_evidence".to_string(),
            query: "vector retrieval".to_string(),
            candidate_count: document_results.len(),
            selected_count: 1,
            dropped_count: 0,
            budget_tokens: Some(512),
            selected_json: Some(format!(r#"[{{"chunk_id":"{}"}}]"#, chunk.chunk_id)),
            dropped_json: Some("[]".to_string()),
            payload_json: Some(r#"{"phase":"postgres_conformance"}"#.to_string()),
        })
        .await
        .unwrap();
    assert_eq!(trace.trace_id, "trace-postgres-1");
    assert_eq!(trace.source, "agent_context.document_evidence");
    assert!(trace.selected_json.unwrap().contains("doc-postgres-1:chunk:0"));

    let compaction = mem
        .append_compaction_run(CompactionRunInput {
            run_id: Some("compact-postgres-1".to_string()),
            workspace_id: "workspace-a".to_string(),
            owner_id: Some("owner-a".to_string()),
            session_key: Some("telegram_sender-1".to_string()),
            agent_id: Some("agent-a".to_string()),
            persona_id: Some("persona-a".to_string()),
            trigger: "overflow_retry".to_string(),
            mode: "safeguard".to_string(),
            source_message_count: 3,
            source_token_estimate: 512,
            summary: "## Decisions\n- preserve source anchors".to_string(),
            summary_memory_key: Some("compaction_summary_pg".to_string()),
            source_event_ids_json: Some(format!(r#"["{}"]"#, user.event_id)),
            source_event_range_json: Some(
                serde_json::json!({
                    "first_event_id": user.event_id,
                    "last_event_id": user.event_id,
                    "first_row_id": user.id,
                    "last_row_id": user.id,
                    "source_event_count": 1
                })
                .to_string(),
            ),
            source_document_refs_json: Some(format!(r#"[{{"chunk_id":"{}"}}]"#, chunk.chunk_id)),
            fidelity_status: "accepted".to_string(),
            payload_json: Some(r#"{"phase":"postgres_conformance"}"#.to_string()),
        })
        .await
        .unwrap();
    assert_eq!(compaction.run_id, "compact-postgres-1");
    assert_eq!(compaction.summary_memory_key.as_deref(), Some("compaction_summary_pg"));
    assert!(
        compaction
            .source_event_range_json
            .as_deref()
            .is_some_and(|json| json.contains(&user.event_id))
    );
    assert_eq!(compaction.fidelity_status, "accepted");

    crate::memory::traits::conformance::assert_scoped_memory_acl_conformance(&mem, "postgres-scoped-conformance").await;

    drop(mem);
    tokio::task::spawn_blocking(move || {
        let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
            .unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_embedding_vector_reindex_from_env() {
    let db_url = live_postgres_url();
    let schema = format!("prx_vector_test_{}", Uuid::new_v4().simple());
    let table = "memories";
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = PostgresMemory::with_embedder(
        &db_url,
        &schema,
        table,
        Some(5),
        Arc::new(CountingEmbedding {
            calls: Arc::clone(&calls),
        }),
        1.0,
        0.0,
    )
    .unwrap();

    mem.store("core-vector", "alpha beta gamma", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("core-vector-copy", "alpha beta gamma", MemoryCategory::Core, None)
        .await
        .unwrap();
    mem.store("daily-vector", "daily alpha beta", MemoryCategory::Daily, None)
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "Postgres should cache repeated durable core/custom memory embeddings"
    );

    let vector_results = mem.recall("semantic-neighbor", 5, None).await.unwrap();
    assert!(
        vector_results.iter().any(|entry| entry.key == "core-vector"),
        "Postgres memory recall should use app-level vector scan when lexical search has no match"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "Postgres should compute the first query embedding"
    );
    let _ = mem.recall("semantic-neighbor", 5, None).await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "Postgres should reuse cached query embeddings"
    );

    mem.ingest_document(DocumentIngestInput {
        document_id: Some("doc-vector-postgres-1".to_string()),
        workspace_id: "workspace-a".to_string(),
        owner_id: Some("owner-a".to_string()),
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        source_kind: "test".to_string(),
        source_uri: None,
        title: Some("Vector Postgres Doc".to_string()),
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
    mem.ingest_document(DocumentIngestInput {
        document_id: Some("doc-vector-postgres-2".to_string()),
        workspace_id: "workspace-a".to_string(),
        owner_id: Some("owner-a".to_string()),
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        source_kind: "test".to_string(),
        source_uri: None,
        title: Some("Vector Postgres Doc Copy".to_string()),
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
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "Postgres should cache repeated document chunk embeddings"
    );
    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("chat:postgres-vector".to_string()),
        channel: Some("terminal".to_string()),
        sender: Some("local-user".to_string()),
        owner_id: Some("owner-a".to_string()),
        legacy_session_key: None,
    };
    let document_results = mem
        .search_document_chunks(&principal, "semantic-neighbor", 5)
        .await
        .unwrap();
    assert!(
        document_results
            .iter()
            .any(|result| result.chunk.document_id == "doc-vector-postgres-1"),
        "Postgres document search should use app-level vector scan when lexical search has no match"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "Postgres document search must not reuse a system-scoped query embedding across owners"
    );
    {
        let client_slot = mem.client.clone();
        let qualified_embedding_cache_table = mem.qualified_embedding_cache_table.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            client_slot.with_client(|client| {
                let count: i64 = client
                    .query_one(&format!("SELECT COUNT(*) FROM {qualified_embedding_cache_table}"), &[])
                    .map(|row| row.get(0))?;
                assert!(
                    count >= 4,
                    "Postgres embedding cache should retain stored/query embeddings"
                );
                Ok(())
            })
        })
        .await
        .unwrap()
        .unwrap();
    }

    {
        let client_slot = mem.client.clone();
        let qualified_table = mem.qualified_table.clone();
        let qualified_document_chunks_table = mem.qualified_document_chunks_table.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            client_slot.with_client(|client| {
                client.execute(
                    &format!(
                        "UPDATE {} SET embedding_provider = 'stale-provider',
                         embedding_model = 'stale-model',
                         embedding_dimensions = 999
                         WHERE key = 'core-vector'",
                        qualified_table
                    ),
                    &[],
                )?;
                client.execute(
                    &format!(
                        "UPDATE {} SET embedding_provider = 'stale-provider',
                         embedding_model = 'stale-model',
                         embedding_dimensions = 999
                         WHERE document_id = 'doc-vector-postgres-1'",
                        qualified_document_chunks_table
                    ),
                    &[],
                )?;
                Ok(())
            })
        })
        .await
        .unwrap()
        .unwrap();
    }

    let stale_memory = mem.recall("semantic-neighbor", 5, None).await.unwrap();
    assert!(
        stale_memory.iter().all(|entry| entry.key != "core-vector"),
        "stale Postgres memory vector metadata must be ignored before reindex"
    );
    let stale_document = mem
        .search_document_chunks(&principal, "semantic-neighbor", 5)
        .await
        .unwrap();
    assert!(
        stale_document
            .iter()
            .all(|result| result.chunk.document_id != "doc-vector-postgres-1"),
        "stale Postgres document chunk vector metadata must be ignored before reindex"
    );

    let repaired = mem.reindex().await.unwrap();
    assert_eq!(repaired, 2);

    let restored_memory = mem.recall("semantic-neighbor", 5, None).await.unwrap();
    assert!(
        restored_memory.iter().any(|entry| entry.key == "core-vector"),
        "Postgres memory vector recall should work after reindex"
    );
    let restored_document = mem
        .search_document_chunks(&principal, "semantic-neighbor", 5)
        .await
        .unwrap();
    assert!(
        restored_document
            .iter()
            .any(|result| result.chunk.document_id == "doc-vector-postgres-1"),
        "Postgres document vector recall should work after reindex"
    );

    {
        let client_slot = mem.client.clone();
        let qualified_table = mem.qualified_table.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            client_slot.with_client(|client| {
                let (provider, model, dimensions): (String, String, i64) = client
                    .query_one(
                        &format!(
                            "SELECT embedding_provider, embedding_model, embedding_dimensions
                             FROM {} WHERE key = 'core-vector'",
                            qualified_table
                        ),
                        &[],
                    )
                    .map(|row| (row.get(0), row.get(1), row.get(2)))?;
                assert_eq!(provider, "counting");
                assert_eq!(model, "counting-v1");
                assert_eq!(dimensions, 3);
                Ok(())
            })
        })
        .await
        .unwrap()
        .unwrap();
    }

    drop(mem);
    tokio::task::spawn_blocking(move || {
        let mut client = db_url.parse::<postgres::Config>().unwrap().connect(NoTls).unwrap();
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_identifier(&schema)))
            .unwrap();
    })
    .await
    .unwrap();
}
