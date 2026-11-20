#![cfg(test)]
//! Per-row recovery outcomes over the real SQLite store and real lock files:
//! no prior row stops new input, and each ends in one durable outcome.
use super::*;
use crate::memory::ingress_recovery::OrphanCause;
use crate::memory::sqlite::{IngressLockReach, UnsettledIngressRow, inspect_unsettled_ingress};

/// Most lock directories a fixture workspace may hold.
const MAX_LOCK_DIRECTORIES: usize = 8;

/// A prior input recorded by its own lifetime, whose process then ended: the
/// recorder, its cleanup ticket and the store handle are all released.
async fn ended_prior_input(dir: &Path, run: &str) -> (MessageEvent, IngressCapsule) {
    let config = recovery_config(dir);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir).expect("test: prior store"));
    let (recorder, ticket) = recovery_recorder(memory.clone(), &config).await;
    let event = recorder
        .append(recovery_input(dir, run))
        .await
        .expect("test: prior input recorded");
    ticket.finish(true);
    drop(recorder);
    drop(memory);
    let capsule = capsule_from_event(&event).expect("test: prior capsule");
    (event, capsule)
}

/// The workspace's single lock directory, as a st_dev change would orphan it.
fn lock_directory(dir: &Path) -> PathBuf {
    let root = dir.join(".prx-ingress-recovery-v1");
    let entries: Vec<_> = std::fs::read_dir(&root)
        .expect("test: lock root")
        .take(MAX_LOCK_DIRECTORIES + 1)
        .map(|entry| entry.expect("test: lock entry").path())
        .collect();
    assert_eq!(entries.len(), 1, "test: one lock directory: {entries:?}");
    entries.into_iter().next().expect("test: lock directory")
}

fn unsettled(dir: &Path) -> Vec<UnsettledIngressRow> {
    let inspection = inspect_unsettled_ingress(&dir.join("memory/brain.db")).expect("test: read-only inspection");
    assert!(!inspection.truncated, "test: bounded fixture");
    inspection.rows
}

fn row_of<'a>(rows: &'a [UnsettledIngressRow], capsule: &IngressCapsule) -> Option<&'a UnsettledIngressRow> {
    rows.iter().find(|row| row.run_id == capsule.run_id)
}

async fn evidence(memory: &Arc<dyn Memory>, dir: &Path, event: &MessageEvent, suffix: &str) -> Option<MessageEvent> {
    memory
        .find_owned_message_event_by_idempotency_key(
            &dir.to_string_lossy(),
            "prx_user",
            &format!("ingress:{}:{suffix}", event.event_id),
        )
        .await
        .expect("test: evidence lookup")
}

fn binding(event: &MessageEvent) -> serde_json::Value {
    let payload: serde_json::Value =
        serde_json::from_str(event.raw_payload_json.as_deref().expect("test: evidence payload"))
            .expect("test: evidence JSON");
    payload.get("ingress_recovery").expect("test: evidence binding").clone()
}

/// A new input is admitted by a fresh lifetime; it returns the new event.
async fn next_input(memory: &Arc<dyn Memory>, dir: &Path, run: &str) -> MessageEvent {
    let (recorder, ticket) = recovery_recorder(memory.clone(), &recovery_config(dir)).await;
    let event = recorder
        .append(recovery_input(dir, run))
        .await
        .expect("test: new input must be accepted");
    ticket.finish(true);
    event
}

/// An orphan of either cause: the row is `unconfirmed`, its evidence binds the
/// cause, it carries no input content, and no other outcome was written.
async fn assert_orphaned(memory: &Arc<dyn Memory>, dir: &Path, prior: &MessageEvent, cause: OrphanCause) {
    let interrupted = evidence(memory, dir, prior, "unconfirmed")
        .await
        .expect("test: the orphan is recorded unconfirmed");
    assert_eq!(interrupted.event_type, "turn.ingress.interrupted");
    assert_eq!(
        binding(&interrupted).get("cause").and_then(serde_json::Value::as_str),
        Some(cause.as_str())
    );
    assert!(interrupted.content.ends_with(&format!("cause={}", cause.as_str())));
    assert!(!interrupted.content.contains("recovery fixture"), "no input content");
    for absent in ["reconciled", "quarantine"] {
        assert!(
            evidence(memory, dir, prior, absent).await.is_none(),
            "test: no {absent} outcome"
        );
    }
}

