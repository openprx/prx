#![cfg(test)]

use super::recovery_config;
use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::store::testing::{open_private, record};
use crate::causal_tree::store::{CausalDecisionStore, Enqueue, arm_wal_reprobe_gate, db_path};
use crate::decision::telemetry::testing::RecordingSink;
use crate::memory::{Memory, SqliteMemory};
use crate::runtime::ingress_recovery::{PreparedAdvisory, RecoveryScope};
use crate::runtime::turn_cleanup::CleanupTracker;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

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

struct DecisionFiles {
    key_bytes: Option<Vec<u8>>,
    key_mtime: Option<SystemTime>,
    db_bytes: Option<Vec<u8>>,
    db_mtime: Option<SystemTime>,
    wal_bytes: Option<Vec<u8>>,
    names: BTreeSet<String>,
}

fn file_bytes_mtime(path: &Path) -> (Option<Vec<u8>>, Option<SystemTime>) {
    std::fs::metadata(path).map_or((None, None), |meta| (std::fs::read(path).ok(), meta.modified().ok()))
}

fn snapshot_decision_files(workspace: &Path) -> DecisionFiles {
    let tree = workspace.join("self/causal_tree");
    let (key_bytes, key_mtime) = file_bytes_mtime(&tree.join(".decision_key"));
    let (db_bytes, db_mtime) = file_bytes_mtime(&tree.join("decisions.db"));
    let (wal_bytes, _) = file_bytes_mtime(&tree.join("decisions.db-wal"));
    let names = std::fs::read_dir(&tree).map_or_else(
        |_| BTreeSet::new(),
        |entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect()
        },
    );
    DecisionFiles {
        key_bytes,
        key_mtime,
        db_bytes,
        db_mtime,
        wal_bytes,
        names,
    }
}

fn assert_files_unchanged(before: &DecisionFiles, after: &DecisionFiles) {
    assert_eq!(before.key_bytes, after.key_bytes, "test: .decision_key bytes unchanged");
    assert_eq!(before.key_mtime, after.key_mtime, "test: .decision_key mtime unchanged");
    assert_eq!(before.db_bytes, after.db_bytes, "test: decisions.db bytes unchanged");
    assert_eq!(before.db_mtime, after.db_mtime, "test: decisions.db mtime unchanged");
    assert_eq!(before.wal_bytes, after.wal_bytes, "test: -wal bytes unchanged");
    let strip = |names: &BTreeSet<String>| {
        names
            .iter()
            .filter(|name| !name.ends_with("-shm"))
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        strip(&before.names),
        strip(&after.names),
        "test: no file created besides -shm"
    );
}

fn events() -> Arc<RecordingSink> {
    Arc::new(RecordingSink::default())
}

fn seeded_record(n: u8) -> crate::causal_tree::store::DecisionRecord {
    record(DecisionId::legacy(&[n; 32]), "ws", Some("prx_user"), 10)
}

async fn flush(store: &CausalDecisionStore) {
    assert!(
        store
            .writer()
            .expect("test: writer")
            .drain(Instant::now() + Duration::from_secs(30))
            .await,
        "test: writer drain"
    );
}

async fn prepare_recovery_only(workspace: &Path, memory: Arc<dyn Memory>) -> (Arc<RecoveryScope>, Arc<CleanupTracker>) {
    let mut config = recovery_config(workspace);
    config.causal_tree.enabled = true;
    let generation = crate::config::ConfigGenerationManager::new(config).pin();
    let tracker = Arc::new(CleanupTracker::default());
    let recovery = RecoveryScope::prepare(
        &generation,
        memory,
        crate::causal_tree::state::TurnSurfaceKind::AgentLoop,
        None,
        &tracker,
        None,
        PreparedAdvisory::default(),
    )
    .await
    .expect("test: recovery-only prepare");
    (recovery, tracker)
}

fn assert_recovery_only_reader(recovery: &RecoveryScope) {
    assert!(recovery.legacy_store().is_some(), "test: recovery-only legacy store");
    assert!(recovery.legacy_writer().is_none(), "test: recovery-only writer is None");
    assert!(recovery.legacy_events().is_none(), "test: recovery-only events is None");
    assert_eq!(
        recovery.legacy_store().expect("test: store").mode(),
        crate::causal_tree::store::StoreMode::ReadOnly,
        "test: recovery-only never opens ReadWrite"
    );
}

