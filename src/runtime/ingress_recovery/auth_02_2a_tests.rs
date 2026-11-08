#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::{recovery_config, recovery_input, recovery_recorder};
use crate::memory::Memory;
use crate::memory::recovery_authority::{RecoveryChain, RecoveryPublication};
use crate::runtime::ingress_recovery::tombstone_fixture;
use crate::runtime::ingress_recovery::{
    IngressRecorder, LegacyUnconfirmed, RecoveryScope, SettledTerminalFacts, backend_memory,
};
use crate::runtime::turn_cleanup::CleanupTracker;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn home_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tombstone_fixture::home_dir();
    let workspace = dir.path().canonicalize().expect("test: workspace");
    (dir, workspace)
}

fn decision_tree(workspace: &Path) -> Vec<(String, u64, Option<std::time::SystemTime>)> {
    let dir = workspace.join("self/causal_tree");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .map(|entry| {
            let entry = entry.expect("test: decision entry");
            let meta = entry.metadata().expect("test: decision metadata");
            (
                entry.file_name().to_string_lossy().into_owned(),
                meta.len(),
                meta.modified().ok(),
            )
        })
        .collect();
    files.sort();
    files
}

fn assert_no_decision_files(workspace: &Path) {
    let tree = workspace.join("self/causal_tree");
    assert!(
        !tree.join(".decision_key").exists(),
        "plane off must not create .decision_key"
    );
    assert!(
        !tree.join("decisions.db").exists(),
        "plane off must not create decisions.db"
    );
    assert!(
        !tree.join("decisions.db-wal").exists(),
        "plane off must not create decisions.db-wal"
    );
    assert!(
        !tree.join("decisions.db-shm").exists(),
        "plane off must not create decisions.db-shm"
    );
}

fn table_rows(workspace: &Path, table: &str) -> i64 {
    if tombstone_fixture::table_exists(workspace, table) {
        tombstone_fixture::row_count(workspace, table)
    } else {
        0
    }
}

fn cell_memory(backend: &str, workspace: &Path, k: bool) -> Arc<dyn Memory> {
    let memory = backend_memory(backend, workspace).expect("test: backend");
    let Some(repository) = memory.recovery_repository() else {
        return memory;
    };
    repository
        .activate_recovery_authority()
        .expect("test: first activation");
    if k {
        return memory;
    }
    tombstone_fixture::stash_keys(workspace);
    backend_memory(backend, workspace).expect("test: unavailable handle")
}

struct MatrixCell {
    backend: &'static str,
    plane: bool,
    recording: bool,
    k: bool,
    l: bool,
}

fn cell_config(workspace: &Path, plane: bool, recording: bool) -> crate::config::Config {
    let mut config = recovery_config(workspace);
    config.causal_tree.enabled = plane;
    config.memory.events.record_user_messages = recording;
    config
}

fn observer() -> Arc<dyn crate::decision::telemetry::CausalEventSink> {
    Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::new(
        crate::observability::NoopObserver,
    )))
}

async fn run_matrix_cell(cell: MatrixCell) {
    let MatrixCell {
        backend,
        plane,
        recording,
        k,
        l,
    } = cell;
    let (_dir, workspace) = home_workspace();
    let memory = cell_memory(backend, &workspace, k);
    let has_repository = memory.recovery_repository().is_some();
    let ledger = crate::memory::serves_tool_execution_ledger(memory.name());
    if l {
        let mut input = recovery_input(&workspace, "prx_legacy");
        input.idempotency_key = Some("turn:prx_legacy:final".into());
        memory.append_message_event(input).await.expect("test: legacy final");
    }
    let before = decision_tree(&workspace);
    let config = Arc::new(cell_config(&workspace, plane, recording));
    let recorder_owner = Arc::new(CleanupTracker::default());
    let recorder_ticket = recorder_owner
        .register(tokio_util::sync::CancellationToken::new())
        .expect("test: ticket");
    let recorder = IngressRecorder::prepare(Arc::clone(&memory), &config, &recorder_ticket)
        .await
        .expect("test: recorder prepare");
    let recorder_ready = recorder.is_some();
    recorder_ticket.finish(true);
    drop(recorder);
    let generation = crate::config::ConfigGenerationManager::new(config.as_ref().clone()).pin();
    let workspace_key = if plane && recording {
        Some(crate::decision::key::DecisionKey::load_or_create(&workspace).expect("test: workspace key"))
    } else {
        None
    };
    let tracker = Arc::new(CleanupTracker::default());
    let recovery = RecoveryScope::prepare(
        &generation,
        Arc::clone(&memory),
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        observer(),
        None,
        &tracker,
        workspace_key,
    )
    .await
    .expect("test: recovery prepare");
    let repository_ready = has_repository && k;
    let recordable = recording && (repository_ready || (plane && ledger && !has_repository));
    let open_legacy = plane && recording && ledger;
    assert_eq!(
        recorder_ready, recordable,
        "{backend} plane={plane} recording={recording} k={k} l={l} recorder"
    );
    assert_eq!(
        recovery.ingress_recovery_enabled(),
        recordable,
        "{backend} plane={plane} recording={recording} k={k} l={l} recordable"
    );
    assert_eq!(
        recovery.authority.legacy.is_some(),
        open_legacy,
        "{backend} plane={plane} recording={recording} k={k} l={l} legacy"
    );
    if open_legacy {
        assert!(
            recovery.legacy_store().and_then(|store| store.writer()).is_some(),
            "{backend} plane-on repair writer"
        );
        assert!(recovery.legacy_events().is_some(), "{backend} plane-on sink");
    }
    if !plane {
        assert_eq!(
            decision_tree(&workspace),
            before,
            "{backend} plane-off decision tree bytes/mtime/listing"
        );
        assert_no_decision_files(&workspace);
    }
    if l && recordable && !plane {
        let error = recovery
            .rejoin_terminal("prx_legacy", "prx_user")
            .await
            .expect_err("unattested plane-off rejoin");
        assert!(
            error.chain().any(|cause| cause.is::<LegacyUnconfirmed>()),
            "{backend} unattested legacy must stay unconfirmed: {error}"
        );
        assert_no_decision_files(&workspace);
    }
    if l && recordable && plane {
        let _ = recovery.rejoin_terminal("prx_legacy", "prx_user").await;
        assert_eq!(table_rows(&workspace, "recovery_attestations"), 0);
        assert_eq!(table_rows(&workspace, "recovery_transport_index"), 0);
    }
    drop(memory);
    let _reopened = backend_memory(backend, &workspace).expect("test: reopen");
    if !plane {
        assert_no_decision_files(&workspace);
    }
    recovery.drain(Instant::now() + Duration::from_secs(5)).await;
    tracker
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .expect("test: drain");
}

