#![cfg(test)]

use super::recovery_config;
use crate::memory::{Memory, SqliteMemory};
use crate::runtime::turn_cleanup::CleanupTracker;
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn publish_plane_on(
    workspace: &std::path::Path,
    memory: Arc<dyn Memory>,
) -> anyhow::Result<Arc<crate::runtime::turn_decision::TurnDecisionService>> {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config.config_path = workspace.join("config.toml");
    config.causal_tree.enabled = true;
    config.causal_tree.backend.kind =
        crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::Builtin);
    config.causal_tree.backend.remote_enabled = false;
    let generation = crate::config::new_shared(config).pin();
    let registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![
        Box::new(crate::tools::MemorySearchTool::new(
            workspace.to_path_buf(),
            Arc::clone(&memory),
            true,
        )),
        Box::new(crate::tools::StaySilentTool::new()),
    ]);
    crate::runtime::turn_publication::TurnDecisionPublication {
        generation: Arc::clone(&generation),
        registry,
        mcp_tool: None,
        security: Arc::new(crate::security::SecurityPolicy::from_config(
            &generation.effective.autonomy,
            workspace,
        )),
        observer: Arc::new(crate::observability::NoopObserver {}),
        hooks: Arc::new(crate::hooks::HookManager::new(workspace.to_path_buf())),
        memory,
        plugin_generation: 0,
    }
    .publish(crate::causal_tree::state::TurnSurfaceKind::AgentLoop)
    .await
}

pub(super) async fn recovery_builder_repair_writes_outcome_through_passed_writer() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let workspace = dir.path();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
    let (terminal_id, owner_id) =
        crate::agent::terminal::seed_replayable_completed_terminal(workspace, Arc::clone(&memory)).await;
    assert_eq!(terminal_id, "prx_legacy");
    let tracker = Arc::new(CleanupTracker::default());
    let service = tracker
        .scope(publish_plane_on(workspace, Arc::clone(&memory)))
        .await
        .expect("test: plane-on builder publish");
    let recovery = service.recovery_scope();
    let store = recovery.legacy_store().expect("test: builder recovery store");
    assert_eq!(store.reader().counts().expect("test: outcomes before").outcomes, 0);
    recovery
        .rejoin_terminal(&terminal_id, &owner_id)
        .await
        .expect("test: builder rejoin");
    assert_eq!(store.reader().counts().expect("test: outcomes after").outcomes, 1);
    service.shutdown(Instant::now() + Duration::from_secs(5)).await;
}

pub(super) async fn recovery_failed_readwrite_open_leaves_drain_unclean() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let workspace = dir.path();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
    crate::decision::key::DecisionKey::load_or_create(workspace).expect("test: plane-on key");
    std::fs::create_dir_all(workspace.join("self/causal_tree/decisions.db")).expect("test: block ReadWrite db path");
    let tracker = Arc::new(CleanupTracker::default());
    match tracker.scope(publish_plane_on(workspace, memory)).await {
        Err(_) => {}
        Ok(_) => panic!("test: directory at decisions.db must refuse ReadWrite open"),
    }
    let error = tracker
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .expect_err("test: failed open leaves drain unclean");
    assert_eq!(error.to_string(), "runtime terminal cleanup has unconfirmed failures");
}

pub(super) async fn recovery_store_without_writer_rejoin_fails_closed() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let workspace = dir.path();
    let sqlite = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
    let memory: Arc<dyn Memory> = Arc::clone(&sqlite) as Arc<dyn Memory>;
    let (terminal_id, owner_id) =
        crate::agent::terminal::seed_replayable_completed_terminal(workspace, Arc::clone(&memory)).await;
    assert_eq!(terminal_id, "prx_legacy");
    let workspace_id = workspace.to_string_lossy().into_owned();
    let marker = memory
        .find_message_event_by_idempotency_key(&workspace_id, "turn:prx_legacy:final")
        .await
        .expect("test: load marker")
        .expect("test: seeded marker");
    let mut payload: serde_json::Value =
        serde_json::from_str(marker.raw_payload_json.as_deref().expect("test: marker payload"))
            .expect("test: marker json");
    assert!(
        payload
            .pointer_mut("/terminal_replay_v1")
            .expect("test: replay binding")
            .as_object_mut()
            .expect("test: replay object")
            .remove("journal_version")
            .is_some(),
        "test: seeded marker must seal journal_version"
    );
    let assistant = match payload
        .pointer("/terminal_replay_v1/assistant/event_id")
        .and_then(|value| value.as_str())
    {
        Some(event_id) => memory
            .find_owned_message_event(&workspace_id, &owner_id, event_id)
            .await
            .expect("test: assistant lookup"),
        None => None,
    };
    let key = crate::decision::key::DecisionKey::load_existing(workspace).expect("test: existing key");
    crate::agent::terminal::reseal_payload_for_test(&key, &mut payload, assistant.as_ref())
        .expect("test: reseal pre-journal marker");
    sqlite
        .replace_event_payload_for_test(&marker.event_id, &payload.to_string())
        .expect("test: persist pre-journal marker");
    let mut config = recovery_config(workspace);
    config.causal_tree.enabled = true;
    let generation = crate::config::ConfigGenerationManager::new(config).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let observer: Arc<dyn crate::decision::telemetry::CausalEventSink> = Arc::new(
        crate::decision::telemetry::ObserverCausalSink::new(Arc::new(crate::observability::NoopObserver)),
    );
    let (store, events) =
        crate::runtime::ingress_recovery::open_advisory_store(&generation, Arc::clone(&memory), observer, &tracker)
            .await
            .expect("test: advisory store");
    let recovery = crate::runtime::ingress_recovery::RecoveryScope::prepare(
        &generation,
        memory,
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        None,
        &tracker,
        Some(key),
        crate::runtime::ingress_recovery::PreparedAdvisory {
            writer: None,
            events: Some(events),
            store: Some(store),
        },
    )
    .await
    .expect("test: recovery prepare");
    let error = recovery
        .rejoin_terminal(&terminal_id, &owner_id)
        .await
        .expect_err("test: store without writer must fail closed");
    assert_eq!(error.to_string(), "terminal rejoin writer is missing");
}