pub(super) async fn recovery_legacy_reader_creates_no_files() {
    // Cell 1: no legacy state. Missing key and db ⇒ legacy = None; no files created.
    {
        let dir = tempfile::tempdir().expect("test: workspace");
        let workspace = dir.path();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
        let before = snapshot_decision_files(workspace);
        let (recovery, tracker) = prepare_recovery_only(workspace, memory).await;
        assert!(
            recovery.legacy_store().is_none(),
            "test: no-legacy cell leaves legacy unconfirmed"
        );
        assert!(recovery.legacy_writer().is_none());
        assert!(recovery.legacy_events().is_none());
        let after = snapshot_decision_files(workspace);
        assert_files_unchanged(&before, &after);
        assert!(
            !workspace.join("self/causal_tree/.decision_key").exists(),
            "test: no-legacy cell must not create .decision_key"
        );
        assert!(
            !workspace.join("self/causal_tree/decisions.db").exists(),
            "test: no-legacy cell must not create decisions.db"
        );
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: drain");
    }

    // Cell 2: legacy db without -wal (immutable=1). Checkpointed row is visible.
    {
        let dir = tempfile::tempdir().expect("test: workspace");
        let workspace = dir.path();
        crate::decision::key::DecisionKey::load_or_create(workspace).expect("test: key");
        let store = open_private(workspace, events());
        assert_eq!(
            store.writer().expect("test: writer").record_decision(seeded_record(1)),
            Enqueue::Queued
        );
        flush(&store).await;
        CausalDecisionStore::close(Arc::new(store), Instant::now() + Duration::from_secs(5)).await;
        assert!(
            !std::path::Path::new(&format!("{}-wal", db_path(workspace).display())).exists()
                || std::fs::metadata(format!("{}-wal", db_path(workspace).display()))
                    .map(|meta| meta.len() == 0)
                    .unwrap_or(true),
            "test: checkpointed store has no live WAL"
        );
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
        let before = snapshot_decision_files(workspace);
        let (recovery, tracker) = prepare_recovery_only(workspace, memory).await;
        assert_recovery_only_reader(&recovery);
        assert_eq!(
            recovery
                .legacy_store()
                .expect("test: store")
                .reader()
                .counts()
                .expect("test: counts")
                .decisions,
            1,
            "test: immutable read sees the checkpointed row"
        );
        let after = snapshot_decision_files(workspace);
        assert_files_unchanged(&before, &after);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: drain");
    }

    // Cell 3: live WAL, writer held open, uncheckpointed row visible (mode=ro).
    {
        let dir = tempfile::tempdir().expect("test: workspace");
        let workspace = dir.path();
        crate::decision::key::DecisionKey::load_or_create(workspace).expect("test: key");
        let store = open_private(workspace, events());
        assert_eq!(
            store.writer().expect("test: writer").record_decision(seeded_record(1)),
            Enqueue::Queued
        );
        flush(&store).await;
        assert!(
            std::fs::metadata(format!("{}-wal", db_path(workspace).display()))
                .map(|meta| meta.len() > 0)
                .unwrap_or(false),
            "test: held writer leaves a live WAL"
        );
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
        let before = snapshot_decision_files(workspace);
        let (recovery, tracker) = prepare_recovery_only(workspace, memory).await;
        assert_recovery_only_reader(&recovery);
        assert_eq!(
            recovery
                .legacy_store()
                .expect("test: store")
                .reader()
                .counts()
                .expect("test: counts")
                .decisions,
            1,
            "test: mode=ro reader sees the uncheckpointed row"
        );
        let after = snapshot_decision_files(workspace);
        assert_files_unchanged(&before, &after);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: drain");
        drop(store);
    }

    // Cell 4: WAL created between the probe and the end of the immutable=1
    // read (test barrier) ⇒ result discarded; mode=ro re-read sees the row.
    {
        let dir = tempfile::tempdir().expect("test: workspace");
        let workspace = dir.path();
        crate::decision::key::DecisionKey::load_or_create(workspace).expect("test: key");
        let writer_store = open_private(workspace, events());
        assert_eq!(
            writer_store
                .writer()
                .expect("test: writer")
                .record_decision(seeded_record(1)),
            Enqueue::Queued
        );
        flush(&writer_store).await;
        {
            let conn = rusqlite::Connection::open(writer_store.path()).expect("test: checkpoint conn");
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .expect("test: truncate WAL before the immutable probe");
        }
        assert!(
            !std::fs::metadata(format!("{}-wal", writer_store.path().display())).is_ok_and(|meta| meta.len() > 0),
            "test: truncated WAL so the recovery probe uses immutable=1"
        );
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: sqlite"));
        let (recovery, tracker) = prepare_recovery_only(workspace, Arc::clone(&memory)).await;
        assert_recovery_only_reader(&recovery);
        let (probed_tx, probed_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        arm_wal_reprobe_gate(probed_tx, resume_rx);
        let reader = recovery.legacy_store().expect("test: store").reader();
        let read_task = tokio::task::spawn_blocking(move || reader.counts());
        tokio::task::spawn_blocking(move || probed_rx.recv())
            .await
            .expect("test: join probed")
            .expect("test: immutable probe reached the barrier");
        assert_eq!(
            writer_store
                .writer()
                .expect("test: writer")
                .record_decision(seeded_record(2)),
            Enqueue::Queued
        );
        flush(&writer_store).await;
        assert!(
            std::fs::metadata(format!("{}-wal", writer_store.path().display())).is_ok_and(|meta| meta.len() > 0),
            "test: barrier write leaves a live WAL"
        );
        assert_eq!(
            writer_store.reader().counts().expect("test: writer counts").decisions,
            2,
            "test: writer sees the uncheckpointed row"
        );
        let mid = snapshot_decision_files(workspace);
        resume_tx.send(()).expect("test: resume immutable read");
        let counts = read_task
            .await
            .expect("test: join counts")
            .expect("test: counts after re-probe");
        assert_eq!(
            counts.decisions, 2,
            "test: discarded immutable read; mode=ro re-read sees the uncheckpointed row"
        );
        let after = snapshot_decision_files(workspace);
        assert_files_unchanged(&mid, &after);
        tracker
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .expect("test: drain");
        drop(writer_store);
    }
}
