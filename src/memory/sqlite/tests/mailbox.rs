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
            crate::config::schema::RemoteTreeLimits::default(),
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
            MailboxError::InvalidMime,
            "mailbox mime and payload share byte limits: expected messaging contract changed"
        );
    }
    let mut too_large = f.message(1, 1);
    too_large.body.bytes = vec![0; crate::messaging::MAX_BODY_BYTES];
    assert_eq!(
        f.store.accept(&f.authority, too_large).unwrap_err(),
        MailboxError::BodyTooLarge,
        "mailbox mime and payload share byte limits: expected messaging contract changed"
    );
    let mut fits = f.message(1, 1);
    fits.body.mime = "x".repeat(255);
    fits.body.bytes = vec![0; crate::messaging::MAX_BODY_BYTES - 255];
    f.store
        .accept(&f.authority, fits)
        .expect("test: mailbox_mime_and_payload_share_byte_limits: accept succeeds");
    assert_eq!(
        f.scalar("SELECT length(body_bytes)+length(CAST(body_mime AS BLOB)) FROM prx_mailbox_messages"),
        262144,
        "mailbox mime and payload share byte limits: expected messaging contract changed"
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
        restored.mailbox_cursor().streams.get(&1),
        Some(&1),
        "child session reconstructs mailbox cursor: atomic adoption or history projection changed"
    );
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
        "request-1",
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
    assert_eq!(
        after_gc.mailbox_cursor().streams.get(&1),
        Some(&1),
        "child session reconstructs mailbox cursor: atomic adoption or history projection changed"
    );
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
                .expect(
                    "test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: execute succeeds",
                );
        }
    }
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    let a = SessionAddr::new("prx-node", O, "child-a", Some("prx_agent_a"));
    let b = SessionAddr::new("prx-node", O, "child-b", Some("prx_agent_b"));
    registry
        .insert(&ChildSession::new(a.clone(), None, None))
        .await
        .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: insert succeeds");
    registry
        .insert(&ChildSession::new(b.clone(), None, None))
        .await
        .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: insert succeeds");
    let active = registry
        .reconstruct(&a)
        .await
        .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: reconstruct succeeds");
    let scope = active
        .mailbox_scope()
        .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: mailbox_scope succeeds")
        .clone();
    drop(active);
    f.store
        .erase_scope(&scope, Duration::from_secs(5), &|| Ok(()))
        .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: erase_scope succeeds");
    assert!(
        registry.reconstruct(&a).await.is_err(),
        "child sessions owner and agent erase remove records and block relaunch: payload retention and erasure boundary changed"
    );
    assert!(
        registry.relaunch(&a).await.is_err(),
        "child sessions owner and agent erase remove records and block relaunch: payload retention and erasure boundary changed"
    );
    let other = registry
        .reconstruct(&b)
        .await
        .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: reconstruct succeeds");
    assert_eq!(
        other.addr(),
        &b,
        "child sessions owner and agent erase remove records and block relaunch: payload retention and erasure boundary changed"
    );
    drop(other);
    // The global MA owner eraser enumerates all owner snapshots. Exercise its
    // mailbox/child adapter for that complete historical set.
    let scopes = {
        let writer = f.memory.pool.write();
        let mut query = writer
            .prepare("SELECT scope_id,incarnation,generation FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2")
            .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: prepare succeeds");
        query
            .query_map(params![W, O], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: query_map succeeds")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: collect succeeds")
    };
    for (id, incarnation, generation) in scopes {
        f.store
            .erase_scope(
                &MailboxScope {
                    workspace_id: W.into(),
                    owner_id: O.into(),
                    scope_id: Uuid::parse_str(&id).expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: Uuid::parse_str succeeds"),
                    incarnation: Uuid::parse_str(&incarnation).expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: Uuid::parse_str succeeds"),
                    generation,
                },
                Duration::from_secs(5),
                &|| Ok(()),
            )
            .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: erase_scope succeeds");
    }
    assert_eq!(
        f.scalar("SELECT count(*) FROM child_sessions"),
        0,
        "child sessions owner and agent erase remove records and block relaunch: payload retention and erasure boundary changed"
    );
    assert!(
        registry.reconstruct(&b).await.is_err(),
        "child sessions owner and agent erase remove records and block relaunch: payload retention and erasure boundary changed"
    );
    // A quarantined legacy/orphan cannot be silently omitted by scope SQL.
    f.memory
        .pool
        .write()
        .execute("INSERT INTO child_sessions(key,record) VALUES ('legacy','{}')", [])
        .expect("test: child_sessions_owner_and_agent_erase_remove_records_and_block_relaunch: execute succeeds");
    assert_eq!(
        f.store
            .erase_scope(&f.sender, Duration::from_secs(5), &|| Ok(()))
            .unwrap_err(),
        MailboxError::Fenced,
        "child sessions owner and agent erase remove records and block relaunch: payload retention and erasure boundary changed"
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
            method: "jwt".into(),
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
        let accepted = f
            .store
            .accept(&f.authority, message.clone())
            .expect("test: cross_instance_chain_depth_is_capped: accept succeeds");
        assert_eq!(
            accepted.chain.depth,
            n as u8 - 1,
            "cross instance chain depth is capped: capacity enforcement or bounded delivery changed"
        );
        parent = Some(message.id);
    }
    let mut too_deep = f.message(10, 10);
    too_deep.in_reply_to = parent;
    assert_eq!(
        f.store.accept(&f.authority, too_deep).unwrap_err(),
        MailboxError::ChainDepth,
        "cross instance chain depth is capped: capacity enforcement or bounded delivery changed"
    );
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
fn idempotent_retry_after_parent_erase_returns_same_ack() {
    let f = Fixture::new();
    f.receive(1, 1);
    let mut reply = f.message(2, 2);
    reply.in_reply_to = Some(Uuid::from_u128(1001));
    let accepted = f
        .store
        .accept(&f.authority, reply.clone())
        .expect("test: idempotent_retry_after_parent_erase_returns_same_ack: accept succeeds");
    f.store
        .erase_scope(&f.recipient, Duration::from_secs(5), &|| Ok(()))
        .expect("test: idempotent_retry_after_parent_erase_returns_same_ack: erase_scope succeeds");
    assert_eq!(
        f.store
            .accept(&f.authority, reply)
            .expect("test: idempotent_retry_after_parent_erase_returns_same_ack: accept succeeds"),
        accepted,
        "idempotent retry after parent erase returns same ack: payload retention and erasure boundary changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        0,
        "idempotent retry after parent erase returns same ack: payload retention and erasure boundary changed"
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
            .expect("test: sender_port: SqliteMailboxSender::new succeeds"),
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
    .expect("test: prx_message_tool_emits_one_external_send_envelope: fixture setup succeeds");
    assert_eq!(
        catalog
            .entry_for_public_name("prx_message")
            .expect("test: prx_message_tool_emits_one_external_send_envelope: entry_for_public_name succeeds")
            .effect,
        crate::causal_tree::branch::SideEffectClass::ExternalSend,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    let result = tool
        .execute(serde_json::json!({"to":{"session":"target"},"text":"ordinary Data"}))
        .await
        .expect("test: prx_message_tool_emits_one_external_send_envelope: execute succeeds");
    assert!(result.success, "{}", result.output);
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    let out = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: prx_message_tool_emits_one_external_send_envelope: pending_outbox succeeds");
    assert_eq!(
        out[0].message().kind,
        MessageKind::Data,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    assert_eq!(
        out[0].message().body.bytes,
        b"ordinary Data",
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    assert_eq!(
        out[0].message().from,
        f.source,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "sender tool does not execute the recipient"
    );
    for invalid in [
        serde_json::json!({"to":{"session":"target","owner":null},"text":"x"}),
        serde_json::json!({"to":{"session":"target"},"text":"x","in_reply_to":null}),
    ] {
        assert!(
            tool.execute(invalid).await.is_err(),
            "prx message tool emits one external send envelope: expected messaging contract changed"
        );
    }
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "prx message tool emits one external send envelope: expected messaging contract changed"
    );
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
        .expect("test: prx_message_tool_send_is_stamped_with_runtime_causal_chain: execute succeeds");
    assert!(result.success, "{}", result.output);
    let out = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: prx_message_tool_send_is_stamped_with_runtime_causal_chain: pending_outbox succeeds");
    assert_eq!(
        out[0].message().in_reply_to,
        Some(inherited.chain.trigger),
        "prx message tool send is stamped with runtime causal chain: expected messaging contract changed"
    );
    assert_eq!(
        out[0].message().hop_count,
        1,
        "prx message tool send is stamped with runtime causal chain: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT sends FROM prx_remote_trees"),
        1,
        "prx message tool send is stamped with runtime causal chain: expected messaging contract changed"
    );
}

#[tokio::test]
async fn sessions_send_remote_addr_is_not_steer() {
    use crate::tools::Tool;
    let f = Fixture::new();
    let tool = crate::tools::sessions_send::SessionsSendTool::new(Arc::new(tokio::sync::RwLock::new(Vec::new())))
        .with_mailbox(f.sender_port(None));
    assert!(
        tool.parameters_schema().get("oneOf").is_some(),
        "sessions send remote addr is not steer: expected messaging contract changed"
    );
    assert!(
        tool.execute(serde_json::json!({"to":{"session":"target"},"message":"mailbox Data"}))
            .await
            .expect("test: sessions_send_remote_addr_is_not_steer: execute succeeds")
            .success,
        "sessions send remote addr is not steer: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "sessions send remote addr is not steer: expected messaging contract changed"
    );
    assert!(
        tool.execute(serde_json::json!({"run_id":"run-target","to":{"session":"target"},"message":"ambiguous"}))
            .await
            .is_err(),
        "sessions send remote addr is not steer: expected messaging contract changed"
    );
}

#[test]
fn slash_msg_builds_envelope_to_peer_session() {
    let f = Fixture::new();
    let (target, text) = crate::chat::commands::msg::parse("/msg peer-node target/prx_agent hello peer", &f.source)
        .expect("test: slash_msg_builds_envelope_to_peer_session: fixture setup succeeds");
    assert_eq!(
        target,
        SessionAddr::new("peer-node", O, "target", Some("prx_agent")),
        "slash msg builds envelope to peer session: expected messaging contract changed"
    );
    assert_eq!(
        text, "hello peer",
        "slash msg builds envelope to peer session: expected messaging contract changed"
    );
    assert!(
        crate::chat::commands::msg::parse("/msg local target/ ", &f.source).is_err(),
        "slash msg builds envelope to peer session: expected messaging contract changed"
    );
}

#[test]
fn mailbox_formats_and_natural_key_collisions_fail_closed() {
    let f = Fixture::new();
    let original = f.message(1, 1);
    f.store
        .accept(&f.authority, original.clone())
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: accept succeeds");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=json_set(receipt_meta,'$.unknown',1)",
            [],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, original.clone()).unwrap_err(),
        MailboxError::InvalidMetadata,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=json_remove(receipt_meta,'$.unknown')",
            [],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    let canonical = f
        .memory
        .pool
        .write()
        .query_row("SELECT receipt_meta FROM prx_mailbox_receipts", [], |r| {
            r.get::<_, String>(0)
        })
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: query_row succeeds");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET receipt_meta=?1",
            [format!(" {canonical}")],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, original.clone()).unwrap_err(),
        MailboxError::InvalidMetadata,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_receipts SET receipt_meta=?1", [canonical])
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET wire_stream_id=zeroblob(15)", [])
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, original.clone()).unwrap_err(),
        MailboxError::CorruptState,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET wire_stream_id=?1",
            [Uuid::from_u128(STREAM).as_bytes().as_slice()],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    let mut changed = f.target.clone();
    changed.session = "other-target".into();
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET to_addr=?1",
            [serde_json::to_string(&changed).expect(
                "test: mailbox_formats_and_natural_key_collisions_fail_closed: serde_json::to_string succeeds",
            )],
        )
        .expect("test: mailbox_formats_and_natural_key_collisions_fail_closed: execute succeeds");
    assert_eq!(
        f.store.accept(&f.authority, original).unwrap_err(),
        MailboxError::NaturalKeyCollision,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "mailbox formats and natural key collisions fail closed: expected messaging contract changed"
    );
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
        MailboxError::StaleRun,
        "mailbox scope pin role and run checks fail closed: recipient authority or scope isolation changed"
    );
    f.receive(1, 1);
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET recipient_generation=NULL", [])
        .expect("test: mailbox_scope_pin_role_and_run_checks_fail_closed: execute succeeds");
    assert!(
        matches!(
            f.store.deliver(&f.recipient, &f.target),
            Err(MailboxError::CorruptState)
        ),
        "mailbox scope pin role and run checks fail closed: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        0,
        "mailbox scope pin role and run checks fail closed: recipient authority or scope isolation changed"
    );
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
        .expect("test: mailbox_identity_and_state_transitions_are_immutable: execute succeeds");
    assert_eq!(
        f.store.adopt(&delivery).unwrap_err(),
        MailboxError::IdentityImmutable,
        "mailbox identity and state transitions are immutable: replay identity or receipt stability changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        0,
        "mailbox identity and state transitions are immutable: replay identity or receipt stability changed"
    );
    assert_eq!(
        f.scalar("SELECT state FROM prx_mailbox_receipts"),
        1,
        "mailbox identity and state transitions are immutable: replay identity or receipt stability changed"
    );
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
    let receipt = f
        .store
        .accept(&f.authority, message)
        .expect("test: mailbox_transport_and_chain_metadata_is_runtime_owned: accept succeeds");
    assert_eq!(
        receipt.chain.depth, 0,
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
    assert!(
        receipt.remote.is_none(),
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
    let delivered = f.deliver();
    assert_eq!(
        delivered.message().from,
        f.source,
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
    assert_eq!(
        delivered.message().origin,
        Some(crate::messaging::MessageOrigin::LocalModel),
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
    assert!(
        delivered.message().auth.is_none(),
        "mailbox transport and chain metadata is runtime owned: expected messaging contract changed"
    );
}

#[test]
fn mailbox_replay_slot_and_message_id_reuse_are_checked() {
    let f = Fixture::new();
    let message = f.message(1, 1);
    let first = f
        .store
        .accept(&f.authority, message.clone())
        .expect("test: mailbox_replay_slot_and_message_id_reuse_are_checked: accept succeeds");
    let mut occupied = f.message(2, 1);
    assert_eq!(
        f.store.accept(&f.authority, occupied.clone()).unwrap_err(),
        MailboxError::SeqConflict,
        "mailbox replay slot and message id reuse are checked: replay identity or receipt stability changed"
    );
    occupied.seq = 2;
    occupied.id = message.id;
    assert_eq!(
        f.store.accept(&f.authority, occupied).unwrap_err(),
        MailboxError::IdReuse,
        "mailbox replay slot and message id reuse are checked: replay identity or receipt stability changed"
    );
    assert_eq!(
        f.store
            .accept(&f.authority, message)
            .expect("test: mailbox_replay_slot_and_message_id_reuse_are_checked: accept succeeds"),
        first,
        "mailbox replay slot and message id reuse are checked: replay identity or receipt stability changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "mailbox replay slot and message id reuse are checked: replay identity or receipt stability changed"
    );
}

#[test]
fn mailbox_progress_and_gap_transitions_are_checked() {
    let f = Fixture::new();
    let message = f.message(1, 1026);
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::Quota("seq"),
        "mailbox progress and gap transitions are checked: expected messaging contract changed"
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
        MailboxError::RoleBusy,
        "mailbox progress and gap transitions are checked: expected messaging contract changed"
    );
    f.store
        .adopt(&delivery)
        .expect("test: mailbox_progress_and_gap_transitions_are_checked: adopt succeeds");
    drop(delivery);
    f.store
        .recover_delivery(&f.recipient, &f.target)
        .expect("test: mailbox_progress_and_gap_transitions_are_checked: recover_delivery succeeds");
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: mailbox_progress_and_gap_transitions_are_checked: deliver succeeds")
            .is_none(),
        "mailbox progress and gap transitions are checked: expected messaging contract changed"
    );
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_seq SET progress=json_set(progress,'$.next_seq',0)",
            [],
        )
        .expect("test: mailbox_progress_and_gap_transitions_are_checked: execute succeeds");
    assert!(
        matches!(
            f.store.deliver(&f.recipient, &f.target),
            Err(MailboxError::CorruptState)
        ),
        "mailbox progress and gap transitions are checked: expected messaging contract changed"
    );
}