pub(super) async fn recovery_plane_recording_key_legacy_matrix() {
    for plane in [true, false] {
        for recording in [true, false] {
            for k in [true, false] {
                for l in [true, false] {
                    run_matrix_cell(MatrixCell {
                        backend: "sqlite",
                        plane,
                        recording,
                        k,
                        l,
                    })
                    .await;
                }
            }
        }
    }
    for cell in [
        MatrixCell {
            backend: "lucid",
            plane: false,
            recording: true,
            k: true,
            l: true,
        },
        MatrixCell {
            backend: "encrypted",
            plane: false,
            recording: true,
            k: true,
            l: true,
        },
        MatrixCell {
            backend: "lucid",
            plane: true,
            recording: true,
            k: true,
            l: false,
        },
        MatrixCell {
            backend: "encrypted",
            plane: true,
            recording: true,
            k: true,
            l: false,
        },
        MatrixCell {
            backend: "postgres_class",
            plane: true,
            recording: true,
            k: false,
            l: true,
        },
        MatrixCell {
            backend: "postgres_class",
            plane: false,
            recording: true,
            k: false,
            l: true,
        },
    ] {
        run_matrix_cell(cell).await;
    }
}

fn settled_facts(terminal_id: &str, claim: &str, final_id: &str, key_id: &str) -> SettledTerminalFacts {
    SettledTerminalFacts {
        v: 1,
        terminal_id: terminal_id.to_owned(),
        claim_event_id: claim.to_owned(),
        assistant_event_id: None,
        usage_event_id: None,
        final_event_id: final_id.to_owned(),
        semantic_mac_v3: "00".repeat(32),
        status: "completed".into(),
        surface: crate::causal_tree::state::TurnSurfaceKind::AgentLoop
            .as_str()
            .to_owned(),
        parent_authority_id: None,
        pricing: "unknown".into(),
        usage_digest: "00".repeat(32),
        key_id: key_id.to_owned(),
        skill_observation: super::super::authority::SkillObservation {
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            awaiting_approval: false,
            verified: true,
        },
    }
}

