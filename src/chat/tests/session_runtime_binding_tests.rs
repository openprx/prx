#![cfg(test)]

use super::*;
use crate::memory::SqliteMemory;
use tempfile::TempDir;

#[test]
fn resumed_session_uses_runtime_provider_model_for_current_chat() {
    let mut session = session::ChatSession::new("old-provider", "old-model");
    session.title = "resumed".to_string();
    session.add_user_turn("hello");

    bind_session_to_runtime_provider_model(&mut session, "moonshot", "kimi-k2.5");

    assert_eq!(session.provider, "moonshot");
    assert_eq!(session.model, "kimi-k2.5");
    assert_eq!(session.title, "resumed");
    assert_eq!(session.turn_count(), 1);
}

#[test]
fn brand_new_chat_seeds_redux_with_durable_session_id() {
    let session = session::ChatSession::new("mock", "mock-model");
    assert_eq!(session.turn_count(), 0);

    let state = initial_chat_dispatcher_state("mock", "mock-model", CancellationToken::new(), &session);

    assert_eq!(state.session.id, session.id);
    assert_eq!(state.session.provider.as_ref(), session.provider);
    assert_eq!(state.session.model.as_ref(), session.model);
}

#[tokio::test]
async fn chat_entrypoint_records_user_and_assistant_message_events() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let fabric = MemoryFabric::new(memory.clone(), tmp.path().to_string_lossy());
    let session = session::ChatSession::new("ollama", "prx-local-model");
    let session_key = format!("chat:{}", session.id);
    let run_id = "chat-run-test";

    let (binding, service, user_event) = decision_source::admission_failure_tests::decided_binding_for_input(
        &fabric,
        &session_key,
        run_id,
        "hello from chat",
    )
    .await;
    let route = RouteDecision::single_candidate("ollama", "prx-local-model");
    finalize_chat_success_turn(
        Some(binding.as_ref()),
        &fabric,
        &session_key,
        run_id,
        "ollama",
        "prx-local-model",
        binding.envelope().message_scope(),
        ProviderExecutionOutcome::success_for_decision(&route, chrono::Utc::now()),
        Some("hello from assistant".to_string()),
        1,
        "test reply",
        &crate::config::schema::CostConfig::default(),
        tmp.path(),
    )
    .await
    .unwrap();
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;

    // D4 C6: the durable session_key is now the stable canonical derived from
    // the session id (NOT legacy chat:{id}, NOT {provider}/{model}). Recall by
    // the canonical key — exactly what chat_runtime_envelope reads — returns
    // both events.
    let canonical_key = RuntimeEnvelope::chat_canonical_session_key(&session.id.to_string());
    let events = memory
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: tmp.path().to_string_lossy().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some(canonical_key.clone()),
                channel: Some("terminal".to_string()),
                sender: Some("local-user".to_string()),
                owner_id: None,
                legacy_session_key: None,
            },
            0,
            10,
        )
        .await
        .unwrap();
    // the production reply commit also records the provider and terminal
    // events on this key; the user and assistant turns are the messages
    let events: Vec<_> = events
        .into_iter()
        .filter(|event| event.event_type == "message.created")
        .collect();

    assert_eq!(events.len(), 2);
    // The durable session_key truly changed to canonical (not just recipient).
    assert_eq!(
        events.first().and_then(|e| e.session_key.as_deref()),
        Some(canonical_key.as_str())
    );
    assert_ne!(
        events.first().and_then(|e| e.session_key.as_deref()),
        Some(session_key.as_str())
    );
    let user_recorded = events.first();
    let assistant_recorded = events.get(1);
    assert_eq!(user_recorded.map(|event| event.source.as_str()), Some("chat"));
    assert_eq!(user_recorded.map(|event| event.role.as_str()), Some("user"));
    assert_eq!(
        user_recorded.map(|event| event.content.as_str()),
        Some("hello from chat")
    );
    assert_eq!(
        user_recorded.map(|event| event.event_id.as_str()),
        Some(user_event.event_id.as_str())
    );
    assert_eq!(assistant_recorded.map(|event| event.role.as_str()), Some("assistant"));
    assert_eq!(
        assistant_recorded.and_then(|event| event.sender.as_deref()),
        Some("ollama/prx-local-model")
    );
    assert_eq!(
        assistant_recorded.and_then(|event| event.recipient.as_deref()),
        Some("local-user")
    );

    // Write/read durable canonical strict equality (C6 / R6): the read
    // envelope's principal session_key equals the persisted event session_key.
    let read_envelope = chat_runtime_envelope(&tmp.path().to_string_lossy(), &session_key);
    assert_eq!(
        read_envelope.memory_principal().session_key.as_deref(),
        Some(canonical_key.as_str())
    );
    assert_eq!(
        read_envelope.message_scope().session_key.as_deref(),
        Some(canonical_key.as_str())
    );
    // legacy chat:{id} carried for read-merge.
    assert_eq!(
        read_envelope.memory_principal().legacy_session_key.as_deref(),
        Some(session_key.as_str())
    );

    // Read-merge proof on the session-key-filtered path. load_recent_session_context
    // applies a hard `session_key` filter for every visibility, so it is the
    // path where the canonical migration + legacy union actually matters. A
    // pre-cutover legacy event must recall via read-merge under the canonical key.
    memory
        .append_message_event(crate::memory::MessageEventInput {
            event_id: None,
            idempotency_key: None,
            workspace_id: tmp.path().to_string_lossy().to_string(),
            owner_id: None,
            source: "chat".into(),
            channel: Some("terminal".to_string()),
            session_key: Some(session_key.clone()),
            parent_session_key: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: Some("local-user".to_string()),
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
            content: "legacy pre-cutover turn".to_string(),
            raw_payload_json: None,
            visibility: crate::memory::MemoryVisibility::Workspace,
        })
        .await
        .unwrap();
    let merged = memory
        .load_recent_session_context(crate::memory::SessionContextQuery {
            principal: read_envelope.memory_principal(),
            since_event_id: None,
            limit: 20,
            include_roles: vec!["user".to_string(), "assistant".to_string()],
        })
        .await
        .unwrap();
    assert!(
        merged.iter().any(|event| event.content == "legacy pre-cutover turn"),
        "legacy history must read-merge under canonical key on the session-filtered path"
    );
    assert!(merged.iter().any(|event| event.content == "hello from chat"));

    // Without the legacy key, the same session-filtered path sees only the
    // canonical history (single-key degradation), not the legacy turn.
    let canonical_only = crate::memory::MemoryPrincipal {
        legacy_session_key: None,
        ..read_envelope.memory_principal()
    };
    let single = memory
        .load_recent_session_context(crate::memory::SessionContextQuery {
            principal: canonical_only,
            since_event_id: None,
            limit: 20,
            include_roles: vec!["user".to_string(), "assistant".to_string()],
        })
        .await
        .unwrap();
    assert!(single.iter().any(|event| event.content == "hello from chat"));
    assert!(
        !single.iter().any(|event| event.content == "legacy pre-cutover turn"),
        "single-key path must NOT see legacy history"
    );
}
