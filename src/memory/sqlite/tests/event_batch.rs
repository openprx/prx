#![cfg(test)]
use super::*;

fn batch_input(content: &str) -> MessageEventInput {
    let mut input = message_input(
        "prx_workspace",
        content,
        MemoryVisibility::Workspace,
        None,
        Some("prx_session"),
        Some("prx_user"),
    );
    input.owner_id = Some("prx_user".to_owned());
    input.config_generation_id = Some(42);
    input.config_source_revision = Some("prx_revision".to_owned());
    input
}

#[tokio::test]
async fn event_batch_preserves_order_metadata_replay_and_outbox() {
    let (_workspace, memory) = temp_sqlite();
    let mut first = batch_input("prx_first");
    first.idempotency_key = Some("prx_batch_first".to_owned());
    first.subject = Some(crate::memory::MessageEventSubject::Task("prx_task".to_owned()));
    first.goal_id = Some("prx_goal".to_owned());
    first.causation_event_id = Some("prx_parent_event".to_owned());
    first.correlation_id = Some("prx_correlation".to_owned());
    first.attempt_id = Some("prx_attempt".to_owned());
    first.lease_epoch = Some(3);
    let mut second = batch_input("prx_second");
    second.idempotency_key = Some("prx_batch_second".to_owned());
    let results = memory.append_message_events(vec![first.clone(), second.clone()]).await;
    assert_eq!(results.len(), 2);
    let rows: Vec<_> = results.into_iter().map(Result::unwrap).collect();
    assert!(rows[0].id < rows[1].id);
    assert_eq!(rows[0].content, "prx_first");
    assert_eq!(rows[1].content, "prx_second");
    assert_eq!(rows[0].owner_id, first.owner_id);
    assert_eq!(rows[0].subject, first.subject);
    assert_eq!(rows[0].goal_id, first.goal_id);
    assert_eq!(rows[0].causation_event_id, first.causation_event_id);
    assert_eq!(rows[0].correlation_id, first.correlation_id);
    assert_eq!(rows[0].attempt_id, first.attempt_id);
    assert_eq!(rows[0].lease_epoch, first.lease_epoch);
    assert_eq!(rows[0].config_generation_id, first.config_generation_id);
    assert_eq!(rows[0].config_source_revision, first.config_source_revision);
    let replayed: Vec<_> = memory
        .append_message_events(vec![first, second])
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        replayed.iter().map(|row| row.id).collect::<Vec<_>>(),
        rows.iter().map(|row| row.id).collect::<Vec<_>>()
    );
    let connection = memory.pool.write();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM message_events", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM memory_events", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn event_batch_isolates_validation_and_sql_failures_without_losing_neighbors() {
    let (_workspace, memory) = temp_sqlite();
    memory.pool.write().execute_batch("CREATE TRIGGER prx_batch_failure BEFORE INSERT ON memory_events WHEN NEW.event_type='prx.injected_failure' BEGIN SELECT RAISE(ABORT,'prx injected failure'); END;").unwrap();
    let mut invalid = batch_input("prx_invalid");
    invalid.event_type.clear();
    let mut sql_failure = batch_input("prx_failed_sql");
    sql_failure.event_type = "prx.injected_failure".to_owned();
    let results = memory
        .append_message_events(vec![
            batch_input("prx_first"),
            invalid,
            sql_failure,
            batch_input("prx_last"),
        ])
        .await;
    assert_eq!(results.len(), 4);
    assert!(results[0].is_ok());
    assert!(results[1].is_err());
    assert!(results[2].is_err());
    assert!(results[3].is_ok());
    let connection = memory.pool.write();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM message_events", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM memory_events", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM message_events WHERE content='prx_failed_sql'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn event_batch_rejects_oversize_and_does_not_write_empty_inputs() {
    let (_workspace, memory) = temp_sqlite();
    assert!(memory.append_message_events(Vec::new()).await.is_empty());
    let count = crate::memory::traits::MESSAGE_EVENT_BATCH_LIMIT + 1;
    let results = memory
        .append_message_events(vec![batch_input("prx_over_capacity"); count])
        .await;
    assert_eq!(results.len(), count);
    assert!(results.iter().all(Result::is_err));
    assert_eq!(
        memory
            .pool
            .write()
            .query_row("SELECT count(*) FROM message_events", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn event_batch_default_preserves_unsupported_backend_errors() {
    let backend = crate::memory::NoneMemory::new();
    let results = backend
        .append_message_events(vec![batch_input("prx_first"), batch_input("prx_last")])
        .await;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(Result::is_err));
}
