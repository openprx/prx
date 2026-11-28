// Pending-message erase of an rf delete: what an erased run sent and no receiver
// has adopted closes as a hole above the delivery cursor (never past an earlier
// kept slot), and a closed slot is "skip" for every adopting loop.

const RUN_ERASED: &str = "run-erased";

fn message_hex(n: u64) -> String {
    Uuid::from_u128(1000 + u128::from(n)).simple().to_string()
}

fn tag_receipt(f: &Fixture, n: u64, run: &str) {
    f.memory
        .pool
        .write()
        .execute(
            "UPDATE prx_mailbox_receipts SET sender_run_id=?1 WHERE message_id=?2",
            params![run, Uuid::from_u128(1000 + u128::from(n)).as_bytes().as_slice()],
        )
        .expect("test: tag_receipt: update succeeds");
}

fn erase_runs(f: &Fixture, runs: &[&str]) -> (usize, usize, usize) {
    let json = serde_json::to_string(runs).expect("test: erase_runs: encode run list");
    let writer = f.memory.pool.write();
    crate::memory::sqlite::mailbox::erase_pending_from_runs(&writer, W, O, &json, f.clock.now())
        .expect("test: erase_runs: erase succeeds")
}

fn progress_json(f: &Fixture) -> serde_json::Value {
    let raw: String = f
        .memory
        .pool
        .write()
        .query_row("SELECT progress FROM prx_mailbox_seq", [], |row| row.get(0))
        .expect("test: progress_json: one stream");
    serde_json::from_str(&raw).expect("test: progress_json: valid json")
}

/// (consume_cursor, deliver_cursor, hole seqs)
fn progress(f: &Fixture) -> (u64, u64, Vec<u64>) {
    let value = progress_json(f);
    let cursor = |key: &str| value[key].as_u64().expect("test: progress: cursor is a number");
    let holes = value["seq_skipped"]
        .as_array()
        .expect("test: progress: holes are an array")
        .iter()
        .map(|hole| hole["seq"].as_u64().expect("test: progress: hole seq"))
        .collect();
    (cursor("consume_cursor"), cursor("deliver_cursor"), holes)
}

fn receipt_erased(f: &Fixture, n: u64) -> i64 {
    f.scalar(&format!(
        "SELECT count(*) FROM prx_mailbox_receipts WHERE message_id=x'{}' AND json_extract(receipt_meta,'$.body_erased_at') IS NOT NULL",
        message_hex(n)
    ))
}

#[test]
fn pending_erase_hole_never_moves_cursor_past_an_earlier_kept_slot() {
    let f = Fixture::new();
    f.receive(1, 1);
    f.receive(2, 2);
    f.receive(3, 3);
    tag_receipt(&f, 2, RUN_ERASED);
    assert_eq!(
        erase_runs(&f, &[RUN_ERASED]),
        (1, 0, 0),
        "one pending receipt of the erased run must be erased"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        2,
        "erased body row is gone"
    );
    assert_eq!(receipt_erased(&f, 2), 1, "receipt carries body_erased_at");
    assert_eq!(receipt_erased(&f, 1), 0, "the earlier kept slot is untouched");
    assert_eq!(progress(&f), (0, 0, vec![2]), "hole above the cursors, cursors unmoved");
    let first = f.deliver();
    assert_eq!(first.message().seq, 1, "the earlier kept slot is delivered first");
    f.store.adopt(&first).expect("test: adopt the kept slot");
    assert_eq!(progress(&f), (2, 2, vec![2]), "adopt walks over the hole");
    let third = f.deliver();
    assert_eq!(third.message().seq, 3, "the slot after the hole is delivered");
    f.store.adopt(&third).expect("test: adopt the slot after the hole");
    assert_eq!(progress(&f), (3, 3, vec![2]), "all kept slots consumed");
    assert_eq!(
        f.scalar("SELECT count(*) FROM message_events WHERE content LIKE '%request-2%'"),
        0,
        "the erased message never reaches the transcript"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM message_events WHERE source='prx_mailbox'"),
        2
    );
}

#[test]
fn pending_erase_at_the_head_raises_deliver_with_consume() {
    let f = Fixture::new();
    f.receive(1, 1);
    tag_receipt(&f, 1, RUN_ERASED);
    assert_eq!(erase_runs(&f, &[RUN_ERASED]).0, 1, "head slot erased");
    assert_eq!(
        progress(&f),
        (1, 1, vec![1]),
        "consume walked over the hole and deliver kept up (consume <= deliver)"
    );
    f.receive(2, 2);
    let next = f.deliver();
    assert_eq!(next.message().seq, 2, "the loop continues after the head hole");
    f.store.adopt(&next).expect("test: adopt after head hole");
}

#[test]
fn pending_erase_of_delivered_slot_makes_adopt_a_skip() {
    let f = Fixture::new();
    f.receive(1, 1);
    f.receive(2, 2);
    tag_receipt(&f, 1, RUN_ERASED);
    let erased = f.deliver();
    let kept = f.deliver();
    assert_eq!((erased.message().seq, kept.message().seq), (1, 2));
    assert_eq!(erase_runs(&f, &[RUN_ERASED]).0, 1, "delivered-unconsumed slot erased");
    assert_eq!(progress(&f), (1, 2, vec![1]), "consume walked, deliver stays ahead");
    assert_eq!(
        f.store.adopt(&erased).unwrap_err(),
        MailboxError::StaleRun,
        "adopting the erased slot is the stale-run signal"
    );
    assert!(
        f.store
            .adopt_if_open(&erased)
            .expect("test: closed slot is not an error")
            .is_none(),
        "the loops treat a closed slot as skip"
    );
    assert!(
        f.store.adopt_if_open(&kept).expect("test: kept slot adopts").is_some(),
        "the kept slot after it is adopted"
    );
    assert_eq!(progress(&f), (2, 2, vec![1]));
}

