//! Real SQLite mailbox/store tests. The only injected component is the clock.
use super::*;
use crate::memory::sqlite::mailbox::{MailboxDelivery, SqliteMailbox};
use crate::messaging::mailbox::{MailboxAuthority, MailboxClock, MailboxError, MailboxScope};
use crate::messaging::{DeliveryState, ExecRef, IdempotencyKey, MessageBody, MessageKind, PrxMessage, SessionAddr};
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
use uuid::Uuid;

const W: &str = "prx_workspace";
const O: &str = "prx_owner";
const INC: u128 = 1;
const SCOPE_SOURCE: u128 = 10;
const SCOPE_TARGET: u128 = 20;
const STREAM: u128 = 500;
struct Clock(AtomicI64);
impl Clock {
    fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, AtomicOrdering::SeqCst);
    }
}
impl MailboxClock for Clock {
    fn now(&self) -> i64 {
        self.0.load(AtomicOrdering::SeqCst)
    }
}

struct Fixture {
    _dir: TempDir,
    memory: SqliteMemory,
    store: SqliteMailbox,
    clock: Arc<Clock>,
    source: SessionAddr,
    target: SessionAddr,
    sender: MailboxScope,
    recipient: MailboxScope,
    authority: MailboxAuthority,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let memory = SqliteMemory::new(dir.path()).unwrap();
        {
            let writer = memory.pool.write();
            writer
                .execute(
                    "INSERT INTO agent_incarnations VALUES (?1,?2,'m',?3,'active',?4)",
                    params![W, O, Uuid::from_u128(INC).to_string(), "0".repeat(64)],
                )
                .unwrap();
            for (id, canonical) in [(SCOPE_SOURCE, "canonical-source"), (SCOPE_TARGET, "canonical-target")] {
                writer
                    .execute(
                        "INSERT INTO agent_scopes VALUES (?1,?2,?3,?4,'m',?5,1)",
                        params![
                            Uuid::from_u128(id).to_string(),
                            W,
                            O,
                            canonical,
                            Uuid::from_u128(INC).to_string()
                        ],
                    )
                    .unwrap();
            }
        }
        let clock = Arc::new(Clock(AtomicI64::new(1000)));
        let store = SqliteMailbox::new(
            memory.clone(),
            "prx-node".into(),
            [7; 32],
            clock.clone(),
            crate::config::schema::RemoteTreeLimits::default(),
        )
        .unwrap();
        let source = SessionAddr::new("prx-node", O, "source", None);
        let target = SessionAddr::new("prx-node", O, "target", None);
        let sender = MailboxScope {
            workspace_id: W.into(),
            owner_id: O.into(),
            scope_id: Uuid::from_u128(SCOPE_SOURCE),
            incarnation: Uuid::from_u128(INC),
            generation: 1,
        };
        let recipient = MailboxScope {
            scope_id: Uuid::from_u128(SCOPE_TARGET),
            ..sender.clone()
        };
        let authority = MailboxAuthority::local(W.into(), source.clone(), target.clone()).unwrap();
        store
            .set_execution(&recipient, &target, Some("run-target".into()))
            .unwrap();
        Self {
            _dir: dir,
            memory,
            store,
            clock,
            source,
            target,
            sender,
            recipient,
            authority,
        }
    }
    fn message(&self, n: u64, seq: u64) -> PrxMessage {
        let pin = self
            .store
            .recipient_pin(&self.authority, &self.recipient, "canonical-target")
            .unwrap();
        PrxMessage {
            id: Uuid::from_u128(1000 + u128::from(n)),
            idempotency: IdempotencyKey::parse(&format!("mailbox-key-{n:08}")).unwrap(),
            stream_id: Some(Uuid::from_u128(STREAM)),
            seq,
            ts: self.clock.now(),
            from: self.source.clone(),
            to: self.target.clone(),
            to_pin: pin,
            in_reply_to: None,
            hop_count: 0,
            ancestry: Vec::new(),
            kind: MessageKind::Data,
            body: MessageBody {
                mime: "text/plain".into(),
                bytes: format!("request-{n}").into_bytes(),
            },
            origin: None,
            auth: None,
            exec: None,
            canonical_hash: None,
        }
    }
    fn receive(&self, n: u64, seq: u64) -> crate::messaging::mailbox::MailboxReceipt {
        self.store.accept(&self.authority, self.message(n, seq)).unwrap()
    }
    fn deliver(&self) -> MailboxDelivery {
        self.store.deliver(&self.recipient, &self.target).unwrap().unwrap()
    }
    fn scalar(&self, sql: &str) -> i64 {
        self.memory.pool.write().query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

#[test]
fn in_process_send_is_durable_and_acked() {
    let f = Fixture::new();
    let provisional = f.message(1, 0);
    let published = f
        .store
        .publish(&f.authority, provisional, &f.sender, f.clock.now() + 300, None)
        .unwrap();
    assert_eq!(published.seq, 1);
    let outgoing = f.store.pending_outbox(&f.sender, &f.source).unwrap();
    let sent = outgoing.first().unwrap().message().clone();
    let accepted = f.store.accept(&f.authority, sent.clone()).unwrap();
    f.store
        .acknowledge(&f.sender, &f.target, sent.id, accepted.state)
        .unwrap();
    let retry = f
        .store
        .publish(&f.authority, sent, &f.sender, f.clock.now() + 300, None)
        .unwrap();
    assert_eq!(retry.ack_state, Some(DeliveryState::Accepted));
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 1);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 1);
}

#[test]
fn adopt_is_atomic_transcript_and_consume() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    let first = f.store.adopt(&delivery).unwrap();
    let again = f.store.adopt(&delivery).unwrap();
    assert_eq!(first, again);
    assert_eq!(first.state, DeliveryState::Consumed);
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 1);
    assert_eq!(
        f.scalar("SELECT count(*) FROM message_events WHERE source='prx_mailbox'"),
        1
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts WHERE state=2 AND adopted_turn_id IS NOT NULL"),
        1
    );
}

#[test]
fn same_id_different_key_different_body_is_id_reuse() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store.accept(&f.authority, original.clone()).unwrap();
    let mut other = original;
    other.idempotency = IdempotencyKey::parse("another-mailbox-key").unwrap();
    other.seq = 2;
    other.body.bytes = b"changed".to_vec();
    assert_eq!(f.store.accept(&f.authority, other).unwrap_err(), MailboxError::IdReuse);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 1);
}

