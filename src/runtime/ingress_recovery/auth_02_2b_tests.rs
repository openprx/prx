#![cfg(test)]

use super::recovery_config;
use crate::memory::{Memory, SqliteMemory};
use crate::runtime::turn_cleanup::CleanupTracker;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn observer() -> Arc<dyn crate::decision::telemetry::CausalEventSink> {
    Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::new(
        crate::observability::NoopObserver,
    )))
}

async fn publish_plane_on(
    workspace: &std::path::Path,
    memory: Arc<dyn Memory>,
) -> Arc<crate::runtime::turn_decision::TurnDecisionService> {
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
    .expect("test: plane-on builder publish")
}

async fn plane_on_repair_writes_outcome_through_builder_writer() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let workspace = dir.path();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
    let (terminal_id, owner_id) =
        crate::agent::terminal::seed_replayable_completed_terminal(workspace, Arc::clone(&memory)).await;
    assert_eq!(terminal_id, "prx_legacy");
    let tracker = Arc::new(CleanupTracker::default());
    let service = tracker.scope(publish_plane_on(workspace, Arc::clone(&memory))).await;
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

async fn failing_readwrite_open_leaves_owner_drain_unclean() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let workspace = dir.path();
    std::fs::create_dir_all(workspace.join("self")).expect("test: self dir");
    std::fs::write(workspace.join("self/causal_tree"), b"not-a-dir").expect("test: block store dir");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
    let mut config = recovery_config(workspace);
    config.causal_tree.enabled = true;
    let generation = crate::config::ConfigGenerationManager::new(config).pin();
    let tracker = Arc::new(CleanupTracker::default());
    match crate::runtime::ingress_recovery::open_advisory_store(&generation, memory, observer(), &tracker).await {
        Err(_) => {}
        Ok(_) => panic!("test: file at causal_tree must refuse ReadWrite open"),
    }
    let error = tracker
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .expect_err("test: failed open leaves drain unclean");
    assert_eq!(error.to_string(), "runtime terminal cleanup has unconfirmed failures");
}

pub(super) async fn recovery_legacy_reader_creates_no_files() {
    plane_on_repair_writes_outcome_through_builder_writer().await;
    failing_readwrite_open_leaves_owner_drain_unclean().await;
}
