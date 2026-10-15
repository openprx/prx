#![cfg(test)]
use super::*;

#[tokio::test]
async fn append_message_event_inserts_event_and_outbox_row() {
    let (_tmp, mem) = temp_sqlite();
    let mut input = message_input(
        "workspace-a",
        "hello from terminal",
        MemoryVisibility::Workspace,
        None,
        Some("chat:1"),
        Some("alice"),
    );
    input.config_generation_id = Some(42);
    input.config_source_revision = Some("sha256:test-revision".to_string());

    let event = mem.append_message_event(input).await.unwrap();

    assert!(event.id > 0);
    assert_eq!(event.workspace_id, "workspace-a");
    assert_eq!(event.visibility, MemoryVisibility::Workspace);
    assert!(event.content_hash.is_some());
    assert_eq!(event.config_generation_id, Some(42));
    assert_eq!(event.config_source_revision.as_deref(), Some("sha256:test-revision"));

    let outbox_count: i64 = mem
        .pool
        .write()
        .query_row(
            "SELECT COUNT(*) FROM memory_events WHERE event_type = 'message.created' AND subject_id = ?1",
            [event.event_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outbox_count, 1);
}

#[tokio::test]
async fn append_message_event_sets_runtime_event_type_column() {
    let (_tmp, mem) = temp_sqlite();
    let mut input = message_input(
        "workspace-a",
        "decision_id=decision-1",
        MemoryVisibility::Workspace,
        Some("llm-router"),
        Some("chat:1"),
        Some("router"),
    );
    input.role = "event".to_string();
    input.event_type = "router.route_decision".to_string();
    input.subject = Some(crate::memory::MessageEventSubject::Task("task-1".to_string()));
    input.goal_id = Some("goal-1".to_string());
    input.causation_event_id = Some("event-parent".to_string());
    input.correlation_id = Some("correlation-1".to_string());
    input.attempt_id = Some("attempt-2".to_string());
    input.lease_epoch = Some(3);

    let event = mem.append_message_event(input).await.unwrap();
    assert_eq!(event.event_type, "router.route_decision");
    assert_eq!(event.source, "test");
    assert_eq!(
        event.subject,
        Some(crate::memory::MessageEventSubject::Task("task-1".to_string()))
    );
    assert_eq!(event.goal_id.as_deref(), Some("goal-1"));
    assert_eq!(event.causation_event_id.as_deref(), Some("event-parent"));
    assert_eq!(event.correlation_id.as_deref(), Some("correlation-1"));
    assert_eq!(event.attempt_id.as_deref(), Some("attempt-2"));
    assert_eq!(event.lease_epoch, Some(3));
    let conn = mem.pool.write();
    let event_type: String = conn
        .query_row(
            "SELECT event_type FROM message_events WHERE event_id = ?1",
            [event.event_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_type, "router.route_decision");
    let outbox_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_events WHERE event_type = 'router.route_decision'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outbox_count, 1);
}

#[tokio::test]
async fn append_message_event_rejects_missing_explicit_event_type_before_transaction() {
    let (_tmp, mem) = temp_sqlite();
    let mut input = message_input(
        "workspace-a",
        "content cannot define its own type",
        MemoryVisibility::Workspace,
        None,
        Some("chat:1"),
        Some("alice"),
    );
    input.event_type.clear();

    let error = mem.append_message_event(input).await.unwrap_err();
    assert!(error.to_string().contains("event type must not be empty"));
    let count: i64 = mem
        .pool
        .write()
        .query_row("SELECT COUNT(*) FROM message_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn append_message_event_is_idempotent_by_key() {
    let (_tmp, mem) = temp_sqlite();
    let mut first = message_input(
        "workspace-a",
        "first content",
        MemoryVisibility::Workspace,
        None,
        Some("chat:1"),
        Some("alice"),
    );
    first.idempotency_key = Some("telegram:message:42".to_string());

    let mut second = message_input(
        "workspace-a",
        "second content should not overwrite",
        MemoryVisibility::Workspace,
        None,
        Some("chat:1"),
        Some("alice"),
    );
    second.idempotency_key = Some("telegram:message:42".to_string());

    let first_event = mem.append_message_event(first).await.unwrap();
    let second_event = mem.append_message_event(second).await.unwrap();

    assert_eq!(first_event.id, second_event.id);
    assert_eq!(second_event.content, "first content");

    let conn = mem.pool.write();
    let message_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM message_events", [], |row| row.get(0))
        .unwrap();
    let outbox_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(message_count, 1);
    assert_eq!(outbox_count, 1);
}

#[tokio::test]
async fn append_message_event_idempotency_key_is_scoped_to_workspace() {
    let (_tmp, mem) = temp_sqlite();
    let mut workspace_a = message_input(
        "workspace-a",
        "workspace a content",
        MemoryVisibility::Workspace,
        None,
        Some("chat:a"),
        Some("alice"),
    );
    workspace_a.idempotency_key = Some("shared-provider-key".to_string());

    let mut workspace_b = message_input(
        "workspace-b",
        "workspace b content",
        MemoryVisibility::Workspace,
        None,
        Some("chat:b"),
        Some("bob"),
    );
    workspace_b.idempotency_key = Some("shared-provider-key".to_string());

    let first_a = mem.append_message_event(workspace_a.clone()).await.unwrap();
    let first_b = mem.append_message_event(workspace_b).await.unwrap();
    let replay_a = mem.append_message_event(workspace_a).await.unwrap();

    assert_ne!(first_a.event_id, first_b.event_id);
    assert_eq!(first_a.workspace_id, "workspace-a");
    assert_eq!(first_b.workspace_id, "workspace-b");
    assert_eq!(replay_a.event_id, first_a.event_id);
    assert_eq!(replay_a.content, "workspace a content");

    let conn = mem.pool.write();
    let message_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM message_events", [], |row| row.get(0))
        .unwrap();
    let outbox_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(message_count, 2);
    assert_eq!(outbox_count, 2);
}

#[tokio::test]
async fn append_worker_result_message_event_emits_worker_outbox_type() {
    let (_tmp, mem) = temp_sqlite();
    let mut input = message_input(
        "workspace-a",
        "worker result",
        MemoryVisibility::Workspace,
        Some("agent-a"),
        Some("session-a"),
        None,
    );
    input.source = "sessions_spawn".into();
    input.role = "event".to_string();
    input.event_type = "worker.result.created".to_string();

    let event = mem.append_message_event(input).await.unwrap();
    let outbox = mem
        .list_memory_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace-a".to_string(),
                agent_id: Some("agent-a".to_string()),
                persona_id: None,
                session_key: Some("session-a".to_string()),
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

    assert_eq!(event.role, "event");
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].event_type, "worker.result.created");
    assert_eq!(outbox[0].subject_id, event.event_id);
}