#[test]
fn same_key_changed_exec_is_idempotency_conflict() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store.accept(&f.authority, original.clone()).unwrap();
    let mut retry = original;
    retry.exec = Some(ExecRef {
        run: Some("different-run".into()),
    });
    assert_eq!(
        f.store.accept(&f.authority, retry).unwrap_err(),
        MailboxError::IdempotencyConflict
    );
}

#[test]
fn reordered_duplicate_seq_holds_fifo() {
    let f = Fixture::new();
    let second = f.message(2, 2);
    let accepted = f.store.accept(&f.authority, second.clone()).unwrap();
    assert!(f.store.deliver(&f.recipient, &f.target).unwrap().is_none());
    assert_eq!(f.store.accept(&f.authority, second).unwrap(), accepted);
    f.receive(1, 1);
    let first = f.deliver();
    assert_eq!(first.message().seq, 1);
    f.store.adopt(&first).unwrap();
    drop(first);
    let second = f.deliver();
    assert_eq!(second.message().seq, 2);
    f.store.adopt(&second).unwrap();
}

#[test]
fn permanent_seq_gap_retires_and_unblocks() {
    let f = Fixture::new();
    f.receive(2, 2);
    assert!(f.store.deliver(&f.recipient, &f.target).unwrap().is_none());
    f.clock.advance(30);
    let delivery = f.deliver();
    assert_eq!(delivery.message().seq, 2);
    f.store.adopt(&delivery).unwrap();
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts WHERE state=2"), 1);
}

#[test]
fn late_seq_at_or_below_consume_cursor_is_rejected_after_hole_gc() {
    let f = Fixture::new();
    f.receive(2, 2);
    f.clock.advance(30);
    let delivery = f.deliver();
    f.store.adopt(&delivery).unwrap();
    drop(delivery);
    f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
    // Fresh pending work keeps the stream initialized while eligible old
    // bodies/receipts/holes are collected. No forbidden same-UUID reset.
    f.receive(3, 3);
    f.store.gc_scope(&f.recipient).unwrap();
    assert_eq!(
        f.scalar("SELECT json_array_length(progress,'$.seq_skipped') FROM prx_mailbox_seq"),
        0
    );
    assert_eq!(
        f.store.accept(&f.authority, f.message(4, 1)).unwrap_err(),
        MailboxError::SeqAlreadyConsumed
    );
}

#[test]
fn outbox_parent_id_is_not_reused_after_outbox_gc() {
    let f = Fixture::new();
    let published = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    f.store
        .acknowledge(&f.sender, &f.target, published.message_id, DeliveryState::Accepted)
        .unwrap();
    f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
    let gc = f.store.gc_scope(&f.sender).unwrap();
    assert_eq!(gc.outboxes, 1);
    assert_eq!(gc.parents, 0);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_parents"), 1);
    let mut next = f.message(2, 0);
    next.stream_id = Some(Uuid::from_u128(STREAM + 1));
    let published2 = f
        .store
        .publish(&f.authority, next, &f.sender, f.clock.now() + 300, None)
        .unwrap();
    assert!(published2.row_id > published.row_id);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_parents"), 2);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_outbox"), 1);
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages WHERE id NOT IN(SELECT id FROM prx_mailbox_receipts)"),
        0
    );
}

#[test]
fn crash_before_adopt_redelivers_once() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    let id = delivery.message().id;
    drop(delivery);
    f.store.recover_delivery(&f.recipient, &f.target).unwrap();
    let retry = f.deliver();
    assert_eq!(retry.message().id, id);
    f.store.adopt(&retry).unwrap();
    drop(retry);
    f.store.recover_delivery(&f.recipient, &f.target).unwrap();
    assert!(f.store.deliver(&f.recipient, &f.target).unwrap().is_none());
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 1);
}

#[test]
fn crash_during_adopt_rolls_back() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    f.memory.pool.write().execute_batch("CREATE TEMP TRIGGER test_refuse_consume BEFORE UPDATE OF state ON prx_mailbox_receipts WHEN NEW.state=2 BEGIN SELECT RAISE(ABORT,'fixture consume refusal'); END;").unwrap();
    assert_eq!(f.store.adopt(&delivery).unwrap_err(), MailboxError::Storage);
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 0);
    assert_eq!(
        f.scalar("SELECT count(*) FROM message_events WHERE source='prx_mailbox'"),
        0
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts WHERE state=1 AND adopted_turn_id IS NULL"),
        1
    );
    f.memory
        .pool
        .write()
        .execute_batch("DROP TRIGGER test_refuse_consume")
        .unwrap();
    f.store.adopt(&delivery).unwrap();
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 1);
}

#[test]
fn mailbox_shared_ids_survive_rowid_reuse_after_gc() {
    let f = Fixture::new();
    f.memory.pool.write().execute_batch("CREATE TEMP TRIGGER test_refuse_body BEFORE INSERT ON prx_mailbox_messages BEGIN SELECT RAISE(ABORT,'fixture body refusal'); END;").unwrap();
    assert_eq!(
        f.store.accept(&f.authority, f.message(1, 1)).unwrap_err(),
        MailboxError::Storage
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 0);
    f.memory
        .pool
        .write()
        .execute_batch("DROP TRIGGER test_refuse_body")
        .unwrap();
    let accepted = f.receive(1, 1);
    let delivery = f.deliver();
    f.store.adopt(&delivery).unwrap();
    drop(delivery);
    f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
    f.receive(2, 2);
    // Observe the required lifetime order, rather than a zero-row assertion.
    f.memory.pool.write().execute_batch("CREATE TEMP TRIGGER test_require_body_first BEFORE DELETE ON prx_mailbox_receipts WHEN EXISTS(SELECT 1 FROM prx_mailbox_messages WHERE id=OLD.id) BEGIN SELECT RAISE(ABORT,'body still references receipt'); END;").unwrap();
    let gc = f.store.gc_scope(&f.recipient).unwrap();
    assert_eq!(gc.bodies, 1);
    assert_eq!(gc.receipts, 1);
    assert_eq!(
        f.scalar(&format!(
            "SELECT count(*) FROM prx_mailbox_messages WHERE id={}",
            accepted.row_id
        )),
        0
    );
    assert_eq!(
        f.scalar(&format!(
            "SELECT count(*) FROM prx_mailbox_receipts WHERE id={}",
            accepted.row_id
        )),
        0
    );
}

#[test]
fn missing_recipient_pin_is_rejected_not_filled() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.to_pin.token.clear();
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::InvalidPin
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 0);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_seq"), 0);
}

#[test]
fn unknown_parent_is_nacked() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.in_reply_to = Some(Uuid::from_u128(999));
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::UnknownParent
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
}

