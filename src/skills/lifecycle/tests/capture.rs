#![cfg(test)]

use super::{activate, entry, md, owner};
use crate::skills::lifecycle::{
    Bundle, Correlation, ExperienceOutcome, ExperienceRetention, LifecycleError, MAX_HYDRATION_RECEIPTS,
    MAX_SELECTION_RECEIPTS, ManagedReadRequest, NewRevision, PinnedSkillRevision, SkillId, SkillLifecycleStore,
    SourceKind, TaskTerminal, experience_id, on_task_terminal, read_managed_resource, task_key, task_pin,
};
use std::fs;
use tempfile::TempDir;

const OWNER: &str = "owner:ws:cli:alice";

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

fn setup() -> (TempDir, SkillLifecycleStore, PinnedSkillRevision) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let bundle = Bundle::from_entries(vec![
        entry("SKILL.md", md("Deploy helper", "Helps with deploys").as_bytes()),
        entry("references/guide.md", b"guide body"),
    ])
    .expect("test: bundle");
    let written = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write");
    activate(
        &store,
        &skill(),
        &(written.revision.revision_id, written.record_digest),
        0,
        true,
    );
    let pin = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    (tmp, store, pin)
}

fn corr(run: &str, segment: &str, child: &str) -> Correlation {
    Correlation {
        run_id: Some(run.to_string()),
        segment_id: Some(segment.to_string()),
        child_id: Some(child.to_string()),
        ..Correlation::default()
    }
}

#[test]
fn selection_alone_does_not_credit_use_and_hydration_does() {
    let (_tmp, store, pin) = setup();
    assert!(store.read_experience("task-1").expect("test: read").is_none());
    let selected = store
        .record_selection("task-1", &pin, &Correlation::default())
        .expect("test: select");
    assert_eq!(selected.outcome, ExperienceOutcome::Incomplete);
    assert!(
        selected.used_skills().is_empty(),
        "a selected but never loaded skill is not used"
    );

    let hydrated = store
        .record_hydration("task-1", &pin, "SKILL.md", b"exact bytes", &Correlation::default())
        .expect("test: hydrate");
    assert_eq!(hydrated.used_skills(), vec![&skill()]);
    let receipt = hydrated.hydration_receipts.first().expect("test: receipt");
    assert_eq!((receipt.bytes, receipt.generation), (11, 1));
    assert_eq!(
        receipt.resource_sha256,
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"exact bytes"))
    );
    assert_eq!(receipt.revision_id, pin.revision_id);
}