#[tokio::test]
async fn list_message_events_since_applies_visibility_policy() {
    let (_tmp, mem) = temp_sqlite();
    for input in [
        message_input(
            "workspace-a",
            "workspace visible",
            MemoryVisibility::Workspace,
            None,
            None,
            None,
        ),
        message_input(
            "workspace-a",
            "agent visible",
            MemoryVisibility::Agent,
            Some("agent-a"),
            None,
            None,
        ),
        message_input(
            "workspace-a",
            "session visible",
            MemoryVisibility::Session,
            None,
            Some("session-a"),
            None,
        ),
        // OWNER-VIS OV-E: a `private` row is readable only by its owner.
        MessageEventInput {
            owner_id: Some("owner:workspace-a:terminal:alice".to_string()),
            ..message_input(
                "workspace-a",
                "private visible",
                MemoryVisibility::Private,
                None,
                None,
                Some("alice"),
            )
        },
        message_input(
            "workspace-a",
            "system hidden",
            MemoryVisibility::System,
            None,
            None,
            None,
        ),
        message_input(
            "workspace-b",
            "other workspace",
            MemoryVisibility::Workspace,
            None,
            None,
            None,
        ),
    ] {
        mem.append_message_event(input).await.unwrap();
    }

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: Some("agent-a".to_string()),
        persona_id: None,
        session_key: Some("session-a".to_string()),
        channel: None,
        sender: Some("alice".to_string()),
        owner_id: Some("owner:workspace-a:terminal:alice".to_string()),
        legacy_session_key: None,
    };
    let visible = mem.list_message_events_since(&principal, 0, 20).await.unwrap();
    let contents = visible.iter().map(|event| event.content.as_str()).collect::<Vec<_>>();

    assert_eq!(
        contents,
        vec![
            "workspace visible",
            "agent visible",
            "session visible",
            "private visible"
        ]
    );
}