#[test]
fn retry_with_altered_in_reply_to_is_idempotency_conflict() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store.accept(&f.authority, original.clone()).unwrap();
    let mut changed = original;
    changed.in_reply_to = Some(Uuid::from_u128(999));
    assert_eq!(
        f.store.accept(&f.authority, changed).unwrap_err(),
        MailboxError::IdempotencyConflict
    );
}

#[test]
fn ack_does_not_emit_ack() {
    let f = Fixture::new();
    let published = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    f.store
        .acknowledge(&f.sender, &f.target, published.message_id, DeliveryState::Accepted)
        .unwrap();
    f.store
        .acknowledge(&f.sender, &f.target, published.message_id, DeliveryState::Consumed)
        .unwrap();
    f.store
        .acknowledge(&f.sender, &f.target, published.message_id, DeliveryState::Accepted)
        .unwrap();
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
    assert!(f.store.pending_outbox(&f.sender, &f.source).unwrap().is_empty());
    let retry = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    assert_eq!(retry.ack_state, Some(DeliveryState::Consumed));
}

#[test]
fn offline_session_receives_after_attach() {
    let f = Fixture::new();
    f.store.close_execution(&f.recipient, &f.target, "run-target").unwrap();
    f.receive(1, 1);
    assert!(f.store.deliver(&f.recipient, &f.target).unwrap().is_none());
    f.store
        .set_execution(&f.recipient, &f.target, Some("reopened-target".into()))
        .unwrap();
    let delivered = f.deliver();
    assert_eq!(delivered.message().body.bytes, b"request-1");
}

#[test]
fn mailbox_erase_drops_bodies_keeps_receipts() {
    let f = Fixture::new();
    f.receive(1, 1);
    let queued = f.message(2, 2);
    let report = f.store.erase_scope(&f.recipient, &|| Ok(())).unwrap();
    assert_eq!(report.remaining, 0);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 1);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_seq WHERE state=1"), 1);
    assert_eq!(f.store.accept(&f.authority, queued).unwrap_err(), MailboxError::Fenced);
}

#[test]
fn outbox_erase_drops_bodies_keeps_status() {
    let f = Fixture::new();
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    f.store.erase_scope(&f.sender, &|| Ok(())).unwrap();
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_outbox WHERE state=3 AND body_bytes IS NULL AND json_extract(envelope_meta,'$.to_pin_token') IS NULL AND json_extract(envelope_meta,'$.body_mime') IS NULL"),1);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_parents"), 1);
    let replay = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    assert_eq!(replay.state, crate::messaging::mailbox::MailboxSendState::Failed);
}

#[test]
fn held_delivery_across_erase_recreate_does_not_resurrect_body() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    let (fenced_tx, fenced_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let store = f.store.clone();
    let scope = f.recipient.clone();
    let erase = std::thread::spawn(move || {
        let result = store.erase_scope(&scope, &|| {
            fenced_tx.send(()).unwrap();
            Ok(())
        });
        done_tx.send(result).unwrap();
    });
    fenced_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(f.store.adopt(&delivery).unwrap_err(), MailboxError::Fenced);
    assert!(
        done_rx.try_recv().is_err(),
        "erase cannot finish while queue/IPC owns the payload"
    );
    drop(delivery);
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap().remaining,
        0
    );
    erase.join().unwrap();
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 0);
}

fn new_target_generation(f: &Fixture) -> MailboxScope {
    let writer = f.memory.pool.write();
    writer
        .execute(
            "UPDATE agent_incarnations SET state='retired' WHERE workspace_id=?1 AND owner_id=?2 AND agent_key='m'",
            params![W, O],
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO agent_incarnations VALUES (?1,?2,'m',?3,'active',?4)",
            params![W, O, Uuid::from_u128(2).to_string(), "1".repeat(64)],
        )
        .unwrap();
    let scope = MailboxScope {
        scope_id: Uuid::from_u128(30),
        incarnation: Uuid::from_u128(2),
        ..f.recipient.clone()
    };
    writer
        .execute(
            "INSERT INTO agent_scopes VALUES (?1,?2,?3,'canonical-target','m',?4,1)",
            params![scope.scope_id.to_string(), W, O, scope.incarnation.to_string()],
        )
        .unwrap();
    scope
}

#[test]
fn recipient_rebind_with_unconsumed_old_scope_retires_stream() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut second = f.message(2, 2);
    let new_scope = new_target_generation(&f);
    second.to_pin = f
        .store
        .recipient_pin(&f.authority, &new_scope, "canonical-target")
        .unwrap();
    assert_eq!(
        f.store.accept(&f.authority, second.clone()).unwrap_err(),
        MailboxError::SeqStreamRetired
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 1);
    f.store
        .set_execution(&new_scope, &f.target, Some("new-run".into()))
        .unwrap();
    second.stream_id = Some(Uuid::from_u128(STREAM + 1));
    second.seq = 1;
    f.store.accept(&f.authority, second).unwrap();
    let delivered = f.store.deliver(&new_scope, &f.target).unwrap().unwrap();
    assert_eq!(delivered.message().seq, 1);
    f.store.adopt(&delivered).unwrap();
    drop(delivered);
    f.store.erase_scope(&f.recipient, &|| Ok(())).unwrap();
    assert_eq!(f.scalar(&format!("SELECT count(*) FROM prx_mailbox_seq s JOIN prx_mailbox_receipts r ON r.stream_id=s.stream_id WHERE s.state=0 AND r.state<>2 AND r.ma_scope_id='{}'",f.recipient.scope_id)),0);
}

#[test]
fn recipient_erase_retires_stream_before_recreated_scope_accept() {
    let f = Fixture::new();
    f.receive(3, 3);
    let mut fourth = f.message(4, 4);
    // Represent a row left by the pre-fix rebind bug: its current role is Y,
    // but an accepted X-pinned slot is still present. Erase must enumerate X
    // receipts, not only the current recipient role.
    let new_scope = new_target_generation(&f);
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET recipient_ma_scope_id=?1,recipient_incarnation=?2,recipient_generation=1",
            params![new_scope.scope_id.to_string(), new_scope.incarnation.to_string()],
        )
        .unwrap();
    f.store.erase_scope(&f.recipient, &|| Ok(())).unwrap();
    fourth.to_pin = f
        .store
        .recipient_pin(&f.authority, &new_scope, "canonical-target")
        .unwrap();
    assert_eq!(
        f.store.accept(&f.authority, fourth.clone()).unwrap_err(),
        MailboxError::SeqStreamRetired
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
    fourth.stream_id = Some(Uuid::from_u128(STREAM + 1));
    fourth.seq = 1;
    f.store.accept(&f.authority, fourth).unwrap();
    f.store
        .set_execution(&new_scope, &f.target, Some("new-run".into()))
        .unwrap();
    assert_eq!(
        f.store.deliver(&new_scope, &f.target).unwrap().unwrap().message().seq,
        1
    );
}