#[test]
fn fresh_sender_session_labels_after_receipt_gc_hit_stream_quota() {
    let f = Fixture::new();
    f.receive(1, 1);
    let delivery = f.deliver();
    f.store
        .adopt(&delivery)
        .expect("test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: adopt succeeds");
    drop(delivery);
    f.clock.advance(604801);
    f.store
        .gc_scope(&f.recipient)
        .expect("test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: gc_scope succeeds");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        0,
        "fresh sender session labels after receipt gc hit stream quota: capacity enforcement or bounded delivery changed"
    );
    // Populate the remaining bounded tombstone inventory from the real retired
    // row; no messages/receipts remain to provide accidental quota coverage.
    f.memory.pool.write().execute_batch("WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<4095) INSERT INTO prx_mailbox_seq(workspace_id,owner_id,sender_ma_scope_id,sender_incarnation,sender_generation,recipient_ma_scope_id,recipient_incarnation,recipient_generation,from_addr,to_addr,wire_stream_id,state,progress,updated_at) SELECT workspace_id,owner_id,sender_ma_scope_id,sender_incarnation,sender_generation,recipient_ma_scope_id,recipient_incarnation,recipient_generation,json_set(from_addr,'$.session','retired-'||v),to_addr,randomblob(16),state,progress,updated_at FROM prx_mailbox_seq,n WHERE stream_id=1;").expect("test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: fixture setup succeeds");
    let mut source = f.source.clone();
    source.session = "fresh-sender-label".into();
    let authority = MailboxAuthority::local(W.into(), source, f.target.clone()).expect(
        "test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: MailboxAuthority::local succeeds",
    );
    let mut message = f.message(2, 1);
    message.stream_id = Some(Uuid::new_v4());
    message.to_pin = f
        .store
        .recipient_pin(&authority, &f.recipient, "canonical-target")
        .expect("test: fresh_sender_session_labels_after_receipt_gc_hit_stream_quota: recipient_pin succeeds");
    assert_eq!(
        f.store.accept(&authority, message).unwrap_err(),
        MailboxError::Quota("streams"),
        "fresh sender session labels after receipt gc hit stream quota: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        4096,
        "fresh sender session labels after receipt gc hit stream quota: capacity enforcement or bounded delivery changed"
    );
}