#[tokio::test]
async fn a_missing_lock_orphans_the_row_and_new_input_is_accepted() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let (prior, capsule) = ended_prior_input(dir.path(), "prx_prior_run").await;
    // A reboot that renumbers the volume moved every old lock out of reach.
    let locks = lock_directory(dir.path());
    std::fs::rename(&locks, locks.with_extension("moved")).expect("test: move the lock directory");

    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: next store"));
    let next = next_input(&memory, dir.path(), "prx_next_run").await;
    assert_ne!(next.event_id, prior.event_id);

    assert_orphaned(&memory, dir.path(), &prior, OrphanCause::LockMissing).await;
    let rows = unsettled(dir.path());
    let row = row_of(&rows, &capsule).expect("test: the orphan stays visible");
    assert_eq!(row.state, IngressRecoveryState::Unconfirmed);
    assert_eq!(row.cause, Some(OrphanCause::LockMissing));
    assert_eq!(row.lock, IngressLockReach::Missing);
}

#[tokio::test]
async fn a_missing_input_capsule_orphans_the_row_and_new_input_is_accepted() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let (prior, capsule) = ended_prior_input(dir.path(), "prx_prior_run").await;
    // The input event remains, but its payload no longer carries the capsule.
    let store = SqliteMemory::new(dir.path()).expect("test: next store");
    store
        .replace_event_payload_for_test(&prior.event_id, r#"{"original_payload_json":null}"#)
        .expect("test: drop the input capsule");
    let memory: Arc<dyn Memory> = Arc::new(store);
    next_input(&memory, dir.path(), "prx_next_run").await;

    assert_orphaned(&memory, dir.path(), &prior, OrphanCause::InputMissing).await;
    let rows = unsettled(dir.path());
    let row = row_of(&rows, &capsule).expect("test: the orphan stays visible");
    assert_eq!(row.state, IngressRecoveryState::Unconfirmed);
    assert_eq!(row.cause, Some(OrphanCause::InputMissing));
}

#[tokio::test]
async fn a_live_prior_lifetime_is_skipped_and_new_input_is_accepted() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let config = recovery_config(dir.path());
    // Two store handles in one process own two real lifetimes and two real locks.
    let live_memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: live store"));
    let (live, live_ticket) = recovery_recorder(live_memory.clone(), &config).await;
    let prior = live
        .append(recovery_input(dir.path(), "prx_live_run"))
        .await
        .expect("test: live input");
    let capsule = capsule_from_event(&prior).expect("test: live capsule");

    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: second store"));
    let next = next_input(&memory, dir.path(), "prx_next_run").await;
    let next_capsule = capsule_from_event(&next).expect("test: next capsule");
    assert_ne!(capsule.lifetime, next_capsule.lifetime, "test: two lifetimes");

    for absent in ["unconfirmed", "reconciled", "quarantine"] {
        assert!(
            evidence(&memory, dir.path(), &prior, absent).await.is_none(),
            "a live row is never labelled ({absent})"
        );
    }
    let rows = unsettled(dir.path());
    let row = row_of(&rows, &capsule).expect("test: the live row is untouched");
    assert_eq!(row.state, IngressRecoveryState::Pending);
    assert_eq!(row.lock, IngressLockReach::Held);
    live_ticket.finish(true);
}

#[tokio::test]
async fn a_corrupt_prior_row_is_quarantined_and_new_input_is_accepted() {
    let dir = tempfile::tempdir().expect("test: workspace");
    let (prior, capsule) = ended_prior_input(dir.path(), "prx_prior_run").await;
    let store = SqliteMemory::new(dir.path()).expect("test: next store");
    store
        .replace_event_content_for_test(&prior.event_id, "tampered after its digest")
        .expect("test: corrupt the input");
    let memory: Arc<dyn Memory> = Arc::new(store);
    next_input(&memory, dir.path(), "prx_next_run").await;
    // A second input in the same process admits too and adds no outcome.
    next_input(&memory, dir.path(), "prx_third_run").await;

    let marker = evidence(&memory, dir.path(), &prior, "quarantine")
        .await
        .expect("test: the corrupt row is quarantined");
    assert_eq!(marker.event_type, "turn.ingress.quarantined");
    assert_eq!(marker.content, "turn.ingress.quarantined input_digest");
    assert!(!marker.content.contains("tampered"), "no input content");
    for absent in ["unconfirmed", "reconciled"] {
        assert!(
            evidence(&memory, dir.path(), &prior, absent).await.is_none(),
            "test: no {absent} outcome"
        );
    }
    let rows = unsettled(dir.path());
    let row = row_of(&rows, &capsule).expect("test: the quarantined row is left as it is");
    assert_eq!(row.state, IngressRecoveryState::Pending);
    assert_eq!(row.lock, IngressLockReach::Present);
}
