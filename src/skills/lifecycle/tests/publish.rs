use super::{entry, md, owner};
use crate::skills::lifecycle::publish::crash_hooks;
use crate::skills::lifecycle::{
    ActiveBinding, Bundle, LifecycleError, NewRevision, PublishRequest, RevisionId, RollbackRequest, SkillId,
    SkillLifecycleStore, SourceKind, TransactionId, TransactionKind, TransactionState,
};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::Duration;
use tempfile::TempDir;

const OWNER: &str = "owner:ws:cli:alice";

type Rev = (RevisionId, String);

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

fn open(dir: &Path) -> SkillLifecycleStore {
    SkillLifecycleStore::open(dir, owner(OWNER)).expect("test: store opens")
}

fn setup() -> (TempDir, SkillLifecycleStore) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = open(tmp.path());
    (tmp, store)
}

fn tx(name: &str) -> TransactionId {
    TransactionId::parse(name).expect("test: transaction id")
}

/// Writes a revision whose body differs per `tag`; returns id and record digest.
fn write(store: &SkillLifecycleStore, tag: &str) -> Rev {
    let bundle =
        Bundle::from_entries(vec![entry("SKILL.md", md("Deploy helper", tag).as_bytes())]).expect("test: bundle");
    let outcome = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write revision");
    (outcome.revision.revision_id, outcome.record_digest)
}

fn request(name: &str, rev: &Rev, expected: u64) -> PublishRequest {
    PublishRequest {
        transaction_id: tx(name),
        skill_id: skill(),
        candidate: rev.0.clone(),
        candidate_record_digest: rev.1.clone(),
        expected_generation: expected,
    }
}

fn rollback_request(name: &str, target: &RevisionId, expected: u64) -> RollbackRequest {
    RollbackRequest {
        transaction_id: tx(name),
        skill_id: skill(),
        target: target.clone(),
        expected_generation: expected,
    }
}

fn binding(store: &SkillLifecycleStore) -> Option<ActiveBinding> {
    store.active_binding(&skill()).expect("test: binding readable")
}

fn staging_count(store: &SkillLifecycleStore) -> usize {
    fs::read_dir(store.root().join("staging"))
        .expect("test: staging")
        .count()
}

#[test]
fn first_publish_sets_generation_one_and_journals_the_pinned_digest() {
    let (_tmp, store) = setup();
    let a = write(&store, "a");
    let outcome = store.publish(&request("t1", &a, 0)).expect("test: publish");
    assert!(!outcome.replayed);
    assert_eq!(outcome.entry.state, TransactionState::Committed);
    assert_eq!(outcome.entry.kind, TransactionKind::Publish);
    assert_eq!(outcome.entry.candidate_record_digest, a.1);
    assert_eq!(outcome.entry.predecessor_revision_id, None);
    let b = binding(&store).expect("test: bound");
    assert_eq!((b.generation, &b.revision_id, &b.record_digest), (1, &a.0, &a.1));
    assert_eq!(b.transaction_id, tx("t1"));
    let stored = store
        .journal_entry(&tx("t1"))
        .expect("test: read")
        .expect("test: present");
    assert_eq!(stored, outcome.entry);
}

