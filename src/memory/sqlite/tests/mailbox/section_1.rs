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
    memory: SqliteMemory,
    store: SqliteMailbox,
    clock: Arc<Clock>,
    source: SessionAddr,
    target: SessionAddr,
    sender: MailboxScope,
    recipient: MailboxScope,
    authority: MailboxAuthority,
    // Close every owned connection before removing its database directory.
    _dir: TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("test: new: fixture setup succeeds");
        let memory = SqliteMemory::new(dir.path()).expect("test: new: SqliteMemory::new succeeds");
        {
            let writer = memory.pool.write();
            writer
                .execute(
                    "INSERT INTO agent_incarnations VALUES (?1,?2,'m',?3,'active',?4)",
                    params![W, O, Uuid::from_u128(INC).to_string(), "0".repeat(64)],
                )
                .expect("test: new: execute succeeds");
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
                    .expect("test: new: execute succeeds");
            }
        }
        let clock = Arc::new(Clock(AtomicI64::new(1000)));
        let store = SqliteMailbox::new(
            memory.clone(),
            "prx-node".into(),
            [7; 32],
            clock.clone(),
        )
        .expect("test: new: SqliteMailbox::new succeeds");
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
        let authority = MailboxAuthority::local(W.into(), source.clone(), target.clone())
            .expect("test: new: MailboxAuthority::local succeeds");
        store
            .set_execution(&recipient, &target, Some("run-target".into()))
            .expect("test: new: set_execution succeeds");
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
        let canonical: String = self
            .memory
            .pool
            .write()
            .query_row(
                "SELECT session_key FROM agent_scopes WHERE scope_id=?1",
                [self.recipient.scope_id.to_string()],
                |row| row.get(0),
            )
            .expect("test: read recipient canonical key");
        let pin = self
            .store
            .recipient_pin(&self.authority, &self.recipient, &canonical)
            .expect("test: message: recipient_pin succeeds");
        PrxMessage {
            id: Uuid::from_u128(1000 + u128::from(n)),
            idempotency: IdempotencyKey::parse(&format!("mailbox-key-{n:08}"))
                .expect("test: message: fixture setup succeeds"),
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
        self.store
            .accept(&self.authority, self.message(n, seq))
            .expect("test: receive: accept succeeds")
    }
    fn deliver(&self) -> MailboxDelivery {
        self.store
            .deliver(&self.recipient, &self.target)
            .expect("test: deliver: deliver succeeds")
            .expect("test: deliver: deliver returns a value")
    }
    fn scalar(&self, sql: &str) -> i64 {
        self.memory
            .pool
            .write()
            .query_row(sql, [], |r| r.get(0))
            .expect("test: scalar: query_row succeeds")
    }
}

#[test]
fn in_process_send_is_durable_and_acked() {
    let f = Fixture::new();
    let provisional = f.message(1, 0);
    let published = f
        .store
        .publish(&f.authority, provisional, &f.sender, f.clock.now() + 300, None)
        .expect("test: in_process_send_is_durable_and_acked: publish succeeds");
    assert_eq!(
        published.seq, 1,
        "in process send is durable and acked: IPC identity or delivery state changed"
    );
    let outgoing = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: in_process_send_is_durable_and_acked: pending_outbox succeeds");
    let sent = outgoing
        .first()
        .expect("test: in_process_send_is_durable_and_acked: first succeeds")
        .message()
        .clone();
    let accepted = f
        .store
        .accept(&f.authority, sent.clone())
        .expect("test: in_process_send_is_durable_and_acked: accept succeeds");
    f.store
        .acknowledge(&f.sender, &f.target, sent.id, accepted.state)
        .expect("test: in_process_send_is_durable_and_acked: acknowledge succeeds");
    let retry = f
        .store
        .publish(&f.authority, sent, &f.sender, f.clock.now() + 300, None)
        .expect("test: in_process_send_is_durable_and_acked: publish succeeds");
    assert_eq!(
        retry.ack_state,
        Some(DeliveryState::Accepted),
        "in process send is durable and acked: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "in process send is durable and acked: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        1,
        "in process send is durable and acked: IPC identity or delivery state changed"
    );
}

#[test]
fn adopt_is_atomic_transcript_and_consume() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    let first = f
        .store
        .adopt(&delivery)
        .expect("test: adopt_is_atomic_transcript_and_consume: adopt succeeds");
    let again = f
        .store
        .adopt(&delivery)
        .expect("test: adopt_is_atomic_transcript_and_consume: adopt succeeds");
    assert_eq!(
        first, again,
        "adopt is atomic transcript and consume: atomic adoption or history projection changed"
    );
    assert_eq!(
        first.state,
        DeliveryState::Consumed,
        "adopt is atomic transcript and consume: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        1,
        "adopt is atomic transcript and consume: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM message_events WHERE source='prx_mailbox'"),
        1,
        "adopt is atomic transcript and consume: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts WHERE state=2 AND adopted_turn_id IS NOT NULL"),
        1,
        "adopt is atomic transcript and consume: atomic adoption or history projection changed"
    );
}

#[test]
fn same_id_different_key_different_body_is_id_reuse() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store
        .accept(&f.authority, original.clone())
        .expect("test: same_id_different_key_different_body_is_id_reuse: accept succeeds");
    let mut other = original;
    other.idempotency = IdempotencyKey::parse("another-mailbox-key")
        .expect("test: same_id_different_key_different_body_is_id_reuse: IdempotencyKey::parse succeeds");
    other.seq = 2;
    other.body.bytes = b"changed".to_vec();
    assert_eq!(
        f.store.accept(&f.authority, other).unwrap_err(),
        MailboxError::IdReuse,
        "same id different key different body is id reuse: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        1,
        "same id different key different body is id reuse: payload retention and erasure boundary changed"
    );
}

#[test]
fn same_key_changed_exec_is_idempotency_conflict() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store
        .accept(&f.authority, original.clone())
        .expect("test: same_key_changed_exec_is_idempotency_conflict: accept succeeds");
    let mut retry = original;
    retry.exec = Some(ExecRef {
        run: Some("different-run".into()),
    });
    assert_eq!(
        f.store.accept(&f.authority, retry).unwrap_err(),
        MailboxError::IdempotencyConflict,
        "same key changed exec is idempotency conflict: replay identity or receipt stability changed"
    );
}