#[test]
fn mailbox_mime_and_payload_share_byte_limits() {
    let f = Fixture::new();
    for mime in [
        String::new(),
        "x".repeat(256),
        "text/plain\0bad".into(),
        "text/\u{e9}".into(),
    ] {
        let mut message = f.message(1, 1);
        message.body.mime = mime;
        assert_eq!(
            f.store.accept(&f.authority, message).unwrap_err(),
            MailboxError::InvalidMime
        );
    }
    let mut too_large = f.message(1, 1);
    too_large.body.bytes = vec![0; crate::messaging::MAX_BODY_BYTES];
    assert_eq!(
        f.store.accept(&f.authority, too_large).unwrap_err(),
        MailboxError::BodyTooLarge
    );
    let mut fits = f.message(1, 1);
    fits.body.mime = "x".repeat(255);
    fits.body.bytes = vec![0; crate::messaging::MAX_BODY_BYTES - 255];
    f.store.accept(&f.authority, fits).unwrap();
    assert_eq!(
        f.scalar("SELECT length(body_bytes)+length(CAST(body_mime AS BLOB)) FROM prx_mailbox_messages"),
        262144
    );
}

#[tokio::test]
async fn child_session_reconstructs_mailbox_cursor() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.target.clone(), None, None))
        .await
        .unwrap();
    let empty = registry.reconstruct(&f.target).await.unwrap();
    let scope = empty.mailbox_scope().unwrap().clone();
    drop(empty);
    let canonical = ChildSessionRegistry::canonical_session_key(&f.target).unwrap();
    let mut message = f.message(1, 1);
    message.to_pin = f.store.recipient_pin(&f.authority, &scope, &canonical).unwrap();
    f.store
        .set_execution(&scope, &f.target, Some("child-run".into()))
        .unwrap();
    f.store.accept(&f.authority, message).unwrap();
    let delivery = f.store.deliver(&scope, &f.target).unwrap().unwrap();
    f.store.adopt(&delivery).unwrap();
    drop(delivery);
    let restored = registry.reconstruct(&f.target).await.unwrap();
    assert_eq!(restored.mailbox_cursor().streams.get(&1), Some(&1));
    assert_eq!(restored.history().len(), 1);
    assert_eq!(restored.history().first().unwrap().content, "request-1");
    registry.insert(&restored).await.unwrap();
    drop(restored);
    let again = registry.reconstruct(&f.target).await.unwrap();
    assert_eq!(
        again.history().len(),
        1,
        "snapshot projection must merge by event identity exactly once"
    );
    drop(again);
    f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
    f.store.gc_scope(&scope).unwrap();
    let after_gc = registry.reconstruct(&f.target).await.unwrap();
    assert_eq!(after_gc.history().len(), 1, "source_ref and seq survive receipt GC");
    assert_eq!(after_gc.mailbox_cursor().streams.get(&1), Some(&1));
}

#[tokio::test]
async fn child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let f = Fixture::new();
    {
        let writer = f.memory.pool.write();
        for (key, inc) in [("n:prx_agent_a", 40), ("n:prx_agent_b", 50)] {
            writer
                .execute(
                    "INSERT INTO agent_incarnations VALUES (?1,?2,?3,?4,'active',?5)",
                    params![W, O, key, Uuid::from_u128(inc).to_string(), "2".repeat(64)],
                )
                .unwrap();
        }
    }
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    let a = SessionAddr::new("prx-node", O, "child-a", Some("prx_agent_a"));
    let b = SessionAddr::new("prx-node", O, "child-b", Some("prx_agent_b"));
    registry
        .insert(&ChildSession::new(a.clone(), None, None))
        .await
        .unwrap();
    registry
        .insert(&ChildSession::new(b.clone(), None, None))
        .await
        .unwrap();
    let active = registry.reconstruct(&a).await.unwrap();
    let scope = active.mailbox_scope().unwrap().clone();
    drop(active);
    f.store.erase_scope(&scope, &|| Ok(())).unwrap();
    assert!(registry.reconstruct(&a).await.is_err());
    assert!(registry.relaunch(&a).await.is_err());
    let other = registry.reconstruct(&b).await.unwrap();
    assert_eq!(other.addr(), &b);
    drop(other);
    // The global MA owner eraser enumerates all owner snapshots. Exercise its
    // mailbox/child adapter for that complete historical set.
    let scopes = {
        let writer = f.memory.pool.write();
        let mut query = writer
            .prepare("SELECT scope_id,incarnation,generation FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2")
            .unwrap();
        query
            .query_map(params![W, O], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    for (id, incarnation, generation) in scopes {
        f.store
            .erase_scope(
                &MailboxScope {
                    workspace_id: W.into(),
                    owner_id: O.into(),
                    scope_id: Uuid::parse_str(&id).unwrap(),
                    incarnation: Uuid::parse_str(&incarnation).unwrap(),
                    generation,
                },
                &|| Ok(()),
            )
            .unwrap();
    }
    assert_eq!(f.scalar("SELECT count(*) FROM child_sessions"), 0);
    assert!(registry.reconstruct(&b).await.is_err());
    // A quarantined legacy/orphan cannot be silently omitted by scope SQL.
    f.memory
        .pool
        .write()
        .execute("INSERT INTO child_sessions(key,record) VALUES ('legacy','{}')", [])
        .unwrap();
    assert_eq!(
        f.store.erase_scope(&f.sender, &|| Ok(())).unwrap_err(),
        MailboxError::Fenced
    );
}

#[test]
fn mailbox_scope_and_address_validation_is_transactional() {
    let f = Fixture::new();
    let mut forged = f.message(1, 1);
    forged.to.owner = "prx_other_owner".into();
    assert_eq!(
        f.store.accept(&f.authority, forged).unwrap_err(),
        MailboxError::Unauthorized
    );
    let wrong = MailboxScope {
        generation: 2,
        ..f.recipient.clone()
    };
    assert_eq!(
        f.store
            .recipient_pin(&f.authority, &wrong, "canonical-target")
            .unwrap_err(),
        MailboxError::StaleIncarnation
    );
    assert_eq!(
        f.store
            .recipient_pin(&f.authority, &f.recipient, "wrong-canonical")
            .unwrap_err(),
        MailboxError::StaleIncarnation
    );
    assert!(
        MailboxAuthority::local(
            W.into(),
            f.source.clone(),
            SessionAddr::new("prx-node", O, "x".repeat(513), None)
        )
        .is_err()
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 0);
}

#[test]
fn queued_outbox_first_post_after_recipient_recreate_is_stale_incarnation() {
    let f = Fixture::new();
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    let pending = f.store.pending_outbox(&f.sender, &f.source).unwrap();
    let held = pending.first().unwrap().message().clone();
    f.store.erase_scope(&f.recipient, &|| Ok(())).unwrap();
    let _new = new_target_generation(&f);
    assert_eq!(
        f.store.accept(&f.authority, held).unwrap_err(),
        MailboxError::StaleIncarnation
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
}

#[test]
fn stale_incarnation_delayed_send_is_rejected() {
    let f = Fixture::new();
    let held = f.message(1, 1);
    let _new = new_target_generation(&f);
    assert_eq!(
        f.store.accept(&f.authority, held).unwrap_err(),
        MailboxError::StaleIncarnation
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
}

#[test]
fn mailbox_row_requires_scope() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.to_pin.generation = 99;
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::StaleIncarnation
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
}

#[test]
fn mailbox_pk_includes_from_instance() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut source = f.source.clone();
    source.instance = "peer-two".into();
    let authority = MailboxAuthority::verified_peer(
        W.into(),
        source,
        f.target.clone(),
        crate::messaging::MessageAuth {
            issuer: "prx-issuer".into(),
            subject: "prx-peer".into(),
            method: "jwt".into(),
        },
    )
    .unwrap();
    let mut message = f.message(1, 1);
    message.to_pin = f
        .store
        .recipient_pin(&authority, &f.recipient, "canonical-target")
        .unwrap();
    message.id = Uuid::from_u128(9001);
    f.store.accept(&authority, message).unwrap();
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 2);
}