#[test]
fn stale_expected_generation_is_refused_without_side_effects() {
    let (_tmp, store) = setup();
    let (a, b) = (write(&store, "a"), write(&store, "b"));
    store.publish(&request("t1", &a, 0)).expect("test: publish a");
    let err = store.publish(&request("t2", &b, 0)).expect_err("test: stale writer");
    assert!(
        matches!(
            err,
            LifecycleError::StaleGeneration {
                expected: 0,
                actual: 1,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(binding(&store).expect("test: bound").revision_id, a.0);
    assert!(store.journal_entry(&tx("t2")).expect("test: read").is_none());
    store.publish(&request("t3", &b, 1)).expect("test: publish b");
    assert_eq!(binding(&store).expect("test: bound").generation, 2);
}

#[test]
fn candidate_with_a_wrong_pinned_digest_is_refused() {
    let (_tmp, store) = setup();
    let (id, digest) = write(&store, "a");
    let mut forged = digest;
    forged.replace_range(0..1, if forged.starts_with('0') { "1" } else { "0" });
    let err = store
        .publish(&request("t1", &(id, forged), 0))
        .expect_err("test: digest mismatch");
    assert!(matches!(err, LifecycleError::Tampered { .. }), "{err}");
    assert!(binding(&store).is_none());
    assert!(store.journal_entry(&tx("t1")).expect("test: read").is_none());
}

#[test]
fn candidate_that_does_not_exist_for_the_skill_is_refused() {
    let (_tmp, store) = setup();
    let rev = write(&store, "a");
    let mut req = request("t1", &rev, 0);
    req.skill_id = SkillId::parse("other-skill").expect("test: skill id");
    let err = store.publish(&req).expect_err("test: wrong skill");
    assert!(matches!(err, LifecycleError::RevisionNotFound(_)), "{err}");
}

#[test]
fn duplicate_transaction_is_idempotent_and_reused_ids_with_other_content_conflict() {
    let (_tmp, store) = setup();
    let (a, b) = (write(&store, "a"), write(&store, "b"));
    store.publish(&request("t1", &a, 0)).expect("test: publish");
    let again = store.publish(&request("t1", &a, 0)).expect("test: replay");
    assert!(again.replayed);
    assert_eq!(binding(&store).expect("test: bound").generation, 1);
    let err = store.publish(&request("t1", &b, 0)).expect_err("test: reused id");
    assert!(matches!(err, LifecycleError::TransactionConflict { .. }), "{err}");
    let err = store
        .rollback(&rollback_request("t1", &a.0, 0))
        .expect_err("test: reused id with other kind");
    assert!(matches!(err, LifecycleError::TransactionConflict { .. }), "{err}");
}

#[test]
fn republishing_the_active_revision_is_refused() {
    let (_tmp, store) = setup();
    let a = write(&store, "a");
    store.publish(&request("t1", &a, 0)).expect("test: publish");
    let err = store.publish(&request("t2", &a, 1)).expect_err("test: already active");
    assert!(matches!(err, LifecycleError::RevisionAlreadyActive(_)), "{err}");
}

#[test]
fn threads_racing_on_one_generation_have_exactly_one_winner() {
    let tmp = TempDir::new().expect("test: tempdir");
    let setup_store = open(tmp.path());
    let revisions: Vec<Rev> = (0..8).map(|i| write(&setup_store, &format!("candidate {i}"))).collect();
    let barrier = Arc::new(Barrier::new(revisions.len()));
    let mut handles = Vec::new();
    for (i, rev) in revisions.into_iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        let path = tmp.path().to_path_buf();
        handles.push(std::thread::spawn(move || {
            let store = open(&path);
            barrier.wait();
            store.publish(&request(&format!("race-{i}"), &rev, 0))
        }));
    }
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.join().expect("test: thread"));
    }
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert!(
            matches!(
                r,
                LifecycleError::StaleGeneration {
                    expected: 0,
                    actual: 1,
                    ..
                }
            ),
            "{r}"
        );
    }
    assert_eq!(binding(&setup_store).expect("test: bound").generation, 1);
    let committed = setup_store
        .journal_entries()
        .expect("test: journal")
        .into_iter()
        .filter(|e| e.state == TransactionState::Committed)
        .count();
    assert_eq!(committed, 1);
}

const RACE_ENV: &str = "PRX_SL08_RACE_DIR";
const RACE_INDEX_ENV: &str = "PRX_SL08_RACE_INDEX";

/// Entry point of the child processes of the process race test; a no-op when
/// run as a normal test.
#[test]
fn race_child_entry() {
    let (Ok(dir), Ok(index)) = (std::env::var(RACE_ENV), std::env::var(RACE_INDEX_ENV)) else {
        return;
    };
    let dir = Path::new(&dir);
    let store = open(&dir.join("workspace"));
    let rev = write(&store, &format!("process candidate {index}"));
    fs::write(dir.join(format!("ready-{index}")), b"").expect("test: ready marker");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !dir.join("go").exists() {
        assert!(std::time::Instant::now() < deadline, "test: go signal never came");
        std::thread::sleep(Duration::from_millis(2));
    }
    let result = match store.publish(&request(&format!("proc-{index}"), &rev, 0)) {
        Ok(_) => "won",
        Err(LifecycleError::StaleGeneration { .. }) => "stale",
        Err(e) => panic!("test: unexpected error {e}"),
    };
    fs::write(dir.join(format!("result-{index}")), result).expect("test: result");
}

