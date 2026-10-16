#![cfg(test)]
use super::*;

/// A read-only store never creates a database: a missing one reads as
/// empty.
#[test]
fn a_read_only_store_creates_nothing() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = CausalDecisionStore::open(
        temp.path(),
        Retention::days(30, 30),
        sink(),
        StoreMode::ReadOnly,
        WriterSetup::default(),
    )
    .unwrap();
    assert!(store.writer().is_none());
    assert!(store.reader().get(&operator(&store), &id(1)).unwrap().is_none());
    assert!(store.workspace_id().is_none(), "no store, no id");
    assert_eq!(store.reader().counts().unwrap(), StoreCounts::default());
    assert!(!temp.path().join("self").exists(), "nothing was created");
}

/// Retention deletes everything past the horizon in bounded batches,
/// and the outcomes of deleted decisions go with them.
#[tokio::test]
async fn retention_cleanup_bounded() {
    let temp = tempfile::TempDir::new().unwrap();
    let old = now_ms() - 40 * DAY_MS;
    {
        let store = open_private(temp.path(), sink());
        let writer = store.writer().unwrap();
        for n in 0..2_500_u32 {
            let mut digest = [0_u8; 32];
            digest[..4].copy_from_slice(&n.to_be_bytes());
            let queued = loop {
                if writer.record_decision(record(DecisionId::legacy(&digest), "ws", Some("a"), old)) == Enqueue::Queued
                {
                    break true;
                }
                flushed(&store).await;
            };
            assert!(queued);
        }
        writer.record_decision(record(id(200), "ws", Some("a"), now_ms()));
        flushed(&store).await;
    }
    write_outcome(temp.path(), id(200), &OutcomeFixture::default());
    let mut conn = Connection::open(db_path(temp.path())).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    let first_old: String = conn
        .query_row(
            "SELECT decision_id FROM decisions WHERE created_at_ms = ?1 LIMIT 1",
            [old],
            |row| row.get(0),
        )
        .unwrap();
    write_outcome(
        temp.path(),
        DecisionId::parse(&first_old).unwrap(),
        &OutcomeFixture::default(),
    );
    let deleted = prune(&mut conn, Retention::days(30, 90).cutoff(now_ms()).unwrap()).unwrap();
    assert_eq!(deleted, 2_500);
    let left: i64 = conn
        .query_row("SELECT count(*) FROM decisions", [], |row| row.get(0))
        .unwrap();
    let outcomes: i64 = conn
        .query_row("SELECT count(*) FROM outcomes", [], |row| row.get(0))
        .unwrap();
    assert_eq!((left, outcomes), (1, 1), "old outcomes cascade");
    drop(conn);

    // The store's own connection cascades too: an old decision with an
    // outcome is gone, with its outcome, after the next open.
    {
        let store = open_private(temp.path(), sink());
        store
            .writer()
            .unwrap()
            .record_decision(record(id(201), "ws", Some("a"), old));
        flushed(&store).await;
    }
    write_outcome(temp.path(), id(201), &OutcomeFixture::default());
    let store = open_private(temp.path(), sink());
    let counts = store.reader().counts().unwrap();
    assert_eq!(
        (counts.decisions, counts.outcomes),
        (1, 1),
        "pruned at open, outcome cascaded"
    );
    assert_eq!(
        Retention::days(0, 90),
        Retention::days(90, 7),
        "0 follows the fitness retention"
    );
}

/// D-040 OQ-A8: a full queue drops the record without blocking, counts
/// it and reports it.
#[tokio::test]
async fn a_full_queue_drops_without_blocking() {
    let temp = tempfile::TempDir::new().unwrap();
    let events = sink();
    let hold = Arc::new(parking_lot::Mutex::new(()));
    let store = CausalDecisionStore::open(
        temp.path(),
        Retention::days(30, 30),
        Arc::clone(&events) as Arc<dyn CausalEventSink>,
        StoreMode::ReadWrite,
        WriterSetup {
            capacity: 1,
            hold: Some(Arc::clone(&hold)),
        },
    )
    .unwrap();
    let writer = store.writer().unwrap();
    let held = hold.lock();
    let started = Instant::now();
    let mut outcomes = Vec::new();
    for n in 1..=4 {
        outcomes.push(writer.record_decision(record(id(n), "ws", Some("a"), now_ms())));
    }
    assert!(
        started.elapsed() < std::time::Duration::from_millis(50),
        "enqueueing never waits"
    );
    drop(held);
    assert!(outcomes.contains(&Enqueue::Dropped));
    let dropped = outcomes.iter().filter(|outcome| **outcome == Enqueue::Dropped).count();
    assert_eq!(
        incomplete(&events)
            .iter()
            .filter(|reason| **reason == IncompleteReason::StoreDropped)
            .count(),
        dropped
    );
    flushed(&store).await;
    let stored = usize::try_from(store.reader().counts().unwrap().decisions).unwrap();
    assert_eq!(stored + dropped, 4);
}