#[test]
fn reordered_duplicate_seq_holds_fifo() {
    let f = Fixture::new();
    let second = f.message(2, 2);
    let accepted = f
        .store
        .accept(&f.authority, second.clone())
        .expect("test: reordered_duplicate_seq_holds_fifo: accept succeeds");
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: reordered_duplicate_seq_holds_fifo: deliver succeeds")
            .is_none(),
        "reordered duplicate seq holds fifo: expected messaging contract changed"
    );
    assert_eq!(
        f.store
            .accept(&f.authority, second)
            .expect("test: reordered_duplicate_seq_holds_fifo: accept succeeds"),
        accepted,
        "reordered duplicate seq holds fifo: expected messaging contract changed"
    );
    f.receive(1, 1);
    let first = f.deliver();
    assert_eq!(
        first.message().seq,
        1,
        "reordered duplicate seq holds fifo: expected messaging contract changed"
    );
    f.store
        .adopt(&first)
        .expect("test: reordered_duplicate_seq_holds_fifo: adopt succeeds");
    drop(first);
    let second = f.deliver();
    assert_eq!(
        second.message().seq,
        2,
        "reordered duplicate seq holds fifo: expected messaging contract changed"
    );
    f.store
        .adopt(&second)
        .expect("test: reordered_duplicate_seq_holds_fifo: adopt succeeds");
}

#[test]
fn permanent_seq_gap_retires_and_unblocks() {
    let f = Fixture::new();
    f.receive(2, 2);
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: permanent_seq_gap_retires_and_unblocks: deliver succeeds")
            .is_none(),
        "permanent seq gap retires and unblocks: expected messaging contract changed"
    );
    f.clock.advance(30);
    let delivery = f.deliver();
    assert_eq!(
        delivery.message().seq,
        2,
        "permanent seq gap retires and unblocks: expected messaging contract changed"
    );
    f.store
        .adopt(&delivery)
        .expect("test: permanent_seq_gap_retires_and_unblocks: adopt succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts WHERE state=2"),
        1,
        "permanent seq gap retires and unblocks: expected messaging contract changed"
    );
}

#[test]
fn late_seq_at_or_below_consume_cursor_is_rejected_after_hole_gc() {
    let f = Fixture::new();
    f.receive(2, 2);
    f.clock.advance(30);
    let delivery = f.deliver();
    f.store
        .adopt(&delivery)
        .expect("test: late_seq_at_or_below_consume_cursor_is_rejected_after_hole_gc: adopt succeeds");
    drop(delivery);
    f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
    // Fresh pending work keeps the stream initialized while eligible old
    // bodies/receipts/holes are collected. No forbidden same-UUID reset.
    f.receive(3, 3);
    f.store
        .gc_scope(&f.recipient)
        .expect("test: late_seq_at_or_below_consume_cursor_is_rejected_after_hole_gc: gc_scope succeeds");
    assert_eq!(
        f.scalar("SELECT json_array_length(progress,'$.seq_skipped') FROM prx_mailbox_seq"),
        0,
        "late seq at or below consume cursor is rejected after hole gc: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.store.accept(&f.authority, f.message(4, 1)).unwrap_err(),
        MailboxError::SeqAlreadyConsumed,
        "late seq at or below consume cursor is rejected after hole gc: atomic adoption or history projection changed"
    );
}

#[test]
fn outbox_parent_id_is_not_reused_after_outbox_gc() {
    let f = Fixture::new();
    let published = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: outbox_parent_id_is_not_reused_after_outbox_gc: publish succeeds");
    f.store
        .acknowledge(&f.sender, &f.target, published.message_id, DeliveryState::Accepted)
        .expect("test: outbox_parent_id_is_not_reused_after_outbox_gc: acknowledge succeeds");
    f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
    let gc = f
        .store
        .gc_scope(&f.sender)
        .expect("test: outbox_parent_id_is_not_reused_after_outbox_gc: gc_scope succeeds");
    assert_eq!(
        gc.outboxes, 1,
        "outbox parent id is not reused after outbox gc: expected messaging contract changed"
    );
    assert_eq!(
        gc.parents, 0,
        "outbox parent id is not reused after outbox gc: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_parents"),
        1,
        "outbox parent id is not reused after outbox gc: expected messaging contract changed"
    );
    let mut next = f.message(2, 0);
    next.stream_id = Some(Uuid::from_u128(STREAM + 1));
    let published2 = f
        .store
        .publish(&f.authority, next, &f.sender, f.clock.now() + 300, None)
        .expect("test: outbox_parent_id_is_not_reused_after_outbox_gc: publish succeeds");
    assert!(
        published2.row_id > published.row_id,
        "outbox parent id is not reused after outbox gc: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_parents"),
        2,
        "outbox parent id is not reused after outbox gc: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "outbox parent id is not reused after outbox gc: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages WHERE id NOT IN(SELECT id FROM prx_mailbox_receipts)"),
        0,
        "outbox parent id is not reused after outbox gc: expected messaging contract changed"
    );
}

#[test]
fn crash_before_adopt_redelivers_once() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    let id = delivery.message().id;
    drop(delivery);
    f.store
        .recover_delivery(&f.recipient, &f.target)
        .expect("test: crash_before_adopt_redelivers_once: recover_delivery succeeds");
    let retry = f.deliver();
    assert_eq!(
        retry.message().id,
        id,
        "crash before adopt redelivers once: atomic adoption or history projection changed"
    );
    f.store
        .adopt(&retry)
        .expect("test: crash_before_adopt_redelivers_once: adopt succeeds");
    drop(retry);
    f.store
        .recover_delivery(&f.recipient, &f.target)
        .expect("test: crash_before_adopt_redelivers_once: recover_delivery succeeds");
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: crash_before_adopt_redelivers_once: deliver succeeds")
            .is_none(),
        "crash before adopt redelivers once: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        1,
        "crash before adopt redelivers once: atomic adoption or history projection changed"
    );
}

