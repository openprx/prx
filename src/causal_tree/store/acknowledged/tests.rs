#![cfg(test)]

use super::super::{CausalDecisionStore, DecisionId, Retention, StoreMode, WriterSetup, now_ms, testing};
use super::{DecisionScope, Instant, StoreError};
use anyhow::Context;
use std::{sync::Arc, time::Duration};

fn deadline() -> Instant {
    Instant::now() + Duration::from_millis(500)
}

#[tokio::test]
async fn acknowledged_decision_reads_the_committed_scoped_row_including_conflicts() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = testing::open_private(
        workspace.path(),
        Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
    );
    let writer = store.writer().context("writer")?;
    let workspace_id = store.workspace_id().context("workspace")?;
    let record = testing::record(DecisionId::new_runtime(), &workspace_id, Some("prx_user"), now_ms());
    let expected = record.replay_digest(&workspace_id)?;
    let scope = DecisionScope::Owner {
        workspace_id,
        owner_id: Arc::from("prx_user"),
    };
    let row = writer
        .record_and_replay(record.clone(), scope.clone(), deadline())
        .await?
        .context("committed row")?;
    assert_eq!(row.digest, expected);
    assert_eq!(
        store
            .reader()
            .replay_record_until(&scope, &record.decision_id, deadline())?
            .context("independent read")?
            .digest,
        expected
    );
    let foreign = DecisionScope::Owner {
        workspace_id: store.workspace_id().context("workspace")?,
        owner_id: Arc::from("prx_other_user"),
    };
    assert!(
        writer
            .record_and_replay(record.clone(), foreign, deadline())
            .await?
            .is_none()
    );
    let mut changed = record;
    changed.policy_version.push_str("-changed");
    let conflict = changed.replay_digest(&store.workspace_id().context("workspace")?)?;
    let actual = writer
        .record_and_replay(changed, scope, deadline())
        .await?
        .context("original row survives conflict")?;
    assert_eq!(actual.digest, expected);
    assert_ne!(actual.digest, conflict);
    CausalDecisionStore::close(Arc::new(store), deadline()).await;
    Ok(())
}

#[tokio::test]
async fn acknowledged_decision_expiry_never_acknowledges_a_blocked_commit() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = CausalDecisionStore::open(
        workspace.path(),
        Retention::days(30, 30),
        Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
        StoreMode::ReadWrite,
        WriterSetup::default(),
    )?;
    let blocker = rusqlite::Connection::open(store.path())?;
    blocker.execute_batch("BEGIN IMMEDIATE;")?;
    let writer = store.writer().context("writer")?;
    let workspace_id = store.workspace_id().context("workspace")?;
    let record = testing::record(DecisionId::new_runtime(), &workspace_id, Some("prx_user"), now_ms());
    let scope = DecisionScope::Owner {
        workspace_id,
        owner_id: Arc::from("prx_user"),
    };
    let id = record.decision_id;
    let result = writer
        .record_and_replay(record, scope.clone(), Instant::now() + Duration::from_millis(20))
        .await;
    assert!(matches!(result, Err(StoreError::RecoveryDeadline)));
    blocker.execute_batch("ROLLBACK;")?;
    assert!(writer.drain(deadline()).await);
    assert!(store.reader().replay_record_until(&scope, &id, deadline())?.is_some());
    CausalDecisionStore::close(Arc::new(store), deadline()).await;
    Ok(())
}

#[tokio::test]
async fn queued_chain_replay_reads_real_scoped_rows_and_preserves_missing_hops() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = testing::open_private(
        workspace.path(),
        Arc::new(crate::decision::telemetry::testing::RecordingSink::default()),
    );
    let writer = store.writer().context("writer")?;
    let workspace_id = store.workspace_id().context("workspace")?;
    let root = testing::record(DecisionId::new_runtime(), &workspace_id, Some("prx_user"), now_ms());
    let mut next = testing::record(DecisionId::new_runtime(), &workspace_id, Some("prx_user"), now_ms());
    next.lineage.prior_decision_id = Some(root.decision_id);
    let ids = vec![root.decision_id, next.decision_id];
    let expected = [root.replay_digest(&workspace_id)?, next.replay_digest(&workspace_id)?];
    assert_eq!(writer.record_chain(vec![root, next]), super::super::Enqueue::Queued);
    let scope = DecisionScope::Owner {
        workspace_id: Arc::clone(&workspace_id),
        owner_id: Arc::from("prx_user"),
    };
    // No drain or independent reader precedes this FIFO acknowledgement.
    let rows = writer.replay_chain(scope.clone(), ids.clone(), deadline()).await?;
    assert_eq!(rows.len(), 2);
    for (row, expected) in rows.into_iter().zip(expected) {
        assert_eq!(row.context("actual stored chain hop")?.digest, expected);
    }
    let foreign = DecisionScope::Owner {
        workspace_id,
        owner_id: Arc::from("prx_other_user"),
    };
    assert!(
        writer
            .replay_chain(foreign, ids.clone(), deadline())
            .await?
            .iter()
            .all(Option::is_none)
    );
    let missing = DecisionId::new_runtime();
    let rows = writer
        .replay_chain(
            scope.clone(),
            vec![*ids.first().context("root id")?, missing],
            deadline(),
        )
        .await?;
    assert_eq!(rows.len(), 2);
    assert!(rows.first().context("root result")?.is_some());
    assert!(rows.get(1).context("missing result")?.is_none());
    assert!(matches!(
        writer.replay_chain(scope.clone(), ids.clone(), Instant::now()).await,
        Err(StoreError::RecoveryDeadline)
    ));
    assert!(
        writer
            .replay_chain(scope.clone(), Vec::new(), deadline())
            .await
            .is_err()
    );
    assert!(writer.replay_chain(scope, vec![missing; 6], deadline()).await.is_err());
    CausalDecisionStore::close(Arc::new(store), deadline()).await;
    Ok(())
}
