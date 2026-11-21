//! Restart recovery against the live registry, over a real SQLite fabric.
use super::*;
use crate::memory::{MemoryFabric, MemoryVisibility, MessageEventScope, SqliteMemory};
use crate::tools::sessions_spawn::process_control::ProcessRunControl;
use crate::tools::sessions_spawn::process_terminal::{
    RecoveredProcessOutcome, RecoveredProcessRun, RecoveredWorkerUsage,
};

const OWNER: &str = "owner-live";
const RUN: &str = "run-live";

/// The child input event a process recovery reads, recorded in real SQLite.
async fn recovered_run(temp: &tempfile::TempDir) -> RecoveredProcessRun {
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let fabric = MemoryFabric::new(memory, temp.path().to_string_lossy());
    let scope = MessageEventScope::new("sessions_spawn", MemoryVisibility::Workspace)
        .with_owner_id(OWNER)
        .with_session_key("child-session")
        .with_run_id(RUN);
    let source = fabric
        .record_inbound_user_message(scope, "child task", None, None)
        .await
        .unwrap();
    RecoveredProcessRun {
        agent_partition: None,
        source,
        batch_id: None,
        outcome: RecoveredProcessOutcome::OrdinaryUnconfirmed(RecoveredWorkerUsage {
            records: Vec::new(),
            unfinished_segments: 0,
        }),
    }
}

/// A registry row owned by `OWNER` whose source is the parent turn's event.
fn registry_row(control: Arc<ProcessRunControl>) -> SubAgentRun {
    let mut run = restore_test_run(RUN, SubAgentStatus::Running);
    run.owner_id = Some(OWNER.to_string());
    run.source_message_event_id = Some("parent-turn-event".to_string());
    run.process_control = Some(control);
    run
}

#[tokio::test]
async fn live_row_is_not_a_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let recovered = recovered_run(&temp).await;
    let runs = Arc::new(RwLock::new(vec![registry_row(ProcessRunControl::new())]));

    let restored = recovered.restore(&runs).await.expect("a live owner is not a conflict");

    assert!(!restored, "the live row keeps its sole owner");
    let rows = runs.read().await;
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].status, SubAgentStatus::Running));
    assert_eq!(rows[0].source_message_event_id.as_deref(), Some("parent-turn-event"));
}

#[tokio::test]
async fn recovery_only_row_with_another_source_is_a_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let recovered = recovered_run(&temp).await;
    let runs = Arc::new(RwLock::new(vec![registry_row(
        ProcessRunControl::recovered_unconfirmed(),
    )]));

    let error = recovered
        .restore(&runs)
        .await
        .expect_err("an upgrade must match its source");

    assert!(
        error.to_string().contains("conflicts with registry ownership"),
        "{error:#}"
    );
}
