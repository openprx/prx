#![cfg(test)]

use super::{recovery_config, recovery_input, recovery_recorder};
use crate::memory::Memory;
use crate::memory::MessageEventSource;
use crate::memory::SqliteMemory;
use crate::memory::ingress_recovery::{IngressAttestationError, IngressCapsule, IngressRecoveryScope};
use crate::memory::recovery_authority::{RecoveryChain, RecoveryPublication};
use crate::runtime::ingress_recovery::tombstone_fixture;
use crate::runtime::ingress_recovery::{
    IngressRecorder, LegacyUnconfirmed, RecoveryScope, SettledTerminalFacts, backend_memory,
};
use crate::runtime::turn_cleanup::CleanupTracker;
use crate::skills::lifecycle::{TerminalKind, take_settled_skill_observations};
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

#[derive(Clone, Copy)]
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

fn assert_exact_error(error: &anyhow::Error, expected: &str) {
    assert!(
        error.chain().any(|cause| cause.to_string() == expected),
        "expected exact error {expected}, got {error:#}"
    );
}

async fn assert_plane_on_legacy_rejoin(cell: &MatrixCell, recovery: &RecoveryScope, owner_id: &str, seeded: bool) {
    match (cell.backend, cell.plane, cell.recording, cell.k, cell.l) {
        ("sqlite", true, true, true, true) => {
            assert!(seeded, "sqlite plane-on recording+k+l seeds a replayable terminal");
            let store = recovery.legacy_store().expect("test: plane-on store");
            let before = store.reader().counts().expect("test: outcomes before").outcomes;
            recovery
                .rejoin_terminal("prx_legacy", owner_id)
                .await
                .expect("test: seeded plane-on rejoin");
            let after = store.reader().counts().expect("test: outcomes after").outcomes;
            let directive = recovery.spies().directive_repair();
            assert!(
                after > before || directive > 0,
                "sqlite plane-on rejoin must write an outcomes row or repair the chain (before={before} after={after} directive={directive})"
            );
        }
        ("sqlite", true, true, false, true) => {
            let error = recovery
                .rejoin_terminal("prx_legacy", owner_id)
                .await
                .expect_err("test: sqlite k-false rejoin");
            assert_exact_error(&error, "terminal rejoin requires recording plus recovery authority");
        }
        ("postgres_class", true, true, false, true) => {
            let error = recovery
                .rejoin_terminal("prx_legacy", owner_id)
                .await
                .expect_err("test: postgres_class dummy rejoin");
            assert_exact_error(&error, "terminal replay payload is missing");
        }
        (backend, plane, recording, k, l) => {
            panic!("test: unhandled plane-on repair cell {backend} plane={plane} rec={recording} k={k} l={l}")
        }
    }
}

fn observer() -> Arc<dyn crate::decision::telemetry::CausalEventSink> {
    Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::new(
        crate::observability::NoopObserver,
    )))
}

async fn open_advisory(
    generation: &crate::config::ConfigGeneration,
    memory: Arc<dyn Memory>,
    tracker: &Arc<CleanupTracker>,
) -> crate::runtime::ingress_recovery::PreparedAdvisory {
    let (store, events) =
        crate::runtime::ingress_recovery::open_advisory_store(generation, memory, observer(), tracker)
            .await
            .expect("test: advisory store");
    crate::runtime::ingress_recovery::PreparedAdvisory::opened(store, events)
        .expect("test: advisory store requires a writer")
}

async fn prepare_recovery(
    generation: &crate::config::ConfigGeneration,
    memory: Arc<dyn Memory>,
    tracker: &Arc<CleanupTracker>,
    workspace_key: Option<Arc<crate::decision::key::DecisionKey>>,
    advisory: crate::runtime::ingress_recovery::PreparedAdvisory,
) -> Arc<RecoveryScope> {
    RecoveryScope::prepare(
        generation,
        memory,
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        None,
        tracker,
        workspace_key,
        advisory,
    )
    .await
    .expect("test: recovery prepare")
}