#[test]
fn crash_during_adopt_rolls_back() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    f.memory.pool.write().execute_batch("CREATE TEMP TRIGGER test_refuse_consume BEFORE UPDATE OF state ON prx_mailbox_receipts WHEN NEW.state=2 BEGIN SELECT RAISE(ABORT,'fixture consume refusal'); END;").expect("test: crash_during_adopt_rolls_back: fixture setup succeeds");
    assert_eq!(
        f.store.adopt(&delivery).unwrap_err(),
        MailboxError::Storage,
        "crash during adopt rolls back: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        0,
        "crash during adopt rolls back: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM message_events WHERE source='prx_mailbox'"),
        0,
        "crash during adopt rolls back: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts WHERE state=1 AND adopted_turn_id IS NULL"),
        1,
        "crash during adopt rolls back: atomic adoption or history projection changed"
    );
    f.memory
        .pool
        .write()
        .execute_batch("DROP TRIGGER test_refuse_consume")
        .expect("test: crash_during_adopt_rolls_back: execute_batch succeeds");
    f.store
        .adopt(&delivery)
        .expect("test: crash_during_adopt_rolls_back: adopt succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        1,
        "crash during adopt rolls back: atomic adoption or history projection changed"
    );
}

#[test]
fn mailbox_shared_ids_survive_rowid_reuse_after_gc() {
    let f = Fixture::new();
    f.memory.pool.write().execute_batch("CREATE TEMP TRIGGER test_refuse_body BEFORE INSERT ON prx_mailbox_messages BEGIN SELECT RAISE(ABORT,'fixture body refusal'); END;").expect("test: mailbox_shared_ids_survive_rowid_reuse_after_gc: execute_batch succeeds");
    assert_eq!(
        f.store.accept(&f.authority, f.message(1, 1)).unwrap_err(),
        MailboxError::Storage,
        "mailbox shared ids survive rowid reuse after gc: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        0,
        "mailbox shared ids survive rowid reuse after gc: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute_batch("DROP TRIGGER test_refuse_body")
        .expect("test: mailbox_shared_ids_survive_rowid_reuse_after_gc: execute_batch succeeds");
    let accepted = f.receive(1, 1);
    let delivery = f.deliver();
    f.store
        .adopt(&delivery)
        .expect("test: mailbox_shared_ids_survive_rowid_reuse_after_gc: adopt succeeds");
    drop(delivery);
    f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
    f.receive(2, 2);
    // Observe the required lifetime order, rather than a zero-row assertion.
    f.memory.pool.write().execute_batch("CREATE TEMP TRIGGER test_require_body_first BEFORE DELETE ON prx_mailbox_receipts WHEN EXISTS(SELECT 1 FROM prx_mailbox_messages WHERE id=OLD.id) BEGIN SELECT RAISE(ABORT,'body still references receipt'); END;").expect("test: mailbox_shared_ids_survive_rowid_reuse_after_gc: fixture setup succeeds");
    let gc = f
        .store
        .gc_scope(&f.recipient)
        .expect("test: mailbox_shared_ids_survive_rowid_reuse_after_gc: gc_scope succeeds");
    assert_eq!(
        gc.bodies, 1,
        "mailbox shared ids survive rowid reuse after gc: expected messaging contract changed"
    );
    assert_eq!(
        gc.receipts, 1,
        "mailbox shared ids survive rowid reuse after gc: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar(&format!(
            "SELECT count(*) FROM prx_mailbox_messages WHERE id={}",
            accepted.row_id
        )),
        0,
        "mailbox shared ids survive rowid reuse after gc: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar(&format!(
            "SELECT count(*) FROM prx_mailbox_receipts WHERE id={}",
            accepted.row_id
        )),
        0,
        "mailbox shared ids survive rowid reuse after gc: expected messaging contract changed"
    );
}

#[test]
fn missing_recipient_pin_is_rejected_not_filled() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.to_pin.token.clear();
    assert_eq!(
        f.store.accept(&f.authority, message.clone()).unwrap_err(),
        MailboxError::PinRequired,
        "missing recipient pin is rejected not filled: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.store
            .publish(&f.authority, message, &f.sender, i64::MAX, None)
            .expect_err("test: publish requires a recipient pin"),
        MailboxError::PinRequired,
        "missing authority must use the same required-pin code before publishing or accepting"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        0,
        "publication without a pin must not retain an outgoing payload"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        0,
        "missing recipient pin is rejected not filled: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        0,
        "missing recipient pin is rejected not filled: recipient authority or scope isolation changed"
    );
}

#[test]
fn unknown_parent_is_nacked() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.in_reply_to = Some(Uuid::from_u128(999));
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::UnknownParent,
        "unknown parent is nacked: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "unknown parent is nacked: expected messaging contract changed"
    );
}

#[test]
fn retry_with_altered_in_reply_to_is_idempotency_conflict() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store
        .accept(&f.authority, original.clone())
        .expect("test: retry_with_altered_in_reply_to_is_idempotency_conflict: accept succeeds");
    let mut changed = original;
    changed.in_reply_to = Some(Uuid::from_u128(999));
    assert_eq!(
        f.store.accept(&f.authority, changed).unwrap_err(),
        MailboxError::IdempotencyConflict,
        "retry with altered in reply to is idempotency conflict: replay identity or receipt stability changed"
    );
}

#[test]
fn ack_does_not_emit_ack() {
    let f = Fixture::new();
    let published = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: ack_does_not_emit_ack: publish succeeds");
    f.store
        .acknowledge(&f.sender, &f.target, published.message_id, DeliveryState::Accepted)
        .expect("test: ack_does_not_emit_ack: acknowledge succeeds");
    f.store
        .acknowledge(&f.sender, &f.target, published.message_id, DeliveryState::Consumed)
        .expect("test: ack_does_not_emit_ack: acknowledge succeeds");
    f.store
        .acknowledge(&f.sender, &f.target, published.message_id, DeliveryState::Accepted)
        .expect("test: ack_does_not_emit_ack: acknowledge succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "ack does not emit ack: expected messaging contract changed"
    );
    assert!(
        f.store
            .pending_outbox(&f.sender, &f.source)
            .expect("test: ack_does_not_emit_ack: pending_outbox succeeds")
            .is_empty(),
        "ack does not emit ack: expected messaging contract changed"
    );
    let retry = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: ack_does_not_emit_ack: publish succeeds");
    assert_eq!(
        retry.ack_state,
        Some(DeliveryState::Consumed),
        "ack does not emit ack: expected messaging contract changed"
    );
}

#[test]
fn offline_session_receives_after_attach() {
    let f = Fixture::new();
    f.store
        .close_execution(&f.recipient, &f.target, "run-target")
        .expect("test: offline_session_receives_after_attach: close_execution succeeds");
    f.receive(1, 1);
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: offline_session_receives_after_attach: deliver succeeds")
            .is_none(),
        "offline session receives after attach: expected messaging contract changed"
    );
    f.store
        .set_execution(&f.recipient, &f.target, Some("reopened-target".into()))
        .expect("test: offline_session_receives_after_attach: set_execution succeeds");
    let delivered = f.deliver();
    assert_eq!(
        delivered.message().body.bytes,
        b"request-1",
        "offline session receives after attach: expected messaging contract changed"
    );
}