#[test]
fn mailbox_pk_omits_run() {
    let f = Fixture::new();
    let message = f.message(1, 1);
    let saved = f.store.accept(&f.authority, message.clone()).unwrap();
    f.store
        .set_execution(&f.recipient, &f.target, Some("replacement-run".into()))
        .unwrap();
    assert_eq!(f.store.accept(&f.authority, message).unwrap(), saved);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 1);
}

#[test]
fn mailbox_seq_pk_includes_stream_id() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut other = f.message(2, 1);
    other.stream_id = Some(Uuid::from_u128(501));
    f.store.accept(&f.authority, other).unwrap();
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_seq"), 2);
}

#[test]
fn idempotency_does_not_collide_across_senders() {
    mailbox_pk_includes_from_instance();
}

#[test]
fn seq_is_allocated_in_outbox_transaction() {
    let f = Fixture::new();
    f.memory.pool.write().execute_batch("CREATE TEMP TRIGGER reject_publish BEFORE INSERT ON prx_mailbox_outbox BEGIN SELECT RAISE(ABORT,'injected write failure'); END;").unwrap();
    assert_eq!(
        f.store
            .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
            .unwrap_err(),
        MailboxError::Storage
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_seq"), 0);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_parents"), 0);
    f.memory
        .pool
        .write()
        .execute_batch("DROP TRIGGER reject_publish")
        .unwrap();
    let first = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    assert_eq!(first.seq, 1);
    let again = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(
        f.store
            .publish(&f.authority, f.message(2, 0), &f.sender, f.clock.now() + 300, None)
            .unwrap()
            .seq,
        2
    );
}

#[test]
fn seq2_before_seq1_on_never_seen_stream_is_held_fifo() {
    let f = Fixture::new();
    f.receive(2, 2);
    assert!(f.store.deliver(&f.recipient, &f.target).unwrap().is_none());
    f.receive(1, 1);
    let first = f.deliver();
    assert_eq!(first.message().seq, 1);
    f.store.adopt(&first).unwrap();
    drop(first);
    assert_eq!(f.deliver().message().seq, 2);
}

#[test]
fn late_seq_after_stream_retirement_is_seq_stream_retired() {
    let f = Fixture::new();
    let pending = f.message(2, 2);
    f.store.accept(&f.authority, pending).unwrap();
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET state=1,progress=json_set(progress,'$.retired_at',1000)",
            [],
        )
        .unwrap();
    assert_eq!(
        f.store.accept(&f.authority, f.message(1, 1)).unwrap_err(),
        MailboxError::SeqStreamRetired
    );
}

#[test]
fn delayed_old_epoch_message_is_rejected_after_stream_reopen() {
    let f = Fixture::new();
    f.receive(1, 1);
    let first = f.deliver();
    f.store.adopt(&first).unwrap();
    drop(first);
    f.clock.advance(604801);
    f.store.gc_scope(&f.recipient).unwrap();
    let mut reopened = f.message(3, 1);
    reopened.stream_id = Some(Uuid::from_u128(501));
    f.store.accept(&f.authority, reopened).unwrap();
    assert_eq!(
        f.store.accept(&f.authority, f.message(2, 2)).unwrap_err(),
        MailboxError::SeqStreamRetired
    );
}

#[test]
fn mailbox_accept_nacks_while_fenced() {
    let f = Fixture::new();
    let queued = f.message(1, 1);
    f.store.erase_scope(&f.recipient, &|| Ok(())).unwrap();
    assert_eq!(f.store.accept(&f.authority, queued).unwrap_err(), MailboxError::Fenced);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 0);
}

#[test]
fn full_queue_nacks_without_drop() {
    let f = Fixture::new();
    for n in 1..=64 {
        f.clock.advance(1);
        f.receive(n, n);
    }
    f.clock.advance(1);
    assert_eq!(
        f.store.accept(&f.authority, f.message(65, 65)).unwrap_err(),
        MailboxError::QueueFull
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 64);
    assert_eq!(f.deliver().message().seq, 1);
}

#[test]
fn fast_consumer_hits_retained_body_quota() {
    let f = Fixture::new();
    // 257 near-limit bodies exceed 64 MiB even though every row is consumed.
    for n in 1..=256 {
        f.clock.advance(1);
        let mut message = f.message(n, n);
        message.body.bytes = vec![b'x'; crate::messaging::MAX_BODY_BYTES - 10];
        f.store.accept(&f.authority, message).unwrap();
        let selected = f.deliver();
        f.store.adopt(&selected).unwrap();
    }
    f.clock.advance(1);
    let mut over = f.message(257, 257);
    over.body.bytes = vec![b'x'; crate::messaging::MAX_BODY_BYTES - 10];
    assert_eq!(
        f.store.accept(&f.authority, over).unwrap_err(),
        MailboxError::Quota("bodies")
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 256);
    f.clock.advance(604801);
    let mut fresh = f.message(258, 1);
    fresh.stream_id = Some(Uuid::from_u128(501));
    f.store.accept(&f.authority, fresh).unwrap();
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        1,
        "expired consumed bodies must be GCed before quota refusal"
    );
}