#[test]
fn pending_erase_between_deliver_and_adopt_leaves_the_loop_running() {
    let f = Fixture::new();
    f.receive(1, 1);
    tag_receipt(&f, 1, RUN_ERASED);
    let delivery = f.deliver();
    assert_eq!(
        erase_runs(&f, &[RUN_ERASED]).0,
        1,
        "delete commits between deliver and adopt"
    );
    assert!(
        f.store
            .adopt_if_open(&delivery)
            .expect("test: skip, not an error")
            .is_none(),
        "no input is produced for the erased slot"
    );
    assert!(
        f.store
            .deliver(&f.recipient, &f.target)
            .expect("test: deliver after erase")
            .is_none(),
        "nothing left to deliver"
    );
    f.receive(2, 2);
    let next = f.deliver();
    assert_eq!(next.message().seq, 2, "later messages still arrive");
    f.store.adopt(&next).expect("test: adopt later message");
}

#[test]
fn pending_erase_reserves_the_seq_of_an_outbox_row_without_receipt() {
    let f = Fixture::new();
    let published = f
        .store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: publish succeeds");
    assert_eq!(published.seq, 1);
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_outbox SET sender_run_id=?1", [RUN_ERASED])
        .expect("test: tag outbox");
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_receipts"),
        0,
        "crash window: no receipt"
    );
    assert_eq!(
        erase_runs(&f, &[RUN_ERASED]),
        (0, 1, 0),
        "only the outbox row is erased"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox WHERE body_bytes IS NULL AND state=3"),
        1
    );
    assert_eq!(progress(&f), (1, 1, vec![1]), "the seq is reserved as a hole");
    assert_eq!(
        f.store.accept(&f.authority, f.message(1, 1)).unwrap_err(),
        MailboxError::SeqAlreadyConsumed,
        "a retry of the erased seq is refused"
    );
    f.receive(2, 2);
    assert_eq!(
        f.deliver().message().seq,
        2,
        "the next seq is delivered without the gap wait"
    );
}

#[test]
fn pending_erase_skips_a_slot_when_the_hole_table_is_full() {
    let f = Fixture::new();
    f.receive(1, 1);
    f.receive(2, 2);
    tag_receipt(&f, 2, RUN_ERASED);
    // `decode` accepts only the canonical encoding (struct field order), so the
    // holes are spliced into the stored text instead of re-serialised through a map.
    let stored: String = f
        .memory
        .pool
        .write()
        .query_row("SELECT progress FROM prx_mailbox_seq", [], |row| row.get(0))
        .expect("test: stored progress");
    let holes: Vec<String> = (3..3 + 1024u64)
        .map(|seq| format!(r#"{{"seq":{seq},"skipped_at":1}}"#))
        .collect();
    let filled = stored.replace(
        r#""seq_skipped":[]"#,
        &format!(r#""seq_skipped":[{}]"#, holes.join(",")),
    );
    assert_ne!(filled, stored, "test: the hole list was spliced in");
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_seq SET progress=?1", [filled])
        .expect("test: fill the hole table");
    assert_eq!(erase_runs(&f, &[RUN_ERASED]), (0, 0, 1), "slot skipped and counted");
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 2, "body kept");
    assert_eq!(receipt_erased(&f, 2), 0, "receipt not marked");
    assert_eq!(progress(&f).2.len(), 1024, "hole table unchanged");
}

#[test]
fn pending_erase_leaves_rows_without_a_recorded_sender_run() {
    let f = Fixture::new();
    f.receive(1, 1);
    f.receive(2, 2);
    tag_receipt(&f, 2, "run-kept");
    assert_eq!(erase_runs(&f, &[RUN_ERASED]), (0, 0, 0), "nothing matches");
    assert_eq!(f.scalar("SELECT count(*) FROM prx_mailbox_messages"), 2);
    assert_eq!(progress(&f), (0, 0, vec![]), "no hole, no cursor move");
}

#[test]
fn pending_erase_refuses_a_replay_of_the_erased_seq() {
    let f = Fixture::new();
    f.receive(1, 1);
    f.receive(2, 2);
    tag_receipt(&f, 2, RUN_ERASED);
    erase_runs(&f, &[RUN_ERASED]);
    assert_eq!(
        f.store.accept(&f.authority, f.message(9, 2)).unwrap_err(),
        MailboxError::SeqAlreadyConsumed,
        "the hole reserves the erased seq against a late copy"
    );
}

#[test]
fn pending_erase_follows_the_sender_outbox_of_the_same_message() {
    let f = Fixture::new();
    f.store
        .publish(&f.authority, f.message(1, 0), &f.sender, f.clock.now() + 300, None)
        .expect("test: publish succeeds");
    let sent = f
        .store
        .pending_outbox(&f.sender, &f.source)
        .expect("test: pending_outbox succeeds")
        .first()
        .expect("test: one pending outbox row")
        .message()
        .clone();
    f.store.accept(&f.authority, sent).expect("test: accept succeeds");
    f.memory
        .pool
        .write()
        .execute("UPDATE prx_mailbox_outbox SET sender_run_id=?1", [RUN_ERASED])
        .expect("test: tag outbox");
    tag_receipt(&f, 1, RUN_ERASED);
    assert_eq!(
        erase_runs(&f, &[RUN_ERASED]),
        (1, 0, 0),
        "the receipt pass reaches the outbox row"
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_outbox WHERE body_bytes IS NULL AND state=3"),
        1
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM prx_mailbox_messages"),
        0,
        "receiver body gone"
    );
    assert_eq!(progress(&f), (1, 1, vec![1]));
}
