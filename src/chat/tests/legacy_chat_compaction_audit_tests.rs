#![cfg(test)]

use super::*;
use crate::memory::SqliteMemory;
use tempfile::TempDir;

#[test]
fn legacy_chat_compaction_audit_source_is_bounded() {
    let mut source_history = vec![ChatMessage::system("system rules ".repeat(200))];
    for idx in 0..20 {
        source_history.push(ChatMessage::user(format!("turn {idx} {}", "payload ".repeat(200))));
    }

    let bounded = bounded_legacy_chat_compaction_audit_source(&source_history);
    assert_eq!(bounded.first().map(|msg| msg.role.as_str()), Some("system"));
    assert!(
        bounded.len() <= COMPACT_KEEP_MESSAGES + 1,
        "audit projection should not retain every historical turn"
    );
    assert!(
        bounded
            .iter()
            .all(|msg| msg.content.chars().count() <= COMPACT_CONTENT_CHARS + 3),
        "audit projection should truncate each retained message"
    );
}

#[test]
fn legacy_chat_compaction_original_audit_source_tracks_untruncated_losses() {
    let mut source_history = vec![ChatMessage::system("system rules")];
    for idx in 0..12 {
        source_history.push(ChatMessage::user(format!(
            "evicted-original-{idx} {}",
            "payload ".repeat(8)
        )));
    }

    let audit_source = original_legacy_chat_compaction_audit_source(&source_history);

    assert_eq!(
        audit_source.len(),
        4,
        "12 non-system messages with keep window 8 must audit the 4 original evicted turns"
    );
    assert_eq!(
        audit_source
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        source_history
            .iter()
            .skip(1)
            .take(4)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        "audit source must retain the original untruncated evicted turns in order"
    );
}

