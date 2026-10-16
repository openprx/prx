#![cfg(test)]
use super::*;

/// The audit counterexample plus its complement: isolation must deny the
/// other issuer without denying the owner itself, without leaking across
/// workspaces, and without changing the documented `workspace` contract.
#[tokio::test]
async fn c03_owner_isolation_matrix_for_session_and_private_rows() {
    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    for (visibility, content) in [
        (MemoryVisibility::Session, "a-session"),
        (MemoryVisibility::Private, "a-private"),
    ] {
        mem.append_message_event(c03_message_input(
            C03_WORKSPACE,
            Some(&owner_a),
            C03_SESSION_KEY,
            visibility,
            content,
        ))
        .await
        .expect("test: append issuer-a row");
    }
    mem.append_message_event(c03_message_input(
        C03_WORKSPACE,
        Some(&owner_b),
        C03_SESSION_KEY,
        MemoryVisibility::Session,
        "b-session",
    ))
    .await
    .expect("test: append issuer-b row");
    mem.append_message_event(c03_message_input(
        C03_WORKSPACE,
        Some(&owner_a),
        C03_SESSION_KEY,
        MemoryVisibility::Workspace,
        "a-workspace",
    ))
    .await
    .expect("test: append workspace row");
    mem.append_message_event(c03_message_input(
        "ws-other",
        Some(&owner_a),
        C03_SESSION_KEY,
        MemoryVisibility::Session,
        "other-workspace",
    ))
    .await
    .expect("test: append foreign workspace row");

    let seen_by_b = c03_contents(&mem, &c03_principal(C03_WORKSPACE, Some(&owner_b), true)).await;
    assert!(
        !seen_by_b.iter().any(|content| content == "a-session"),
        "issuer B must not read issuer A's session row: {seen_by_b:?}"
    );
    assert!(
        !seen_by_b.iter().any(|content| content == "a-private"),
        "issuer B must not read issuer A's private row: {seen_by_b:?}"
    );
    assert!(
        seen_by_b.iter().any(|content| content == "b-session"),
        "issuer B must still read its own session row: {seen_by_b:?}"
    );
    assert!(
        !seen_by_b.iter().any(|content| content == "a-workspace"),
        "OWNER-VIS OV-C: an isolated caller no longer reads another owner's workspace row: {seen_by_b:?}"
    );
    assert!(
        !seen_by_b.iter().any(|content| content == "other-workspace"),
        "a different workspace stays isolated: {seen_by_b:?}"
    );

    let seen_by_a = c03_contents(&mem, &c03_principal(C03_WORKSPACE, Some(&owner_a), true)).await;
    assert!(
        seen_by_a.iter().any(|content| content == "a-session") && seen_by_a.iter().any(|c| c == "a-private"),
        "issuer A must still read its own rows: {seen_by_a:?}"
    );
    assert!(
        !seen_by_a.iter().any(|content| content == "b-session"),
        "isolation is symmetric: {seen_by_a:?}"
    );

    // A principal that does not ask for isolation keeps the session-key
    // semantics; since OWNER-VIS OV-E its `private` arm requires the owner
    // for every reader, so another owner's private row stays hidden.
    let unisolated = c03_contents(&mem, &c03_principal(C03_WORKSPACE, Some(&owner_b), false)).await;
    assert!(
        unisolated.iter().any(|content| content == "a-session")
            && !unisolated.iter().any(|content| content == "a-private"),
        "a non-isolated principal keeps the session semantics and no longer reads another owner's \
         private row: {unisolated:?}"
    );
}

/// Fail closed: isolation with no owner id must never degrade to
/// sender-only matching.
#[tokio::test]
async fn c03_owner_isolated_principal_without_an_owner_id_reads_nothing_private() {
    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");
    for (visibility, content) in [
        (MemoryVisibility::Session, "a-session"),
        (MemoryVisibility::Private, "a-private"),
        (MemoryVisibility::Workspace, "a-workspace"),
    ] {
        mem.append_message_event(c03_message_input(
            C03_WORKSPACE,
            Some(&owner_a),
            C03_SESSION_KEY,
            visibility,
            content,
        ))
        .await
        .expect("test: append row");
    }

    let seen = c03_contents(&mem, &c03_principal(C03_WORKSPACE, None, true)).await;
    assert!(
        seen.is_empty(),
        "OWNER-VIS OV-C: an isolated principal with no owner id admits nothing: {seen:?}"
    );
}

