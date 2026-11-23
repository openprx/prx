#[tokio::test]
async fn erase_scope_deadline_preserves_fence_until_child_lease_drops() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.target.clone(), None))
        .await
        .expect("test: persist child session");
    let child = registry
        .reconstruct(&f.target)
        .await
        .expect("test: hold reconstructed history lease");
    let scope = child.mailbox_scope().expect("test: reconstructed scope").clone();
    let store = f.store.clone();
    let pending_scope = scope.clone();
    let pending =
        tokio::task::spawn_blocking(move || store.erase_scope(&pending_scope, Duration::from_millis(20), &|| Ok(())));
    let result = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("test: erase wait has a hard deadline")
        .expect("test: erase task joins");
    assert_eq!(
        result.expect_err("test: retained child history cannot be erased"),
        MailboxError::Fenced,
        "incomplete quiescence must return history_pending"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM agent_erase_fences WHERE state='draining'"),
        1,
        "timeout must keep the durable erasure fence in place"
    );
    drop(child);
    let erased = f
        .store
        .erase_scope(&scope, Duration::from_secs(5), &|| Ok(()))
        .expect("test: retry after holder joins");
    assert_eq!(
        erased.remaining, 0,
        "released history lease must allow complete erasure"
    );
}

#[test]
fn mailbox_wire_errors_and_unacknowledged_result_preserve_contract() {
    use crate::messaging::mailbox::{MailboxPublished, MailboxSendState};
    for (error, code) in [
        (MailboxError::PinRequired, "recipient_pin_required"),
        (MailboxError::InvalidPin, "pin_invalid"),
        (MailboxError::PinExpired, "pin_invalid"),
        (MailboxError::ScopeErasing, "scope_erasing"),
        (MailboxError::Fenced, "history_pending"),
    ] {
        assert_eq!(
            error.to_string(),
            code,
            "wire error must distinguish authority, admission fence and erase retry"
        );
    }
    for state in [MailboxSendState::Pending, MailboxSendState::Failed] {
        let result = crate::messaging::send::send_result(Ok(MailboxPublished {
            row_id: 1,
            message_id: Uuid::from_u128(1),
            stream_id: Uuid::from_u128(STREAM),
            seq: 1,
            state,
            ack_state: None,
            error: None,
        }));
        assert_eq!(
            result.get("state"),
            Some(&serde_json::Value::Null),
            "without an Ack the result must not claim accepted delivery"
        );
    }
}