#[test]
fn full_queue_drops_the_whole_decision_chain() {
    let events = sink();
    // No consumer runs until the assertions finish, so capacity one is
    // provably full without depending on the writer thread's schedule.
    let (queue, receiver) = std::sync::mpsc::sync_channel(1);
    let writer = DecisionStoreWriter {
        queue,
        events: Arc::clone(&events) as Arc<dyn CausalEventSink>,
        workspace_id: Arc::from("prx_workspace"),
    };
    assert_eq!(
        writer.record_decision(record(id(1), "ws", Some("prx_user"), 10)),
        Enqueue::Queued
    );
    let first = record(id(3), "ws", Some("prx_user"), 10);
    let mut second = record(id(4), "ws", Some("prx_user"), 10);
    second.lineage.prior_decision_id = Some(first.decision_id);
    assert_eq!(writer.record_chain(vec![first, second]), Enqueue::Dropped);
    let queued = receiver.try_recv().expect("only the filler is queued");
    assert!(
        matches!(queued, WriterMessage::DecisionBatch(records) if records.len() == 1 && records[0].decision_id == id(1))
    );
    assert!(matches!(receiver.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
    assert_eq!(
        incomplete(&events)
            .iter()
            .filter(|reason| **reason == IncompleteReason::StoreDropped)
            .count(),
        2,
    );
}

/// The store shares one instance per database; a read-write request
/// replaces a read-only one.
#[test]
fn stores_are_shared_per_database() {
    let temp = tempfile::TempDir::new().unwrap();
    let read_only =
        CausalDecisionStore::open_shared(temp.path(), Retention::days(1, 1), sink(), StoreMode::ReadOnly).unwrap();
    let again =
        CausalDecisionStore::open_shared(temp.path(), Retention::days(1, 1), sink(), StoreMode::ReadOnly).unwrap();
    assert!(Arc::ptr_eq(&read_only, &again));
    let writing =
        CausalDecisionStore::open_shared(temp.path(), Retention::days(1, 1), sink(), StoreMode::ReadWrite).unwrap();
    assert_eq!(writing.mode(), StoreMode::ReadWrite);
    let shared =
        CausalDecisionStore::open_shared(temp.path(), Retention::days(1, 1), sink(), StoreMode::ReadOnly).unwrap();
    assert!(
        Arc::ptr_eq(&writing, &shared),
        "the writer serves read-only requests too"
    );
}

fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().to_string(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

/// D-091 F-1: a symlinked store directory is refused before anything is
/// created or changed through it.
#[test]
fn a_symlinked_store_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::TempDir::new().unwrap();
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("self")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, workspace.join("self/causal_tree")).unwrap();
    for mode in [StoreMode::ReadWrite, StoreMode::ReadOnly] {
        let opened = CausalDecisionStore::open(
            &workspace,
            Retention::days(30, 30),
            sink(),
            mode,
            WriterSetup::default(),
        );
        assert!(matches!(opened, Err(StoreError::Unsafe(_))), "{mode:?}");
    }
    assert!(
        std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing created through the link"
    );
    let mode = std::fs::metadata(&elsewhere).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "the target is not chmodded");
}

/// D-091 F-1: a symlinked database or side file is refused; the link's
/// target keeps its bytes and mode.
#[test]
fn a_symlinked_database_or_side_file_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    for name in ["decisions.db", "decisions.db-wal", "decisions.db-shm"] {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("target-file");
        std::fs::write(&target, b"not a database, a file that must stay as it is").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let dir = crate::decision::key::prepare_decision_dir(&temp.path().join("ws")).unwrap();
        std::os::unix::fs::symlink(&target, dir.join(name)).unwrap();
        for mode in [StoreMode::ReadWrite, StoreMode::ReadOnly] {
            let opened = CausalDecisionStore::open(
                &temp.path().join("ws"),
                Retention::days(30, 30),
                sink(),
                mode,
                WriterSetup::default(),
            );
            assert!(matches!(opened, Err(StoreError::Unsafe(_))), "{name} {mode:?}");
        }
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"not a database, a file that must stay as it is"
        );
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
    }
}