/// Legacy rows written before the owner column carried a value stay in the
/// database, are invisible to an isolated principal, and come back after the
/// documented recovery statement attributes them.
#[tokio::test]
async fn c03_unattributed_rows_are_invisible_until_the_documented_recovery_sql_runs() {
    let (tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");
    mem.append_message_event(c03_message_input(
        C03_WORKSPACE,
        None,
        C03_SESSION_KEY,
        MemoryVisibility::Session,
        "legacy-unowned",
    ))
    .await
    .expect("test: append legacy row");

    let principal = c03_principal(C03_WORKSPACE, Some(&owner_a), true);
    let before = c03_contents(&mem, &principal).await;
    assert!(
        before.is_empty(),
        "an unattributed row must not reach an isolated principal: {before:?}"
    );

    // The recovery procedure documented in docs/tools.md, applied
    // exactly as written: a dry-run count, then a bounded parameterized
    // UPDATE for one stated session key.
    let db_path = tmp.path().join("memory").join("brain.db");
    let conn = Connection::open(&db_path).expect("test: open brain.db");
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM message_events
             WHERE workspace_id = ?1 AND session_key = ?2 AND owner_id IS NULL",
            params![C03_WORKSPACE, C03_SESSION_KEY],
            |row| row.get(0),
        )
        .expect("test: dry-run count");
    assert_eq!(pending, 1, "the dry run must find exactly the legacy row");
    let updated = conn
        .execute(
            "UPDATE message_events SET owner_id = ?3
             WHERE workspace_id = ?1 AND session_key = ?2 AND owner_id IS NULL",
            params![C03_WORKSPACE, C03_SESSION_KEY, owner_a],
        )
        .expect("test: recovery update");
    assert_eq!(updated, 1);
    drop(conn);

    let after = c03_contents(&mem, &principal).await;
    assert_eq!(
        after,
        vec!["legacy-unowned".to_string()],
        "the recovered row must be readable by the owner it was assigned to: {after:?}"
    );
}

/// The D4 legacy-key read merge must not become an isolation bypass: a
/// foreign owner's row under the *legacy* key is still refused.
#[tokio::test]
async fn c03_legacy_session_key_read_merge_is_owner_qualified() {
    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");
    const LEGACY_KEY: &str = "mcp-legacy:shared-subject";

    mem.append_message_event(c03_message_input(
        C03_WORKSPACE,
        Some(&owner_a),
        LEGACY_KEY,
        MemoryVisibility::Session,
        "a-legacy",
    ))
    .await
    .expect("test: append issuer-a legacy row");
    mem.append_message_event(c03_message_input(
        C03_WORKSPACE,
        Some(&owner_b),
        LEGACY_KEY,
        MemoryVisibility::Session,
        "b-legacy",
    ))
    .await
    .expect("test: append issuer-b legacy row");

    let mut principal = c03_principal(C03_WORKSPACE, Some(&owner_b), true);
    principal.legacy_session_key = Some(LEGACY_KEY.to_string());
    let seen = c03_contents(&mem, &principal).await;
    assert_eq!(
        seen,
        vec!["b-legacy".to_string()],
        "the read merge must return only the caller's own legacy history: {seen:?}"
    );
}

/// C03 (P2-4): the draft-lifecycle and document-ingest inserts must record
/// the owner in the `memory_events.owner_id` column, not only inside
/// `payload_json`. Those rows carry the draft's or the document's own
/// visibility, which can be `session` or `private`, so an unattributed one
/// would be invisible to its own owner from the moment it was written — not
/// only for legacy data.
///
/// The draft rows are written at `session` visibility, which is the arm the
/// owner predicate guards, so they exercise attribution *and* isolation. The
/// `document.ingested` row is written workspace-visible here so this test
/// stays about attribution; the `session`-visibility ingest arm has its own
/// tests below.
#[tokio::test]
async fn c03_draft_lifecycle_and_ingest_events_carry_the_owner() {
    let (_tmp, mem) = temp_sqlite();
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
        .expect("test: create draft");
    }
    mem.merge_memory_draft(&c03_principal(C03_WORKSPACE, Some(&owner_b), false), "draft-b")
        .await
        .expect("test: merge issuer-b draft");
    mem.reject_memory_draft(
        &c03_principal(C03_WORKSPACE, Some(&owner_a), false),
        "draft-a",
        Some("not wanted"),
    )
    .await
    .expect("test: reject issuer-a draft");

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
        .expect("test: ingest document");
    }

    // Every lifecycle row is attributed, and to the right owner.
    let events = mem
        .list_memory_events_recent(&c03_principal(C03_WORKSPACE, Some(&owner_a), false), 200)
        .await
        .expect("test: recent memory events");
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

    // And the attribution is load-bearing: each owner reads only its own
    // `session`-visibility lifecycle rows.
    let seen_by_b = mem
        .list_memory_events_recent(&c03_principal(C03_WORKSPACE, Some(&owner_b), true), 200)
        .await
        .expect("test: isolated recent memory events")
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