#[test]
fn mailbox_erase_drops_bodies_keeps_receipts() {
    let f = Fixture::new();
    f.receive(1, 1);
    let queued = f.message(2, 2);
    let report = f
        .store
        .erase_scope(&f.recipient, Duration::from_secs(5), &|| Ok(()))
        .expect("test: mailbox_erase_drops_bodies_keeps_receipts: erase_scope succeeds");
    assert_eq!(
        report.remaining, 0,
        "mailbox erase drops bodies keeps receipts: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "mailbox erase drops bodies keeps receipts: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "mailbox erase drops bodies keeps receipts: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq WHERE state=1"),
        1,
        "mailbox erase drops bodies keeps receipts: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.store.accept(&f.authority, queued).unwrap_err(),
        MailboxError::ScopeErasing,
        "mailbox erase drops bodies keeps receipts: payload retention and erasure boundary changed"
    );
}

#[test]
fn outbox_erase_drops_bodies_keeps_status() {
    let f = Fixture::new();
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: outbox_erase_drops_bodies_keeps_status: publish succeeds");
    f.store
        .erase_scope(&f.sender, Duration::from_secs(5), &|| Ok(()))
        .expect("test: outbox_erase_drops_bodies_keeps_status: erase_scope succeeds");
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_outbox WHERE state=3 AND body_bytes IS NULL AND json_extract(envelope_meta,'$.to_pin_token') IS NULL AND json_extract(envelope_meta,'$.body_mime') IS NULL"),1, "outbox erase drops bodies keeps status: payload retention and erasure boundary changed");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_parents"),
        1,
        "outbox erase drops bodies keeps status: payload retention and erasure boundary changed"
    );
    let replay = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: outbox_erase_drops_bodies_keeps_status: publish succeeds");
    assert_eq!(
        replay.state,
        crate::messaging::mailbox::MailboxSendState::Failed,
        "outbox erase drops bodies keeps status: payload retention and erasure boundary changed"
    );
}

#[test]
fn held_delivery_across_erase_recreate_does_not_resurrect_body() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    let (fenced_tx, fenced_rx) = std::sync::mpsc::sync_channel(1);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let store = f.store.clone();
    let scope = f.recipient.clone();
    let erase = std::thread::spawn(move || {
        let result = store.erase_scope(&scope, Duration::from_secs(5), &|| {
            fenced_tx
                .send(())
                .expect("test: held_delivery_across_erase_recreate_does_not_resurrect_body: send succeeds");
            Ok(())
        });
        done_tx
            .send(result)
            .expect("test: held_delivery_across_erase_recreate_does_not_resurrect_body: send succeeds");
    });
    fenced_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("test: held_delivery_across_erase_recreate_does_not_resurrect_body: recv_timeout succeeds");
    assert_eq!(
        f.store.adopt(&delivery).unwrap_err(),
        MailboxError::ScopeErasing,
        "held delivery across erase recreate does not resurrect body: payload retention and erasure boundary changed"
    );
    assert!(
        done_rx.try_recv().is_err(),
        "erase cannot finish while queue/IPC owns the payload"
    );
    drop(delivery);
    assert_eq!(
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("test: held_delivery_across_erase_recreate_does_not_resurrect_body: recv_timeout succeeds")
            .expect("test: held_delivery_across_erase_recreate_does_not_resurrect_body: recv_timeout returns a value")
            .remaining,
        0,
        "held delivery across erase recreate does not resurrect body: payload retention and erasure boundary changed"
    );
    erase
        .join()
        .expect("test: held_delivery_across_erase_recreate_does_not_resurrect_body: join succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "held delivery across erase recreate does not resurrect body: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        0,
        "held delivery across erase recreate does not resurrect body: payload retention and erasure boundary changed"
    );
}

fn new_target_generation(f: &Fixture) -> MailboxScope {
    let writer = f.memory.pool.write();
    writer
        .execute(
            "UPDATE agent_incarnations SET state='retired' WHERE workspace_id=?1 AND owner_id=?2 AND agent_key='m'",
            params![W, O],
        )
        .expect("test: new_target_generation: fixture setup succeeds");
    writer
        .execute(
            "INSERT INTO agent_incarnations VALUES (?1,?2,'m',?3,'active',?4)",
            params![W, O, Uuid::from_u128(2).to_string(), "1".repeat(64)],
        )
        .expect("test: new_target_generation: execute succeeds");
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
        .expect("test: new_target_generation: execute succeeds");
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
        .expect("test: recipient_rebind_with_unconsumed_old_scope_retires_stream: recipient_pin succeeds");
    assert_eq!(
        f.store.accept(&f.authority, second.clone()).unwrap_err(),
        MailboxError::SeqStreamRetired,
        "recipient rebind with unconsumed old scope retires stream: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "recipient rebind with unconsumed old scope retires stream: recipient authority or scope isolation changed"
    );
    f.store
        .set_execution(&new_scope, &f.target, Some("new-run".into()))
        .expect("test: recipient_rebind_with_unconsumed_old_scope_retires_stream: set_execution succeeds");
    second.stream_id = Some(Uuid::from_u128(STREAM + 1));
    second.seq = 1;
    f.store
        .accept(&f.authority, second)
        .expect("test: recipient_rebind_with_unconsumed_old_scope_retires_stream: accept succeeds");
    let delivered = f
        .store
        .deliver(&new_scope, &f.target)
        .expect("test: recipient_rebind_with_unconsumed_old_scope_retires_stream: deliver succeeds")
        .expect("test: recipient_rebind_with_unconsumed_old_scope_retires_stream: deliver returns a value");
    assert_eq!(
        delivered.message().seq,
        1,
        "recipient rebind with unconsumed old scope retires stream: recipient authority or scope isolation changed"
    );
    f.store
        .adopt(&delivered)
        .expect("test: recipient_rebind_with_unconsumed_old_scope_retires_stream: adopt succeeds");
    drop(delivered);
    f.store
        .erase_scope(&f.recipient, Duration::from_secs(5), &|| Ok(()))
        .expect("test: recipient_rebind_with_unconsumed_old_scope_retires_stream: erase_scope succeeds");
    assert_eq!(f.scalar(&format!("SELECT count(*) FROM prx_mailbox_seq s JOIN prx_mailbox_receipts r ON r.stream_id=s.stream_id WHERE s.state=0 AND r.state<>2 AND r.ma_scope_id='{}'",f.recipient.scope_id)),0, "recipient rebind with unconsumed old scope retires stream: recipient authority or scope isolation changed");
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
        .expect("test: recipient_erase_retires_stream_before_recreated_scope_accept: execute succeeds");
    f.store
        .erase_scope(&f.recipient, Duration::from_secs(5), &|| Ok(()))
        .expect("test: recipient_erase_retires_stream_before_recreated_scope_accept: erase_scope succeeds");
    fourth.to_pin = f
        .store
        .recipient_pin(&f.authority, &new_scope, "canonical-target")
        .expect("test: recipient_erase_retires_stream_before_recreated_scope_accept: recipient_pin succeeds");
    assert_eq!(
        f.store.accept(&f.authority, fourth.clone()).unwrap_err(),
        MailboxError::SeqStreamRetired,
        "recipient erase retires stream before recreated scope accept: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "recipient erase retires stream before recreated scope accept: payload retention and erasure boundary changed"
    );
    fourth.stream_id = Some(Uuid::from_u128(STREAM + 1));
    fourth.seq = 1;
    f.store
        .accept(&f.authority, fourth)
        .expect("test: recipient_erase_retires_stream_before_recreated_scope_accept: accept succeeds");
    f.store
        .set_execution(&new_scope, &f.target, Some("new-run".into()))
        .expect("test: recipient_erase_retires_stream_before_recreated_scope_accept: set_execution succeeds");
    assert_eq!(
        f.store
            .deliver(&new_scope, &f.target)
            .expect("test: recipient_erase_retires_stream_before_recreated_scope_accept: deliver succeeds")
            .expect("test: recipient_erase_retires_stream_before_recreated_scope_accept: deliver returns a value")
            .message()
            .seq,
        1,
        "recipient erase retires stream before recreated scope accept: payload retention and erasure boundary changed"
    );
}