/// Literal sqlite 16-cell table: (plane, recording, k, l) → (recordable, legacy_some).
const SQLITE_MATRIX: [((bool, bool, bool, bool), (bool, bool)); 16] = [
    ((true, true, true, true), (true, true)),
    ((true, true, true, false), (true, true)),
    ((true, true, false, true), (false, true)),
    ((true, true, false, false), (false, true)),
    ((true, false, true, true), (false, false)),
    ((true, false, true, false), (false, false)),
    ((true, false, false, true), (false, false)),
    ((true, false, false, false), (false, false)),
    ((false, true, true, true), (true, false)),
    ((false, true, true, false), (true, false)),
    ((false, true, false, true), (false, false)),
    ((false, true, false, false), (false, false)),
    ((false, false, true, true), (false, false)),
    ((false, false, true, false), (false, false)),
    ((false, false, false, true), (false, false)),
    ((false, false, false, false), (false, false)),
];

fn expected_recordable_legacy(cell: &MatrixCell) -> (bool, bool) {
    if cell.backend == "sqlite" {
        return SQLITE_MATRIX
            .iter()
            .find(|(key, _)| *key == (cell.plane, cell.recording, cell.k, cell.l))
            .map(|(_, expected)| *expected)
            .expect("test: sqlite matrix cell");
    }
    match (cell.backend, cell.plane, cell.recording, cell.k, cell.l) {
        ("lucid", false, true, true, true) => (true, false),
        ("encrypted", false, true, true, true) => (true, false),
        ("lucid", true, true, true, false) => (true, true),
        ("encrypted", true, true, true, false) => (true, true),
        ("postgres_class", true, true, false, true) => (true, true),
        ("postgres_class", false, true, false, true) => (false, false),
        _ => panic!(
            "test: extra matrix cell {} plane={} rec={} k={} l={}",
            cell.backend, cell.plane, cell.recording, cell.k, cell.l
        ),
    }
}

fn decision_key_snapshot(workspace: &Path) -> (bool, Option<Vec<u8>>, Option<std::time::SystemTime>) {
    let path = workspace.join("self/causal_tree/.decision_key");
    (
        path.exists(),
        std::fs::read(&path).ok(),
        std::fs::metadata(&path).ok().and_then(|meta| meta.modified().ok()),
    )
}