pub(super) async fn recovery_off_never_writes_outcome_or_child_link() {
    let (_dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = backend_memory("sqlite", &workspace).expect("test: sqlite");
    let mut config = cell_config(&workspace, false, true);
    config.causal_tree.enabled = false;
    let config = Arc::new(config);
    let (recorder, ticket) = recovery_recorder(Arc::clone(&memory), &config).await;
    let input = recorder
        .append(recovery_input(&workspace, "prx_run"))
        .await
        .expect("test: ingress");
    let mut final_input = recovery_input(&workspace, "prx_run");
    final_input.idempotency_key = Some("turn:prx_run:final".into());
    final_input.event_id = Some(uuid::Uuid::new_v4().to_string());
    let final_event = memory.append_message_event(final_input).await.expect("test: final");
    let repository = memory.recovery_repository().expect("test: repository");
    let chain = RecoveryChain {
        workspace_id: workspace.to_string_lossy().into_owned(),
        owner_id: "prx_user".into(),
        source: "agent".into(),
        run_id: "prx_run".into(),
    };
    let pending = repository
        .read_recovery_receipt(&chain)
        .expect("test: pending read")
        .expect("test: pending receipt");
    assert_eq!(pending.state, "pending");
    let facts = settled_facts("prx_run", &input.event_id, &final_event.event_id, &pending.key_id);
    repository
        .settle_recovery(&RecoveryPublication {
            chain: chain.clone(),
            input: Some(
                repository
                    .recovery_event_evidence(&chain, &input.event_id)
                    .expect("test: input evidence"),
            ),
            terminal: Some(
                repository
                    .recovery_event_evidence(&chain, &final_event.event_id)
                    .expect("test: terminal evidence"),
            ),
            facts: serde_json::to_vec(&facts).expect("test: facts"),
        })
        .expect("test: settle");
    ticket.finish(true);
    drop(recorder);
    let before = decision_tree(&workspace);
    let attestations = table_rows(&workspace, "recovery_attestations");
    let generation = crate::config::ConfigGenerationManager::new(config.as_ref().clone()).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let recovery = RecoveryScope::prepare(
        &generation,
        Arc::clone(&memory),
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        observer(),
        None,
        &tracker,
        None,
    )
    .await
    .expect("test: plane-off prepare");
    assert!(recovery.ingress_recovery_enabled());
    assert!(recovery.authority.legacy.is_none());
    recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect("test: settled rejoin");
    crate::skills::lifecycle::drain_background_for_test("prx_run").await;
    assert_eq!(decision_tree(&workspace), before);
    assert_no_decision_files(&workspace);
    assert_eq!(table_rows(&workspace, "recovery_attestations"), attestations);
    let settled = repository
        .read_recovery_receipt(&chain)
        .expect("test: settled read")
        .expect("test: settled receipt");
    assert_eq!(settled.state, "settled");
    recovery.drain(Instant::now() + Duration::from_secs(2)).await;
    assert_no_decision_files(&workspace);
}

pub(super) async fn recovery_legacy_chain_writes_no_attestation() {
    let (_dir, workspace) = home_workspace();
    let memory = backend_memory("postgres_class", &workspace).expect("test: postgres_class");
    let config = Arc::new(cell_config(&workspace, true, true));
    let (recorder, ticket) = recovery_recorder(Arc::clone(&memory), &config).await;
    recorder
        .append(recovery_input(&workspace, "prx_legacy"))
        .await
        .expect("test: legacy capsule");
    let mut final_input = recovery_input(&workspace, "prx_legacy");
    final_input.idempotency_key = Some("turn:prx_legacy:final".into());
    memory
        .append_message_event(final_input)
        .await
        .expect("test: legacy final");
    ticket.finish(true);
    drop(recorder);
    let generation = crate::config::ConfigGenerationManager::new(config.as_ref().clone()).pin();
    let key = crate::decision::key::DecisionKey::load_or_create(&workspace).expect("test: workspace key");
    let tracker = Arc::new(CleanupTracker::default());
    let recovery = RecoveryScope::prepare(
        &generation,
        Arc::clone(&memory),
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        observer(),
        None,
        &tracker,
        Some(key),
    )
    .await
    .expect("test: plane-on prepare");
    assert!(recovery.authority.legacy.is_some());
    let _ = recovery.rejoin_terminal("prx_legacy", "prx_user").await;
    assert_eq!(table_rows(&workspace, "recovery_attestations"), 0);
    assert_eq!(table_rows(&workspace, "recovery_transport_index"), 0);

    let (_dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = backend_memory("sqlite", &workspace).expect("test: sqlite");
    let config = Arc::new(cell_config(&workspace, false, true));
    let (recorder, ticket) = recovery_recorder(Arc::clone(&memory), &config).await;
    recorder
        .append(recovery_input(&workspace, "prx_pending"))
        .await
        .expect("test: pending capsule");
    let mut final_input = recovery_input(&workspace, "prx_pending");
    final_input.idempotency_key = Some("turn:prx_pending:final".into());
    memory
        .append_message_event(final_input)
        .await
        .expect("test: unattested final");
    let repository = memory.recovery_repository().expect("test: repository");
    let chain = RecoveryChain {
        workspace_id: workspace.to_string_lossy().into_owned(),
        owner_id: "prx_user".into(),
        source: "agent".into(),
        run_id: "prx_pending".into(),
    };
    let before = repository
        .read_recovery_receipt(&chain)
        .expect("test: receipt")
        .expect("test: pending");
    assert_eq!(before.state, "pending");
    ticket.finish(true);
    drop(recorder);
    let generation = crate::config::ConfigGenerationManager::new(config.as_ref().clone()).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let recovery = RecoveryScope::prepare(
        &generation,
        Arc::clone(&memory),
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        observer(),
        None,
        &tracker,
        None,
    )
    .await
    .expect("test: plane-off prepare");
    let error = recovery
        .rejoin_terminal("prx_pending", "prx_user")
        .await
        .expect_err("pending stays unconfirmed");
    assert!(error.chain().any(|cause| cause.is::<LegacyUnconfirmed>()));
    let after = repository
        .read_recovery_receipt(&chain)
        .expect("test: receipt after")
        .expect("test: pending after");
    assert_eq!(before, after);
    assert_eq!(table_rows(&workspace, "recovery_attestations"), 1);
    assert_no_decision_files(&workspace);
}