/// The `memory_events` outbox is the second read the sessions read model
/// makes; its `session`/`private` arms are owner-qualified too.
#[tokio::test]
async fn c03_memory_events_are_owner_qualified() {
    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    for (owner, subject) in [(&owner_a, "run-a"), (&owner_b, "run-b")] {
        mem.append_memory_event(MemoryEventInput {
            event_id: None,
            workspace_id: C03_WORKSPACE.to_string(),
            owner_id: Some(owner.clone()),
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
        .expect("test: append memory event");
    }

    let seen = mem
        .list_memory_events_recent(&c03_principal(C03_WORKSPACE, Some(&owner_b), true), 50)
        .await
        .expect("test: recent memory events")
        .into_iter()
        .map(|event| event.subject_id)
        .collect::<Vec<_>>();
    assert_eq!(
        seen,
        vec!["run-b".to_string()],
        "an isolated principal must only see its own task events: {seen:?}"
    );
}

/// The V02-R6 regression: before the fix the row was written with a NULL
/// `session_key`, so the `session` arm (`session_key = ?4`) could never
/// match and the event was invisible to the very session that wrote it.
#[tokio::test]
async fn c03_session_visibility_ingest_event_is_readable_by_its_own_session() {
    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");

    mem.ingest_document(c03_ingest_input(
        "doc-s",
        Some(&owner_a),
        Some(C03_SESSION_KEY),
        MemoryVisibility::Session,
    ))
    .await
    .expect("test: ingest session-visibility document");

    let events = mem
        .list_memory_events_recent(&c03_principal(C03_WORKSPACE, Some(&owner_a), false), 200)
        .await
        .expect("test: recent memory events");
    let event = events
        .iter()
        .find(|event| event.event_type == "document.ingested" && event.subject_id == "doc-s")
        .expect("test: the session ingest event must be readable by its own session");
    assert_eq!(event.session_key.as_deref(), Some(C03_SESSION_KEY));
    assert_eq!(
        event.owner_id.as_deref(),
        Some(owner_a.as_str()),
        "the owner landed with 7da31202 and must stay on the row"
    );
}

/// The same row under owner isolation: the session arm's owner suffix can
/// only narrow, so the key must be present for the arm to admit at all.
#[tokio::test]
async fn c03_session_visibility_ingest_event_is_readable_under_owner_isolation() {
    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");

    mem.ingest_document(c03_ingest_input(
        "doc-s",
        Some(&owner_a),
        Some(C03_SESSION_KEY),
        MemoryVisibility::Session,
    ))
    .await
    .expect("test: ingest session-visibility document");

    let seen = c03_ingest_subjects(&mem, &c03_principal(C03_WORKSPACE, Some(&owner_a), true)).await;
    assert_eq!(
        seen,
        vec!["doc-s".to_string()],
        "an isolated owner must read its own session ingest event: {seen:?}"
    );
}

/// Two issuers share one subject-derived session key. Carrying the key must
/// not hand either issuer the other's ingest event.
#[tokio::test]
async fn c03_session_visibility_ingest_event_is_hidden_from_another_owner() {
    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    for (owner, document_id) in [(&owner_a, "doc-a"), (&owner_b, "doc-b")] {
        mem.ingest_document(c03_ingest_input(
            document_id,
            Some(owner),
            Some(C03_SESSION_KEY),
            MemoryVisibility::Session,
        ))
        .await
        .expect("test: ingest session-visibility document");
    }

    let seen = c03_ingest_subjects(&mem, &c03_principal(C03_WORKSPACE, Some(&owner_b), true)).await;
    assert_eq!(
        seen,
        vec!["doc-b".to_string()],
        "an isolated owner must not read another owner's ingest event: {seen:?}"
    );
}

/// Owner equality is not a substitute for session equality: the same owner
/// reading from a different session sees nothing.
#[tokio::test]
async fn c03_ingest_event_is_hidden_from_another_session() {
    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");

    mem.ingest_document(c03_ingest_input(
        "doc-other-session",
        Some(&owner_a),
        Some("mcp:other-subject"),
        MemoryVisibility::Session,
    ))
    .await
    .expect("test: ingest session-visibility document");

    for isolated in [false, true] {
        let seen = c03_ingest_subjects(&mem, &c03_principal(C03_WORKSPACE, Some(&owner_a), isolated)).await;
        assert!(
            seen.is_empty(),
            "a foreign session key must not admit the ingest event (isolated={isolated}): {seen:?}"
        );

        // Positive control: the session the document was ingested under
        // does read the event. Without it the emptiness above also holds
        // when the three scoping keys revert to NULL, which is the defect
        // this fixture exists to catch.
        let owning_session = MemoryPrincipal {
            session_key: Some("mcp:other-subject".to_string()),
            ..c03_principal(C03_WORKSPACE, Some(&owner_a), isolated)
        };
        let served = c03_ingest_subjects(&mem, &owning_session).await;
        assert_eq!(
            served,
            vec!["doc-other-session".to_string()],
            "the ingesting session must read its own ingest event (isolated={isolated}): {served:?}"
        );
    }
}

/// The in-process contract is unchanged: a workspace-visibility ingest is
/// still read by a plain workspace principal, and still carries no keys.
#[tokio::test]
async fn ingest_event_visibility_unchanged_for_a_non_isolated_workspace_caller() {
    let (_tmp, mem) = temp_sqlite();

    mem.ingest_document(c03_ingest_input(
        "doc-workspace",
        None,
        None,
        MemoryVisibility::Workspace,
    ))
    .await
    .expect("test: ingest workspace-visibility document");

    let principal = MemoryPrincipal {
        workspace_id: C03_WORKSPACE.to_string(),
        agent_id: None,
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: None,
        require_owner_match: false,
        legacy_session_key: None,
    };
    let event = mem
        .list_memory_events_recent(&principal, 200)
        .await
        .expect("test: recent memory events")
        .into_iter()
        .find(|event| event.event_type == "document.ingested" && event.subject_id == "doc-workspace")
        .expect("test: a workspace ingest event stays readable without any key");
    assert_eq!(event.session_key, None);
    assert_eq!(event.agent_id, None);
    assert_eq!(event.persona_id, None);
}

/// V02-R6: a tool-ingest event carries no agent or persona, so its exact
/// owner is its author identity. The author can read the private event
/// through either event-list API, while another owner cannot.
#[tokio::test]
async fn private_visibility_ingest_event_is_readable_by_its_author() {
    let (tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    mem.ingest_document(c03_ingest_input(
        "doc-private",
        Some(&owner_a),
        Some(C03_SESSION_KEY),
        MemoryVisibility::Private,
    ))
    .await
    .expect("test: ingest private-visibility document");

    let db_path = tmp.path().join("memory").join("brain.db");
    let conn = Connection::open(&db_path).expect("test: open brain.db");
    let (session_key, agent_id, persona_id, owner_id): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT session_key, agent_id, persona_id, owner_id FROM memory_events
             WHERE subject_id = ?1 AND event_type = 'document.ingested'",
            params!["doc-private"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("test: read the stored ingest event");
    assert_eq!(session_key.as_deref(), Some(C03_SESSION_KEY));
    assert_eq!(agent_id, None);
    assert_eq!(persona_id, None);
    assert_eq!(owner_id.as_deref(), Some(owner_a.as_str()));
    drop(conn);

    for isolated in [false, true] {
        let principal = c03_principal(C03_WORKSPACE, Some(&owner_a), isolated);
        let recent = c03_ingest_subjects(&mem, &principal).await;
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

        let foreign = c03_ingest_subjects(&mem, &c03_principal(C03_WORKSPACE, Some(&owner_b), isolated)).await;
        assert!(
            foreign.is_empty(),
            "another owner cannot read the private ingest event (isolated={isolated}): {foreign:?}"
        );
    }
}

/// The `private` arm of the memory-event read requires the owner for every
/// principal, owner-isolated or not (OWNER-VIS OV-E; OQ-1, closed by
/// D-019). Until then a turn of another owner in the same workspace,
/// running under the same configured sub-agent name, read a `private`
/// ingest event on `agent_id` alone; this test used to pin that behaviour.
#[tokio::test]
async fn a_private_ingest_event_requires_the_owner_even_for_a_non_isolated_principal() {
    const SUB_AGENT: &str = "release-notes";

    let (_tmp, mem) = temp_sqlite();
    let owner_a = c03_owner("issuer-a");
    let owner_b = c03_owner("issuer-b");

    mem.ingest_document(DocumentIngestInput {
        agent_id: Some(SUB_AGENT.to_string()),
        ..c03_ingest_input(
            "doc-private-agent",
            Some(&owner_a),
            Some(C03_SESSION_KEY),
            MemoryVisibility::Private,
        )
    })
    .await
    .expect("test: ingest private-visibility document");

    // The owner, not owner-isolated, from another session under the same
    // agent id: the event is readable.
    let same_agent_same_owner = MemoryPrincipal {
        agent_id: Some(SUB_AGENT.to_string()),
        session_key: Some("chat:unrelated".to_string()),
        ..c03_principal(C03_WORKSPACE, Some(&owner_a), false)
    };
    let seen = c03_ingest_subjects(&mem, &same_agent_same_owner).await;
    assert_eq!(
        seen,
        vec!["doc-private-agent".to_string()],
        "the owner's own turn reads its private event: {seen:?}"
    );

    // A different owner under the same agent id, not owner-isolated: the
    // event is refused.
    let same_agent_other_owner = MemoryPrincipal {
        owner_id: Some(owner_b.clone()),
        ..same_agent_same_owner.clone()
    };
    let refused_to_other_owner = c03_ingest_subjects(&mem, &same_agent_other_owner).await;
    assert!(
        refused_to_other_owner.is_empty(),
        "another owner's turn must not read the private event on agent id alone: {refused_to_other_owner:?}"
    );

    // A different agent id reads nothing even for the owner, so the match
    // above really is the agent id and not the workspace.
    let other_agent = MemoryPrincipal {
        agent_id: Some("unrelated-agent".to_string()),
        ..same_agent_same_owner.clone()
    };
    let none_for_other_agent = c03_ingest_subjects(&mem, &other_agent).await;
    assert!(
        none_for_other_agent.is_empty(),
        "a different agent id must not admit the event: {none_for_other_agent:?}"
    );

    // The same principal, owner-isolated: the owner suffix applies and the
    // row is refused.
    let isolated = MemoryPrincipal {
        require_owner_match: true,
        ..same_agent_other_owner
    };
    let refused = c03_ingest_subjects(&mem, &isolated).await;
    assert!(
        refused.is_empty(),
        "an owner-isolated principal of a different owner reads nothing on the same arm: {refused:?}"
    );
}

/// A database created before schema v16 has no `memory_events.owner_id`.
/// `init_schema` must add it idempotently and the new read path must work on
/// the reopened file (mirrors the v7 run-lineage regression above).
#[test]
fn legacy_memory_events_gets_owner_id_column_on_init() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("legacy-owner.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE memory_events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id      TEXT NOT NULL UNIQUE,
            workspace_id  TEXT NOT NULL,
            event_type    TEXT NOT NULL,
            subject_table TEXT NOT NULL,
            subject_id    TEXT NOT NULL,
            session_key   TEXT,
            run_id        TEXT,
            parent_run_id TEXT,
            agent_id      TEXT,
            persona_id    TEXT,
            visibility    TEXT NOT NULL DEFAULT 'workspace',
            payload_json  TEXT,
            created_at    TEXT NOT NULL
        );
         INSERT INTO memory_events
             (event_id, workspace_id, event_type, subject_table, subject_id, visibility, created_at)
         VALUES ('evt-legacy', 'ws-c03', 'task.spawned', 'tasks', 'run-legacy', 'session', '2026-01-01T00:00:00Z');",
    )
    .unwrap();
    let before = legacy_table_columns(&conn, "memory_events");
    assert!(
        !before.contains("owner_id"),
        "precondition: the legacy table lacks owner_id"
    );

    SqliteMemory::init_schema(&conn).expect("init_schema upgrades a pre-v16 memory_events");
    let after = legacy_table_columns(&conn, "memory_events");
    assert!(after.contains("owner_id"), "owner_id column must be added");
    assert!(
        legacy_index_exists(&conn, "idx_memory_events_owner"),
        "idx_memory_events_owner must be created"
    );
    SqliteMemory::init_schema(&conn).expect("init_schema is idempotent on re-run");

    let unattributed: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_events WHERE owner_id IS NULL", [], |row| {
            row.get(0)
        })
        .expect("query owner_id must not fail with `no such column`");
    assert_eq!(unattributed, 1, "the preserved legacy row is unattributed");
}

