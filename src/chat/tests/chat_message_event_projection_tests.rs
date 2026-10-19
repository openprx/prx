#![cfg(test)]

use super::*;
use crate::llm::route_decision::TokenUsage;
use crate::memory::SqliteMemory;
use tempfile::TempDir;

#[tokio::test]
async fn equivalent_events_drive_resume_export_and_cost_without_output_drift() {
    let tmp = TempDir::new().unwrap();
    let workspace_id = tmp.path().to_string_lossy().to_string();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let fabric = MemoryFabric::new(memory.clone(), workspace_id.clone());
    let cost_config = crate::config::schema::CostConfig::default();

    let mut snapshot = session::ChatSession::new("anthropic", "claude-sonnet-4");
    snapshot.id = "event-projection-session".to_string();
    snapshot.add_user_turn("hello from the blob");
    snapshot.add_assistant_turn("hello from the event log", Vec::new());
    let decision = RouteDecision::single_candidate("anthropic", "claude-sonnet-4");
    let outcome = ProviderExecutionOutcome::success_for_decision_with_usage(
        &decision,
        chrono::Utc::now(),
        TokenUsage::reported(Some(120), Some(30), Some(150)),
    );
    let mut settled =
        crate::llm::route_decision::MeteredTokenUsageRecord::from_provider_outcome(&outcome, &cost_config).unwrap();
    settled.settlement_id = Some("turn-1".to_string());
    snapshot.record_usage_settlement(settled).unwrap();
    save_session(memory.as_ref(), &snapshot).await.unwrap();

    let session_key = format!("chat:{}", snapshot.id);
    let (binding, service, _) = decision_source::admission_failure_tests::decided_binding_for_provider(
        &fabric,
        &session_key,
        "turn-1",
        "hello from the blob",
        "anthropic",
        "claude-sonnet-4",
    )
    .await;
    finalize_chat_success_turn(
        Some(binding.as_ref()),
        &fabric,
        &session_key,
        "turn-1",
        "anthropic",
        "claude-sonnet-4",
        binding.envelope().message_scope(),
        outcome.clone(),
        Some("hello from the event log".to_string()),
        1,
        "test reply",
        &cost_config,
        tmp.path(),
    )
    .await
    .unwrap();
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;

    // Workspace-visible noise from another chat must never enter this projection.
    let noise = session::ChatSession::new("other", "model");
    record_chat_user_message_event(
        &fabric,
        &noise,
        &format!("chat:{}", noise.id),
        "noise-turn",
        "other",
        "model",
        1,
        "unrelated workspace event",
    )
    .await
    .unwrap();

    let before_json = snapshot.to_json().unwrap();
    let before_cost = commands::format_cost_feedback(&snapshot);
    let projected =
        project_chat_session_from_message_events(memory.as_ref(), &workspace_id, snapshot.clone(), &cost_config).await;

    assert!(projected.turns_from_events, "equivalent turns must use MessageEvent");
    assert!(
        projected.usage_from_events,
        "equivalent usage must use provider outcome events"
    );
    assert_eq!(
        projected.session.to_json().unwrap(),
        before_json,
        "export projection drifted"
    );
    assert_eq!(commands::format_cost_feedback(&projected.session), before_cost);

    let resumed = load_session_by_id_with_message_events(memory.as_ref(), &workspace_id, &snapshot.id, &cost_config)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.to_json().unwrap(), before_json, "resume projection drifted");
}

#[tokio::test]
async fn non_equivalent_events_keep_the_blob_compatibility_snapshot() {
    let tmp = TempDir::new().unwrap();
    let workspace_id = tmp.path().to_string_lossy().to_string();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let fabric = MemoryFabric::new(memory.clone(), workspace_id.clone());
    let cost_config = crate::config::schema::CostConfig::default();
    let mut snapshot = session::ChatSession::new("provider", "model");
    snapshot.id = "event-fallback-session".to_string();
    snapshot.add_user_turn("blob is authoritative until parity");
    let before = snapshot.to_json().unwrap();

    record_chat_user_message_event(
        &fabric,
        &snapshot,
        &format!("chat:{}", snapshot.id),
        "turn-1",
        "provider",
        "model",
        1,
        "different event content",
    )
    .await
    .unwrap();

    let projected =
        project_chat_session_from_message_events(memory.as_ref(), &workspace_id, snapshot, &cost_config).await;
    assert!(!projected.turns_from_events);
    assert!(!projected.usage_from_events);
    assert_eq!(projected.session.to_json().unwrap(), before);
}

#[tokio::test]
async fn unsupported_event_reader_keeps_the_blob_compatibility_snapshot() {
    let mut snapshot = session::ChatSession::new("provider", "model");
    snapshot.id = "blob-only-session".to_string();
    snapshot.add_user_turn("backend has no event log");
    let before = snapshot.to_json().unwrap();

    let projected = project_chat_session_from_message_events(
        &crate::memory::NoneMemory::new(),
        "workspace",
        snapshot,
        &crate::config::schema::CostConfig::default(),
    )
    .await;

    assert!(!projected.turns_from_events);
    assert!(!projected.usage_from_events);
    assert_eq!(projected.session.to_json().unwrap(), before);
}