#[test]
fn mailbox_mime_and_payload_have_separate_byte_limits() {
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
            MailboxError::InvalidMime,
            "mailbox mime and payload share byte limits: expected messaging contract changed"
        );
    }
    let mut too_large = f.message(1, 1);
    too_large.body.bytes = vec![0; crate::messaging::MAX_BODY_BYTES + 1];
    assert_eq!(
        f.store.accept(&f.authority, too_large).unwrap_err(),
        MailboxError::BodyTooLarge,
        "mailbox mime and payload share byte limits: expected messaging contract changed"
    );
    let mut fits = f.message(1, 1);
    fits.body.mime = "x".repeat(255);
    fits.body.bytes = vec![0; crate::messaging::MAX_BODY_BYTES];
    f.store
        .accept(&f.authority, fits)
        .expect("test: mailbox_mime_and_payload_have_separate_byte_limits: accept succeeds");
    assert_eq!(
        f.scalar("SELECT length(body_bytes)+length(CAST(body_mime AS BLOB)) FROM prx_mailbox_messages"),
        262399,
        "mailbox mime and payload share byte limits: expected messaging contract changed"
    );
}

#[tokio::test]
async fn child_session_reconstructs_mailbox_cursor() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.target.clone(), None))
        .await
        .expect("test: child_session_reconstructs_mailbox_cursor: insert succeeds");
    let empty = registry
        .reconstruct(&f.target)
        .await
        .expect("test: child_session_reconstructs_mailbox_cursor: reconstruct succeeds");
    let scope = empty
        .mailbox_scope()
        .expect("test: child_session_reconstructs_mailbox_cursor: mailbox_scope succeeds")
        .clone();
    drop(empty);
    let canonical = ChildSessionRegistry::canonical_session_key(&f.target).expect(
        "test: child_session_reconstructs_mailbox_cursor: ChildSessionRegistry::canonical_session_key succeeds",
    );
    let mut message = f.message(1, 1);
    message.to_pin = f
        .store
        .recipient_pin(&f.authority, &scope, &canonical)
        .expect("test: child_session_reconstructs_mailbox_cursor: recipient_pin succeeds");
    f.store
        .set_execution(&scope, &f.target, Some("child-run".into()))
        .expect("test: child_session_reconstructs_mailbox_cursor: set_execution succeeds");
    f.store
        .accept(&f.authority, message)
        .expect("test: child_session_reconstructs_mailbox_cursor: accept succeeds");
    let delivery = f
        .store
        .deliver(&scope, &f.target)
        .expect("test: child_session_reconstructs_mailbox_cursor: deliver succeeds")
        .expect("test: child_session_reconstructs_mailbox_cursor: deliver returns a value");
    f.store
        .adopt(&delivery)
        .expect("test: child_session_reconstructs_mailbox_cursor: adopt succeeds");
    drop(delivery);
    let restored = registry
        .reconstruct(&f.target)
        .await
        .expect("test: child_session_reconstructs_mailbox_cursor: reconstruct succeeds");
    assert_eq!(
        restored.history().len(),
        1,
        "child session reconstructs mailbox cursor: atomic adoption or history projection changed"
    );
    assert_eq!(
        restored
            .history()
            .first()
            .expect("test: child_session_reconstructs_mailbox_cursor: first succeeds")
            .content,
        "[message from source/main]\nrequest-1",
        "child session reconstructs mailbox cursor: atomic adoption or history projection changed"
    );
    registry
        .insert(&restored)
        .await
        .expect("test: child_session_reconstructs_mailbox_cursor: insert succeeds");
    drop(restored);
    let again = registry
        .reconstruct(&f.target)
        .await
        .expect("test: child_session_reconstructs_mailbox_cursor: reconstruct succeeds");
    assert_eq!(
        again.history().len(),
        1,
        "snapshot projection must merge by event identity exactly once"
    );
    drop(again);
    f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
    f.store
        .gc_scope(&scope)
        .expect("test: child_session_reconstructs_mailbox_cursor: gc_scope succeeds");
    let after_gc = registry
        .reconstruct(&f.target)
        .await
        .expect("test: child_session_reconstructs_mailbox_cursor: reconstruct succeeds");
    assert_eq!(after_gc.history().len(), 1, "source_ref and seq survive receipt GC");
}