/// Rollback: a binary from before schema v16 must still work against a
/// migrated database. It never issues `SELECT *` and never names `owner_id`,
/// so the added nullable column is invisible to it; and the migration ledger
/// row it does not know about is simply never inspected, because the runner
/// only walks its own registry.
#[test]
fn a_pre_v16_binary_still_reads_and_writes_a_migrated_memory_events_table() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("migrated.db");
    let conn = Connection::open(&db_path).unwrap();
    SqliteMemory::init_schema(&conn).expect("init_schema creates the v16 schema");
    SqliteMemory::run_memory_schema_migrations(&conn).expect("ledger records v16");

    // The pre-v16 write path: an explicit column list without owner_id.
    conn.execute(
        "INSERT INTO memory_events (
            event_id, workspace_id, event_type, subject_table, subject_id,
            session_key, agent_id, persona_id, visibility, payload_json, created_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            "evt-old-binary",
            "ws-c03",
            "task.spawned",
            "tasks",
            "run-old",
            "mcp:shared-subject",
            None::<String>,
            None::<String>,
            "session",
            None::<String>,
            "2026-01-01T00:00:00Z",
        ],
    )
    .expect("a pre-v16 insert must still be accepted");

    // The pre-v16 read path: the old explicit column list.
    let subject: String = conn
        .query_row(
            "SELECT subject_id FROM memory_events WHERE event_id = ?1",
            params!["evt-old-binary"],
            |row| row.get(0),
        )
        .expect("a pre-v16 select must still work");
    assert_eq!(subject, "run-old");

    // The ledger carries a version the old registry does not know about, and
    // the old registry's own entries still verify.
    let recorded: i64 = conn
        .query_row("SELECT MAX(version) FROM memory_schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("ledger is readable");
    assert!(recorded >= 16, "the new version is recorded: {recorded}");
    SqliteMemory::run_memory_schema_migrations(&conn).expect("re-running the ledger is a no-op");
}