#[tokio::test]
async fn global_message_events_are_visible_across_workspaces() {
    let (_tmp, mem) = temp_sqlite();
    mem.append_message_event(message_input(
        "workspace-a",
        "global visible everywhere",
        MemoryVisibility::Global,
        None,
        None,
        None,
    ))
    .await
    .unwrap();
    mem.append_message_event(message_input(
        "workspace-a",
        "workspace local only",
        MemoryVisibility::Workspace,
        None,
        None,
        None,
    ))
    .await
    .unwrap();

    let visible = mem
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace-b".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            20,
        )
        .await
        .unwrap();
    let contents = visible.iter().map(|event| event.content.as_str()).collect::<Vec<_>>();

    assert_eq!(contents, vec!["global visible everywhere"]);
}

#[tokio::test]
async fn time_range_system_query_includes_configured_worker_descendants_only() {
    let (_tmp, mem) = temp_sqlite();
    for (workspace, content) in [
        ("/workspace", "root event"),
        ("/workspace/workers/run-1", "worker event"),
        ("/workspace/workers-old/run-2", "prefix sibling hidden"),
        ("/other/workers/run-3", "other workspace hidden"),
    ] {
        mem.append_message_event(message_input(
            workspace,
            content,
            MemoryVisibility::Workspace,
            None,
            None,
            None,
        ))
        .await
        .unwrap();
    }
    let mut private_root = message_input(
        "/workspace",
        "private root event",
        MemoryVisibility::Private,
        Some("other-agent"),
        None,
        None,
    );
    private_root.sender = Some("other-sender".to_string());
    mem.append_message_event(private_root).await.unwrap();
    mem.append_message_event(message_input(
        "/workspace/workers/run-1",
        "private worker event",
        MemoryVisibility::Private,
        Some("other-agent"),
        None,
        None,
    ))
    .await
    .unwrap();
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

    assert_eq!(
        contents,
        vec![
            "root event",
            "worker event",
            "private root event",
            "private worker event"
        ]
    );

    let non_system = MemoryPrincipal {
        agent_id: Some("regular-agent".to_string()),
        ..principal
    };
    assert!(
        mem.list_message_events_time_range(&non_system, Some("/workspace/workers"), &start, &end, 20,)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn selected_session_read_is_scoped_and_stops_before_source_event() {
    let (_tmp, mem) = temp_sqlite();
    let mut ids = Vec::new();
    for (text, owner, session, role) in [
        ("earlier question", "prx_owner", "prx_session", "user"),
        ("earlier answer", "prx_owner", "prx_session", "assistant"),
        ("foreign owner", "prx_other", "prx_session", "user"),
        ("foreign session", "prx_owner", "prx_other_session", "user"),
        ("current source", "prx_owner", "prx_session", "user"),
        ("later event", "prx_owner", "prx_session", "assistant"),
    ] {
        let mut input = message_input(
            "prx_workspace",
            text,
            MemoryVisibility::Session,
            None,
            Some(session),
            Some("prx_user"),
        );
        input.owner_id = Some(owner.to_owned());
        input.role = role.to_owned();
        ids.push(mem.append_message_event(input).await.unwrap().event_id);
    }
    let mut principal = isolated_document_principal("prx_workspace", Some("prx_owner"));
    principal.session_key = Some("prx_session".to_owned());
    let deadline = || std::time::Instant::now() + std::time::Duration::from_secs(5);
    let hits = mem
        .retrieve_decision_session(&principal, &ids[4], 5, deadline())
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.content.as_str()).collect::<Vec<_>>(),
        ["earlier question", "earlier answer"]
    );
    assert_eq!(hits[1].role, "assistant");
    assert!(!format!("{:?}", hits[0]).contains("earlier"));
    assert_eq!(
        mem.retrieve_decision_session(&principal, &ids[4], 1, deadline())
            .await
            .unwrap()[0]
            .content,
        "earlier answer"
    );
    for source in [&ids[2], &ids[3]] {
        assert!(
            mem.retrieve_decision_session(&principal, source, 5, deadline())
                .await
                .is_err()
        );
    }
    assert!(
        mem.retrieve_decision_session(&principal, &ids[4], 0, deadline())
            .await
            .is_err()
    );
    assert!(
        mem.retrieve_decision_session(&principal, &ids[4], 6, deadline())
            .await
            .is_err()
    );
    assert!(
        mem.retrieve_decision_session(&principal, &ids[4], 5, std::time::Instant::now())
            .await
            .is_err()
    );
    principal.owner_id = None;
    assert!(
        mem.retrieve_decision_session(&principal, &ids[4], 5, deadline())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn load_recent_session_context_is_not_evicted_by_external_events() {
    let (_tmp, mem) = temp_sqlite();
    mem.append_message_event(message_input(
        "workspace-a",
        "current session survives",
        MemoryVisibility::Workspace,
        None,
        Some("chat:current"),
        None,
    ))
    .await
    .unwrap();
    for index in 0..40 {
        mem.append_message_event(message_input(
            "workspace-a",
            &format!("external event {index}"),
            MemoryVisibility::Workspace,
            None,
            Some("gateway:external"),
            None,
        ))
        .await
        .unwrap();
    }

    let events = mem
        .load_recent_session_context(SessionContextQuery {
            principal: MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace-a".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some("chat:current".to_string()),
                channel: Some("terminal".to_string()),
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            since_event_id: None,
            limit: 10,
            include_roles: vec!["user".to_string()],
        })
        .await
        .unwrap();

    assert_eq!(
        events.iter().map(|event| event.content.as_str()).collect::<Vec<_>>(),
        vec!["current session survives"]
    );
}

// ---- D4 read-merge (legacy_session_key) ----

#[tokio::test]
async fn d4_load_recent_session_context_read_merges_legacy_key() {
    let (_tmp, mem) = temp_sqlite();
    // Pre-cutover history under the legacy key, plus new history under the
    // canonical key.
    mem.append_message_event(message_input(
        "workspace-a",
        "legacy history",
        MemoryVisibility::Session,
        None,
        Some("chat:legacy-1"),
        None,
    ))
    .await
    .unwrap();
    mem.append_message_event(message_input(
        "workspace-a",
        "canonical history",
        MemoryVisibility::Session,
        None,
        Some("chat:terminal:local-user:room-1"),
        None,
    ))
    .await
    .unwrap();

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("chat:terminal:local-user:room-1".to_string()),
        channel: Some("terminal".to_string()),
        sender: None,
        owner_id: None,
        legacy_session_key: Some("chat:legacy-1".to_string()),
    };
    let merged = mem
        .load_recent_session_context(SessionContextQuery {
            principal: principal.clone(),
            since_event_id: None,
            limit: 10,
            include_roles: vec!["user".to_string()],
        })
        .await
        .unwrap();
    let contents: Vec<&str> = merged.iter().map(|event| event.content.as_str()).collect();
    assert!(
        contents.contains(&"legacy history"),
        "legacy history must be read-merged: {contents:?}"
    );
    assert!(
        contents.contains(&"canonical history"),
        "canonical history must be present: {contents:?}"
    );

    // Single-key degradation (legacy=None) must NOT see the legacy row.
    let single = mem
        .load_recent_session_context(SessionContextQuery {
            principal: MemoryPrincipal {
                legacy_session_key: None,
                ..principal
            },
            since_event_id: None,
            limit: 10,
            include_roles: vec!["user".to_string()],
        })
        .await
        .unwrap();
    let single_contents: Vec<&str> = single.iter().map(|event| event.content.as_str()).collect();
    assert_eq!(single_contents, vec!["canonical history"]);
}

#[tokio::test]
async fn d4_list_message_events_recent_read_merges_legacy_key() {
    let (_tmp, mem) = temp_sqlite();
    mem.append_message_event(message_input(
        "workspace-a",
        "legacy event",
        MemoryVisibility::Session,
        None,
        Some("gateway:webchat:alice"),
        None,
    ))
    .await
    .unwrap();
    mem.append_message_event(message_input(
        "workspace-a",
        "canonical event",
        MemoryVisibility::Session,
        None,
        Some("gateway:webchat:alice:agent-bot"),
        None,
    ))
    .await
    .unwrap();

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: None,
        persona_id: None,
        session_key: Some("gateway:webchat:alice:agent-bot".to_string()),
        channel: Some("webchat".to_string()),
        sender: None,
        owner_id: None,
        legacy_session_key: Some("gateway:webchat:alice".to_string()),
    };
    let merged = mem.list_message_events_recent(&principal, 50).await.unwrap();
    let contents: Vec<&str> = merged.iter().map(|event| event.content.as_str()).collect();
    assert!(
        contents.contains(&"legacy event"),
        "legacy event must be read-merged: {contents:?}"
    );
    assert!(
        contents.contains(&"canonical event"),
        "canonical event must be present: {contents:?}"
    );
}