#[test]
fn processes_racing_on_one_generation_have_exactly_one_winner() {
    let tmp = TempDir::new().expect("test: tempdir");
    let workspace = tmp.path().join("workspace");
    fs::create_dir(&workspace).expect("test: workspace");
    let store = open(&workspace);
    let exe = std::env::current_exe().expect("test: current exe");
    let mut children: Vec<_> = (0..2)
        .map(|i| {
            std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "skills::lifecycle::tests::publish::race_child_entry",
                    "--test-threads=1",
                ])
                .env(RACE_ENV, tmp.path())
                .env(RACE_INDEX_ENV, i.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("test: spawn child")
        })
        .collect();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !(0..2).all(|i| tmp.path().join(format!("ready-{i}")).exists()) {
        assert!(
            std::time::Instant::now() < deadline,
            "test: children never became ready"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::write(tmp.path().join("go"), b"").expect("test: go");
    for child in &mut children {
        assert!(child.wait().expect("test: wait").success());
    }
    let mut results: Vec<String> = (0..2)
        .map(|i| fs::read_to_string(tmp.path().join(format!("result-{i}"))).expect("test: result"))
        .collect();
    results.sort();
    assert_eq!(results, ["stale", "won"]);
    assert_eq!(binding(&store).expect("test: bound").generation, 1);
}

#[test]
fn lock_is_exclusive_and_times_out() {
    let (_tmp, store) = setup();
    store.ensure_publication_dirs().expect("test: dirs");
    let held = store.lock_skill(&skill(), Duration::from_secs(1)).expect("test: lock");
    let err = store
        .lock_skill(&skill(), Duration::from_millis(50))
        .expect_err("test: contended");
    assert!(matches!(err, LifecycleError::LockTimeout { .. }), "{err}");
    drop(held);
    store
        .lock_skill(&skill(), Duration::from_secs(1))
        .expect("test: free again");
}

#[test]
fn rollback_reactivates_an_older_revision_under_a_new_generation() {
    let (_tmp, store) = setup();
    let (a, b) = (write(&store, "a"), write(&store, "b"));
    store.publish(&request("t1", &a, 0)).expect("test: publish a");
    store.publish(&request("t2", &b, 1)).expect("test: publish b");
    let out = store
        .rollback(&rollback_request("t3", &a.0, 2))
        .expect("test: rollback");
    assert_eq!(out.entry.kind, TransactionKind::Rollback);
    assert_eq!(out.entry.predecessor_revision_id.as_ref(), Some(&b.0));
    let bound = binding(&store).expect("test: bound");
    assert_eq!(
        (bound.generation, &bound.revision_id, &bound.record_digest),
        (3, &a.0, &a.1)
    );
    // History is intact and the rolled-back revision stays readable for tasks
    // that pinned it.
    assert_eq!(store.journal_entries().expect("test: journal").len(), 3);
    store
        .read_revision(&skill(), &b.0, Some(&b.1))
        .expect("test: pinned revision still readable");
    store
        .rollback(&rollback_request("t4", &b.0, 3))
        .expect("test: back to b");
    assert_eq!(binding(&store).expect("test: bound").generation, 4);
}

#[test]
fn rollback_refuses_unknown_active_and_stale_targets() {
    let (_tmp, store) = setup();
    let (a, b, never) = (write(&store, "a"), write(&store, "b"), write(&store, "never published"));
    store.publish(&request("t1", &a, 0)).expect("test: publish a");
    store.publish(&request("t2", &b, 1)).expect("test: publish b");
    let err = store
        .rollback(&rollback_request("r1", &never.0, 2))
        .expect_err("test: never published");
    assert!(matches!(err, LifecycleError::RollbackTargetUnknown(_)), "{err}");
    let err = store
        .rollback(&rollback_request("r2", &b.0, 2))
        .expect_err("test: active");
    assert!(matches!(err, LifecycleError::RevisionAlreadyActive(_)), "{err}");
    let err = store
        .rollback(&rollback_request("r3", &a.0, 1))
        .expect_err("test: stale");
    assert!(
        matches!(
            err,
            LifecycleError::StaleGeneration {
                expected: 1,
                actual: 2,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(binding(&store).expect("test: bound").revision_id, b.0);
}

#[test]
fn rollback_detects_a_tampered_target_through_the_journal_pin() {
    let (_tmp, store) = setup();
    let (a, b) = (write(&store, "a"), write(&store, "b"));
    store.publish(&request("t1", &a, 0)).expect("test: publish a");
    store.publish(&request("t2", &b, 1)).expect("test: publish b");
    // Change a field that is not derived from the bundle (provenance); only
    // the pinned record digest can notice that.
    let record = store
        .root()
        .join("revisions")
        .join(skill().as_str())
        .join(a.0.as_str())
        .join("revision.json");
    let text = fs::read_to_string(&record).expect("test: read record");
    fs::write(&record, text.replace("\"operator\"", "\"learned\"")).expect("test: tamper");
    let err = store
        .rollback(&rollback_request("t3", &a.0, 2))
        .expect_err("test: tampered");
    assert!(matches!(err, LifecycleError::Tampered { .. }), "{err}");
    assert_eq!(binding(&store).expect("test: bound").revision_id, b.0);
}

/// Publishes `a`, then crashes at `point` while publishing `b`; returns the
/// store reopened as a restarted process would see it.
fn crash_during_publish(point: &'static str) -> (TempDir, SkillLifecycleStore, Rev, Rev) {
    let (tmp, store) = setup();
    let (a, b) = (write(&store, "a"), write(&store, "b"));
    store.publish(&request("t1", &a, 0)).expect("test: publish a");
    crash_hooks::arm(point);
    let err = store.publish(&request("t2", &b, 1)).expect_err("test: injected crash");
    assert!(err.to_string().contains("injected crash point"), "{err}");
    drop(store);
    let restarted = open(tmp.path());
    (tmp, restarted, a, b)
}

#[test]
fn crash_after_prepared_recovers_to_the_old_generation() {
    let (_tmp, store, a, b) = crash_during_publish("after_prepared");
    let before = store
        .journal_entry(&tx("t2"))
        .expect("test: read")
        .expect("test: present");
    assert_eq!(before.state, TransactionState::Prepared);
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.aborted, [tx("t2")]);
    assert_eq!(report.committed.len(), 0);
    let bound = binding(&store).expect("test: bound");
    assert_eq!((bound.generation, &bound.revision_id), (1, &a.0));
    // The aborted candidate is not silently published by retrying the same
    // transaction; a fresh transaction id succeeds.
    let err = store.publish(&request("t2", &b, 1)).expect_err("test: aborted tx");
    assert!(
        matches!(err, LifecycleError::TransactionNotCommitted { state: "aborted", .. }),
        "{err}"
    );
    store.publish(&request("t5", &b, 1)).expect("test: fresh transaction");
    assert_eq!(binding(&store).expect("test: bound").revision_id, b.0);
}

#[test]
fn crash_with_a_staged_binding_leaves_the_old_binding_and_sweepable_staging() {
    let (_tmp, store, a, _b) = crash_during_publish("binding_staged");
    assert_eq!(staging_count(&store), 1, "staged binding temp file");
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.aborted, [tx("t2")]);
    assert_eq!(report.swept_staging, 1);
    assert_eq!(staging_count(&store), 0);
    let bound = binding(&store).expect("test: bound");
    assert_eq!((bound.generation, &bound.revision_id), (1, &a.0));
}

#[test]
fn crash_after_the_binding_swap_is_rolled_forward() {
    let (_tmp, store, _a, b) = crash_during_publish("after_binding");
    assert_eq!(binding(&store).expect("test: bound").revision_id, b.0);
    let before = store
        .journal_entry(&tx("t2"))
        .expect("test: read")
        .expect("test: present");
    assert_eq!(before.state, TransactionState::Prepared);
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.committed, [tx("t2")]);
    let after = store
        .journal_entry(&tx("t2"))
        .expect("test: read")
        .expect("test: present");
    assert_eq!(after.state, TransactionState::Committed);
    let bound = binding(&store).expect("test: bound");
    assert_eq!((bound.generation, bound.transaction_id), (2, tx("t2")));
    assert!(store.publish(&request("t2", &b, 1)).expect("test: replay").replayed);
}

#[test]
fn retry_of_a_crashed_transaction_reconciles_without_explicit_recovery() {
    let (_tmp, store, _a, b) = crash_during_publish("after_binding");
    let out = store.publish(&request("t2", &b, 1)).expect("test: replay after crash");
    assert!(out.replayed);
    assert_eq!(out.entry.state, TransactionState::Committed);
}

#[test]
fn crash_during_a_rollback_recovers_like_a_publish() {
    let (_tmp, store) = setup();
    let (a, b) = (write(&store, "a"), write(&store, "b"));
    store.publish(&request("t1", &a, 0)).expect("test: publish a");
    store.publish(&request("t2", &b, 1)).expect("test: publish b");
    crash_hooks::arm("after_prepared");
    store
        .rollback(&rollback_request("t3", &a.0, 2))
        .expect_err("test: crash");
    store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(binding(&store).expect("test: bound").revision_id, b.0);
    crash_hooks::arm("after_binding");
    store
        .rollback(&rollback_request("t4", &a.0, 2))
        .expect_err("test: crash");
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.committed, [tx("t4")]);
    let bound = binding(&store).expect("test: bound");
    assert_eq!((bound.generation, bound.revision_id), (3, a.0));
}