/// Regression guard for the deployment bug: a legacy `memory_events` table
/// created before schema v7 lacked `run_id` / `parent_run_id`, and the
/// checksum-anchor registry alone never altered it — so the gateway hit
/// `no such column: run_id` at runtime. `init_schema` must idempotently add
/// the columns and their lineage indexes to such a legacy table. The same
/// applies to a pre-`source_event_id` `memory_drafts` table.
#[test]
fn legacy_memory_events_gets_run_lineage_columns_on_init() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("legacy.db");
    let conn = Connection::open(&db_path).unwrap();

    // Simulate the pre-v7 `memory_events` table: no run_id / parent_run_id,
    // and therefore none of the lineage indexes.
    conn.execute_batch(
        "CREATE TABLE memory_events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id      TEXT NOT NULL UNIQUE,
            workspace_id  TEXT NOT NULL,
            event_type    TEXT NOT NULL,
            subject_table TEXT NOT NULL,
            subject_id    TEXT NOT NULL,
            session_key   TEXT,
            agent_id      TEXT,
            persona_id    TEXT,
            visibility    TEXT NOT NULL DEFAULT 'workspace',
            payload_json  TEXT,
            created_at    TEXT NOT NULL
        );",
    )
    .unwrap();

    // Simulate a very old `memory_drafts` lacking `source_event_id` (and the
    // other v-bumped columns) to prove the inline index no longer trips.
    conn.execute_batch(
        "CREATE TABLE memory_drafts (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            draft_id      TEXT NOT NULL UNIQUE,
            workspace_id  TEXT NOT NULL,
            worker_run_id TEXT NOT NULL,
            session_key   TEXT,
            key           TEXT NOT NULL,
            content       TEXT NOT NULL,
            category      TEXT NOT NULL,
            status        TEXT NOT NULL DEFAULT 'pending',
            created_at    TEXT NOT NULL,
            updated_at    TEXT NOT NULL
        );",
    )
    .unwrap();

    let before = legacy_table_columns(&conn, "memory_events");
    assert!(!before.contains("run_id"), "precondition: legacy table lacks run_id");
    assert!(
        !before.contains("parent_run_id"),
        "precondition: legacy table lacks parent_run_id"
    );

    // First init must not error (the original bug aborted here) and must add
    // the missing columns + lineage indexes.
    SqliteMemory::init_schema(&conn).expect("init_schema upgrades legacy memory_events");

    let after = legacy_table_columns(&conn, "memory_events");
    assert!(after.contains("run_id"), "run_id column must be added");
    assert!(after.contains("parent_run_id"), "parent_run_id column must be added");
    assert!(
        legacy_index_exists(&conn, "idx_memory_events_run"),
        "idx_memory_events_run must be created"
    );
    assert!(
        legacy_index_exists(&conn, "idx_memory_events_parent_run"),
        "idx_memory_events_parent_run must be created"
    );

    let drafts = legacy_table_columns(&conn, "memory_drafts");
    assert!(
        drafts.contains("source_event_id"),
        "memory_drafts.source_event_id must be backfilled"
    );
    assert!(
        legacy_index_exists(&conn, "idx_memory_drafts_source_event"),
        "idx_memory_drafts_source_event must be created after the ALTER"
    );

    // Idempotency: re-running init_schema on the now-upgraded DB must not error
    // (duplicate-column / existing-index paths are all handled).
    SqliteMemory::init_schema(&conn).expect("init_schema is idempotent on re-run");

    // And a query that selects the previously-missing column must now succeed,
    // mirroring the gateway path that originally failed.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_events WHERE run_id IS NULL AND parent_run_id IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("query run_id must not fail with `no such column`");
    assert_eq!(count, 0, "empty legacy table has no rows");
}