fn plant_v2_capsule(workspace: &Path, namespace: &str, run: &str) -> crate::memory::MessageEventInput {
    let event_id = uuid::Uuid::new_v4().to_string();
    let capsule = IngressCapsule {
        version: 2,
        key_id: None,
        backend_namespace: namespace.to_owned(),
        scope: IngressRecoveryScope {
            workspace_id: workspace.to_string_lossy().into_owned(),
            owner_id: "prx_user".into(),
            source: MessageEventSource::Agent,
        },
        lifetime: uuid::Uuid::nil().to_string(),
        input_event_id: event_id.clone(),
        input_key: format!("turn:{run}:input"),
        run_id: run.to_owned(),
        input_digest: "00".repeat(32),
        authentication: "00".repeat(32),
    };
    let mut input = recovery_input(workspace, run);
    input.event_id = Some(event_id);
    input.raw_payload_json = Some(
        serde_json::json!({
            "prx_ingress_capsule": capsule,
            "original_payload_json": serde_json::Value::Null
        })
        .to_string(),
    );
    input
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
    let seed_replay = backend == "sqlite" && plane && recording && k && l;
    let rejoin_owner = if seed_replay {
        let (terminal_id, owner) =
            crate::agent::terminal::seed_replayable_completed_terminal(&workspace, Arc::clone(&memory)).await;
        assert_eq!(terminal_id, "prx_legacy");
        owner
    } else {
        if l {
            let mut input = recovery_input(&workspace, "prx_legacy");
            input.idempotency_key = Some("turn:prx_legacy:final".into());
            memory.append_message_event(input).await.expect("test: legacy final");
        }
        "prx_user".into()
    };
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
    let (recordable, open_legacy) = expected_recordable_legacy(&MatrixCell {
        backend,
        plane,
        recording,
        k,
        l,
    });
    let advisory = if open_legacy {
        open_advisory(&generation, Arc::clone(&memory), &tracker).await
    } else {
        crate::runtime::ingress_recovery::PreparedAdvisory::default()
    };
    let recovery = prepare_recovery(&generation, Arc::clone(&memory), &tracker, workspace_key, advisory).await;
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
        assert!(recovery.legacy_writer().is_some(), "{backend} plane-on repair writer");
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
    if l && open_legacy {
        assert_plane_on_legacy_rejoin(&cell, &recovery, &rejoin_owner, seed_replay).await;
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
    seed_v2_capsule_plane_off_does_not_open_decision_key().await;
}

async fn seed_v2_capsule_plane_off_does_not_open_decision_key() {
    let (_dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = backend_memory("sqlite", &workspace).expect("test: sqlite");
    let config = Arc::new(cell_config(&workspace, false, true));
    let (recorder, ticket) = recovery_recorder(Arc::clone(&memory), &config).await;
    let listing_before = decision_tree(&workspace);
    let key_before = decision_key_snapshot(&workspace);
    assert!(!key_before.0, "plane-off prepare must not create .decision_key");
    let planted = memory
        .append_message_event(plant_v2_capsule(&workspace, recorder.backend_namespace(), "prx_v2"))
        .await
        .expect("test: plant v2 capsule");
    let error = recorder
        .verify_receipt(&planted)
        .expect_err("plane-off v2 verify must not open the key");
    assert!(
        error.chain().any(|cause| cause.is::<LegacyUnconfirmed>()),
        "plane-off v2 capsule is LegacyUnconfirmed: {error}"
    );
    assert_eq!(decision_tree(&workspace), listing_before);
    assert_eq!(decision_key_snapshot(&workspace), key_before);
    assert_no_decision_files(&workspace);

    let key_path = workspace.join("self/causal_tree/.decision_key");
    std::fs::create_dir_all(key_path.parent().expect("test: key dir")).expect("test: key dir");
    std::fs::write(&key_path, [42u8; 32]).expect("test: fixture key");
    let seeded = decision_key_snapshot(&workspace);
    let listing_seeded = decision_tree(&workspace);
    let error = recorder
        .verify_receipt(&planted)
        .expect_err("plane-off v2 verify must not open a seeded key");
    assert!(error.chain().any(|cause| cause.is::<LegacyUnconfirmed>()));
    assert_eq!(decision_key_snapshot(&workspace), seeded);
    assert_eq!(decision_tree(&workspace), listing_seeded);
    ticket.finish(true);
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
    let _ = take_settled_skill_observations();
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
    let recovery = prepare_recovery(
        &generation,
        Arc::clone(&memory),
        &tracker,
        None,
        crate::runtime::ingress_recovery::PreparedAdvisory::default(),
    )
    .await;
    assert!(recovery.ingress_recovery_enabled());
    assert!(recovery.authority.legacy.is_none());
    recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect("test: settled rejoin");
    crate::skills::lifecycle::drain_background_for_test("prx_run").await;
    let settled_obs = take_settled_skill_observations();
    assert!(
        settled_obs.iter().any(|(id, kind, awaiting, verified)| {
            id == "prx_run" && *kind == TerminalKind::Completed && !*awaiting && *verified
        }),
        "skill settle must run from skill_observation (Completed, false, true): {settled_obs:?}"
    );
    assert!(recovery.spies().rejoin() >= 1, "rejoin spy");
    assert_eq!(
        recovery.spies().directive_repair(),
        0,
        "plane-off skips directive repair"
    );
    assert_eq!(
        recovery.spies().child_recovery(),
        0,
        "off-writes does not invoke child recovery"
    );
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

    let generation = crate::config::ConfigGenerationManager::new(cell_config(&workspace, true, true)).pin();
    let tracker_on = Arc::new(CleanupTracker::default());
    let plane_on_without_key = prepare_recovery(
        &generation,
        Arc::clone(&memory),
        &tracker_on,
        None,
        crate::runtime::ingress_recovery::PreparedAdvisory::default(),
    )
    .await;
    assert!(plane_on_without_key.authority.legacy.is_none());
    assert!(
        crate::runtime::turn_decision::child_recovery_gate(&plane_on_without_key, true, true, true).is_some(),
        "plane-on without key skips child recovery"
    );
    assert!(plane_on_without_key.spies().child_recovery() >= 1, "child recovery spy");
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
    let advisory = open_advisory(&generation, Arc::clone(&memory), &tracker).await;
    let recovery = prepare_recovery(&generation, Arc::clone(&memory), &tracker, Some(key), advisory).await;
    assert!(recovery.authority.legacy.is_some());
    let error = recovery
        .rejoin_terminal("prx_legacy", "prx_user")
        .await
        .expect_err("test: dummy legacy-chain rejoin");
    assert_exact_error(&error, "terminal replay payload is missing");
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
    let recovery = prepare_recovery(
        &generation,
        Arc::clone(&memory),
        &tracker,
        None,
        crate::runtime::ingress_recovery::PreparedAdvisory::default(),
    )
    .await;
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

struct SettledSetup {
    dir: tempfile::TempDir,
    workspace: std::path::PathBuf,
    memory: Arc<dyn Memory>,
    recovery: Arc<RecoveryScope>,
    tracker: Arc<CleanupTracker>,
    chain: RecoveryChain,
    input_id: String,
    final_id: String,
    key_id: String,
}

async fn settled_setup_with(facts: SettledTerminalFacts, facts_bytes: Option<Vec<u8>>) -> SettledSetup {
    let (dir, workspace) = home_workspace();
    let memory: Arc<dyn Memory> = backend_memory("sqlite", &workspace).expect("test: sqlite");
    let config = Arc::new(cell_config(&workspace, false, true));
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
    let mut facts = facts;
    if facts.key_id.is_empty() {
        facts.key_id = pending.key_id.clone();
    }
    if facts.claim_event_id.is_empty() {
        facts.claim_event_id = input.event_id.clone();
    }
    if facts.final_event_id.is_empty() {
        facts.final_event_id = final_event.event_id.clone();
    }
    let bytes = if let Some(raw) = facts_bytes {
        let mut value: serde_json::Value = serde_json::from_slice(&raw).expect("test: facts patch");
        let object = value.as_object_mut().expect("test: facts object");
        object.insert("claim_event_id".into(), facts.claim_event_id.clone().into());
        object.insert("final_event_id".into(), facts.final_event_id.clone().into());
        object.insert("key_id".into(), facts.key_id.clone().into());
        object.insert("terminal_id".into(), facts.terminal_id.clone().into());
        serde_json::to_vec(&value).expect("test: patched facts")
    } else {
        serde_json::to_vec(&facts).expect("test: facts")
    };
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
            facts: bytes,
        })
        .expect("test: settle");
    ticket.finish(true);
    drop(recorder);
    let generation = crate::config::ConfigGenerationManager::new(config.as_ref().clone()).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let recovery = prepare_recovery(
        &generation,
        Arc::clone(&memory),
        &tracker,
        None,
        crate::runtime::ingress_recovery::PreparedAdvisory::default(),
    )
    .await;
    SettledSetup {
        dir,
        workspace,
        memory,
        recovery,
        tracker,
        chain,
        input_id: input.event_id,
        final_id: final_event.event_id,
        key_id: pending.key_id,
    }
}

fn default_facts() -> SettledTerminalFacts {
    settled_facts("prx_run", "", "", "")
}

fn is_mismatch(error: &anyhow::Error, field: &'static str) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<IngressAttestationError>()
            .is_some_and(|attestation| *attestation == IngressAttestationError::Mismatch(field))
            || cause.to_string().contains(&format!("`{field}`"))
    })
}