#[tokio::test]
async fn child_sessions_owner_and_agent_erase_remove_records() {
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
                .expect("test: child_sessions_owner_and_agent_erase_remove_records: execute succeeds");
        }
    }
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    let a = SessionAddr::new("prx-node", O, "child-a", Some("prx_agent_a"));
    let b = SessionAddr::new("prx-node", O, "child-b", Some("prx_agent_b"));
    registry
        .insert(&ChildSession::new(a.clone(), None))
        .await
        .expect("test: child_sessions_owner_and_agent_erase_remove_records: insert succeeds");
    registry
        .insert(&ChildSession::new(b.clone(), None))
        .await
        .expect("test: child_sessions_owner_and_agent_erase_remove_records: insert succeeds");
    let active = registry
        .reconstruct(&a)
        .await
        .expect("test: child_sessions_owner_and_agent_erase_remove_records: reconstruct succeeds");
    let scope = active
        .mailbox_scope()
        .expect("test: child_sessions_owner_and_agent_erase_remove_records: mailbox_scope succeeds")
        .clone();
    drop(active);
    f.store
        .erase_scope(&scope, Duration::from_secs(5), &|| Ok(()))
        .expect("test: child_sessions_owner_and_agent_erase_remove_records: erase_scope succeeds");
    assert!(
        registry.reconstruct(&a).await.is_err(),
        "child sessions owner and agent erase remove records: payload retention and erasure boundary changed"
    );
    let other = registry
        .reconstruct(&b)
        .await
        .expect("test: child_sessions_owner_and_agent_erase_remove_records: reconstruct succeeds");
    assert_eq!(
        other.addr(),
        &b,
        "child sessions owner and agent erase remove records: payload retention and erasure boundary changed"
    );
    drop(other);
    // The global MA owner eraser enumerates all owner snapshots. Exercise its
    // mailbox/child adapter for that complete historical set.
    let scopes = {
        let writer = f.memory.pool.write();
        let mut query = writer
            .prepare("SELECT scope_id,incarnation,generation FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2")
            .expect("test: child_sessions_owner_and_agent_erase_remove_records: prepare succeeds");
        query
            .query_map(params![W, O], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .expect("test: child_sessions_owner_and_agent_erase_remove_records: query_map succeeds")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("test: child_sessions_owner_and_agent_erase_remove_records: collect succeeds")
    };
    for (id, incarnation, generation) in scopes {
        f.store
            .erase_scope(
                &MailboxScope {
                    workspace_id: W.into(),
                    owner_id: O.into(),
                    scope_id: Uuid::parse_str(&id)
                        .expect("test: child_sessions_owner_and_agent_erase_remove_records: Uuid::parse_str succeeds"),
                    incarnation: Uuid::parse_str(&incarnation)
                        .expect("test: child_sessions_owner_and_agent_erase_remove_records: Uuid::parse_str succeeds"),
                    generation,
                },
                Duration::from_secs(5),
                &|| Ok(()),
            )
            .expect("test: child_sessions_owner_and_agent_erase_remove_records: erase_scope succeeds");
    }
    assert_eq!(
        f.scalar("SELECT count(*) FROM child_sessions"),
        0,
        "child sessions owner and agent erase remove records: payload retention and erasure boundary changed"
    );
    assert!(
        registry.reconstruct(&b).await.is_err(),
        "child sessions owner and agent erase remove records: payload retention and erasure boundary changed"
    );
    // A quarantined legacy/orphan cannot be silently omitted by scope SQL.
    f.memory
        .pool
        .write()
        .execute("INSERT INTO child_sessions(key,record) VALUES ('legacy','{}')", [])
        .expect("test: child_sessions_owner_and_agent_erase_remove_records: execute succeeds");
    assert_eq!(
        f.store
            .erase_scope(&f.sender, Duration::from_secs(5), &|| Ok(()))
            .unwrap_err(),
        MailboxError::Fenced,
        "child sessions owner and agent erase remove records: payload retention and erasure boundary changed"
    );
}

#[test]
fn mailbox_scope_and_address_validation_is_transactional() {
    let f = Fixture::new();
    let mut forged = f.message(1, 1);
    forged.to.owner = "prx_other_owner".into();
    assert_eq!(
        f.store.accept(&f.authority, forged).unwrap_err(),
        MailboxError::Unauthorized,
        "mailbox scope and address validation is transactional: recipient authority or scope isolation changed"
    );
    let wrong = MailboxScope {
        generation: 2,
        ..f.recipient.clone()
    };
    assert_eq!(
        f.store
            .recipient_pin(&f.authority, &wrong, "canonical-target")
            .unwrap_err(),
        MailboxError::StaleIncarnation,
        "mailbox scope and address validation is transactional: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.store
            .recipient_pin(&f.authority, &f.recipient, "wrong-canonical")
            .unwrap_err(),
        MailboxError::StaleIncarnation,
        "mailbox scope and address validation is transactional: recipient authority or scope isolation changed"
    );
    assert!(
        MailboxAuthority::local(
            W.into(),
            f.source.clone(),
            SessionAddr::new("prx-node", O, "x".repeat(513), None)
        )
        .is_err(),
        "mailbox scope and address validation is transactional: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        0,
        "mailbox scope and address validation is transactional: recipient authority or scope isolation changed"
    );
}

#[test]
fn queued_outbox_first_post_after_recipient_recreate_is_stale_incarnation() {
    let f = Fixture::new();
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: queued_outbox_first_post_after_recipient_recreate_is_stale_incarnation: publish succeeds");
    let pending = f.store.pending_outbox(&f.sender, &f.source).expect(
        "test: queued_outbox_first_post_after_recipient_recreate_is_stale_incarnation: pending_outbox succeeds",
    );
    let held = pending
        .first()
        .expect("test: queued_outbox_first_post_after_recipient_recreate_is_stale_incarnation: first succeeds")
        .message()
        .clone();
    f.store
        .erase_scope(&f.recipient, Duration::from_secs(5), &|| Ok(()))
        .expect("test: queued_outbox_first_post_after_recipient_recreate_is_stale_incarnation: erase_scope succeeds");
    let _new = new_target_generation(&f);
    assert_eq!(
        f.store.accept(&f.authority, held).unwrap_err(),
        MailboxError::StaleIncarnation,
        "queued outbox first post after recipient recreate is stale incarnation: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "queued outbox first post after recipient recreate is stale incarnation: capacity enforcement or bounded delivery changed"
    );
}