#[tokio::test]
async fn legacy_chat_compaction_persists_run_and_summary_memory() {
    let tmp = TempDir::new().unwrap();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::chat("workspace-a", "session-a");
    let mut source_history = vec![ChatMessage::system("system rules")];
    for idx in 0..12 {
        source_history.push(ChatMessage::user(format!(
            "evicted-source-{idx} /tmp/source-a owner lineage {}",
            "payload ".repeat(8)
        )));
    }
    let audit_source = original_legacy_chat_compaction_audit_source(&source_history);
    let summary_projection = bounded_legacy_chat_compaction_audit_source(&source_history);
    let fabric = MemoryFabric::new(mem.clone(), "workspace-a");
    let mut expected_source_event_ids = Vec::new();
    for (index, message) in audit_source.iter().enumerate() {
        let event = fabric
            .record_inbound_user_message(
                envelope.message_scope(),
                message.content.clone(),
                Some(format!("legacy-compaction-source-{index}")),
                None,
            )
            .await
            .unwrap();
        expected_source_event_ids.push(event.event_id);
    }
    let expected_source_tokens = estimate_chat_history_tokens(&audit_source);
    let provider_history = {
        let mut history = source_history.clone();
        history.push(ChatMessage::user(format!(
            "visible @file.txt\n\n[Attached file context from @path mentions]\n{}",
            "hidden enrichment ".repeat(20)
        )));
        history
    };
    let token_metadata = legacy_compaction_token_metadata(&provider_history, &source_history);

    persist_legacy_chat_compaction_audit(
        mem.as_ref(),
        &envelope,
        &audit_source,
        &summary_projection,
        token_metadata,
        "chat_context_overflow",
    )
    .await;

    let conn = rusqlite::Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
    let (
        summary_memory_key,
        mode,
        fidelity_status,
        source_message_count,
        source_token_estimate,
        source_event_ids_json,
        source_event_range_json,
        source_document_refs_json,
        payload_json,
    ): (String, String, String, i64, i64, String, String, Option<String>, String) = conn
        .query_row(
            "SELECT summary_memory_key, mode, fidelity_status, source_message_count,
                        source_token_estimate, source_event_ids_json,
                        source_event_range_json, source_document_refs_json, payload_json
                 FROM compaction_runs
                 WHERE workspace_id = 'workspace-a'
                 ORDER BY id DESC
                 LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .unwrap();
    assert!(summary_memory_key.starts_with("compaction_summary_"));
    assert_eq!(mode, "legacy_chat_overflow");
    assert_eq!(fidelity_status, "accepted_legacy_deterministic");
    assert_eq!(source_message_count, 4);
    assert_eq!(source_token_estimate, expected_source_tokens as i64);
    let source_event_ids: Vec<String> = serde_json::from_str(&source_event_ids_json).unwrap();
    assert_eq!(source_event_ids, expected_source_event_ids);
    let covered_range: crate::memory::CompactionSourceEventRange =
        serde_json::from_str(&source_event_range_json).unwrap();
    assert_eq!(
        Some(covered_range.first_event_id.as_str()),
        expected_source_event_ids.first().map(String::as_str)
    );
    assert_eq!(
        Some(covered_range.last_event_id.as_str()),
        expected_source_event_ids.get(3).map(String::as_str)
    );
    assert_eq!(covered_range.source_event_count, 4);
    let source_document_refs: Vec<serde_json::Value> =
        serde_json::from_str(source_document_refs_json.as_deref().unwrap()).unwrap();
    assert_eq!(source_document_refs.len(), 4);
    assert!(
        source_document_refs.iter().all(|reference| {
            reference.get("kind").and_then(serde_json::Value::as_str) == Some("message_content_hash")
                && reference
                    .get("content_sha256")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|hash| hash.len() == 64)
        }),
        "legacy compaction must retain deterministic content-hash provenance"
    );
    let payload: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
    assert_eq!(
        payload
            .get("provider_token_estimate")
            .and_then(serde_json::Value::as_u64),
        Some(token_metadata.provider_token_estimate as u64)
    );
    assert_eq!(
        payload
            .get("persisted_token_estimate")
            .and_then(serde_json::Value::as_u64),
        Some(token_metadata.persisted_token_estimate as u64)
    );
    assert!(
        payload
            .get("enrichment_token_delta")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|delta| delta > 0),
        "legacy audit metadata must expose non-empty provider enrichment overhead"
    );

    let (summary_event_id, summary_causation, summary_payload): (String, String, String) = conn
        .query_row(
            "SELECT event_id, causation_event_id, raw_payload_json
                 FROM message_events
                 WHERE event_type = 'compaction.summary.created'
                 ORDER BY id DESC
                 LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        Some(summary_causation.as_str()),
        expected_source_event_ids.get(3).map(String::as_str)
    );
    let summary_payload: serde_json::Value = serde_json::from_str(&summary_payload).unwrap();
    assert_eq!(
        summary_payload
            .get("covered_event_range")
            .and_then(|range| range.get("last_event_id"))
            .and_then(serde_json::Value::as_str),
        expected_source_event_ids.get(3).map(String::as_str)
    );
    assert_eq!(
        payload
            .get("summary_message_event_id")
            .and_then(serde_json::Value::as_str),
        Some(summary_event_id.as_str())
    );

    let stored_summary_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
                 FROM memories
                 WHERE key = ?1
                   AND source = 'legacy_chat_compaction_summary'
                   AND session_id = ?2",
            [&summary_memory_key, &envelope.session_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_summary_count, 1);
}