/// D-091 F-5: the disabled plane's reads of an existing database leave
/// every file of the directory as it was, and create no -wal or -shm.
#[tokio::test]
async fn a_read_only_open_of_an_existing_store_changes_no_file() {
    let temp = tempfile::TempDir::new().unwrap();
    {
        let store = open_private(temp.path(), sink());
        store
            .writer()
            .unwrap()
            .record_decision(record(id(1), "ws", Some("a"), now_ms()));
        flushed(&store).await;
    }
    // The writer thread closes its connection (a checkpoint) on its own
    // time; wait until the side files are gone.
    let dir = temp.path().join("self/causal_tree");
    for _ in 0..200 {
        if !dir.join("decisions.db-wal").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let before = listing(&dir);
    let store = CausalDecisionStore::open(
        temp.path(),
        Retention::days(30, 30),
        sink(),
        StoreMode::ReadOnly,
        WriterSetup::default(),
    )
    .unwrap();
    assert!(store.reader().get(&operator(&store), &id(1)).unwrap().is_some());
    assert_eq!(store.reader().counts().unwrap().decisions, 1);
    drop(store);
    assert_eq!(listing(&dir), before, "no file changed or appeared");
}

/// D-091 F-17: a newer database, with frames still in its WAL, is never
/// opened for writing: neither the database nor the WAL changes.
#[tokio::test]
async fn a_newer_schema_with_a_live_wal_is_untouched() {
    let source = tempfile::TempDir::new().unwrap();
    {
        let store = open_private(source.path(), sink());
        flushed(&store).await;
    }
    let path = db_path(source.path());
    // Copy a newer database whose WAL still holds frames, with no
    // connection left open on the copy: a read-write open would
    // checkpoint it on close.
    let copy = tempfile::TempDir::new().unwrap();
    let copied = crate::decision::key::prepare_decision_dir(copy.path())
        .unwrap()
        .join(DB_FILE);
    {
        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("PRAGMA wal_autocheckpoint = 0;").unwrap();
        holder
            .pragma_update(None, "user_version", store_schema::SCHEMA_VERSION + 1)
            .unwrap();
        for suffix in ["", "-wal"] {
            std::fs::copy(side_file(&path, suffix), side_file(&copied, suffix)).unwrap();
        }
    }
    let wal = side_file(&copied, "-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0, "the WAL holds frames");
    let before = (std::fs::read(&copied).unwrap(), std::fs::read(&wal).unwrap());
    let store = open_private(copy.path(), sink());
    assert_eq!(store.mode(), StoreMode::ReadOnly);
    drop(store);
    assert_eq!((std::fs::read(&copied).unwrap(), std::fs::read(&wal).unwrap()), before);
}

/// D-091 F-7: a horizon of 0 days keeps everything.
#[test]
fn a_zero_retention_keeps_everything() {
    assert_eq!(Retention::days(0, 0).cutoff(now_ms()), None);
    assert!(Retention::days(1, 0).cutoff(now_ms()).is_some());
}

/// D-091 F-12: a quarantine name that is taken is refused, never
/// overwritten, and the database stays where it was.
#[test]
fn a_taken_quarantine_name_is_never_overwritten() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = crate::decision::key::prepare_decision_dir(temp.path()).unwrap();
    let path = dir.join(DB_FILE);
    std::fs::write(&path, b"first garbage").unwrap();
    quarantine_as(&path, "fixed").unwrap();
    std::fs::write(&path, b"second garbage").unwrap();
    assert!(quarantine_as(&path, "fixed").is_err());
    assert_eq!(
        std::fs::read(dir.join("decisions.db.corrupt-fixed")).unwrap(),
        b"first garbage"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"second garbage");
}

/// D-091 F-12: two recoveries get two names.
#[test]
fn quarantined_files_are_never_overwritten() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = crate::decision::key::prepare_decision_dir(temp.path()).unwrap();
    let path = dir.join(DB_FILE);
    for garbage in [b"first garbage".as_slice(), b"second garbage".as_slice()] {
        std::fs::write(&path, garbage).unwrap();
        quarantine(&path).unwrap();
    }
    let mut aside: Vec<Vec<u8>> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("decisions.db.corrupt-"))
        .map(|entry| std::fs::read(entry.path()).unwrap())
        .collect();
    aside.sort();
    assert_eq!(aside, [b"first garbage".to_vec(), b"second garbage".to_vec()]);
}