#[tokio::test]
async fn child_message_reaches_main_mailbox() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let mut f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.source.clone(), None, None))
        .await
        .expect("test: child_message_reaches_main_mailbox: insert succeeds");
    let child = registry
        .reconstruct(&f.source)
        .await
        .expect("test: child_message_reaches_main_mailbox: reconstruct succeeds");
    f.sender = child
        .mailbox_scope()
        .expect("test: child_message_reaches_main_mailbox: mailbox_scope succeeds")
        .clone();
    let provisional = f.message(1, 0);
    f.store
        .publish(&f.authority, provisional, &f.sender, f.clock.now() + 300, None)
        .expect("test: child_message_reaches_main_mailbox: publish succeeds");
    let outgoing = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: child_message_reaches_main_mailbox: pending_outbox succeeds");
    f.store
        .accept(&f.authority, outgoing[0].message().clone())
        .expect("test: child_message_reaches_main_mailbox: accept succeeds");
    let delivery = f.deliver();
    let receipt = f
        .store
        .adopt(&delivery)
        .expect("test: child_message_reaches_main_mailbox: adopt succeeds");
    assert_eq!(
        receipt.state,
        DeliveryState::Consumed,
        "child message reaches main mailbox: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        1,
        "child message reaches main mailbox: expected messaging contract changed"
    );
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
            .expect("test: sibling_message_is_owner_scoped: insert succeeds");
    }
    let target = registry
        .reconstruct(&f.target)
        .await
        .expect("test: sibling_message_is_owner_scoped: reconstruct succeeds");
    let scope = target
        .mailbox_scope()
        .expect("test: sibling_message_is_owner_scoped: mailbox_scope succeeds")
        .clone();
    let canonical = ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: sibling_message_is_owner_scoped: ChildSessionRegistry::canonical_session_key succeeds");
    let mut message = f.message(1, 1);
    message.to_pin = f
        .store
        .recipient_pin(&f.authority, &scope, &canonical)
        .expect("test: sibling_message_is_owner_scoped: recipient_pin succeeds");
    f.store
        .accept(&f.authority, message.clone())
        .expect("test: sibling_message_is_owner_scoped: accept succeeds");
    let mut foreign = f.source.clone();
    foreign.owner = "other-owner".into();
    assert!(
        matches!(
            MailboxAuthority::local(W.into(), foreign, f.target.clone()),
            Err(MailboxError::Unauthorized)
        ),
        "sibling message is owner scoped: recipient authority or scope isolation changed"
    );
    let mut foreign_target = f.target.clone();
    foreign_target.owner = "other-owner".into();
    message.to = foreign_target;
    assert_eq!(
        f.store.accept(&f.authority, message).unwrap_err(),
        MailboxError::Unauthorized,
        "sibling message is owner scoped: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "sibling message is owner scoped: recipient authority or scope isolation changed"
    );
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
        .expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: insert succeeds");
    let key = ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: ChildSessionRegistry::canonical_session_key succeeds");
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
        .expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: query_row succeeds");
    f.memory.pool.write().execute("UPDATE child_sessions SET workspace_id=NULL,owner_id=NULL,instance_id=NULL,session_id=NULL,agent_key=NULL,ma_scope_id=NULL,incarnation=NULL,generation=NULL WHERE key=?1",[&key]).expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: fixture setup succeeds");
    let refused = match ChildSessionStore::load(&f.memory, &key, &context) {
        Ok(_) => panic!("quarantined legacy row loaded"),
        Err(error) => error,
    };
    assert!(
        refused.is::<ChildSessionError>(),
        "child sessions migration 22 preserves records and quarantines unbound rows: capacity enforcement or bounded delivery changed"
    );
    let recovered =
        ChildSessionStore::update(&f.memory, &key, &context, &mut |old| Ok(old.expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: fixture setup succeeds").to_owned())).expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: ChildSessionStore::update succeeds");
    assert_eq!(
        recovered.record, original,
        "child sessions migration 22 preserves records and quarantines unbound rows: capacity enforcement or bounded delivery changed"
    );
    drop(recovered);
    // A partially populated row is never a backfill candidate.
    f.memory
        .pool
        .write()
        .execute("UPDATE child_sessions SET generation=NULL WHERE key=?1", [&key])
        .expect("test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: execute succeeds");
    assert!(
        ChildSessionStore::update(&f.memory, &key, &context, &mut |_| Ok(original.clone())).is_err(),
        "child sessions migration 22 preserves records and quarantines unbound rows: capacity enforcement or bounded delivery changed"
    );
    assert_eq!(
        f.memory
            .pool
            .write()
            .query_row("SELECT record FROM child_sessions WHERE key=?1", [&key], |r| r
                .get::<_, String>(0))
            .expect(
                "test: child_sessions_migration_22_preserves_records_and_quarantines_unbound_rows: query_row succeeds"
            ),
        original,
        "child sessions migration 22 preserves records and quarantines unbound rows: capacity enforcement or bounded delivery changed"
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
        let mut statement = writer
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .expect("test: mailbox_index_plans_use_scope_and_stream_prefixes: prepare succeeds");
        let details = statement
            .query_map([], |r| r.get::<_, String>(3))
            .expect("test: mailbox_index_plans_use_scope_and_stream_prefixes: query_map succeeds")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("test: mailbox_index_plans_use_scope_and_stream_prefixes: collect succeeds")
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
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: publish succeeds");
    let progress = f
        .memory
        .pool
        .write()
        .query_row("SELECT progress FROM prx_mailbox_seq", [], |r| r.get::<_, String>(0))
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: query_row succeeds");
    let progress = progress.replacen("\"next_seq\":2", &format!("\"next_seq\":{}", u64::MAX), 1);
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET progress=?1", [progress])
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: execute succeeds");
    let last = f
        .store
        .publish(&f.authority, f.message(2, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: publish succeeds");
    assert_eq!(
        last.seq,
        u64::MAX,
        "mailbox u64 sequence exhaustion never wraps or truncates: expected messaging contract changed"
    );
    assert_eq!(
        f.store
            .publish(&f.authority, f.message(3, 0), &f.sender, f.clock.now() + 300, None)
            .unwrap_err(),
        MailboxError::Quota("seq"),
        "mailbox u64 sequence exhaustion never wraps or truncates: expected messaging contract changed"
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
        .expect("test: mailbox_u64_sequence_exhaustion_never_wraps_or_truncates: fixture setup succeeds");
    assert_eq!(
        raw, "FFFFFFFFFFFFFFFF",
        "mailbox u64 sequence exhaustion never wraps or truncates: expected messaging contract changed"
    );
}

#[tokio::test]
async fn process_worker_message_frame_roundtrips() {
    use crate::session_worker::protocol::{WorkerControlFrame, WorkerOutputFrame};
    let f = Fixture::new();
    let published = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: process_worker_message_frame_roundtrips: publish succeeds");
    let outgoing = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: process_worker_message_frame_roundtrips: pending_outbox succeeds");
    let message = outgoing
        .first()
        .expect("test: process_worker_message_frame_roundtrips: first succeeds")
        .message()
        .clone();
    f.store
        .accept(&f.authority, message.clone())
        .expect("test: process_worker_message_frame_roundtrips: accept succeeds");
    let delivered = f.deliver();
    let frame = WorkerControlFrame::Message {
        envelope: Box::new(delivered.message().clone()),
    };
    let decoded: WorkerControlFrame = serde_json::from_slice(
        &serde_json::to_vec(&frame)
            .expect("test: process_worker_message_frame_roundtrips: serde_json::to_vec succeeds"),
    )
    .expect("test: process_worker_message_frame_roundtrips: serde_json::from_slice succeeds");
    let (mut sender, mut inbox) = crate::session_worker::mailbox::worker_mailbox(f.target.clone(), "run-target".into())
        .expect("test: process_worker_message_frame_roundtrips: mailbox::worker_mailbox succeeds");
    let (sender, ack) = tokio::task::spawn_blocking(move || {
        let WorkerControlFrame::Message { envelope } = decoded else {
            panic!("Data frame became another control kind");
        };
        let ack = sender
            .enqueue(*envelope)
            .expect("test: process_worker_message_frame_roundtrips: enqueue succeeds");
        (sender, ack)
    })
    .await
    .expect("test: process_worker_message_frame_roundtrips: task::spawn_blocking succeeds");
    assert_eq!(
        ack,
        WorkerOutputFrame::Ack {
            message_id: published.message_id,
            state: DeliveryState::Delivered
        },
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    let queued = inbox
        .try_next()
        .expect("test: process_worker_message_frame_roundtrips: try_next succeeds");
    assert_eq!(
        crate::session_worker::mailbox::user_text(&queued)
            .expect("test: process_worker_message_frame_roundtrips: mailbox::user_text succeeds"),
        "request-1",
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    let consumed = inbox
        .consumed(&queued)
        .expect("test: process_worker_message_frame_roundtrips: consumed succeeds");
    let saved = f
        .store
        .adopt(&delivered)
        .expect("test: process_worker_message_frame_roundtrips: adopt succeeds");
    assert_eq!(
        consumed,
        WorkerOutputFrame::Ack {
            message_id: saved.message_id,
            state: saved.state
        },
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    let duplicate = tokio::task::spawn_blocking(move || {
        let mut sender = sender;
        sender
            .enqueue(message)
            .expect("test: process_worker_message_frame_roundtrips: enqueue succeeds")
    })
    .await
    .expect("test: process_worker_message_frame_roundtrips: task::spawn_blocking succeeds");
    assert_eq!(
        duplicate, ack,
        "a pruned worker slot may requeue a transport replay; durable adoption remains idempotent"
    );
    assert!(
        inbox.try_next().is_some(),
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.store
            .adopt(&delivered)
            .expect("test: process_worker_message_frame_roundtrips: adopt succeeds"),
        saved,
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        1,
        "process worker message frame roundtrips: IPC identity or delivery state changed"
    );
}

#[test]
fn worker_data_reaches_parent_mailbox() {
    use crate::session_worker::protocol::WorkerOutputFrame;
    let f = Fixture::new();
    // This worker is the source endpoint; the destination parent is bound by
    // its real MA scope and an HMAC recipient pin, not by asserted frame.from.
    let parent_key = crate::chat::sessions::child::ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: canonical parent key");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE agent_scopes SET session_key=?1 WHERE scope_id=?2",
            params![parent_key, f.recipient.scope_id.to_string()],
        )
        .expect("test: bind parent canonical scope");
    let mut parent = crate::session_worker::mailbox_parent::WorkerMailboxParent::new(
        f.store.clone(),
        f.source.clone(),
        f.sender.clone(),
        f.target.clone(),
        f.recipient.clone(),
        "run-source".into(),
        None,
    )
    .expect("test: worker_data_reaches_parent_mailbox: WorkerMailboxParent::new succeeds");
    let mut message = f.message(1, 0);
    message.to_pin.token.clear();
    message.from = SessionAddr::new("forged-instance", "forged-owner", "forged-session", None);
    let receipt = parent
        .receive(WorkerOutputFrame::Message {
            envelope: Box::new(message.clone()),
        })
        .expect("test: worker_data_reaches_parent_mailbox: receive succeeds")
        .expect("test: worker_data_reaches_parent_mailbox: receive returns a value");
    assert_eq!(
        receipt.state,
        DeliveryState::Accepted,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    assert_eq!(
        receipt.seq, 1,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    let duplicate = parent
        .receive(WorkerOutputFrame::Message {
            envelope: Box::new(message),
        })
        .expect("test: worker_data_reaches_parent_mailbox: receive succeeds")
        .expect("test: worker_data_reaches_parent_mailbox: receive returns a value");
    assert_eq!(
        duplicate, receipt,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    let delivered = f.deliver();
    assert_eq!(
        delivered.message().from,
        f.source,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        1,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        1,
        "worker data reaches parent mailbox: IPC identity or delivery state changed"
    );
}

#[test]
fn worker_data_keeps_sealed_provenance_and_cannot_reset_parent() {
    use crate::session_worker::protocol::WorkerOutputFrame;
    let f = Fixture::new();
    let parent_key = crate::chat::sessions::child::ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: canonical parent key");
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE agent_scopes SET session_key=?1 WHERE scope_id=?2",
            params![parent_key, f.recipient.scope_id.to_string()],
        )
        .expect("test: bind parent canonical scope");
    let mut remote = f.source.clone();
    remote.instance = "prx-peer".into();
    let peer = MailboxAuthority::verified_peer(
        W.into(),
        remote,
        f.target.clone(),
        crate::messaging::MessageAuth {
            issuer: "prx-issuer".into(),
            subject: "prx-peer".into(),
            method: "jwt".into(),
        },
    )
    .expect(
        "test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: MailboxAuthority::verified_peer succeeds",
    );
    let mut root = f.message(1, 1);
    root.to_pin = f
        .store
        .recipient_pin(&peer, &f.recipient, &parent_key)
        .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: recipient_pin succeeds");
    let accepted = f
        .store
        .accept(&peer, root)
        .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: accept succeeds");
    let inherited = crate::messaging::InheritedProvenance {
        remote: accepted.remote.expect(
            "test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: required fixture value is present",
        ),
        chain: accepted.chain,
    };
    let mut actor = crate::session_worker::mailbox_parent::WorkerMailboxParent::new(
        f.store.clone(),
        f.source.clone(),
        f.sender.clone(),
        f.target.clone(),
        f.recipient.clone(),
        "run-source".into(),
        Some(inherited.clone()),
    )
    .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: WorkerMailboxParent::new succeeds");
    let forged_root = f.message(2, 0);
    assert!(
        forged_root.in_reply_to.is_none(),
        "worker data keeps sealed provenance and cannot reset parent: IPC identity or delivery state changed"
    );
    let receipt = actor
        .receive(WorkerOutputFrame::Message {
            envelope: Box::new(forged_root),
        })
        .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: receive succeeds")
        .expect("test: worker_data_keeps_sealed_provenance_and_cannot_reset_parent: receive returns a value");
    assert_eq!(
        receipt.chain.ancestry,
        vec![inherited.chain.trigger],
        "worker data keeps sealed provenance and cannot reset parent: IPC identity or delivery state changed"
    );
    assert_eq!(
        receipt.remote,
        Some(inherited.remote),
        "worker data keeps sealed provenance and cannot reset parent: IPC identity or delivery state changed"
    );
    assert_eq!(
        f.scalar("SELECT sends FROM prx_remote_trees"),
        1,
        "worker data keeps sealed provenance and cannot reset parent: IPC identity or delivery state changed"
    );
}

#[tokio::test]
async fn send_before_and_after_task_reopen_uses_same_session_addr() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry, RelaunchRecipe};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(
            f.target.clone(),
            None,
            Some(RelaunchRecipe {
                addr: f.target.clone(),
                last_run: None,
                inherited_provenance: None,
            }),
        ))
        .await
        .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: insert succeeds");
    let restored = registry
        .reconstruct(&f.target)
        .await
        .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: reconstruct succeeds");
    let scope = restored
        .mailbox_scope()
        .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: mailbox_scope succeeds")
        .clone();
    drop(restored);
    let canonical = ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: ChildSessionRegistry::canonical_session_key succeeds");
    f.store
        .close_execution(&f.recipient, &f.target, "run-target")
        .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: close_execution succeeds");
    let mut previous = None;
    for n in 1..=2 {
        let run = registry
            .relaunch(&f.target)
            .await
            .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: relaunch succeeds")
            .run
            .expect(
                "test: send_before_and_after_task_reopen_uses_same_session_addr: required fixture value is present",
            );
        assert_ne!(
            previous.as_ref(),
            Some(&run),
            "send before and after task reopen uses same session addr: expected messaging contract changed"
        );
        previous = Some(run.clone());
        let mut message = f.message(n, 0);
        message.to_pin = f
            .store
            .recipient_pin(&f.authority, &scope, &canonical)
            .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: recipient_pin succeeds");
        let sent = f
            .store
            .publish(&f.authority, message, &f.sender, f.clock.now() + 300, None)
            .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: publish succeeds");
        assert_eq!(
            sent.seq, n,
            "send before and after task reopen uses same session addr: expected messaging contract changed"
        );
        assert_eq!(
            sent.stream_id,
            Uuid::from_u128(STREAM),
            "send before and after task reopen uses same session addr: expected messaging contract changed"
        );
        let outgoing = f
            .store
            .pending_outbox(&f.sender, &f.source)
            .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: pending_outbox succeeds");
        let message = outgoing
            .iter()
            .find(|pending| pending.message().id == sent.message_id)
            .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: find succeeds")
            .message()
            .clone();
        f.store
            .accept(&f.authority, message)
            .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: accept succeeds");
        drop(outgoing);
        let store = f.store.clone();
        let scope = scope.clone();
        let address = f.target.clone();
        let adopted = tokio::spawn(async move {
            let _owner = store
                .execution_guard(&scope, &address, run)
                .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: execution_guard succeeds");
            let delivered = store
                .deliver(&scope, &address)
                .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: deliver succeeds")
                .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: deliver returns a value");
            assert_eq!(
                delivered.message().to,
                address,
                "send before and after task reopen uses same session addr: expected messaging contract changed"
            );
            store
                .adopt(&delivered)
                .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: adopt succeeds")
        })
        .await
        .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: tokio::spawn succeeds");
        assert_eq!(
            adopted.state,
            DeliveryState::Consumed,
            "send before and after task reopen uses same session addr: expected messaging contract changed"
        );
        f.store
            .acknowledge(&f.sender, &f.target, sent.message_id, adopted.state)
            .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: acknowledge succeeds");
        let restored = registry
            .reconstruct(&f.target)
            .await
            .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: reconstruct succeeds");
        assert_eq!(
            restored.addr(),
            &f.target,
            "send before and after task reopen uses same session addr: expected messaging contract changed"
        );
        assert_eq!(
            restored.history().len(),
            usize::try_from(n)
                .expect("test: send_before_and_after_task_reopen_uses_same_session_addr: usize::try_from succeeds"),
            "send before and after task reopen uses same session addr: expected messaging contract changed"
        );
    }
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        1,
        "send before and after task reopen uses same session addr: expected messaging contract changed"
    );
}

#[tokio::test]
async fn run_pinned_message_nacks_after_relaunch() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry, RelaunchRecipe};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(
            f.target.clone(),
            None,
            Some(RelaunchRecipe {
                addr: f.target.clone(),
                last_run: None,
                inherited_provenance: None,
            }),
        ))
        .await
        .expect("test: run_pinned_message_nacks_after_relaunch: insert succeeds");
    let restored = registry
        .reconstruct(&f.target)
        .await
        .expect("test: run_pinned_message_nacks_after_relaunch: reconstruct succeeds");
    let scope = restored
        .mailbox_scope()
        .expect("test: run_pinned_message_nacks_after_relaunch: mailbox_scope succeeds")
        .clone();
    drop(restored);
    let canonical = ChildSessionRegistry::canonical_session_key(&f.target)
        .expect("test: run_pinned_message_nacks_after_relaunch: ChildSessionRegistry::canonical_session_key succeeds");
    let first = registry
        .relaunch(&f.target)
        .await
        .expect("test: run_pinned_message_nacks_after_relaunch: relaunch succeeds")
        .run
        .expect("test: run_pinned_message_nacks_after_relaunch: required fixture value is present");
    f.store
        .set_execution(&scope, &f.target, Some(first.clone()))
        .expect("test: run_pinned_message_nacks_after_relaunch: set_execution succeeds");
    let mut message = f.message(1, 0);
    message.to_pin = f
        .store
        .recipient_pin(&f.authority, &scope, &canonical)
        .expect("test: run_pinned_message_nacks_after_relaunch: recipient_pin succeeds");
    message.exec = Some(ExecRef {
        run: Some(first.clone()),
    });
    let sent = f
        .store
        .publish(&f.authority, message.clone(), &f.sender, f.clock.now() + 300, None)
        .expect("test: run_pinned_message_nacks_after_relaunch: publish succeeds");
    let second = registry
        .relaunch(&f.target)
        .await
        .expect("test: run_pinned_message_nacks_after_relaunch: relaunch succeeds")
        .run
        .expect("test: run_pinned_message_nacks_after_relaunch: required fixture value is present");
    assert_ne!(
        first, second,
        "run pinned message nacks after relaunch: recipient authority or scope isolation changed"
    );
    f.store
        .set_execution(&scope, &f.target, Some(second))
        .expect("test: run_pinned_message_nacks_after_relaunch: set_execution succeeds");
    let outgoing = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: run_pinned_message_nacks_after_relaunch: pending_outbox succeeds");
    let frame = outgoing
        .first()
        .expect("test: run_pinned_message_nacks_after_relaunch: first succeeds")
        .message()
        .clone();
    let nack = f.store.accept(&f.authority, frame).unwrap_err();
    assert_eq!(
        nack,
        MailboxError::StaleRun,
        "run pinned message nacks after relaunch: recipient authority or scope isolation changed"
    );
    f.store
        .reject_outbox(&f.sender, &f.target, sent.message_id, nack)
        .expect("test: run_pinned_message_nacks_after_relaunch: reject_outbox succeeds");
    let failed = f
        .store
        .publish(&f.authority, message, &f.sender, f.clock.now() + 300, None)
        .expect("test: run_pinned_message_nacks_after_relaunch: publish succeeds");
    assert_eq!(
        failed.state,
        crate::messaging::mailbox::MailboxSendState::Failed,
        "run pinned message nacks after relaunch: recipient authority or scope isolation changed"
    );
    assert_eq!(
        failed.error.as_deref(),
        Some("stale_run"),
        "run pinned message nacks after relaunch: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "run pinned message nacks after relaunch: recipient authority or scope isolation changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM conversation_turns"),
        0,
        "run pinned message nacks after relaunch: recipient authority or scope isolation changed"
    );
}

#[test]
fn permanent_seq_gap_nacks_reachable_sender_outbox() {
    let f = Fixture::new();
    let first = f.message(1, 0);
    f.store
        .publish(&f.authority, first.clone(), &f.sender, f.clock.now() + 300, None)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: publish succeeds");
    let second = f
        .store
        .publish(&f.authority, f.message(2, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: publish succeeds");
    let pending = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: pending_outbox succeeds");
    let message = pending
        .iter()
        .find(|item| item.message().id == second.message_id)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: find succeeds")
        .message()
        .clone();
    drop(pending);
    f.store
        .accept(&f.authority, message)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: accept succeeds");
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: deliver succeeds")
            .is_none(),
        "permanent seq gap nacks reachable sender outbox: expected messaging contract changed"
    );
    f.clock.advance(crate::messaging::mailbox::GAP_SECONDS);
    let delivered = f.deliver();
    assert_eq!(
        delivered.message().seq,
        2,
        "permanent seq gap nacks reachable sender outbox: expected messaging contract changed"
    );
    f.store
        .adopt(&delivered)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: adopt succeeds");
    let failed = f
        .store
        .publish(&f.authority, first, &f.sender, f.clock.now() + 300, None)
        .expect("test: permanent_seq_gap_nacks_reachable_sender_outbox: publish succeeds");
    assert_eq!(
        failed.state,
        crate::messaging::mailbox::MailboxSendState::Failed,
        "permanent seq gap nacks reachable sender outbox: expected messaging contract changed"
    );
    assert_eq!(
        failed.error.as_deref(),
        Some("seq_gap"),
        "permanent seq gap nacks reachable sender outbox: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        1,
        "Nack must not create an Ack receipt"
    );
}

#[test]
fn worker_mime_projection_matches_durable_transcript() {
    let f = Fixture::new();
    for (index, (mime, bytes)) in [
        ("text/plain", b"/approve remains ordinary input".to_vec()),
        ("text/markdown", b"**ordinary input**".to_vec()),
        ("text/plain", vec![0xff, 0]),
        ("application/octet-stream", vec![0, 1, 0xff]),
    ]
    .into_iter()
    .enumerate()
    {
        let seq = u64::try_from(index + 1)
            .expect("test: worker_mime_projection_matches_durable_transcript: u64::try_from succeeds");
        let mut message = f.message(seq, seq);
        message.body = MessageBody {
            mime: mime.into(),
            bytes,
        };
        f.store
            .accept(&f.authority, message)
            .expect("test: worker_mime_projection_matches_durable_transcript: accept succeeds");
        let delivered = f.deliver();
        let worker_text = crate::session_worker::mailbox::user_text(delivered.message())
            .expect("test: worker_mime_projection_matches_durable_transcript: mailbox::user_text succeeds");
        f.store
            .adopt(&delivered)
            .expect("test: worker_mime_projection_matches_durable_transcript: adopt succeeds");
        let durable_text: String = f
            .memory
            .pool
            .write()
            .query_row(
                "SELECT content FROM conversation_turns WHERE message_id=?1",
                [delivered.message().id.to_string()],
                |row| row.get(0),
            )
            .expect("test: worker_mime_projection_matches_durable_transcript: query_row succeeds");
        assert_eq!(
            worker_text, durable_text,
            "worker mime projection matches durable transcript: atomic adoption or history projection changed"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_sender_ports_share_one_durable_stream() {
    let f = Fixture::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for text in ["first", "second"] {
        let port = f.sender_port(None);
        let target = f.target.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            port.send(target, text.into(), "text/plain".into())
                .await
                .expect("test: concurrent_sender_ports_share_one_durable_stream: send succeeds")
        }));
    }
    let mut published = Vec::new();
    for task in tasks {
        published.push(
            task.await
                .expect("test: concurrent_sender_ports_share_one_durable_stream: awaited task completes"),
        );
    }
    published.sort_by_key(|sent| sent.seq);
    assert_eq!(
        published.iter().map(|sent| sent.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    let wire = published
        .first()
        .expect("test: concurrent_sender_ports_share_one_durable_stream: first succeeds")
        .stream_id;
    assert!(
        published.iter().all(|sent| sent.stream_id == wire),
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    let reopened = f
        .sender_port(None)
        .send(f.target.clone(), "reopened port".into(), "text/plain".into())
        .await
        .expect("test: concurrent_sender_ports_share_one_durable_stream: send succeeds");
    assert_eq!(
        reopened.stream_id, wire,
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    assert_eq!(
        reopened.seq, 3,
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_seq"),
        1,
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        3,
        "concurrent sender ports share one durable stream: expected messaging contract changed"
    );
}

#[tokio::test]
async fn pending_send_resolver_keeps_payload_in_scope_until_cancel_join() {
    struct PausedResolver {
        inner: LocalResolver,
        entered: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl crate::messaging::send::MailboxResolver for PausedResolver {
        async fn resolve(
            &self,
            target: &SessionAddr,
        ) -> crate::messaging::mailbox::MailboxResult<crate::messaging::send::MailboxDestination> {
            let resolved = crate::messaging::send::MailboxResolver::resolve(&self.inner, target).await?;
            let entered = self.entered.lock().take();
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            self.release.notified().await;
            Ok(resolved)
        }
    }
    use crate::messaging::send::MailboxSender as _;
    let f = Fixture::new();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let resolver = Arc::new(PausedResolver {
        inner: LocalResolver {
            store: f.store.clone(),
            authority: f.authority.clone(),
            scope: f.recipient.clone(),
        },
        entered: parking_lot::Mutex::new(Some(entered)),
        release: tokio::sync::Notify::new(),
    });
    let port = crate::memory::sqlite::mailbox::SqliteMailboxSender::new(
        f.store.clone(),
        f.source.clone(),
        f.sender.clone(),
        resolver,
        None,
    )
    .expect("test: resolve: SqliteMailboxSender::new succeeds");
    let target = f.target.clone();
    let sending = tokio::spawn(async move {
        port.send(target, "pending scoped payload".into(), "text/plain".into())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), ready)
        .await
        .expect("test: resolve: time::timeout succeeds")
        .expect("test: resolve: time::timeout returns a value");
    let kept_payload_scope = f.memory.mailbox_runtime.has_scope_leases(f.sender.scope_id);
    sending.abort();
    assert!(
        sending.await.unwrap_err().is_cancelled(),
        "pending resolver task must join after cancellation"
    );
    assert!(
        kept_payload_scope,
        "queued send lost its body lease while resolver was pending"
    );
    assert!(
        !f.memory.mailbox_runtime.has_scope_leases(f.sender.scope_id),
        "cancel join retained the send payload"
    );
    let erased = f
        .store
        .erase_scope(&f.sender, Duration::from_secs(5), &|| Ok(()))
        .expect("test: resolve: erase_scope succeeds");
    assert_eq!(
        erased.remaining, 0,
        "joining the cancelled resolver must allow complete scope erasure"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox"),
        0,
        "cancelled resolver must not persist an outbox body"
    );
}

fn stale_slot_case(adopt_race: bool) {
    let f = Fixture::new();
    let mut first = f.message(1, 0);
    first.exec = Some(ExecRef {
        run: Some("run-target".into()),
    });
    let published = f
        .store
        .publish(&f.authority, first, &f.sender, i64::MAX, None)
        .expect("test: publish run-pinned slot");
    f.store
        .publish(&f.authority, f.message(2, 0), &f.sender, i64::MAX, None)
        .expect("test: publish later unpinned slot");
    let outgoing = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: load accepted work");
    for slot in &outgoing {
        let accepted = f
            .store
            .accept(&f.authority, slot.message().clone())
            .expect("test: accept pending slot");
        f.store
            .acknowledge(&f.sender, &f.target, slot.message().id, accepted.state)
            .expect("test: acknowledge accepted work before the run changes");
    }
    drop(outgoing);
    let held = if adopt_race {
        let held = f.deliver();
        f.store
            .acknowledge(&f.sender, &f.target, held.message().id, DeliveryState::Delivered)
            .expect("test: acknowledge delivery before the adoption race");
        Some(held)
    } else {
        None
    };
    f.store
        .set_execution(&f.recipient, &f.target, Some("run-next".into()))
        .expect("test: switch recipient execution");
    if let Some(held) = held {
        assert_eq!(
            f.store.adopt(&held).expect_err("test: stale adoption is refused"),
            MailboxError::StaleRun,
            "run change must terminalize only the held slot"
        );
        drop(held);
    }
    let next = f.deliver();
    assert_eq!(next.message().seq, 2, "later accepted work must survive a stale run");
    assert_eq!(
        f.store.adopt(&next).expect("test: adopt surviving slot").state,
        DeliveryState::Consumed,
        "later unpinned slot must reach its transcript"
    );
    let writer = f.memory.pool.write();
    let (state, meta): (i64, String) = writer
        .query_row(
            "SELECT state,envelope_meta FROM prx_mailbox_outbox WHERE id=?1",
            [published.row_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("test: inspect local sender nack");
    assert_eq!(state, 3, "stale slot must terminate its local sender outbox");
    let failed: i64 = writer
        .query_row("SELECT count(*) FROM prx_mailbox_outbox WHERE state=3", [], |row| {
            row.get(0)
        })
        .expect("test: inspect the single targeted nack");
    assert_eq!(failed, 1, "a stale run must nack only its own slot");
    let meta: serde_json::Value = serde_json::from_str(&meta).expect("test: decode nack metadata");
    assert_eq!(
        meta.get("last_error").and_then(serde_json::Value::as_str),
        Some("stale_run"),
        "sender must see the one-slot stale_run nack"
    );
    let live: i64 = writer
        .query_row("SELECT count(*) FROM prx_mailbox_seq WHERE state=0", [], |row| {
            row.get(0)
        })
        .expect("test: inspect stream lifetime");
    assert_eq!(live, 1, "stale work must not retire the shared stream");
    let erased: i64 = writer.query_row("SELECT count(*) FROM prx_mailbox_receipts WHERE seq=?1 AND json_extract(receipt_meta,'$.body_erased_at') IS NOT NULL", [1u64.to_be_bytes().as_slice()],|row|row.get(0))
        .expect("test: inspect terminal receipt");
    assert_eq!(
        erased, 1,
        "stale payload must be erased in the terminal-slot transaction"
    );
}

#[test]
fn stale_run_slot_is_nacked_and_later_unpinned_slots_still_deliver() {
    stale_slot_case(false);
}

#[test]
fn stale_run_during_adopt_is_nacked_and_later_unpinned_slots_still_deliver() {
    stale_slot_case(true);
}

#[test]
fn worker_run_and_parent_binding_survive_more_than_max_rows_frames() {
    use crate::session_worker::protocol::{WorkerControlFrame, WorkerOutputFrame};
    const FRAMES: i64 = crate::messaging::mailbox::MAX_ROWS + 1;
    let f = Fixture::new();
    let mut parent = crate::session_worker::mailbox_parent::WorkerMailboxParent::new(
        f.store.clone(),
        f.target.clone(),
        f.recipient.clone(),
        f.source.clone(),
        f.sender.clone(),
        "run-target".into(),
        None,
    )
    .expect("test: one persistent parent binding");
    let (mut sender, mut inbox) = crate::session_worker::mailbox::worker_mailbox(f.target.clone(), "run-target".into())
        .expect("test: one persistent worker inbox");
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, i64::MAX, None)
        .expect("test: publish first long-lived worker frame");
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    for index in 1..=FRAMES {
        assert!(
            std::time::Instant::now() < deadline,
            "bounded long-lived worker regression exceeded deadline"
        );
        let pending = f
            .store
            .pending_outbox(&f.sender, &f.source)
            .expect("test: pending single slot");
        let message = pending.first().expect("test: one pending envelope").message().clone();
        f.store.accept(&f.authority, message).expect("test: accept next frame");
        drop(pending);
        let WorkerControlFrame::Message { envelope } = parent
            .next_delivery()
            .expect("test: next parent delivery")
            .expect("test: queued frame")
        else {
            panic!("test: parent produced non-Data control");
        };
        let delivered = sender
            .enqueue(*envelope)
            .expect("test: bounded inbox must not grow by run lifetime");
        parent.receive(delivered).expect("test: parent accepts delivered ack");
        let message = inbox.try_next().expect("test: one in-flight worker input");
        let consumed = inbox
            .consumed(&message)
            .expect("test: consume prunes the in-flight dedupe entry");
        parent
            .receive(consumed.clone())
            .expect("test: parent must not fail after committed adoption");
        parent
            .receive(consumed)
            .expect("test: consumed acknowledgement replay is harmless");
        parent
            .receive(WorkerOutputFrame::Ack {
                message_id: message.id,
                state: DeliveryState::Consumed,
            })
            .expect("test: late acknowledgement remains idempotent");
        f.clock.advance(crate::messaging::mailbox::REPLAY_SECONDS + 1);
        // Keep the stream live while expiring old ledger rows; only the
        // worker's in-flight window, rather than durable storage quota, is tested.
        if index < FRAMES {
            let next = u64::try_from(index + 1).expect("test: bounded frame index");
            f.store
                .publish(&f.authority, f.message(next, 0), &f.sender, i64::MAX, None)
                .expect("test: publish next frame before retention GC");
        }
        f.store
            .gc_scope(&f.sender)
            .expect("test: expire sender's durable replay records");
        f.store
            .gc_scope(&f.recipient)
            .expect("test: expire consumed receiver records");
    }
}

#[tokio::test]
async fn erase_scope_deadline_preserves_fence_until_child_lease_drops() {
    use crate::chat::sessions::child::{ChildSession, ChildSessionRegistry};
    let f = Fixture::new();
    let registry = ChildSessionRegistry::new(Arc::new(f.memory.clone())).with_workspace(W.into());
    registry
        .insert(&ChildSession::new(f.target.clone(), None, None))
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
