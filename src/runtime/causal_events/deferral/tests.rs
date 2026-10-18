#![cfg(test)]

use crate::decision::telemetry::{
    CausalEvent, CausalEventFields, CausalEventKind, CausalEventSink, CausalLineage, testing::RecordingSink,
};
use crate::memory::{Memory, MemoryPrincipal, SqliteMemory};
use crate::runtime::causal_events::DurableCausalEventSink;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[tokio::test(flavor = "current_thread")]
async fn deferred_telemetry_remains_observable_and_drains_in_order_after_all_guards_drop() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    let generation = crate::config::new_shared(config).pin();
    let memory = Arc::new(SqliteMemory::new(workspace.path())?);
    let observer = Arc::new(RecordingSink::default());
    let sink = DurableCausalEventSink::new(&generation, memory.clone(), observer.clone())?;
    let first = sink.defer_ordinary_persistence();
    let second = sink.defer_ordinary_persistence();
    for kind in [CausalEventKind::SnapshotCreated, CausalEventKind::CandidateGated] {
        sink.emit(CausalEvent {
            kind,
            lineage: CausalLineage {
                workspace_id: Arc::from(workspace.path().to_string_lossy().as_ref()),
                owner_id: Some(Arc::from("prx_owner")),
                session_key: Some(Arc::from("prx_session")),
                ..CausalLineage::default()
            },
            fields: CausalEventFields::default(),
        });
    }
    assert_eq!(observer.events.lock().len(), 2);
    let principal = MemoryPrincipal {
        workspace_id: workspace.path().to_string_lossy().into_owned(),
        owner_id: Some("prx_owner".into()),
        session_key: Some("prx_session".into()),
        ..MemoryPrincipal::default()
    };
    let drain = sink.drain(Instant::now() + Duration::from_millis(500));
    tokio::pin!(drain);
    assert!(futures::poll!(drain.as_mut()).is_pending());
    tokio::task::yield_now().await;
    assert!(memory.list_message_events_since(&principal, 0, 10).await?.is_empty());
    drop(first);
    assert!(futures::poll!(drain.as_mut()).is_pending());
    tokio::task::yield_now().await;
    assert!(memory.list_message_events_since(&principal, 0, 10).await?.is_empty());
    drop(second);
    assert!(drain.await);
    let events = memory.list_message_events_since(&principal, 0, 10).await?;
    assert_eq!(
        events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>(),
        ["causal.snapshot.created", "causal.candidate.gated"]
    );
    assert_eq!(observer.events.lock().len(), 2);
    Ok(())
}