/// D-091 F-13: the scope key is the store's own id: it survives a move
/// of the workspace, and every record carries it whatever the caller
/// set.
#[tokio::test]
async fn the_workspace_id_is_stable_across_a_move() {
    let temp = tempfile::TempDir::new().unwrap();
    let first = temp.path().join("first");
    let id_before = {
        let store = open_private(&first, sink());
        store
            .writer()
            .unwrap()
            .record_decision(record(id(1), "a-caller-supplied-id", Some("a"), now_ms()));
        flushed(&store).await;
        store.workspace_id().unwrap()
    };
    let moved = temp.path().join("moved");
    std::fs::rename(&first, &moved).unwrap();
    let store = open_private(&moved, sink());
    assert_eq!(store.workspace_id().unwrap(), id_before);
    assert!(store.reader().get(&operator(&store), &id(1)).unwrap().is_some());
}

/// D-091 F-11: a file's progress is saved only when every decision it
/// lists is stored.
#[tokio::test]
async fn import_progress_needs_every_decision_stored() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = open_private(temp.path(), sink());
    let writer = store.writer().unwrap();
    writer.record_decision(record(id(1), "ws", None, now_ms()));
    let progress = |file: &str| ImportProgress {
        file: file.to_string(),
        digest: "d".to_string(),
        lines: 1,
    };
    writer.mark_imported(progress("stored.jsonl"), vec![id(1)]);
    writer.mark_imported(progress("missing.jsonl"), vec![id(1), id(2)]);
    flushed(&store).await;
    assert!(store.reader().import_progress("stored.jsonl").unwrap().is_some());
    assert!(store.reader().import_progress("missing.jsonl").unwrap().is_none());
}

/// D-093 N-2: with a live WAL (a writer that has not checkpointed), the
/// disabled plane's read sees the latest rows and creates no file.
#[tokio::test]
async fn a_read_only_open_sees_rows_in_a_live_wal() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = open_private(temp.path(), sink());
    store
        .writer()
        .unwrap()
        .record_decision(record(id(1), "ws", Some("a"), now_ms()));
    flushed(&store).await;
    let dir = temp.path().join("self/causal_tree");
    assert!(
        std::fs::metadata(dir.join("decisions.db-wal")).unwrap().len() > 0,
        "a live WAL"
    );
    let before: Vec<String> = listing(&dir).into_iter().map(|(name, _)| name).collect();
    let reader = CausalDecisionStore::open(
        temp.path(),
        Retention::days(30, 30),
        sink(),
        StoreMode::ReadOnly,
        WriterSetup::default(),
    )
    .unwrap();
    assert!(reader.reader().get(&operator(&reader), &id(1)).unwrap().is_some());
    let after: Vec<String> = listing(&dir).into_iter().map(|(name, _)| name).collect();
    assert_eq!(after, before, "no file appeared");
    drop(store);
}

/// D-093 N-6: the disabled plane's read never narrows a directory mode.
#[tokio::test]
async fn a_read_only_open_changes_no_directory_mode() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::TempDir::new().unwrap();
    {
        let store = open_private(temp.path(), sink());
        flushed(&store).await;
    }
    let dir = temp.path().join("self/causal_tree");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
    let store = CausalDecisionStore::open(
        temp.path(),
        Retention::days(30, 30),
        sink(),
        StoreMode::ReadOnly,
        WriterSetup::default(),
    )
    .unwrap();
    assert_eq!(store.reader().counts().unwrap().decisions, 0);
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o750);
}

/// R-I1-phase2 R-2: concurrent first opens of one fresh database all
/// succeed within the bounded wait. Switching to WAL upgrades the
/// pragma's read to a write, and SQLite skips the busy handler for that
/// upgrade, so without a retry one of them fails at once with "database
/// is locked".
#[test]
fn concurrent_first_opens_wait_for_the_wal_switch() {
    const OPENERS: usize = 8;
    for round in 0..60 {
        let temp = tempfile::TempDir::new().unwrap();
        // The writer never follows a symlink (`/var` is one on macOS).
        let path = temp.path().canonicalize().unwrap().join(DB_FILE);
        let barrier = Arc::new(std::sync::Barrier::new(OPENERS));
        let started = Instant::now();
        let handles: Vec<_> = (0..OPENERS)
            .map(|_| {
                let (path, barrier) = (path.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    open_writer(&path)
                        .map(|(_, state)| state)
                        .map_err(|error| error.to_string())
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), Ok(SchemaState::Current), "round {round}");
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "round {round}: the wait is bounded"
        );
    }
}