#[test]
fn cross_instance_chain_depth_is_capped() {
    let f = Fixture::new();
    let mut parent = None;
    for n in 1..=9 {
        f.clock.advance(1);
        let mut message = f.message(n, n);
        message.in_reply_to = parent;
        // Wire depth is maliciously reset; receiver must derive it.
        message.hop_count = 0;
        message.ancestry.clear();
        let accepted = f.store.accept(&f.authority, message.clone()).unwrap();
        assert_eq!(accepted.chain.depth, n as u8 - 1);
        parent = Some(message.id);
    }
    let mut too_deep = f.message(10, 10);
    too_deep.in_reply_to = parent;
    assert_eq!(
        f.store.accept(&f.authority, too_deep).unwrap_err(),
        MailboxError::ChainDepth
    );
}

#[test]
fn send_reply_resolves_parent_from_outbox_without_transcript() {
    let f = Fixture::new();
    let sent = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    let mut reply = f.message(2, 1);
    reply.in_reply_to = Some(sent.message_id);
    reply.stream_id = Some(Uuid::from_u128(502));
    let received = f.store.accept(&f.authority, reply).unwrap();
    assert_eq!(received.chain.depth, 1);
    assert_eq!(received.chain.ancestry, vec![sent.message_id]);
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 0);
}

#[test]
fn idempotent_retry_after_parent_erase_returns_same_ack() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut reply = f.message(2, 2);
    reply.in_reply_to = Some(Uuid::from_u128(1001));
    let accepted = f.store.accept(&f.authority, reply.clone()).unwrap();
    f.store.erase_scope(&f.recipient, &|| Ok(())).unwrap();
    assert_eq!(f.store.accept(&f.authority, reply).unwrap(), accepted);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_outbox"), 0);
}

#[test]
fn altered_parent_retry_after_erase_is_idempotency_conflict() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut reply = f.message(2, 2);
    reply.in_reply_to = Some(Uuid::from_u128(1001));
    f.store.accept(&f.authority, reply.clone()).unwrap();
    f.store.erase_scope(&f.recipient, &|| Ok(())).unwrap();
    reply.in_reply_to = None;
    assert_eq!(
        f.store.accept(&f.authority, reply).unwrap_err(),
        MailboxError::IdempotencyConflict
    );
}

struct LocalResolver {
    store: SqliteMailbox,
    authority: MailboxAuthority,
    scope: MailboxScope,
}
#[async_trait::async_trait]
impl crate::messaging::send::MailboxResolver for LocalResolver {
    async fn resolve(
        &self,
        target: &SessionAddr,
    ) -> crate::messaging::mailbox::MailboxResult<crate::messaging::send::MailboxDestination> {
        if *target != self.authority.destination {
            return Err(MailboxError::Unauthorized);
        }
        Ok(crate::messaging::send::MailboxDestination {
            authority: self.authority.clone(),
            pin: self
                .store
                .recipient_pin(&self.authority, &self.scope, "canonical-target")?,
            pin_expires_at: 10_000,
        })
    }
}
impl Fixture {
    fn sender_port(
        &self,
        inherited: Option<crate::messaging::InheritedProvenance>,
    ) -> Arc<dyn crate::messaging::send::MailboxSender> {
        Arc::new(
            crate::memory::sqlite::mailbox::SqliteMailboxSender::new(
                self.store.clone(),
                self.source.clone(),
                self.sender.clone(),
                Arc::new(LocalResolver {
                    store: self.store.clone(),
                    authority: self.authority.clone(),
                    scope: self.recipient.clone(),
                }),
                inherited,
            )
            .unwrap(),
        )
    }
}

#[tokio::test]
async fn prx_message_tool_emits_one_external_send_envelope() {
    use crate::tools::Tool;
    let f = Fixture::new();
    let tool = crate::tools::prx_message::PrxMessageTool::new(f.sender_port(None));
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::prx_message::PrxMessageTool::new(
        f.sender_port(None),
    ))];
    let catalog = crate::tools::capability::catalog::CapabilityCatalog::build(
        &registry,
        &crate::tools::capability::catalog::CatalogSources::default(),
        &crate::tools::capability::catalog::SurfacePolicy::default(),
    )
    .unwrap();
    assert_eq!(
        catalog.entry_for_public_name("prx_message").unwrap().effect,
        crate::causal_tree::branch::SideEffectClass::ExternalSend
    );
    let result = tool
        .execute(serde_json::json!({"to":{"session":"target"},"text":"ordinary Data"}))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_outbox"), 1);
    let out = f.store.pending_outbox(&f.sender, &f.source).unwrap();
    assert_eq!(out[0].message().kind, MessageKind::Data);
    assert_eq!(out[0].message().body.bytes, b"ordinary Data");
    assert_eq!(out[0].message().from, f.source);
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "sender tool does not execute the recipient"
    );
    for invalid in [
        serde_json::json!({"to":{"session":"target","owner":null},"text":"x"}),
        serde_json::json!({"to":{"session":"target"},"text":"x","in_reply_to":null}),
    ] {
        assert!(tool.execute(invalid).await.is_err());
    }
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_outbox"), 1);
}

#[tokio::test]
async fn prx_message_tool_send_is_stamped_with_runtime_causal_chain() {
    use crate::tools::Tool;
    let f = Fixture::new();
    let accepted = f.receive(1, 1);
    let inherited = crate::messaging::InheritedProvenance {
        remote: crate::messaging::RemotePeerAttr {
            issuer: "prx-issuer".into(),
            subject: "prx-peer".into(),
            message_id: accepted.message_id,
        },
        chain: accepted.chain,
    };
    let tool = crate::tools::prx_message::PrxMessageTool::new(f.sender_port(Some(inherited.clone())));
    let result = tool
        .execute(serde_json::json!({"to":{"session":"target"},"text":"reply"}))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let out = f.store.pending_outbox(&f.sender, &f.source).unwrap();
    assert_eq!(out[0].message().in_reply_to, Some(inherited.chain.trigger));
    assert_eq!(out[0].message().hop_count, 1);
    assert_eq!(f.scalar("SELECT sends FROM prx_remote_trees"), 1);
}