#[tokio::test]
async fn g1_draft_acl_list_only_returns_own_or_ownerless() {
    let (_tmp, mem) = temp_sqlite();
    mem.create_memory_draft(g1_draft_input(Some("alice"), "run-1", "d-alice", "k1"))
        .await
        .expect("create alice");
    mem.create_memory_draft(g1_draft_input(Some("bob"), "run-1", "d-bob", "k2"))
        .await
        .expect("create bob");
    mem.create_memory_draft(g1_draft_input(None, "run-1", "d-none", "k3"))
        .await
        .expect("create ownerless");

    let alice = g1_owner_principal("alice");
    let visible = mem.list_memory_drafts_for_run(&alice, "run-1").await.expect("list");
    let ids: Vec<&str> = visible.iter().map(|d| d.draft_id.as_str()).collect();
    assert!(ids.contains(&"d-alice"), "alice sees own draft");
    assert!(ids.contains(&"d-none"), "alice sees ownerless draft");
    assert!(!ids.contains(&"d-bob"), "alice must NOT see bob's draft");
}

#[tokio::test]
async fn g1_draft_acl_merge_and_reject_block_other_owner() {
    let (_tmp, mem) = temp_sqlite();
    mem.create_memory_draft(g1_draft_input(Some("bob"), "run-x", "d-bob", "k"))
        .await
        .expect("create bob");

    let alice = g1_owner_principal("alice");
    assert!(
        mem.merge_memory_draft(&alice, "d-bob").await.is_err(),
        "alice must not merge bob's draft"
    );
    assert!(
        mem.reject_memory_draft(&alice, "d-bob", Some("nope")).await.is_err(),
        "alice must not reject bob's draft"
    );

    let bob = g1_owner_principal("bob");
    let merged = mem.merge_memory_draft(&bob, "d-bob").await.expect("bob merge ok");
    assert!(merged.is_some(), "owner merge returns the draft");
}