#[test]
fn a_task_without_a_terminal_stays_incomplete_and_is_not_learning_eligible() {
    let (_tmp, store, pin) = setup();
    store
        .record_hydration("task-open", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    let record = store
        .read_experience("task-open")
        .expect("test: read")
        .expect("test: present");
    assert_eq!(record.outcome, ExperienceOutcome::Incomplete);
    assert!(record.terminal.is_none() && !record.learning_eligible());
}

#[test]
fn terminal_join_is_idempotent_and_never_rewrites_the_outcome() {
    let (_tmp, store, pin) = setup();
    store
        .record_hydration("task-t", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    let first = store
        .join_terminal("task-t", "term-1", ExperienceOutcome::Cancelled)
        .expect("test: join");
    assert!(!first.replayed);
    assert!(first.experience.learning_eligible());
    let again = store
        .join_terminal("task-t", "term-1", ExperienceOutcome::Cancelled)
        .expect("test: replay");
    assert!(again.replayed);
    assert_eq!(again.experience.terminal, first.experience.terminal);

    for (id, outcome) in [
        ("term-1", ExperienceOutcome::Success),
        ("term-2", ExperienceOutcome::Cancelled),
    ] {
        let err = store
            .join_terminal("task-t", id, outcome)
            .expect_err("test: conflicting terminal");
        assert!(matches!(err, LifecycleError::TerminalConflict { .. }), "{err}");
    }
    let stored = store
        .read_experience("task-t")
        .expect("test: read")
        .expect("test: present");
    assert_eq!(
        stored.outcome,
        ExperienceOutcome::Cancelled,
        "a cancellation never becomes a success"
    );
    assert!(
        store
            .join_terminal("task-t", "term-3", ExperienceOutcome::Incomplete)
            .is_err()
    );
}

#[test]
fn late_and_overflowing_evidence_blocks_learning_claims() {
    let (_tmp, store, pin) = setup();
    store
        .record_hydration("task-late", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    store
        .join_terminal("task-late", "term", ExperienceOutcome::Success)
        .expect("test: join");
    let late = store
        .record_hydration("task-late", &pin, "references/guide.md", b"y", &Correlation::default())
        .expect("test: late evidence is recorded as a count");
    assert_eq!((late.hydration_receipts.len(), late.late_evidence), (1, 1));
    assert!(!late.learning_eligible());

    for _ in 0..=MAX_SELECTION_RECEIPTS {
        store
            .record_selection("task-sel", &pin, &Correlation::default())
            .expect("test: select");
    }
    let selected = store
        .read_experience("task-sel")
        .expect("test: read")
        .expect("test: present");
    assert_eq!(
        (selected.selection_receipts.len(), selected.dropped_evidence),
        (MAX_SELECTION_RECEIPTS, 1)
    );
    for _ in 0..=MAX_HYDRATION_RECEIPTS {
        store
            .record_hydration("task-hyd", &pin, "SKILL.md", b"x", &Correlation::default())
            .expect("test: hydrate");
    }
    store
        .join_terminal("task-hyd", "term", ExperienceOutcome::Success)
        .expect("test: join");
    let hydrated = store
        .read_experience("task-hyd")
        .expect("test: read")
        .expect("test: present");
    assert_eq!(
        (hydrated.hydration_receipts.len(), hydrated.dropped_evidence),
        (MAX_HYDRATION_RECEIPTS, 1)
    );
    assert!(
        !hydrated.learning_eligible(),
        "dropped evidence disables learning claims"
    );
}

#[test]
fn run_segment_and_child_identities_stay_separate_per_receipt() {
    let (_tmp, store, pin) = setup();
    store
        .record_hydration("task-c", &pin, "SKILL.md", b"x", &corr("run-1", "seg-1", "child-1"))
        .expect("test: hydrate");
    let record = store
        .record_hydration(
            "task-c",
            &pin,
            "SKILL.md",
            b"x",
            &Correlation {
                run_id: Some("run-2".into()),
                ..Correlation::default()
            },
        )
        .expect("test: hydrate");
    let ids: Vec<_> = record
        .hydration_receipts
        .iter()
        .map(|r| r.correlation.clone())
        .collect();
    assert_eq!(ids.first(), Some(&corr("run-1", "seg-1", "child-1")));
    assert_eq!(
        ids.get(1)
            .map(|c| (c.run_id.as_deref(), c.segment_id.as_deref(), c.child_id.as_deref())),
        Some((Some("run-2"), None, None))
    );
    assert_eq!(record.task_id, "task-c");
    assert!(
        store
            .record_hydration(
                "task-c",
                &pin,
                "SKILL.md",
                b"x",
                &Correlation {
                    child_id: Some("bad\nid".into()),
                    ..Correlation::default()
                }
            )
            .is_err()
    );
}

#[test]
fn concurrent_captures_are_all_kept() {
    let (_tmp, store, pin) = setup();
    std::thread::scope(|scope| {
        for index in 0..8 {
            let (store, pin) = (&store, &pin);
            scope.spawn(move || {
                store
                    .record_hydration(
                        "task-par",
                        pin,
                        &format!("references/r{index}.md"),
                        b"x",
                        &Correlation::default(),
                    )
                    .expect("test: concurrent hydrate");
            });
        }
    });
    let record = store
        .read_experience("task-par")
        .expect("test: read")
        .expect("test: present");
    assert_eq!(record.hydration_receipts.len(), 8);
}

#[test]
fn tampered_experiences_are_refused() {
    let (_tmp, store, pin) = setup();
    store
        .record_hydration("task-x", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    let path = store
        .root()
        .join("experiences")
        .join(format!("{}.json", experience_id(store.owner_scope(), "task-x")));
    let text = fs::read_to_string(&path).expect("test: record");
    fs::write(&path, text.replace(OWNER, "owner:ws:cli:mallory")).expect("test: forge owner");
    assert!(matches!(
        store.read_experience("task-x").expect_err("test: owner"),
        LifecycleError::OwnerMismatch { .. }
    ));
    fs::write(
        &path,
        text.replace("\"task_id\": \"task-x\"", "\"task_id\": \"task-y\""),
    )
    .expect("test: forge task");
    assert!(matches!(
        store.read_experience("task-x").expect_err("test: task"),
        LifecycleError::Tampered { .. }
    ));
}

#[test]
fn the_task_terminal_call_in_joins_and_always_releases_pins() {
    let (tmp, store, pin) = setup();
    let who = owner(OWNER);
    task_pin(&who, "task-term", &skill(), || Ok(Some(pin.clone()))).expect("test: pin");
    store
        .record_hydration("task-term", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");

    let terminal = TaskTerminal {
        task_id: "task-term",
        terminal_receipt_id: "term-9",
        outcome: ExperienceOutcome::Failure,
    };
    let joined = on_task_terminal(tmp.path(), &who, &terminal)
        .expect("test: terminal")
        .expect("test: joined");
    assert_eq!(joined.experience.outcome, ExperienceOutcome::Failure);
    // The pin is gone: a new issue call runs its closure again.
    let mut issued = false;
    task_pin(&who, "task-term", &skill(), || {
        issued = true;
        Ok(Some(pin.clone()))
    })
    .expect("test: re-pin");
    assert!(issued);
    crate::skills::lifecycle::release_task_pins(&who, "task-term");

    // A conflicting terminal reports the error but still releases the pins.
    task_pin(&who, "task-term", &skill(), || Ok(Some(pin.clone()))).expect("test: pin");
    let conflicting = TaskTerminal {
        task_id: "task-term",
        terminal_receipt_id: "term-9",
        outcome: ExperienceOutcome::Success,
    };
    assert!(on_task_terminal(tmp.path(), &who, &conflicting).is_err());
    let mut reissued = false;
    task_pin(&who, "task-term", &skill(), || {
        reissued = true;
        Ok(Some(pin.clone()))
    })
    .expect("test: re-pin");
    assert!(reissued, "pins are released even when the join fails");
    crate::skills::lifecycle::release_task_pins(&who, "task-term");

    // A task that never touched a managed skill gets no experience record.
    let untouched = TaskTerminal {
        task_id: "task-none",
        terminal_receipt_id: "term-0",
        outcome: ExperienceOutcome::Success,
    };
    assert!(
        on_task_terminal(tmp.path(), &who, &untouched)
            .expect("test: terminal")
            .is_none()
    );
    assert!(store.read_experience("task-none").expect("test: read").is_none());
}

#[test]
fn managed_reads_capture_hydration_for_the_trusted_task() {
    let (tmp, store, _pin) = setup();
    let who = owner(OWNER);
    let read = read_managed_resource(&ManagedReadRequest {
        workspace_dir: tmp.path(),
        owner: &who,
        skill_name: "deploy helper",
        resource_path: Some("references/guide.md"),
        task_id: Some("task-read"),
        correlation: Correlation {
            run_id: Some("run-7".into()),
            ..Correlation::default()
        },
        pin_task: false,
        max_bytes: 64 * 1024,
    })
    .expect("test: read")
    .expect("test: managed");
    assert!(!read.pinned);
    let record = store
        .read_experience("task-read")
        .expect("test: read")
        .expect("test: present");
    let receipt = record.hydration_receipts.first().expect("test: receipt");
    assert_eq!(receipt.resource_path, "references/guide.md");
    assert_eq!(
        receipt.resource_sha256,
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"guide body"))
    );
    assert_eq!(receipt.correlation.run_id.as_deref(), Some("run-7"));
    assert_eq!(record.outcome, ExperienceOutcome::Incomplete);

    // No task, no capture.
    let anonymous = read_managed_resource(&ManagedReadRequest {
        workspace_dir: tmp.path(),
        owner: &who,
        skill_name: "deploy helper",
        resource_path: None,
        task_id: None,
        correlation: Correlation::default(),
        pin_task: false,
        max_bytes: 64 * 1024,
    })
    .expect("test: read");
    assert!(anonymous.is_some());
    assert!(store.read_experience("task-none").expect("test: read").is_none());
}

#[test]
fn the_key_contract_is_run_first_trimmed_and_child_safe() {
    assert_eq!(task_key(Some("task"), Some("run"), None).as_deref(), Some("run"));
    assert_eq!(task_key(Some("task"), None, None).as_deref(), Some("task"));
    assert_eq!(task_key(Some("task"), Some("  "), None).as_deref(), Some("task"));
    assert_eq!(task_key(Some(" task "), None, None).as_deref(), Some("task"));
    assert_eq!(task_key(None, None, None), None);
    assert_eq!(task_key(Some(" "), Some(""), None), None);
    // A child is keyed by its own run, never by a task id shared with its parent.
    assert_eq!(
        task_key(Some("task"), Some("child"), Some("parent")).as_deref(),
        Some("child")
    );
    assert_eq!(task_key(Some("task"), None, Some("parent")), None);
    // The owner is the trusted owner id through `OwnerScope::new`; the experience
    // id depends on both.
    let alice = owner(OWNER);
    assert_ne!(
        experience_id(&alice, "run-1"),
        experience_id(&owner("owner:ws:cli:bob"), "run-1")
    );
    assert_ne!(experience_id(&alice, "run-1"), experience_id(&alice, "run-2"));
    assert!(crate::skills::lifecycle::OwnerScope::new(" owner").is_err());
}

#[test]
fn receipts_and_the_experience_keep_every_identity_as_authored() {
    let (_tmp, store, pin) = setup();
    let full = Correlation {
        run_id: Some("run-1".into()),
        task_id: Some("task-1".into()),
        parent_run_id: Some("run-0".into()),
        segment_id: Some("seg-1".into()),
        child_id: Some("child-1".into()),
    };
    store
        .record_hydration("run-1", &pin, "SKILL.md", b"x", &full)
        .expect("test: hydrate");
    let later = Correlation {
        task_id: Some("other-task".into()),
        ..Correlation::default()
    };
    let record = store.record_selection("run-1", &pin, &later).expect("test: select");
    assert_eq!(
        record.correlation, full,
        "the first identities win and are never overwritten"
    );
    assert_eq!(record.hydration_receipts.first().map(|r| &r.correlation), Some(&full));
    assert_eq!(
        record
            .selection_receipts
            .first()
            .and_then(|r| r.correlation.task_id.as_deref()),
        Some("other-task"),
        "each receipt keeps its own"
    );
}

#[test]
fn dropped_evidence_is_marked_even_when_the_capture_locks_cannot_be_taken() {
    let (_tmp, store, pin) = setup();
    store
        .record_hydration("task-lock", &pin, "SKILL.md", b"x", &Correlation::default())
        .expect("test: hydrate");
    {
        // An exclusive store lock (a prune in progress) makes capture time out.
        let _held = store
            .lock_store(false, std::time::Duration::from_secs(1))
            .expect("test: lock");
        let started = std::time::Instant::now();
        let err = store
            .record_hydration("task-lock", &pin, "references/guide.md", b"y", &Correlation::default())
            .expect_err("test: capture must time out");
        assert!(matches!(err, LifecycleError::LockTimeout { .. }), "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "capture is bounded"
        );
        store.mark_dropped_evidence("task-lock");
    }
    let joined = store
        .join_terminal("task-lock", "term", ExperienceOutcome::Success)
        .expect("test: join");
    assert_eq!(joined.experience.dropped_evidence, 1);
    assert!(
        !joined.experience.learning_eligible(),
        "an under-credited experience is not eligible"
    );
}

#[test]
fn pruning_removes_old_experiences_with_their_markers_and_locks() {
    let (_tmp, store, pin) = setup();
    let keep_me = |record: &crate::skills::lifecycle::SkillExperience| record.task_id == "task-kept";
    for task in ["task-old", "task-new", "task-open", "task-kept"] {
        store
            .record_hydration(task, &pin, "SKILL.md", b"x", &Correlation::default())
            .expect("test: hydrate");
    }
    for task in ["task-old", "task-new", "task-kept"] {
        store
            .join_terminal(task, "term", ExperienceOutcome::Success)
            .expect("test: join");
    }
    store.mark_dropped_evidence("task-old");
    let id_old = experience_id(store.owner_scope(), "task-old");
    let lock = store.root().join("locks").join(format!("{id_old}.lock"));
    assert!(lock.exists());

    let retention = ExperienceRetention {
        terminal_max_age: chrono::Duration::days(30),
        incomplete_max_age: chrono::Duration::days(7),
    };
    // Nothing is old yet.
    assert_eq!(
        store
            .prune_experiences(&retention, chrono::Utc::now(), keep_me)
            .expect("test: prune"),
        0
    );
    // 10 days on: the incomplete task expires, joined ones have not.
    let later = chrono::Utc::now() + chrono::Duration::days(10);
    assert_eq!(
        store
            .prune_experiences(&retention, later, keep_me)
            .expect("test: prune"),
        1
    );
    assert!(store.read_experience("task-open").expect("test: read").is_none());
    // 40 days on: everything but the protected experience goes.
    let much_later = chrono::Utc::now() + chrono::Duration::days(40);
    assert_eq!(
        store
            .prune_experiences(&retention, much_later, keep_me)
            .expect("test: prune"),
        2
    );
    assert!(store.read_experience("task-old").expect("test: read").is_none());
    assert!(!lock.exists(), "the lock file goes with its experience");
    assert!(
        !store
            .root()
            .join("experiences")
            .join(format!("{id_old}.dropped"))
            .exists()
    );
    assert!(store.read_experience("task-kept").expect("test: read").is_some());
}