#[tokio::test]
async fn sessions_send_remote_addr_is_not_steer() {
    use crate::tools::Tool;
    let f = Fixture::new();
    let tool = crate::tools::sessions_send::SessionsSendTool::new(Arc::new(tokio::sync::RwLock::new(Vec::new())))
        .with_mailbox(f.sender_port(None));
    assert!(tool.parameters_schema().get("oneOf").is_some());
    assert!(
        tool.execute(serde_json::json!({"to":{"session":"target"},"message":"mailbox Data"}))
            .await
            .unwrap()
            .success
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_outbox"), 1);
    assert!(
        tool.execute(serde_json::json!({"run_id":"run-target","to":{"session":"target"},"message":"ambiguous"}))
            .await
            .is_err()
    );
}

#[test]
fn slash_msg_builds_envelope_to_peer_session() {
    let f = Fixture::new();
    let (target, text) =
        crate::chat::commands::msg::parse("/msg peer-node target/prx_agent hello peer", &f.source).unwrap();
    assert_eq!(target, SessionAddr::new("peer-node", O, "target", Some("prx_agent")));
    assert_eq!(text, "hello peer");
    assert!(crate::chat::commands::msg::parse("/msg local target/ ", &f.source).is_err());
}

#[test]
fn mailbox_formats_and_natural_key_collisions_fail_closed() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store.accept(&f.authority, original.clone()).unwrap();
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=json_set(receipt_meta,'$.unknown',1)",
            [],
        )
        .unwrap();
    assert_eq!(
        f.store.accept(&f.authority, original.clone()).unwrap_err(),
        MailboxError::InvalidMetadata
    );
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=json_remove(receipt_meta,'$.unknown')",
            [],
        )
        .unwrap();
    let mut changed = f.target.clone();
    changed.session = "other-target".into();
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET to_addr=?1",
            [serde_json::to_string(&changed).unwrap()],
        )
        .unwrap();
    assert_eq!(
        f.store.accept(&f.authority, original).unwrap_err(),
        MailboxError::NaturalKeyCollision
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 1);
}

#[test]
fn mailbox_scope_pin_role_and_run_checks_fail_closed() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.exec = Some(ExecRef {
        run: Some("replaced-run".into()),
    });
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::StaleRun
    );
    f.receive(1, 1);
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET recipient_generation=NULL", [])
        .unwrap();
    assert!(matches!(
        f.store.deliver(&f.recipient, &f.target),
        Err(MailboxError::CorruptState)
    ));
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 0);
}

#[test]
fn mailbox_identity_and_state_transitions_are_immutable() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_messages SET body_bytes=?1",
            [b"tampered".as_slice()],
        )
        .unwrap();
    assert_eq!(f.store.adopt(&delivery).unwrap_err(), MailboxError::IdentityImmutable);
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 0);
    assert_eq!(f.scalar("SELECT state FROM prx_mailbox_receipts"), 1);
}

#[test]
fn mailbox_transport_and_chain_metadata_is_runtime_owned() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.from = SessionAddr::new("forged-instance", "forged-owner", "forged-session", None);
    message.origin = Some(crate::messaging::MessageOrigin::RemotePeer);
    message.auth = Some(crate::messaging::MessageAuth {
        issuer: "forged".into(),
        subject: "forged".into(),
        method: "jwt".into(),
    });
    message.hop_count = 8;
    message.ancestry = vec![Uuid::nil(); 8];
    let receipt = f.store.accept(&f.authority, message).unwrap();
    assert_eq!(receipt.chain.depth, 0);
    assert!(receipt.remote.is_none());
    let delivered = f.deliver();
    assert_eq!(delivered.message().from, f.source);
    assert_eq!(
        delivered.message().origin,
        Some(crate::messaging::MessageOrigin::LocalModel)
    );
    assert!(delivered.message().auth.is_none());
}

#[test]
fn mailbox_replay_slot_and_message_id_reuse_are_checked() {
    let f = Fixture::new();
    let message = f.message(1, 1);
    let first = f.store.accept(&f.authority, message.clone()).unwrap();
    let mut occupied = f.message(2, 1);
    assert_eq!(
        f.store.accept(&f.authority, occupied.clone()).unwrap_err(),
        MailboxError::SeqConflict
    );
    occupied.seq = 2;
    occupied.id = message.id;
    assert_eq!(
        f.store.accept(&f.authority, occupied).unwrap_err(),
        MailboxError::IdReuse
    );
    assert_eq!(f.store.accept(&f.authority, message).unwrap(), first);
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 1);
}

#[test]
fn mailbox_progress_and_gap_transitions_are_checked() {
    let f = Fixture::new();
    let message = f.message(1, 1026);
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::Quota("seq")
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        0,
        "gap reservation rolls back stream insert"
    );
    f.receive(1, 1);
    let delivery = f.deliver();
    assert_eq!(
        f.store.recover_delivery(&f.recipient, &f.target).unwrap_err(),
        MailboxError::RoleBusy
    );
    f.store.adopt(&delivery).unwrap();
    drop(delivery);
    f.store.recover_delivery(&f.recipient, &f.target).unwrap();
    assert!(f.store.deliver(&f.recipient, &f.target).unwrap().is_none());
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET progress=json_set(progress,'$.next_seq',0)",
            [],
        )
        .unwrap();
    assert!(matches!(
        f.store.deliver(&f.recipient, &f.target),
        Err(MailboxError::CorruptState)
    ));
}

#[test]
fn fresh_sender_session_labels_after_receipt_gc_hit_stream_quota() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    f.store.adopt(&delivery).unwrap();
    drop(delivery);
    f.clock.advance(604801);
    f.store.gc_scope(&f.recipient).unwrap();
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 0);
    // Populate the remaining bounded tombstone inventory from the real retired
    // row; no messages/receipts remain to provide accidental quota coverage.
    f.memory.pool.write().execute_batch("WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<4095) INSERT INTO prx_mailbox_seq(workspace_id,owner_id,sender_ma_scope_id,sender_incarnation,sender_generation,recipient_ma_scope_id,recipient_incarnation,recipient_generation,from_addr,to_addr,wire_stream_id,state,progress,updated_at) SELECT workspace_id,owner_id,sender_ma_scope_id,sender_incarnation,sender_generation,recipient_ma_scope_id,recipient_incarnation,recipient_generation,json_set(from_addr,'$.session','retired-'||v),to_addr,randomblob(16),state,progress,updated_at FROM prx_mailbox_seq,n WHERE stream_id=1;").unwrap();
    let mut source = f.source.clone();
    source.session = "fresh-sender-label".into();
    let authority = MailboxAuthority::local(W.into(), source, f.target.clone()).unwrap();
    let mut message = f.message(2, 1);
    message.stream_id = Some(Uuid::new_v4());
    message.to_pin = f
        .store
        .recipient_pin(&authority, &f.recipient, "canonical-target")
        .unwrap();
    assert_eq!(
        f.store.accept(&authority, message).unwrap_err(),
        MailboxError::Quota("streams")
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_seq"), 4096);
}