#[tokio::test]
async fn d4_list_conversation_turns_read_merges_legacy_key() {
    let (_tmp, mem) = temp_sqlite();
    mem.append_conversation_turn(
        "chat:legacy-2",
        "terminal",
        "tester",
        "user",
        "legacy turn",
        Some("2026-03-05T00:00:00Z"),
        None,
        None,
    )
    .await
    .unwrap();
    mem.append_conversation_turn(
        "chat:terminal:local-user:room-2",
        "terminal",
        "tester",
        "user",
        "canonical turn",
        Some("2026-03-05T00:00:01Z"),
        None,
        None,
    )
    .await
    .unwrap();

    let mut principal = test_conversation_principal("chat:terminal:local-user:room-2", None);
    principal.legacy_session_key = Some("chat:legacy-2".to_string());
    let merged = mem
        .list_conversation_turns(&principal, "chat:terminal:local-user:room-2", 50, 0)
        .await
        .unwrap();
    let contents: Vec<&str> = merged.iter().map(|turn| turn.content.as_str()).collect();
    assert!(
        contents.contains(&"legacy turn"),
        "legacy turn must be read-merged: {contents:?}"
    );
    assert!(
        contents.contains(&"canonical turn"),
        "canonical turn must be present: {contents:?}"
    );

    // Single-key degradation: without a legacy key, only the canonical turn.
    let single_principal = test_conversation_principal("chat:terminal:local-user:room-2", None);
    let single = mem
        .list_conversation_turns(&single_principal, "chat:terminal:local-user:room-2", 50, 0)
        .await
        .unwrap();
    let single_contents: Vec<&str> = single.iter().map(|turn| turn.content.as_str()).collect();
    assert_eq!(single_contents, vec!["canonical turn"]);
}

