//! Admission-local scheduling, with real durable storage and live guards.
#![cfg(test)]

use super::super::{DurableCausalEventSink, QueueItem};
use crate::decision::telemetry::{
    CausalEvent, CausalEventFields, CausalEventKind, CausalEventSink, CausalLineage, testing::RecordingSink,
};
use crate::memory::{Memory, MemoryPrincipal, SqliteMemory};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[tokio::test]
async fn overlapping_admissions_release_independently_and_controls_bypass_live_deferral() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    let generation = crate::config::new_shared(config).pin();
    let memory = Arc::new(SqliteMemory::new(workspace.path())?);
    let observer = Arc::new(RecordingSink::default());
    let sink = DurableCausalEventSink::new(&generation, memory.clone(), observer.clone())?;
    let lineage = |session: &str| CausalLineage {
        workspace_id: Arc::from(workspace.path().to_string_lossy().as_ref()),
        owner_id: Some(Arc::from("prx_owner")),
        session_key: Some(Arc::from(session)),
        source_event_id: Some(Arc::from(session)),
        ..Default::default()
    };
    let first_lineage = lineage("prx_first");
    let second_lineage = lineage("prx_second");
    let deadline = Instant::now() + Duration::from_secs(30);
    let first = sink.defer_ordinary_persistence(&first_lineage, deadline);
    let second = sink.defer_ordinary_persistence(&second_lineage, deadline);
    let event = |kind, lineage| CausalEvent {
        kind,
        lineage,
        fields: CausalEventFields::default(),
    };
    sink.emit(event(CausalEventKind::SnapshotCreated, first_lineage));
    sink.emit(event(CausalEventKind::CandidateGated, second_lineage.clone()));
    assert_eq!(observer.events.lock().len(), 2);
    let principal = |session: &str| MemoryPrincipal {
        workspace_id: workspace.path().to_string_lossy().into_owned(),
        owner_id: Some("prx_owner".into()),
        session_key: Some(session.into()),
        ..Default::default()
    };
    drop(first);
    tokio::time::timeout(Duration::from_secs(5), async {
        while memory
            .list_message_events_since(&principal("prx_first"), 0, 10)
            .await?
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
        anyhow::Ok(())
    })
    .await??;
    assert!(
        memory
            .list_message_events_since(&principal("prx_second"), 0, 10)
            .await?
            .is_empty()
    );
    // Exercise the worker's control queue directly; authenticated application
    // creation is covered separately by the admission/rejoin tests.
    let (reply, received) = tokio::sync::oneshot::channel();
    sink.queue
        .send(QueueItem::Applied {
            event: Box::new(event(CausalEventKind::DirectiveApplied, second_lineage)),
            idempotency_key: "prx-deferral-applied".into(),
            original_generation: None,
            acknowledgement: Some(reply),
        })
        .await?;
    assert!(tokio::time::timeout(Duration::from_secs(5), received).await??);
    let before_drain = memory
        .list_message_events_since(&principal("prx_second"), 0, 10)
        .await?;
    assert_eq!(before_drain.len(), 1);
    assert_eq!(
        before_drain.first().expect("one applied row").event_type,
        "causal.directive.applied"
    );
    // Two concurrent barriers must flush the earlier ordinary event while its
    // admission still holds the guard. Neither barrier depends on guard drop.
    let (a, b) = tokio::join!(
        sink.drain(Instant::now() + Duration::from_secs(5)),
        sink.drain(Instant::now() + Duration::from_secs(5))
    );
    assert!(a && b);
    let events = memory
        .list_message_events_since(&principal("prx_second"), 0, 10)
        .await?;
    assert_eq!(
        events.iter().map(|row| row.event_type.as_str()).collect::<Vec<_>>(),
        ["causal.directive.applied", "causal.candidate.gated"]
    );
    assert_eq!(observer.events.lock().len(), 3);
    let expiring_lineage = lineage("prx_expiring");
    let expiring = sink.defer_ordinary_persistence(&expiring_lineage, Instant::now() + Duration::from_millis(500));
    sink.emit(event(CausalEventKind::SnapshotCreated, expiring_lineage));
    tokio::time::timeout(Duration::from_secs(5), async {
        while memory
            .list_message_events_since(&principal("prx_expiring"), 0, 10)
            .await?
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
        anyhow::Ok(())
    })
    .await??;
    drop(expiring);
    drop(second);
    assert!(sink.drain(Instant::now() + Duration::from_secs(5)).await);
    assert_eq!(
        memory
            .list_message_events_since(&principal("prx_first"), 0, 10)
            .await?
            .len(),
        1
    );
    assert_eq!(
        memory
            .list_message_events_since(&principal("prx_second"), 0, 10)
            .await?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_live_guard_cannot_extend_its_deadline_and_readmission_releases_old_events() {
    let registry = Arc::new(super::PersistenceDeferral::default());
    let lineage = CausalLineage {
        source_event_id: Some(Arc::from("prx_source")),
        ..Default::default()
    };
    let first = registry.begin(
        &lineage,
        tokio::time::Instant::now().into_std() + Duration::from_millis(500),
    );
    let captured = registry.for_event(&lineage).unwrap();
    assert!(!captured.ready());
    tokio::time::advance(Duration::from_millis(500)).await;
    assert!(captured.ready());
    assert!(registry.for_event(&lineage).is_none());
    drop(first);
    let second = registry.begin(
        &lineage,
        tokio::time::Instant::now().into_std() + Duration::from_millis(500),
    );
    let captured = registry.for_event(&lineage).unwrap();
    let third = registry.begin(
        &lineage,
        tokio::time::Instant::now().into_std() + Duration::from_millis(500),
    );
    assert!(captured.ready());
    assert!(!registry.for_event(&lineage).unwrap().ready());
    drop(second);
    assert!(!registry.for_event(&lineage).unwrap().ready());
    drop(third);
    assert!(registry.for_event(&lineage).is_none());
}
