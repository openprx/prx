//! Owner-isolation matrices for identity, ingest events and drafts.
use super::memory_owner_isolation::r5_drop_schema;
use crate::memory::postgres::{PostgresMemory, live_postgres_url};
use crate::memory::traits::{
    DocumentIngestInput, Memory, MemoryCategory, MemoryDraftInput, MemoryEventInput, MemoryPrincipal, MemoryVisibility,
    MessageEventInput,
};
use uuid::Uuid;

// ---------------------------------------------------------------------
// C03 / V02-R4: Postgres parity for owner-qualified identity isolation.
//
// Requires a reachable instance: set `OPENPRX_TEST_POSTGRES_URL` and run
// with `--ignored`, exactly like every other Postgres test here.
// ---------------------------------------------------------------------

pub(super) const C03_WORKSPACE: &str = "ws-c03";
pub(super) const C03_SESSION_KEY: &str = "mcp:shared-subject";

pub(super) fn c03_owner(issuer: &str) -> String {
    format!("owner:{C03_WORKSPACE}:agent:{issuer}:shared-subject")
}

pub(super) fn c03_message_input(
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

pub(super) fn c03_principal(owner_id: Option<&str>, isolated: bool) -> MemoryPrincipal {
    MemoryPrincipal {
        workspace_id: C03_WORKSPACE.to_string(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_c03_owner_isolation_matrix_for_session_and_private_rows() {
    let db_url = live_postgres_url();
    let schema = format!("prx_c03_isolation_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    for (owner, visibility, content) in [
        (&owner_a, MemoryVisibility::Session, "a-session"),
        (&owner_a, MemoryVisibility::Private, "a-private"),
        (&owner_b, MemoryVisibility::Session, "b-session"),
        (&owner_a, MemoryVisibility::Workspace, "a-workspace"),
    ] {
        mem.append_message_event(c03_message_input(
            C03_WORKSPACE,
            Some(owner),
            C03_SESSION_KEY,
            visibility,
            content,
        ))
        .await
        .unwrap();
    }
    mem.append_message_event(c03_message_input(
        "ws-other",
        Some(&owner_a),
        C03_SESSION_KEY,
        MemoryVisibility::Session,
        "other-workspace",
    ))
    .await
    .unwrap();
    mem.append_message_event(c03_message_input(
        C03_WORKSPACE,
        None,
        C03_SESSION_KEY,
        MemoryVisibility::Session,
        "legacy-unowned",
    ))
    .await
    .unwrap();

    let seen_by_b = mem
        .list_message_events_recent(&c03_principal(Some(&owner_b), true), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.content)
        .collect::<Vec<_>>();
    assert!(
        !seen_by_b.iter().any(|content| content == "a-session"),
        "issuer B must not read issuer A's session row: {seen_by_b:?}"
    );
    assert!(
        !seen_by_b.iter().any(|content| content == "a-private"),
        "issuer B must not read issuer A's private row: {seen_by_b:?}"
    );
    assert!(
        !seen_by_b.iter().any(|content| content == "legacy-unowned"),
        "an unattributed row must not reach an isolated principal: {seen_by_b:?}"
    );
    assert!(
        seen_by_b.iter().any(|content| content == "b-session"),
        "issuer B must still read its own row: {seen_by_b:?}"
    );
    assert!(
        !seen_by_b.iter().any(|content| content == "a-workspace"),
        "OWNER-VIS OV-C: an isolated caller no longer reads another owner's workspace row: {seen_by_b:?}"
    );
    assert!(
        !seen_by_b.iter().any(|content| content == "other-workspace"),
        "a different workspace stays isolated: {seen_by_b:?}"
    );

    let no_owner = mem
        .list_message_events_recent(&c03_principal(None, true), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.content)
        .collect::<Vec<_>>();
    assert!(
        no_owner.is_empty(),
        "OWNER-VIS OV-C: an isolated principal with no owner id admits nothing: {no_owner:?}"
    );

    let unisolated = mem
        .list_message_events_recent(&c03_principal(Some(&owner_b), false), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.content)
        .collect::<Vec<_>>();
    assert!(
        unisolated.iter().any(|content| content == "a-session")
            && !unisolated.iter().any(|content| content == "a-private"),
        "a non-isolated principal keeps the session semantics, and since OWNER-VIS OV-E \
             reads another owner's private row no longer: {unisolated:?}"
    );
}

/// C03 (P2-4), Postgres twin: the draft-lifecycle and document-ingest
/// inserts must record the owner in the `memory_events.owner_id` column.
/// The draft rows are written at `session` visibility, the arm the owner
/// predicate guards, so they exercise attribution and isolation; the
/// `document.ingested` row is written workspace-visible here so this test
/// stays about attribution; the `session`-visibility ingest arm has its own
/// tests below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_c03_draft_lifecycle_and_ingest_events_carry_the_owner() {
    let db_url = live_postgres_url();
    let schema = format!("prx_c03_lifecycle_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    for (owner, draft_id, key) in [(&owner_a, "draft-a", "key-a"), (&owner_b, "draft-b", "key-b")] {
        mem.create_memory_draft(MemoryDraftInput {
            draft_id: Some(draft_id.to_string()),
            workspace_id: C03_WORKSPACE.to_string(),
            owner_id: Some(owner.clone()),
            worker_run_id: format!("run-{draft_id}"),
            parent_run_id: None,
            session_key: Some(C03_SESSION_KEY.to_string()),
            agent_id: None,
            persona_id: None,
            key: key.to_string(),
            content: "draft body".to_string(),
            category: MemoryCategory::Core,
            source_event_id: None,
            visibility: MemoryVisibility::Session,
            payload_json: None,
        })
        .await
        .unwrap();
    }
    mem.merge_memory_draft(&c03_principal(Some(&owner_b), false), "draft-b")
        .await
        .unwrap();
    mem.reject_memory_draft(&c03_principal(Some(&owner_a), false), "draft-a", Some("not wanted"))
        .await
        .unwrap();

    for (owner, document_id, body) in [
        (&owner_a, "doc-a", "issuer-a document"),
        (&owner_b, "doc-b", "issuer-b document"),
    ] {
        mem.ingest_document(DocumentIngestInput {
            document_id: Some(document_id.to_string()),
            workspace_id: C03_WORKSPACE.to_string(),
            owner_id: Some(owner.clone()),
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            source_kind: "test".to_string(),
            source_uri: None,
            title: None,
            content: body.to_string(),
            mime_type: Some("text/plain".to_string()),
            visibility: MemoryVisibility::Workspace,
            metadata_json: None,
            session_key: None,
            agent_id: None,
            persona_id: None,
        })
        .await
        .unwrap();
    }

    let events = mem
        .list_memory_events_recent(&c03_principal(Some(&owner_a), false), 200)
        .await
        .unwrap();
    for (event_type, subject_id, expected_owner) in [
        ("memory.draft.created", "draft-a", &owner_a),
        ("memory.draft.created", "draft-b", &owner_b),
        ("memory.draft.merged", "draft-b", &owner_b),
        ("memory.stored", "key-b", &owner_b),
        ("memory.draft.rejected", "draft-a", &owner_a),
        ("document.ingested", "doc-a", &owner_a),
        ("document.ingested", "doc-b", &owner_b),
    ] {
        let event = events
            .iter()
            .find(|event| event.event_type == event_type && event.subject_id == subject_id)
            .unwrap_or_else(|| panic!("no `{event_type}` row for `{subject_id}`"));
        assert_eq!(
            event.owner_id.as_deref(),
            Some(expected_owner.as_str()),
            "`{event_type}` for `{subject_id}` must carry its own owner"
        );
    }

    let seen_by_b = mem
        .list_memory_events_recent(&c03_principal(Some(&owner_b), true), 200)
        .await
        .unwrap()
        .into_iter()
        .map(|event| (event.event_type, event.subject_id))
        .collect::<Vec<_>>();
    assert!(
        seen_by_b.contains(&("memory.draft.created".to_string(), "draft-b".to_string()))
            && seen_by_b.contains(&("memory.draft.merged".to_string(), "draft-b".to_string())),
        "issuer B must read its own draft lifecycle: {seen_by_b:?}"
    );
    assert!(
        !seen_by_b
            .iter()
            .any(|(_, subject)| subject == "draft-a" || subject == "key-a"),
        "issuer B must not read issuer A's draft lifecycle: {seen_by_b:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_c03_memory_events_are_owner_qualified() {
    let db_url = live_postgres_url();
    let schema = format!("prx_c03_events_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).unwrap();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    for (owner, subject) in [
        (Some(&owner_a), "run-a"),
        (Some(&owner_b), "run-b"),
        (None, "run-legacy"),
    ] {
        mem.append_memory_event(MemoryEventInput {
            event_id: None,
            workspace_id: C03_WORKSPACE.to_string(),
            owner_id: owner.cloned(),
            event_type: "task.spawned".to_string(),
            subject_table: "tasks".to_string(),
            subject_id: subject.to_string(),
            session_key: Some(C03_SESSION_KEY.to_string()),
            run_id: Some(subject.to_string()),
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            visibility: MemoryVisibility::Session,
            payload_json: None,
        })
        .await
        .unwrap();
    }

    let seen = mem
        .list_memory_events_recent(&c03_principal(Some(&owner_b), true), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.subject_id)
        .collect::<Vec<_>>();
    assert_eq!(
        seen,
        vec!["run-b".to_string()],
        "an isolated principal must only see its own task events: {seen:?}"
    );

    let all = mem
        .list_memory_events_recent(&c03_principal(Some(&owner_b), false), 50)
        .await
        .unwrap();
    assert_eq!(all.len(), 3, "the non-isolated read is unchanged");
    assert!(
        all.iter()
            .any(|event| event.owner_id.as_deref() == Some(owner_a.as_str())),
        "the owner column must round-trip through Postgres"
    );
}

// ---------------------------------------------------------------------
// V02-R6, Postgres twins. `PostgresMemory::ingest_document` requires a
// non-empty `owner_id` for vector isolation, so every fixture here sets
// one; the SQLite `owner_id: None` cases have no Postgres counterpart.
// ---------------------------------------------------------------------

pub(super) fn postgres_c03_ingest_input(
    document_id: &str,
    owner_id: &str,
    session_key: Option<&str>,
    visibility: MemoryVisibility,
) -> DocumentIngestInput {
    DocumentIngestInput {
        document_id: Some(document_id.to_string()),
        workspace_id: C03_WORKSPACE.to_string(),
        owner_id: Some(owner_id.to_string()),
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

pub(super) async fn postgres_c03_ingest_subjects(mem: &PostgresMemory, principal: &MemoryPrincipal) -> Vec<String> {
    mem.list_memory_events_recent(principal, 200)
        .await
        .expect("test: recent memory events")
        .into_iter()
        .filter(|event| event.event_type == "document.ingested")
        .map(|event| event.subject_id)
        .collect()
}

/// The V02-R6 regression on Postgres: the bound `session_key` is what makes
/// the `session` arm match at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_c03_session_visibility_ingest_event_is_readable_by_its_own_session() {
    let db_url = live_postgres_url();
    let schema = format!("prx_c03_ingest_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");

    mem.ingest_document(postgres_c03_ingest_input(
        "doc-s",
        &owner_a,
        Some(C03_SESSION_KEY),
        MemoryVisibility::Session,
    ))
    .await
    .expect("test: ingest session-visibility document");

    let events = mem
        .list_memory_events_recent(&c03_principal(Some(&owner_a), false), 200)
        .await
        .expect("test: recent memory events");
    let event = events
        .iter()
        .find(|event| event.event_type == "document.ingested" && event.subject_id == "doc-s")
        .expect("test: the session ingest event must be readable by its own session");
    assert_eq!(event.session_key.as_deref(), Some(C03_SESSION_KEY));
    assert_eq!(event.owner_id.as_deref(), Some(owner_a.as_str()));

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_c03_session_visibility_ingest_event_is_readable_under_owner_isolation() {
    let db_url = live_postgres_url();
    let schema = format!("prx_c03_ingest_isolated_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");

    mem.ingest_document(postgres_c03_ingest_input(
        "doc-s",
        &owner_a,
        Some(C03_SESSION_KEY),
        MemoryVisibility::Session,
    ))
    .await
    .expect("test: ingest session-visibility document");

    let seen = postgres_c03_ingest_subjects(&mem, &c03_principal(Some(&owner_a), true)).await;
    assert_eq!(
        seen,
        vec!["doc-s".to_string()],
        "an isolated owner must read its own session ingest event: {seen:?}"
    );

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_c03_session_visibility_ingest_event_is_hidden_from_another_owner() {
    let db_url = live_postgres_url();
    let schema = format!("prx_c03_ingest_owners_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    for (owner, document_id) in [(&owner_a, "doc-a"), (&owner_b, "doc-b")] {
        mem.ingest_document(postgres_c03_ingest_input(
            document_id,
            owner,
            Some(C03_SESSION_KEY),
            MemoryVisibility::Session,
        ))
        .await
        .expect("test: ingest session-visibility document");
    }

    let seen = postgres_c03_ingest_subjects(&mem, &c03_principal(Some(&owner_b), true)).await;
    assert_eq!(
        seen,
        vec!["doc-b".to_string()],
        "an isolated owner must not read another owner's ingest event: {seen:?}"
    );

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

/// V02-R6 live twin: a private tool-ingest event has no agent or persona,
/// so the exact owner is the author identity admitted by both event-list
/// APIs. A different owner remains excluded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_private_visibility_ingest_event_is_readable_by_its_author() {
    let db_url = live_postgres_url();
    let schema = format!("prx_c03_ingest_private_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    mem.ingest_document(postgres_c03_ingest_input(
        "doc-private",
        &owner_a,
        Some(C03_SESSION_KEY),
        MemoryVisibility::Private,
    ))
    .await
    .expect("test: ingest private-visibility document");

    for isolated in [false, true] {
        let principal = c03_principal(Some(&owner_a), isolated);
        let recent = postgres_c03_ingest_subjects(&mem, &principal).await;
        assert_eq!(
            recent,
            vec!["doc-private".to_string()],
            "the author reads its private ingest event (isolated={isolated}): {recent:?}"
        );
        let since = mem
            .list_memory_events_since(&principal, 0, 200)
            .await
            .expect("test: memory events since")
            .into_iter()
            .filter(|event| event.event_type == "document.ingested")
            .map(|event| event.subject_id)
            .collect::<Vec<_>>();
        assert_eq!(since, recent, "both event-list APIs enforce the same author rule");

        let foreign = postgres_c03_ingest_subjects(&mem, &c03_principal(Some(&owner_b), isolated)).await;
        assert!(
            foreign.is_empty(),
            "another owner cannot read the private ingest event (isolated={isolated}): {foreign:?}"
        );
    }

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_c03_ingest_event_is_hidden_from_another_session() {
    let db_url = live_postgres_url();
    let schema = format!("prx_c03_ingest_sessions_{}", Uuid::new_v4().simple());
    let mem = PostgresMemory::new(&db_url, &schema, "memories", Some(5)).expect("test: postgres memory");
    let owner_a = c03_owner("issuer-a");

    mem.ingest_document(postgres_c03_ingest_input(
        "doc-other-session",
        &owner_a,
        Some("mcp:other-subject"),
        MemoryVisibility::Session,
    ))
    .await
    .expect("test: ingest session-visibility document");

    for isolated in [false, true] {
        let seen = postgres_c03_ingest_subjects(&mem, &c03_principal(Some(&owner_a), isolated)).await;
        assert!(
            seen.is_empty(),
            "a foreign session key must not admit the ingest event (isolated={isolated}): {seen:?}"
        );

        // Positive control, as in the SQLite twin: without it the emptiness
        // above also holds when the three scoping keys revert to NULL.
        let owning_session = MemoryPrincipal {
            session_key: Some("mcp:other-subject".to_string()),
            ..c03_principal(Some(&owner_a), isolated)
        };
        let served = postgres_c03_ingest_subjects(&mem, &owning_session).await;
        assert_eq!(
            served,
            vec!["doc-other-session".to_string()],
            "the ingesting session must read its own ingest event (isolated={isolated}): {served:?}"
        );
    }

    drop(mem);
    r5_drop_schema(db_url, schema).await;
}