fn is_unknown_key(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<IngressAttestationError>() && cause.to_string().contains("unknown key"))
}

fn sqlite(workspace: &Path) -> SqliteMemory {
    SqliteMemory::new(workspace).expect("test: sqlite accessor")
}

pub(super) async fn recovery_settled_receipt_each_bound_field_is_refused() {
    let workspace_cell = settled_setup_with(default_facts(), None).await;
    let (other_dir, other_ws) = home_workspace();
    let mut other_config = cell_config(&other_ws, false, true);
    other_config.workspace_dir = other_ws.clone();
    let generation = crate::config::ConfigGenerationManager::new(other_config).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let foreign = prepare_recovery(
        &generation,
        Arc::clone(&workspace_cell.memory),
        &tracker,
        None,
        crate::runtime::ingress_recovery::PreparedAdvisory::default(),
    )
    .await;
    let error = foreign
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("workspace");
    assert!(
        error.to_string().contains("committed terminal marker is missing") || is_mismatch(&error, "workspace"),
        "workspace: {error}"
    );
    drop(other_dir);

    let owner_cell = settled_setup_with(default_facts(), None).await;
    let error = owner_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_other")
        .await
        .expect_err("owner");
    assert!(
        error
            .to_string()
            .contains("terminal replay scope does not match its owner")
            || is_mismatch(&error, "owner"),
        "owner: {error}"
    );

    let source_cell = settled_setup_with(default_facts(), None).await;
    sqlite(&source_cell.workspace)
        .replace_event_source_for_test("prx_run", "channel")
        .expect("test: source");
    let error = source_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("source");
    assert!(
        error.chain().any(|cause| cause.is::<LegacyUnconfirmed>())
            || is_mismatch(&error, "source")
            || error.to_string().contains("Query returned no rows"),
        "source: {error}"
    );

    let run_cell = settled_setup_with(default_facts(), None).await;
    let error = run_cell
        .recovery
        .rejoin_terminal("prx_other", "prx_user")
        .await
        .expect_err("run");
    assert!(
        error.to_string().contains("committed terminal marker is missing") || is_mismatch(&error, "run"),
        "run: {error}"
    );

    let mut input_facts = default_facts();
    input_facts.claim_event_id = uuid::Uuid::new_v4().to_string();
    let input_cell = settled_setup_with(input_facts, None).await;
    let error = input_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("input event");
    assert!(
        is_mismatch(&error, "claim_event_id")
            || error.to_string().contains("QueryReturnedNoRows")
            || error.to_string().contains("no rows"),
        "input event: {error:#}"
    );

    let mut terminal_facts = default_facts();
    terminal_facts.final_event_id = uuid::Uuid::new_v4().to_string();
    let terminal_cell = settled_setup_with(terminal_facts, None).await;
    let error = terminal_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("terminal event");
    assert!(
        is_mismatch(&error, "final_event_id")
            || error.to_string().contains("QueryReturnedNoRows")
            || error.to_string().contains("no rows"),
        "terminal event: {error:#}"
    );

    let bytes_cell = settled_setup_with(default_facts(), None).await;
    sqlite(&bytes_cell.workspace)
        .replace_event_content_for_test(&bytes_cell.final_id, "mutated")
        .expect("test: terminal bytes");
    let error = bytes_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("terminal bytes");
    assert!(
        error.to_string().contains("recovery evidence changed") || error.to_string().contains("invalid recovery"),
        "terminal bytes: {error:#}"
    );

    let mut key_facts = default_facts();
    key_facts.key_id = uuid::Uuid::nil().to_string();
    let key_cell = settled_setup_with(key_facts, None).await;
    let error = key_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("key id");
    assert!(is_mismatch(&error, "key_id"), "key id: {error:#}");

    let relabel = settled_setup_with(default_facts(), None).await;
    let relabel_err = sqlite(&relabel.workspace)
        .replace_recovery_attestation_state_for_test("pending")
        .expect_err("state relabel");
    assert!(
        relabel_err.to_string().contains("immutable recovery receipt"),
        "state relabel: {relabel_err}"
    );

    let mut unknown = serde_json::to_value(default_facts()).expect("test: facts value");
    unknown
        .as_object_mut()
        .expect("test: facts object")
        .insert("extra".into(), serde_json::json!(1));
    let unknown_cell = settled_setup_with(
        default_facts(),
        Some(serde_json::to_vec(&unknown).expect("test: extra")),
    )
    .await;
    let error = unknown_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("facts unknown key");
    assert!(is_unknown_key(&error), "facts unknown key: {error:#}");

    let mut surface_facts = default_facts();
    surface_facts.surface = "channel".into();
    let surface_cell = settled_setup_with(surface_facts, None).await;
    let error = surface_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("surface");
    assert!(is_mismatch(&error, "surface"), "surface: {error:#}");

    let mut parent_facts = default_facts();
    parent_facts.parent_authority_id = Some(uuid::Uuid::nil().to_string());
    let parent_cell = settled_setup_with(parent_facts, None).await;
    let error = parent_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("parent_authority_id");
    assert!(
        is_mismatch(&error, "parent_authority_id"),
        "parent_authority_id: {error:#}"
    );

    let mut skill = serde_json::to_value(default_facts()).expect("test: skill value");
    skill
        .get_mut("skill_observation")
        .and_then(serde_json::Value::as_object_mut)
        .expect("test: skill object")
        .insert("extra".into(), serde_json::json!(false));
    let skill_cell = settled_setup_with(
        default_facts(),
        Some(serde_json::to_vec(&skill).expect("test: skill extra")),
    )
    .await;
    let error = skill_cell
        .recovery
        .rejoin_terminal("prx_run", "prx_user")
        .await
        .expect_err("skill_observation");
    assert!(is_unknown_key(&error), "skill_observation: {error:#}");

    let _keep = (
        &workspace_cell.dir,
        &workspace_cell.tracker,
        workspace_cell.input_id,
        workspace_cell.final_id,
        workspace_cell.key_id,
        workspace_cell.chain,
    );
}