#[tokio::test]
async fn d4_session_key_candidates_dedupes_and_degrades() {
    // None canonical → empty.
    let none = MemoryPrincipal {
        session_key: None,
        ..Default::default()
    };
    assert!(none.session_key_candidates().is_empty(), "expected an empty value");
    // Distinct legacy → two keys, canonical first.
    let two = MemoryPrincipal {
        session_key: Some("canonical".to_string()),
        legacy_session_key: Some("legacy".to_string()),
        ..Default::default()
    };
    assert_eq!(
        two.session_key_candidates(),
        vec!["canonical".to_string(), "legacy".to_string()]
    );
    // Legacy equal to canonical → dedup to single key.
    let dup = MemoryPrincipal {
        session_key: Some("same".to_string()),
        legacy_session_key: Some("same".to_string()),
        ..Default::default()
    };
    assert_eq!(dup.session_key_candidates(), vec!["same".to_string()]);
    // Legacy None → single key (legacy single-key behaviour).
    let one = MemoryPrincipal {
        session_key: Some("only".to_string()),
        ..Default::default()
    };
    assert_eq!(one.session_key_candidates(), vec!["only".to_string()]);
}

#[tokio::test]
async fn list_memory_events_since_applies_cursor_and_visibility_policy() {
    let (_tmp, mem) = temp_sqlite();
    let hidden = mem
        .append_memory_event(memory_event_input(
            "workspace-a",
            "hidden.before_cursor",
            MemoryVisibility::Workspace,
            None,
            None,
        ))
        .await
        .unwrap();
    for input in [
        memory_event_input(
            "workspace-a",
            "workspace.visible",
            MemoryVisibility::Workspace,
            None,
            None,
        ),
        memory_event_input(
            "workspace-a",
            "agent.visible",
            MemoryVisibility::Agent,
            Some("agent-a"),
            None,
        ),
        memory_event_input(
            "workspace-a",
            "session.visible",
            MemoryVisibility::Session,
            None,
            Some("session-a"),
        ),
        memory_event_input("workspace-a", "system.hidden", MemoryVisibility::System, None, None),
        memory_event_input(
            "workspace-b",
            "other_workspace.hidden",
            MemoryVisibility::Workspace,
            None,
            None,
        ),
    ] {
        mem.append_memory_event(input).await.unwrap();
    }

    let principal = MemoryPrincipal {
        require_owner_match: false,
        workspace_id: "workspace-a".to_string(),
        agent_id: Some("agent-a".to_string()),
        persona_id: None,
        session_key: Some("session-a".to_string()),
        channel: None,
        sender: None,
        owner_id: None,
        legacy_session_key: None,
    };
    let visible = mem.list_memory_events_since(&principal, hidden.id, 20).await.unwrap();
    let event_types = visible
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        event_types,
        vec!["workspace.visible", "agent.visible", "session.visible"]
    );
}