#[tokio::test]
async fn legacy_chat_compaction_downgrades_fidelity_when_event_provenance_is_unavailable() {
    let tmp = TempDir::new().unwrap();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::chat("workspace-a", "session-without-events");
    let source_history = vec![
        ChatMessage::user("first compacted turn"),
        ChatMessage::assistant("second compacted turn"),
    ];
    let token_metadata = legacy_compaction_token_metadata(&source_history, &source_history);

    persist_legacy_chat_compaction_audit(
        mem.as_ref(),
        &envelope,
        &source_history,
        &source_history,
        token_metadata,
        "chat_context_overflow",
    )
    .await;

    let conn = rusqlite::Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
    let (fidelity_status, source_event_ids, source_refs): (String, Option<String>, String) = conn
        .query_row(
            "SELECT fidelity_status, source_event_ids_json, source_document_refs_json
                 FROM compaction_runs
                 ORDER BY id DESC
                 LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert_eq!(fidelity_status, "accepted_legacy_content_hash_fallback");
    assert!(source_event_ids.is_none());
    let refs: Vec<serde_json::Value> = serde_json::from_str(&source_refs).unwrap();
    assert_eq!(refs.len(), source_history.len());
}

/// D8-2: chat run_id is per-turn. Two turns within one session must produce
/// two distinct, non-empty run_ids on their message events, and neither turn
/// may set parent_run_id (session relation lives in the session key, not the
/// run lineage).
#[tokio::test]
async fn chat_per_turn_run_ids_are_distinct_and_have_no_parent() {
    let tmp = TempDir::new().unwrap().keep();
    use futures_util::FutureExt as _;
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
        .expect("owned lineage fixture cleanup");
    let mut outcome = None;
    let cleanup_result = cleanup
        .run(async {
            let mut retained = None;
            outcome = Some(
                std::panic::AssertUnwindSafe(async {
                    let mem: Arc<dyn crate::memory::Memory> = Arc::new(SqliteMemory::new(tmp.as_path()).unwrap());
                    let fabric = MemoryFabric::new(mem, tmp.to_string_lossy().into_owned()).with_event_recording(
                        crate::memory::MemoryEventRecording {
                            enabled: true,
                            record_user_messages: true,
                            record_assistant_messages: true,
                            ..crate::memory::MemoryEventRecording::default()
                        },
                    );
                    let session = session::ChatSession::new("test-provider", "test-model");
                    let session_key = format!("chat:{}", session.id);

                    // Two turns, each with its own freshly generated run_id (mirroring the
                    // per-turn generation at the turn-loop entry).
                    let turn1_run_id = uuid::Uuid::new_v4().to_string();
                    let (binding, service, user1) =
                        decision_source::admission_failure_tests::decided_binding_for_input_retained(
                            &fabric,
                            &session_key,
                            &turn1_run_id,
                            "first turn",
                            &mut retained,
                        )
                        .await;
                    let route = RouteDecision::single_candidate("ollama", "prx-local-model");
                    let asst1 = finalize_chat_success_turn(
                        Some(binding.as_ref()),
                        &fabric,
                        &session_key,
                        &turn1_run_id,
                        "ollama",
                        "prx-local-model",
                        binding.envelope().message_scope(),
                        ProviderExecutionOutcome::success_for_decision(&route, chrono::Utc::now()),
                        Some("first reply".to_string()),
                        1,
                        "test reply",
                        &crate::config::schema::CostConfig::default(),
                        tmp.as_path(),
                    )
                    .await
                    .unwrap()
                    .assistant_event
                    .unwrap();
                    service
                        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
                        .await;

                    let turn2_run_id = uuid::Uuid::new_v4().to_string();
                    let user2 = record_chat_user_message_event(
                        &fabric,
                        &session,
                        &session_key,
                        &turn2_run_id,
                        "test-provider",
                        "test-model",
                        2,
                        "second turn",
                    )
                    .await
                    .unwrap();

                    assert_eq!(user1.run_id.as_deref(), Some(turn1_run_id.as_str()));
                    assert_eq!(asst1.run_id.as_deref(), Some(turn1_run_id.as_str()));
                    assert_eq!(user2.run_id.as_deref(), Some(turn2_run_id.as_str()));
                    assert_ne!(turn1_run_id, turn2_run_id, "each turn must get a distinct run_id");
                    assert!(!turn1_run_id.is_empty() && !turn2_run_id.is_empty());
                    assert!(
                        user1.parent_run_id.is_none() && asst1.parent_run_id.is_none() && user2.parent_run_id.is_none(),
                        "chat turns must not set parent_run_id (session relation is via session_key)"
                    );
                })
                .catch_unwind()
                .await,
            );
            drop(retained);
            Ok(())
        })
        .await;
    match outcome {
        Some(Err(panic)) => std::panic::resume_unwind(panic),
        Some(Ok(())) => cleanup_result.expect("lineage fixture queues and accounting drained"),
        None => panic!("lineage fixture did not enter cleanup scope"),
    }
}

#[tokio::test]
async fn save_and_message_event_boundaries_redact_aws_keys() {
    let secret = "AKIAABCDEFGHIJKLMNOP";
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let fabric = MemoryFabric::new(memory.clone(), tmp.path().to_string_lossy());
    let mut session = session::ChatSession::new("mock", "model");
    // Cyrillic "Privet" plus a EURO sign: 2-byte and 3-byte UTF-8 sequences that redaction must
    // preserve intact.
    let non_ascii = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}\u{20ac}";
    session.title = format!("{non_ascii} {secret}");
    session.add_user_turn(&format!("user {non_ascii} {secret}"));
    session.add_assistant_turn(&format!("assistant مرحبا {secret}"), Vec::new());

    save_session(memory.as_ref(), &session).await.unwrap();
    let stored = memory.get(&session.memory_key()).await.unwrap().unwrap();
    assert!(!stored.content.contains(secret), "stored Memory content leaked AWS key");
    let recalled = memory.recall(secret, 10, None).await.unwrap();
    assert!(recalled.iter().all(|entry| !entry.content.contains(secret)));

    let session_key = format!("chat:{}", session.id);
    let (binding, service, user_event) = decision_source::admission_failure_tests::decided_binding_for_input(
        &fabric,
        &session_key,
        "run-secret",
        &format!("user {secret} {non_ascii}"),
    )
    .await;
    let route = RouteDecision::single_candidate("ollama", "prx-local-model");
    let assistant_event = finalize_chat_success_turn(
        Some(binding.as_ref()),
        &fabric,
        &session_key,
        "run-secret",
        "ollama",
        "prx-local-model",
        binding.envelope().message_scope(),
        ProviderExecutionOutcome::success_for_decision(&route, chrono::Utc::now()),
        Some(format!("assistant {secret} 🌍")),
        1,
        "test reply",
        &crate::config::schema::CostConfig::default(),
        tmp.path(),
    )
    .await
    .unwrap()
    .assistant_event
    .unwrap();
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;
    assert!(!user_event.content.contains(secret));
    assert!(!assistant_event.content.contains(secret));
    assert!(user_event.content.contains(non_ascii));
    assert!(assistant_event.content.contains('🌍'));

    let semantic_key = "chat_auto_promote_secret";
    let safe_semantic = sanitize_chat_semantic_memory_content(&format!("promote {secret} Unicode {non_ascii}"));
    fabric
        .record_semantic_memory_from_event(
            semantic_key,
            &safe_semantic,
            MemoryCategory::Conversation,
            None,
            Some(&user_event.event_id),
            None,
            None,
        )
        .await
        .unwrap();
    let semantic = memory.get(semantic_key).await.unwrap().unwrap();
    assert!(!semantic.content.contains(secret));
    assert!(semantic.content.contains(non_ascii));
    let recalled = memory.recall(secret, 10, None).await.unwrap();
    assert!(recalled.iter().all(|entry| !entry.content.contains(secret)));
}