#[test]
fn stale_incarnation_delayed_send_is_rejected() {
    let f = Fixture::new();
    let held = f.message(1, 1);
    let _new = new_target_generation(&f);
    assert_eq!(
        f.store.accept(&f.authority, held).unwrap_err(),
        MailboxError::StaleIncarnation,
        "stale incarnation delayed send is rejected: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "stale incarnation delayed send is rejected: recipient authority or scope isolation changed"
    );
}

#[test]
fn mailbox_row_requires_scope() {
    let f = Fixture::new();
    let mut message = f.message(1, 1);
    message.to_pin.generation = 99;
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::StaleIncarnation,
        "mailbox row requires scope: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "mailbox row requires scope: recipient authority or scope isolation changed"
    );
    let error = f.memory.pool.write().execute(
        "INSERT INTO prx_mailbox_messages(id,stream_id,seq,workspace_id,owner_id,incarnation,generation,state,body_mime,body_bytes,delivery_meta) VALUES (1,1,x'0000000000000001',?1,?2,?3,1,0,'text/plain',x'00','{}')",
        params![W,O,Uuid::from_u128(INC).to_string()],
    ).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("NOT NULL constraint failed: prx_mailbox_messages.ma_scope_id"),
        "mailbox row requires scope: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "mailbox row requires scope: recipient authority or scope isolation changed"
    );
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
            method: "mtls".into(),
        },
    )
    .expect("test: mailbox_pk_includes_from_instance: MailboxAuthority::verified_peer succeeds");
    let mut message = f.message(1, 1);
    message.to_pin = f
        .store
        .recipient_pin(&authority, &f.recipient, "canonical-target")
        .expect("test: mailbox_pk_includes_from_instance: recipient_pin succeeds");
    message.id = Uuid::from_u128(9001);
    f.store
        .accept(&authority, message)
        .expect("test: mailbox_pk_includes_from_instance: accept succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        2,
        "mailbox pk includes from instance: expected messaging contract changed"
    );
}

#[test]
fn mailbox_pk_omits_run() {
    let f = Fixture::new();
    let message = f.message(1, 1);
    let saved = f
        .store
        .accept(&f.authority, message.clone())
        .expect("test: mailbox_pk_omits_run: accept succeeds");
    f.store
        .set_execution(&f.recipient, &f.target, Some("replacement-run".into()))
        .expect("test: mailbox_pk_omits_run: set_execution succeeds");
    assert_eq!(
        f.store
            .accept(&f.authority, message)
            .expect("test: mailbox_pk_omits_run: accept succeeds"),
        saved,
        "mailbox pk omits run: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "mailbox pk omits run: expected messaging contract changed"
    );
}

#[test]
fn mailbox_seq_pk_includes_stream_id() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut other = f.message(2, 1);
    other.stream_id = Some(Uuid::from_u128(501));
    f.store
        .accept(&f.authority, other)
        .expect("test: mailbox_seq_pk_includes_stream_id: accept succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        2,
        "mailbox seq pk includes stream id: expected messaging contract changed"
    );
}

#[test]
fn idempotency_does_not_collide_across_senders() {
    mailbox_pk_includes_from_instance();
}

#[test]
fn seq_is_allocated_in_outbox_transaction() {
    let f = Fixture::new();
    f.memory.pool.write().execute_batch("CREATE TEMP TRIGGER reject_publish BEFORE INSERT ON prx_mailbox_outbox BEGIN SELECT RAISE(ABORT,'injected write failure'); END;").expect("test: seq_is_allocated_in_outbox_transaction: execute_batch succeeds");
    assert_eq!(
        f.store
            .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
            .unwrap_err(),
        MailboxError::Storage,
        "seq is allocated in outbox transaction: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        0,
        "seq is allocated in outbox transaction: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_parents"),
        0,
        "seq is allocated in outbox transaction: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute_batch("DROP TRIGGER reject_publish")
        .expect("test: seq_is_allocated_in_outbox_transaction: execute_batch succeeds");
    let first = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: seq_is_allocated_in_outbox_transaction: publish succeeds");
    assert_eq!(
        first.seq, 1,
        "seq is allocated in outbox transaction: expected messaging contract changed"
    );
    let again = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: seq_is_allocated_in_outbox_transaction: publish succeeds");
    assert_eq!(
        again, first,
        "seq is allocated in outbox transaction: expected messaging contract changed"
    );
    assert_eq!(
        f.store
            .publish(&f.authority, f.message(2, 0), &f.sender, f.clock.now() + 300, None)
            .expect("test: seq_is_allocated_in_outbox_transaction: publish succeeds")
            .seq,
        2,
        "seq is allocated in outbox transaction: expected messaging contract changed"
    );
}

#[test]
fn seq2_before_seq1_on_never_seen_stream_is_held_fifo() {
    let f = Fixture::new();
    f.receive(2, 2);
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: seq2_before_seq1_on_never_seen_stream_is_held_fifo: deliver succeeds")
            .is_none(),
        "seq2 before seq1 on never seen stream is held fifo: expected messaging contract changed"
    );
    f.receive(1, 1);
    let first = f.deliver();
    assert_eq!(
        first.message().seq,
        1,
        "seq2 before seq1 on never seen stream is held fifo: expected messaging contract changed"
    );
    f.store
        .adopt(&first)
        .expect("test: seq2_before_seq1_on_never_seen_stream_is_held_fifo: adopt succeeds");
    drop(first);
    assert_eq!(
        f.deliver().message().seq,
        2,
        "seq2 before seq1 on never seen stream is held fifo: expected messaging contract changed"
    );
}

#[test]
fn late_seq_after_stream_retirement_is_seq_stream_retired() {
    let f = Fixture::new();
    let pending = f.message(2, 2);
    f.store
        .accept(&f.authority, pending)
        .expect("test: late_seq_after_stream_retirement_is_seq_stream_retired: accept succeeds");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET state=1,progress=json_set(progress,'$.retired_at',1000)",
            [],
        )
        .expect("test: late_seq_after_stream_retirement_is_seq_stream_retired: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, f.message(1, 1)).unwrap_err(),
        MailboxError::SeqStreamRetired,
        "late seq after stream retirement is seq stream retired: expected messaging contract changed"
    );
}