#[test]
fn recovery_sweeps_leftover_sl01_staging_directories_but_honours_min_age() {
    let (_tmp, store) = setup();
    let left = store.root().join("staging").join("half-written-revision");
    fs::create_dir(&left).expect("test: leftover");
    let report = store.recover(Duration::from_secs(3600)).expect("test: recover");
    assert_eq!(report.swept_staging, 0);
    assert!(left.exists());
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.swept_staging, 1);
    assert!(!left.exists());
}

#[test]
fn recovery_changes_nothing_on_a_clean_store() {
    let (_tmp, store) = setup();
    let a = write(&store, "a");
    store.publish(&request("t1", &a, 0)).expect("test: publish");
    assert_eq!(
        store.recover(Duration::ZERO).expect("test: recover"),
        Default::default()
    );
    assert_eq!(binding(&store).expect("test: bound").generation, 1);
}

#[test]
fn a_binding_that_belongs_to_another_skill_or_owner_is_rejected() {
    let (tmp, store) = setup();
    let a = write(&store, "a");
    store.publish(&request("t1", &a, 0)).expect("test: publish");
    let path = store.root().join("active").join("deploy-helper.json");
    let text = fs::read_to_string(&path).expect("test: read");
    fs::write(
        &path,
        text.replace("\"skill_id\": \"deploy-helper\"", "\"skill_id\": \"someone-else\""),
    )
    .expect("test: tamper");
    let err = store.active_binding(&skill()).expect_err("test: wrong skill");
    assert!(matches!(err, LifecycleError::Tampered { .. }), "{err}");
    fs::write(&path, text.replace(OWNER, "owner:ws:cli:bob")).expect("test: tamper");
    let err = open(tmp.path())
        .active_binding(&skill())
        .expect_err("test: wrong owner");
    assert!(matches!(err, LifecycleError::OwnerMismatch { .. }), "{err}");
}

#[test]
fn a_damaged_journal_entry_fails_loudly() {
    let (_tmp, store) = setup();
    let a = write(&store, "a");
    store.publish(&request("t1", &a, 0)).expect("test: publish");
    let path = store.root().join("publication").join("t1.json");
    let text = fs::read_to_string(&path).expect("test: read");
    fs::write(&path, text.replace("\"new_generation\": 1", "\"new_generation\": 7")).expect("test: tamper");
    let err = store.journal_entries().expect_err("test: damaged");
    assert!(matches!(err, LifecycleError::InvalidRecord { .. }), "{err}");
}

#[test]
fn transaction_ids_are_validated() {
    let too_long = "x".repeat(65);
    for bad in ["", "a/b", "..", "a b", too_long.as_str()] {
        assert!(TransactionId::parse(bad).is_err(), "{bad:?}");
    }
    assert!(TransactionId::parse("ok-ID_1").is_ok());
    assert!(TransactionId::parse(TransactionId::generate().as_str()).is_ok());
}