// FIX-P0-03 (#9): EvolutionProposalDraft CRUD round-trip through SqliteMemory.
#[tokio::test]
async fn evolution_proposal_crud_round_trips_through_sqlite() {
    use crate::self_system::evolution::config::EvolutionMode;
    use crate::self_system::evolution::proposal::{
        EvolutionProposalDraft, EvolutionTargetResource, JudgeVerdict, ProposalFilter, ProposalStatusUpdate,
        ProposedChange, RiskLevel, RollbackAnchor,
    };

    let (_tmp, mem) = temp_sqlite();
    let system_principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "/ws".to_string(),
        agent_id: Some("self_system".to_string()),
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: Some("self_system".to_string()),
        legacy_session_key: None,
    };

    let draft = EvolutionProposalDraft {
        draft_id: "evo-crud-1".to_string(),
        owner_id: "self_system".to_string(),
        principal_id: "xin:scheduler".to_string(),
        workspace_id: "/ws".to_string(),
        topic_id: None,
        task_id: Some("run-42".to_string()),
        source_message_event_ids: vec![1, 2],
        source_memory_event_ids: vec![3],
        evidence_hashes: vec!["hash-1".to_string()],
        target_resource: EvolutionTargetResource::SemanticMemory {
            memory_id: "conversation:dup".to_string(),
            scope: "workspace".to_string(),
        },
        proposed_change: ProposedChange::MemoryForget {
            reason: "redundant".to_string(),
        },
        risk_level: RiskLevel::Low,
        mode: EvolutionMode::Auto,
        created_at: Utc::now(),
        created_by_runtime: "self_system:l1".to_string(),
        judge_verdict: None,
        applied_at: None,
        applied_by: None,
        rollback_anchor: None,
    };

    let id = mem
        .create_evolution_proposal(draft.clone())
        .await
        .expect("create proposal");
    assert_eq!(id, "evo-crud-1");

    let fetched = mem
        .get_evolution_proposal(&system_principal, "evo-crud-1")
        .await
        .expect("get proposal")
        .expect("proposal exists");
    assert_eq!(fetched.draft_id, draft.draft_id);
    assert_eq!(fetched.task_id, draft.task_id);
    assert_eq!(fetched.source_message_event_ids, vec![1, 2]);
    assert_eq!(fetched.evidence_hashes, vec!["hash-1".to_string()]);
    assert_eq!(fetched.proposed_change, draft.proposed_change);
    assert!(!fetched.is_judged());
    assert!(!fetched.is_applied());

    let listed = mem
        .list_evolution_proposals(
            &system_principal,
            ProposalFilter {
                task_id: Some("run-42".to_string()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("list proposals");
    assert_eq!(listed.len(), 1);

    let other_principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "/ws".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("s".to_string()),
        channel: Some("telegram".to_string()),
        sender: Some("intruder".to_string()),
        owner_id: Some("owner:intruder".to_string()),
        legacy_session_key: None,
    };
    let hidden = mem
        .get_evolution_proposal(&other_principal, "evo-crud-1")
        .await
        .expect("get does not error cross-owner");
    assert!(hidden.is_none(), "cross-owner get must return NotFound");

    mem.update_evolution_proposal_status(
        &system_principal,
        "evo-crud-1",
        ProposalStatusUpdate::Judged {
            verdict: JudgeVerdict::Approved {
                judge_id: "mock".to_string(),
                confidence: 0.7,
                reasoning: "ok".to_string(),
            },
        },
    )
    .await
    .expect("judge proposal");

    let rejudge = mem
        .update_evolution_proposal_status(
            &system_principal,
            "evo-crud-1",
            ProposalStatusUpdate::Judged {
                verdict: JudgeVerdict::Rejected {
                    judge_id: "mock".to_string(),
                    reasoning: "no".to_string(),
                },
            },
        )
        .await;
    assert!(rejudge.is_err(), "re-judging an already-judged proposal must fail");

    mem.update_evolution_proposal_status(
        &system_principal,
        "evo-crud-1",
        ProposalStatusUpdate::Applied {
            applied_by: "grant-abc".to_string(),
            rollback_anchor: RollbackAnchor::MemorySnapshot {
                snapshot_id: "snap-1".to_string(),
            },
        },
    )
    .await
    .expect("apply proposal");

    let applied = mem
        .get_evolution_proposal(&system_principal, "evo-crud-1")
        .await
        .expect("get applied")
        .expect("exists");
    assert!(applied.is_applied());
    assert!(applied.is_judged());
    assert!(applied.rollback_anchor.is_some());

    mem.update_evolution_proposal_status(&system_principal, "evo-crud-1", ProposalStatusUpdate::RolledBack)
        .await
        .expect("rollback proposal");
    let rolled = mem
        .get_evolution_proposal(&system_principal, "evo-crud-1")
        .await
        .expect("get rolled")
        .expect("exists");
    assert!(!rolled.is_applied(), "rollback clears applied_at");
}

// FIX-P1-11 (#12): move_to_trash soft-deletes (does NOT physically remove the
// row) and is idempotent within the grace window.
#[tokio::test]
async fn move_to_trash_soft_deletes_and_is_idempotent() {
    let (_tmp, mem) = temp_sqlite();
    mem.store("redundant_key", "duplicate content", MemoryCategory::Conversation, None)
        .await
        .expect("store memory");
    assert!(mem.get("redundant_key").await.expect("get").is_some());

    let trash_id = mem
        .move_to_trash("redundant_key", "redundant conversation memory", 14)
        .await
        .expect("move_to_trash")
        .expect("key was present");
    assert!(trash_id.starts_with("trash-"));

    assert!(
        mem.get("redundant_key").await.expect("get after trash").is_some(),
        "move_to_trash must not physically delete the memory row"
    );

    let (count, grace_in_future): (i64, bool) = {
        let conn = mem.pool.write();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_trash WHERE memory_key = ?1 AND restored_at IS NULL",
                params!["redundant_key"],
                |row| row.get(0),
            )
            .expect("count trash");
        let grace_raw: String = conn
            .query_row(
                "SELECT grace_until FROM memory_trash WHERE trash_id = ?1",
                params![trash_id],
                |row| row.get(0),
            )
            .expect("grace_until");
        let grace = DateTime::parse_from_rfc3339(&grace_raw).expect("parse grace");
        (count, grace.with_timezone(&Utc) > Utc::now())
    };
    assert_eq!(count, 1, "exactly one unrestored trash entry");
    assert!(grace_in_future, "grace window must be in the future");

    let again = mem
        .move_to_trash("redundant_key", "redundant conversation memory", 14)
        .await
        .expect("second move_to_trash")
        .expect("still present");
    assert_eq!(again, trash_id, "repeated trash returns the existing entry");

    let missing = mem
        .move_to_trash("does_not_exist", "noop", 14)
        .await
        .expect("trash missing key ok");
    assert!(missing.is_none());
}