#[tokio::test]
async fn chat_terminal_without_binding_rejects_before_persistence() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let fabric = MemoryFabric::new(memory.clone(), tmp.path().to_string_lossy());
    let scope = route_event_scope(
        "chat",
        None,
        Some("prx_session".to_string()),
        Some("prx_turn".to_string()),
        Some("local-user".to_string()),
        None,
    );
    let route = RouteDecision::single_candidate("provider", "model");
    let cost = crate::config::schema::CostConfig::default();
    let success = finalize_chat_success_turn(
        None,
        &fabric,
        "prx_session",
        "prx_turn",
        "provider",
        "model",
        scope.clone(),
        ProviderExecutionOutcome::success_for_decision(&route, chrono::Utc::now()),
        Some("prx reply".to_string()),
        1,
        "completed",
        &cost,
        tmp.path(),
    )
    .await;
    assert!(success.err().unwrap().to_string().contains("binding is missing"));
    for status in [
        crate::agent::terminal::TurnTerminalStatus::Failed,
        crate::agent::terminal::TurnTerminalStatus::Cancelled,
    ] {
        let result = finalize_chat_non_success_turn(
            None,
            &fabric,
            "prx_turn",
            scope.clone(),
            status,
            None,
            chrono::Utc::now(),
            "interrupted",
            &cost,
            tmp.path(),
        )
        .await;
        assert!(result.err().unwrap().to_string().contains("binding is missing"));
    }
    let events = memory
        .list_message_events_since(
            &MemoryPrincipal {
                require_owner_match: false,
                workspace_id: fabric.workspace_id().to_string(),
                agent_id: None,
                persona_id: None,
                session_key: Some("prx_session".to_string()),
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
    assert!(
        events.is_empty(),
        "unbound terminals must not persist history or markers"
    );
}

#[tokio::test]
async fn chat_persists_no_secret_from_a_reply_or_a_provider_error() {
    // D-053 / REDACT-TURN probe: a chat reply committed through the production
    // success finalizer, and a failed chat turn whose provider error and
    // summary carry the secret committed through the production failure
    // finalizer, leave the secret in no file of the workspace. Fails before
    // REDACT-TURN moved chat's redaction into `finalize_turn`.
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
    let tmp = TempDir::new().expect("test: tempdir");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let fabric = MemoryFabric::new(memory.clone(), tmp.path().to_string_lossy());
    let (reply_binding, reply_service) =
        decision_source::admission_failure_tests::decided_binding(&fabric, "chat-probe", "chat-probe-reply").await;
    let decision = RouteDecision::single_candidate("ollama", "prx-local-model");
    let reply = finalize_chat_success_turn(
        Some(reply_binding.as_ref()),
        &fabric,
        "chat-probe",
        "chat-probe-reply",
        "ollama",
        "prx-local-model",
        reply_binding.envelope().message_scope(),
        ProviderExecutionOutcome::success_for_decision(&decision, chrono::Utc::now()),
        Some(format!("Here is the key {PROBE_SECRET} you asked for")),
        1,
        "test reply",
        &crate::config::schema::CostConfig::default(),
        tmp.path(),
    )
    .await
    .expect("test: commit the chat reply")
    .assistant_event
    .unwrap();
    assert!(reply.content.contains("[REDACTED]"), "{}", reply.content);
    let (failed_binding, failed_service) =
        decision_source::admission_failure_tests::decided_binding(&fabric, "chat-probe", "chat-probe-failed").await;
    let error = anyhow::anyhow!("upstream rejected key {PROBE_SECRET}");
    finalize_chat_non_success_turn(
        Some(failed_binding.as_ref()),
        &fabric,
        "chat-probe-failed",
        failed_binding.envelope().message_scope(),
        crate::agent::terminal::TurnTerminalStatus::Failed,
        Some(ProviderExecutionOutcome::failed_for_decision(
            &decision,
            chrono::Utc::now(),
            &error,
        )),
        chrono::Utc::now(),
        &format!("redux driver failed: {error}"),
        &crate::config::schema::CostConfig::default(),
        tmp.path(),
    )
    .await
    .expect("test: commit the failed chat turn");
    assert_nothing_under_persists_the_probe_secret(tmp.path());
    for service in [reply_service, failed_service] {
        service
            .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await;
    }
}

#[tokio::test]
async fn chat_route_payloads_remain_valid_json_and_redact_nested_secrets() {
    let secret = "AKIAABCDEFGHIJKLMNOP";
    let bearer = "Authorization: Bearer abcdefghijklmnop";
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let fabric = MemoryFabric::new(memory.clone(), tmp.path().to_string_lossy());
    let session_key = "chat-route-sanitize".to_string();
    let (binding, service) =
        decision_source::admission_failure_tests::decided_binding(&fabric, &session_key, "route-run").await;
    let scope = binding.envelope().message_scope();
    let mut decision = RouteDecision::single_candidate("ollama", "prx-local-model");
    decision.user_hint = Some(format!("hint {secret}"));
    decision
        .filtered_out
        .push(crate::llm::route_decision::RouteFilterReason {
            provider: "fallback".to_string(),
            model: "model".to_string(),
            reason: "auth".to_string(),
            detail: Some(bearer.to_string()),
        });
    let route_event = record_route_decision_event(&fabric, scope.clone(), &decision)
        .await
        .unwrap();
    let route_payload = route_event.raw_payload_json.as_deref().unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(route_payload).is_ok());
    assert!(!route_payload.contains(secret));
    assert!(!route_payload.contains("abcdefghijklmnop"));

    let error = anyhow::anyhow!("provider failed: {bearer}; aws={secret}");
    let outcome = ProviderExecutionOutcome::failed_for_decision(&decision, chrono::Utc::now(), &error);
    // the production failure commit of a chat turn, whose summary (and so
    // its suppression reason) carries the error text, as `run()` passes it
    // (R-LOOPD5 G-1)
    finalize_chat_non_success_turn(
        Some(binding.as_ref()),
        &fabric,
        "route-run",
        scope,
        crate::agent::terminal::TurnTerminalStatus::Failed,
        Some(outcome),
        chrono::Utc::now(),
        &format!("redux driver failed: {error}"),
        &crate::config::schema::CostConfig::default(),
        tmp.path(),
    )
    .await
    .unwrap();
    let events = memory
        .list_message_events_since(&binding.envelope().memory_principal(), 0, 20)
        .await
        .unwrap();
    assert!(!events.is_empty());
    assert!(events.iter().any(|event| event.event_type == "turn.finalized"));
    for payload in events.iter().filter_map(|event| event.raw_payload_json.as_deref()) {
        assert!(serde_json::from_str::<serde_json::Value>(payload).is_ok());
        assert!(!payload.contains(secret));
        assert!(!payload.contains("abcdefghijklmnop"));
    }
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;
}