#[tokio::test]
async fn child_message_reaches_main_mailbox() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let mut f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.source.clone(), None, None))
        .await
        .unwrap();
    let child = registry.reconstruct(&f.source).await.unwrap();
    f.sender = child.mailbox_scope().unwrap().clone();
    let provisional = f.message(1, 0);
    f.store
        .publish(&f.authority, provisional, &f.sender, f.clock.now() + 300, None)
        .unwrap();
    let outgoing = f.store.pending_outbox(&f.sender, &f.source).unwrap();
    f.store.accept(&f.authority, outgoing[0].message().clone()).unwrap();
    let delivery = f.deliver();
    let receipt = f.store.adopt(&delivery).unwrap();
    assert_eq!(receipt.state, DeliveryState::Consumed);
    assert_eq!(f.scalar("SELECT count(*) FROM conversation_turns"), 1);
}

#[tokio::test]
async fn sibling_message_is_owner_scoped() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    for address in [&f.source, &f.target] {
        registry
            .insert(&ChildSession::new(address.clone(), None, None))
            .await
            .unwrap();
    }
    let target = registry.reconstruct(&f.target).await.unwrap();
    let scope = target.mailbox_scope().unwrap().clone();
    let canonical = ChildSessionRegistry::canonical_session_key(&f.target).unwrap();
    let mut message = f.message(1, 1);
    message.to_pin = f.store.recipient_pin(&f.authority, &scope, &canonical).unwrap();
    f.store.accept(&f.authority, message.clone()).unwrap();
    let mut foreign = f.source.clone();
    foreign.owner = "other-owner".into();
    assert!(matches!(
        MailboxAuthority::local(W.into(), foreign, f.target.clone()),
        Err(MailboxError::Unauthorized)
    ));
    let mut foreign_target = f.target.clone();
    foreign_target.owner = "other-owner".into();
    message.to = foreign_target;
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::Unauthorized
    );
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_receipts"), 1);
}

#[tokio::test]
async fn child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    use crate::memory::child_sessions::{ChildSessionContext, ChildSessionError, ChildSessionStore};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.target.clone(), None, None))
        .await
        .unwrap();
    let key = ChildSessionRegistry::canonical_session_key(&f.target).unwrap();
    let context = ChildSessionContext {
        workspace_id: W.into(),
        addr: f.target.clone(),
    };
    let original = f
        .memory
        .pool
        .write()
        .query_row("SELECT record FROM child_sessions WHERE key=?1", [&key], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    f.memory.pool.write().execute("UPDATE child_sessions SET workspace_id=NULL,owner_id=NULL,instance_id=NULL,session_id=NULL,agent_key=NULL,ma_scope_id=NULL,incarnation=NULL,generation=NULL WHERE key=?1",[&key]).unwrap();
    let refused = match ChildSessionStore::load(&f.memory, &key, &context) {
        Ok(_) => panic!("quarantined legacy row loaded"),
        Err(error) => error,
    };
    assert!(refused.is::<ChildSessionError>());
    let recovered =
        ChildSessionStore::update(&f.memory, &key, &context, &mut |old| Ok(old.unwrap().to_owned())).unwrap();
    assert_eq!(recovered.record, original);
    drop(recovered);
    // A partially populated row is never a backfill candidate.
    f.memory
        .pool
        .write()
        .execute("UPDATE child_sessions SET generation=NULL WHERE key=?1", [&key])
        .unwrap();
    assert!(ChildSessionStore::update(&f.memory, &key, &context, &mut |_| Ok(original.clone())).is_err());
    assert_eq!(
        f.memory
            .pool
            .write()
            .query_row("SELECT record FROM child_sessions WHERE key=?1", [&key], |r| r
                .get::<_, String>(0))
            .unwrap(),
        original
    );
}

#[test]
fn mailbox_index_plans_use_scope_and_stream_prefixes() {
    let f = Fixture::new();
    let writer = f.memory.pool.write();
    for (query, index) in [
        (
            "SELECT message_id FROM prx_mailbox_receipts WHERE ma_scope_id='scope' AND expires_at>=0",
            "mailbox_receipt_scope_expiry",
        ),
        (
            "SELECT id FROM prx_mailbox_messages WHERE stream_id=1 AND state=0 AND seq>x'0000000000000000' ORDER BY seq",
            "mailbox_message_delivery",
        ),
        (
            "SELECT id FROM prx_mailbox_outbox WHERE stream_id=1 AND state=0 ORDER BY seq",
            "mailbox_outbox_delivery",
        ),
        (
            "SELECT stream_id FROM prx_mailbox_seq WHERE recipient_ma_scope_id='scope'",
            "mailbox_seq_recipient",
        ),
        (
            "SELECT key FROM child_sessions WHERE ma_scope_id='scope'",
            "child_sessions_scope",
        ),
    ] {
        let mut statement = writer.prepare(&format!("EXPLAIN QUERY PLAN {query}")).unwrap();
        let details = statement
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" ");
        assert!(details.contains(index), "{query}: {details}");
        if index == "mailbox_receipt_scope_expiry" {
            assert!(
                details.contains("COVERING"),
                "id-reuse probe must not load receipt bodies: {details}"
            );
        }
    }
}

#[test]
fn mailbox_u64_sequence_exhaustion_never_wraps_or_truncates() {
    let f = Fixture::new();
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    let mut progress: serde_json::Value = serde_json::from_str(
        &f.memory
            .pool
            .write()
            .query_row("SELECT progress FROM prx_mailbox_seq", [], |r| r.get::<_, String>(0))
            .unwrap(),
    )
    .unwrap();
    progress["next_seq"] = serde_json::json!(u64::MAX);
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET progress=?1", [progress.to_string()])
        .unwrap();
    let last = f
        .store
        .publish(&f.authority, f.message(2, 0), &f.sender, f.clock.now() + 300, None)
        .unwrap();
    assert_eq!(last.seq, u64::MAX);
    assert_eq!(
        f.store
            .publish(&f.authority, f.message(3, 0), &f.sender, f.clock.now() + 300, None)
            .unwrap_err(),
        MailboxError::Quota("seq")
    );
    let raw = f
        .memory
        .pool
        .write()
        .query_row(
            "SELECT hex(seq) FROM prx_mailbox_outbox WHERE id=?1",
            [last.row_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(raw, "FFFFFFFFFFFFFFFF");
}