#[tokio::test]
async fn global_memory_events_are_visible_across_workspaces() {
    let (_tmp, mem) = temp_sqlite();
    mem.append_memory_event(memory_event_input(
        "workspace-a",
        "global.event",
        MemoryVisibility::Global,
        None,
        None,
    ))
    .await
    .unwrap();
    mem.append_memory_event(memory_event_input(
        "workspace-a",
        "workspace.event",
        MemoryVisibility::Workspace,
        None,
        None,
    ))
    .await
    .unwrap();

    let visible = mem
        .list_memory_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace-b".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            20,
        )
        .await
        .unwrap();

    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].event_type, "global.event");
}

#[tokio::test]
async fn load_recent_shared_context_filters_roles_and_orders_chronologically() {
    let (_tmp, mem) = temp_sqlite();
    // OWNER-VIS OV-D: the feed carries the reader's own owner's events, so
    // every row here belongs to the reading owner.
    let owned = |content: &str| MessageEventInput {
        owner_id: Some("owner-a".to_string()),
        ..message_input("workspace-a", content, MemoryVisibility::Workspace, None, None, None)
    };
    mem.append_message_event(owned("first user")).await.unwrap();
    let mut assistant = message_input(
        "workspace-a",
        "assistant reply",
        MemoryVisibility::Workspace,
        None,
        None,
        None,
    );
    assistant.role = "assistant".to_string();
    assistant.owner_id = Some("owner-a".to_string());
    mem.append_message_event(assistant).await.unwrap();
    mem.append_message_event(owned("second user")).await.unwrap();

    let events = mem
        .load_recent_shared_context(SharedContextQuery {
            principal: MemoryPrincipal {
                require_owner_match: false,
                workspace_id: "workspace-a".to_string(),
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: Some("owner-a".to_string()),
                legacy_session_key: None,
            },
            since_event_id: None,
            limit: 10,
            include_roles: vec!["user".to_string()],
        })
        .await
        .unwrap();

    assert_eq!(
        events.iter().map(|event| event.content.as_str()).collect::<Vec<_>>(),
        vec!["first user", "second user"]
    );
}