#[test]
fn delayed_old_epoch_message_is_rejected_after_stream_reopen() {
    let f = Fixture::new();
    f.receive(1, 1);
    let first = f.deliver();
    f.store
        .adopt(&first)
        .expect("test: delayed_old_epoch_message_is_rejected_after_stream_reopen: adopt succeeds");
    drop(first);
    f.clock.advance(604801);
    f.store
        .gc_scope(&f.recipient)
        .expect("test: delayed_old_epoch_message_is_rejected_after_stream_reopen: gc_scope succeeds");
    let mut reopened = f.message(3, 1);
    reopened.stream_id = Some(Uuid::from_u128(501));
    f.store
        .accept(&f.authority, reopened)
        .expect("test: delayed_old_epoch_message_is_rejected_after_stream_reopen: accept succeeds");
    assert_eq!(
        f.store.accept(&f.authority, f.message(2, 2)).unwrap_err(),
        MailboxError::SeqStreamRetired,
        "delayed old epoch message is rejected after stream reopen: expected messaging contract changed"
    );
}

#[test]
fn mailbox_accept_nacks_while_fenced() {
    let f = Fixture::new();
    let queued = f.message(1, 1);
    f.store
        .erase_scope(&f.recipient, Duration::from_secs(5), &|| Ok(()))
        .expect("test: mailbox_accept_nacks_while_fenced: erase_scope succeeds");
    assert_eq!(
        f.store.accept(&f.authority, queued).unwrap_err(),
        MailboxError::ScopeErasing,
        "mailbox accept nacks while fenced: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "mailbox accept nacks while fenced: expected messaging contract changed"
    );
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
        MailboxError::QueueFull,
        "full queue nacks without drop: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        64,
        "full queue nacks without drop: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        f.deliver().message().seq,
        1,
        "full queue nacks without drop: capacity enforcement or bounded delivery changed"
    );
}

#[test]
fn fast_consumer_hits_retained_body_quota() {
    let f = Fixture::new();
    // 257 near-limit bodies exceed 64 MiB even though every row is consumed.
    for n in 1..=256 {
        f.clock.advance(1);
        let mut message = f.message(n, n);
        message.body.bytes = vec![b'x'; crate::messaging::MAX_BODY_BYTES - 10];
        f.store
            .accept(&f.authority, message)
            .expect("test: fast_consumer_hits_retained_body_quota: accept succeeds");
        let selected = f.deliver();
        f.store
            .adopt(&selected)
            .expect("test: fast_consumer_hits_retained_body_quota: adopt succeeds");
    }
    f.clock.advance(1);
    let mut over = f.message(257, 257);
    over.body.bytes = vec![b'x'; crate::messaging::MAX_BODY_BYTES - 10];
    assert_eq!(
        f.store.accept(&f.authority, over).unwrap_err(),
        MailboxError::Quota("bodies"),
        "fast consumer hits retained body quota: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        256,
        "fast consumer hits retained body quota: payload retention and erasure boundary changed"
    );
    f.clock.advance(604801);
    let mut fresh = f.message(258, 1);
    fresh.stream_id = Some(Uuid::from_u128(501));
    f.store
        .accept(&f.authority, fresh)
        .expect("test: fast_consumer_hits_retained_body_quota: accept succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        1,
        "expired consumed bodies must be GCed before quota refusal"
    );
}

#[test]
fn cross_instance_chain_depth_is_receiver_owned() {
    let f = Fixture::new();
    let mut parent = None;
    for n in 1..=9 {
        f.clock.advance(1);
        let mut message = f.message(n, n);
        message.in_reply_to = parent;
        // Wire depth is maliciously reset; receiver must derive it.
        message.hop_count = 0;
        message.ancestry.clear();
        let accepted = f
            .store
            .accept(&f.authority, message.clone())
            .expect("test: cross_instance_chain_depth_is_receiver_owned: accept succeeds");
        assert_eq!(
            accepted.chain.depth,
            n as u8 - 1,
            "cross instance chain depth is capped: capacity enforcement or bounded delivery changed"
        );
        parent = Some(message.id);
    }

}

#[test]
fn send_reply_resolves_parent_from_outbox_without_transcript() {
    let f = Fixture::new();
    let sent = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: send_reply_resolves_parent_from_outbox_without_transcript: publish succeeds");
    let mut reply = f.message(2, 1);
    reply.in_reply_to = Some(sent.message_id);
    reply.stream_id = Some(Uuid::from_u128(502));
    let received = f
        .store
        .accept(&f.authority, reply)
        .expect("test: send_reply_resolves_parent_from_outbox_without_transcript: accept succeeds");
    assert_eq!(
        received.chain.depth, 1,
        "send reply resolves parent from outbox without transcript: atomic adoption or history projection changed"
    );
    assert_eq!(
        received.chain.ancestry,
        vec![sent.message_id],
        "send reply resolves parent from outbox without transcript: atomic adoption or history projection changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        0,
        "send reply resolves parent from outbox without transcript: atomic adoption or history projection changed"
    );
}

#[test]
fn idempotent_retry_after_parent_erase_reports_scope_erasing() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut reply = f.message(2, 2);
    reply.in_reply_to = Some(Uuid::from_u128(1001));
    f.store
        .accept(&f.authority, reply.clone())
        .expect("test: idempotent_retry_after_parent_erase_reports_scope_erasing: accept succeeds");
    f.store
        .erase_scope(&f.recipient, Duration::from_secs(5), &|| Ok(()))
        .expect("test: idempotent_retry_after_parent_erase_reports_scope_erasing: erase_scope succeeds");
    assert_eq!(
        f.store
            .accept(&f.authority, reply)
            .expect_err("test: erased nonconsumed replay is terminal"),
        MailboxError::ScopeErasing,
        "erased nonconsumed replay must report scope erasure"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        0,
        "erased nonconsumed replay must report scope erasure"
    );
}

#[test]
fn altered_parent_retry_after_erase_is_idempotency_conflict() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut reply = f.message(2, 2);
    reply.in_reply_to = Some(Uuid::from_u128(1001));
    f.store
        .accept(&f.authority, reply.clone())
        .expect("test: altered_parent_retry_after_erase_is_idempotency_conflict: accept succeeds");
    f.store
        .erase_scope(&f.recipient, Duration::from_secs(5), &|| Ok(()))
        .expect("test: altered_parent_retry_after_erase_is_idempotency_conflict: erase_scope succeeds");
    reply.in_reply_to = None;
    assert_eq!(
        f.store.accept(&f.authority, reply).unwrap_err(),
        MailboxError::IdempotencyConflict,
        "altered parent retry after erase is idempotency conflict: payload retention and erasure boundary changed"
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