// FIX-P1-16 (#60): memory_events run lineage — a child task event records
// parent_run_id and is queryable by it.
#[tokio::test]
async fn memory_event_records_and_queries_parent_run_id() {
    let (_tmp, mem) = temp_sqlite();
    mem.append_memory_event(MemoryEventInput {
        owner_id: None,
        event_id: None,
        workspace_id: "/ws".to_string(),
        event_type: "task.spawned".to_string(),
        subject_table: "tasks".to_string(),
        subject_id: "child-run-1".to_string(),
        session_key: Some("sess".to_string()),
        run_id: Some("child-run-1".to_string()),
        parent_run_id: Some("parent-run-1".to_string()),
        agent_id: Some("system".to_string()),
        persona_id: None,
        visibility: MemoryVisibility::Workspace,
        payload_json: None,
    })
    .await
    .expect("append child event");

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "/ws".to_string(),
        agent_id: Some("system".to_string()),
        persona_id: None,
        session_key: None,
        channel: None,
        sender: None,
        owner_id: None,
        legacy_session_key: None,
    };
    let events = mem
        .list_memory_events_since(&principal, 0, 50)
        .await
        .expect("list events");
    let child = events
        .iter()
        .find(|event| event.subject_id == "child-run-1")
        .expect("child event present");
    assert_eq!(child.run_id.as_deref(), Some("child-run-1"));
    assert_eq!(
        child.parent_run_id.as_deref(),
        Some("parent-run-1"),
        "parent_run_id must survive the round-trip so children are queryable by parent"
    );

    let by_parent: i64 = {
        let conn = mem.pool.write();
        conn.query_row(
            "SELECT COUNT(*) FROM memory_events WHERE parent_run_id = ?1",
            params!["parent-run-1"],
            |row| row.get(0),
        )
        .expect("count by parent_run_id")
    };
    assert_eq!(by_parent, 1, "exactly one child queryable by parent_run_id");
}
