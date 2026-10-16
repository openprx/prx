#![cfg(test)]

use super::{activate, basic_entries, entry, md, owner};
use crate::skills::lifecycle::binding::PinTable;
use crate::skills::lifecycle::{
    Bundle, BundleEntry, Correlation, LifecycleError, ManagedReadRequest, NewRevision, OwnerScope, RevisionId, SkillId,
    SkillLifecycleStore, SourceKind, read_managed_resource, release_task_pins, task_pin,
};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const OWNER_A: &str = "owner:ws:cli:alice";
const OWNER_B: &str = "owner:ws:cli:bob";
const LIMIT: u64 = 64 * 1024;

type Rev = (RevisionId, String);

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

fn setup() -> (TempDir, SkillLifecycleStore) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER_A)).expect("test: store");
    (tmp, store)
}

fn publish_rev(store: &SkillLifecycleStore, skill: &SkillId, entries: Vec<BundleEntry>) -> Rev {
    let bundle = Bundle::from_entries(entries).expect("test: bundle");
    let outcome = store
        .write_revision(&NewRevision::new(skill.clone(), SourceKind::Operator), &bundle)
        .expect("test: write");
    (outcome.revision.revision_id, outcome.record_digest)
}

fn version(body: &str) -> Vec<BundleEntry> {
    vec![
        entry("SKILL.md", md("Deploy helper", "Helps with deploys").as_bytes()),
        entry("references/guide.md", body.as_bytes()),
    ]
}

fn read(
    workspace: &Path,
    who: &str,
    name: &str,
    resource: Option<&str>,
    task: Option<&str>,
) -> Result<Option<crate::skills::lifecycle::ManagedSkillRead>, LifecycleError> {
    read_managed_resource(&ManagedReadRequest {
        workspace_dir: workspace,
        owner: &owner(who),
        skill_name: name,
        resource_path: resource,
        task_id: task,
        correlation: Correlation::default(),
        pin_task: task.is_some(),
        max_bytes: LIMIT,
    })
}

#[test]
fn legacy_directory_ids_are_stable_distinct_and_name_independent() {
    let a = SkillId::from_legacy_dir("Deploy Helper").expect("test: id");
    assert_eq!(a, SkillId::from_legacy_dir("Deploy Helper").expect("test: id"));
    assert_ne!(
        SkillId::from_legacy_dir("Foo").expect("test: id"),
        SkillId::from_legacy_dir("foo").expect("test: id")
    );
    assert_ne!(
        SkillId::from_legacy_dir("a b").expect("test: id"),
        SkillId::from_legacy_dir("a-b").expect("test: id")
    );
    for odd in [
        "...",
        "--",
        "\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}",
        &"x".repeat(128),
        "_leading",
    ] {
        let id = SkillId::from_legacy_dir(odd).expect("test: odd names still map");
        assert!(SkillId::parse(id.as_str()).is_ok(), "{odd}");
    }
}

#[test]
fn name_lookup_reads_the_authenticated_display_name() {
    let (_tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A"));
    activate(&store, &skill(), &a, 0, true);
    let (binding, name) = store
        .find_binding_by_name("DEPLOY HELPER")
        .expect("test: find")
        .expect("test: bound");
    assert_eq!((binding.generation, name.as_str()), (1, "Deploy helper"));
    assert!(store.find_binding_by_name("unknown").expect("test: find").is_none());
    assert_eq!(store.list_active_bindings().expect("test: list").len(), 1);

    let other = SkillId::parse("second").expect("test: id");
    let second = publish_rev(&store, &other, version("A"));
    activate(&store, &other, &second, 0, true);
    assert!(matches!(
        store
            .find_binding_by_name("deploy helper")
            .expect_err("test: ambiguous"),
        LifecycleError::AmbiguousSkillName(_)
    ));
}

#[test]
fn a_tampered_revision_record_breaks_name_lookup() {
    let (_tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A"));
    activate(&store, &skill(), &a, 0, true);
    let record = store
        .root()
        .join("revisions")
        .join(skill().as_str())
        .join(a.0.as_str())
        .join("revision.json");
    let text = fs::read_to_string(&record).expect("test: record");
    fs::write(&record, text.replace("Deploy helper", "Renamed")).expect("test: tamper");
    assert!(matches!(
        store
            .find_binding_by_name("renamed")
            .expect_err("test: digest mismatch"),
        LifecycleError::Tampered { .. }
    ));
}

#[test]
fn a_pinned_task_reads_a_after_b_is_published() {
    let (_tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("revision A body"));
    activate(&store, &skill(), &a, 0, true);
    let pin_a = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    assert_eq!((pin_a.generation, &pin_a.record_digest), (1, &a.1));

    let b = publish_rev(&store, &skill(), version("revision B body"));
    activate(&store, &skill(), &b, 1, true);

    let pinned = store.read_pinned(&pin_a).expect("test: pinned read");
    assert_eq!(pinned.revision.revision_id, a.0);
    let guide = pinned.bundle.file("references/guide.md").expect("test: guide");
    assert_eq!(guide.bytes(), b"revision A body");
    let fresh = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    assert_eq!((fresh.revision_id, fresh.generation), (b.0, 2));
}

#[test]
fn pinned_reads_fail_when_the_pinned_revision_changed_or_vanished() {
    let (_tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A"));
    activate(&store, &skill(), &a, 0, true);
    let pin = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    let dir = store.root().join("revisions").join(skill().as_str()).join(a.0.as_str());

    let record = dir.join("revision.json");
    let original = fs::read_to_string(&record).expect("test: record");
    fs::write(
        &record,
        original.replace("\"source_kind\": \"operator\"", "\"source_kind\": \"learned\""),
    )
    .expect("test: edit");
    assert!(matches!(
        store.read_pinned(&pin).expect_err("test: digest mismatch"),
        LifecycleError::Tampered { .. }
    ));
    fs::write(&record, original).expect("test: restore");
    store.read_pinned(&pin).expect("test: restored");

    fs::remove_dir_all(&dir).expect("test: remove");
    assert!(matches!(
        store.read_pinned(&pin).expect_err("test: gone"),
        LifecycleError::RevisionNotFound(_)
    ));
}

#[test]
fn a_pin_of_another_owner_is_refused() {
    let (_tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A"));
    activate(&store, &skill(), &a, 0, true);
    let mut pin = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    pin.owner_scope = OwnerScope::new(OWNER_B).expect("test: owner");
    assert!(matches!(
        store.read_pinned(&pin).expect_err("test: foreign pin"),
        LifecycleError::OwnerMismatch { .. }
    ));
}

#[test]
fn task_pins_hold_across_binding_changes_and_release() {
    let (_tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A"));
    activate(&store, &skill(), &a, 0, true);
    let who = owner(OWNER_A);
    let issue = || store.pin_active(&skill());
    let first = task_pin(&who, "task-pin-hold", &skill(), issue)
        .expect("test: issue")
        .expect("test: pin");
    assert_eq!(first.revision_id, a.0);

    let b = publish_rev(&store, &skill(), version("B"));
    activate(&store, &skill(), &b, 1, true);
    let again = task_pin(&who, "task-pin-hold", &skill(), issue)
        .expect("test: reuse")
        .expect("test: pin");
    assert_eq!(again.revision_id, a.0, "the task keeps revision A");
    let other_task = task_pin(&who, "task-pin-other", &skill(), issue)
        .expect("test: issue")
        .expect("test: pin");
    assert_eq!(other_task.revision_id, b.0, "a new task resolves the new binding");

    release_task_pins(&who, "task-pin-hold");
    let after = task_pin(&who, "task-pin-hold", &skill(), issue)
        .expect("test: reissue")
        .expect("test: pin");
    assert_eq!(after.revision_id, b.0);
    release_task_pins(&who, "task-pin-hold");
    release_task_pins(&who, "task-pin-other");
}

#[test]
fn a_full_pin_table_refuses_new_pins_and_never_evicts_live_ones() {
    let (_tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A"));
    activate(&store, &skill(), &a, 0, true);
    let who = owner(OWNER_A);
    let table = PinTable::new(2);
    let issue = || store.pin_active(&skill());
    for task in ["t1", "t2"] {
        table
            .pin(&who, task, &skill(), issue)
            .expect("test: pin")
            .expect("test: pinned");
    }
    let err = table.pin(&who, "t3", &skill(), issue).expect_err("test: full table");
    assert!(matches!(err, LifecycleError::PinTableFull(2)), "{err}");
    // Existing pins are still served, and releasing one frees a slot.
    assert!(
        table
            .pin(&who, "t1", &skill(), issue)
            .expect("test: existing")
            .is_some()
    );
    table.release(&who, "t1");
    assert!(table.pin(&who, "t3", &skill(), issue).expect("test: freed").is_some());
    let err = table.pin(&who, "t1", &skill(), issue).expect_err("test: full again");
    assert!(matches!(err, LifecycleError::PinTableFull(2)), "{err}");
}

#[test]
fn managed_reads_pin_per_task_and_verify_resources() {
    let (tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A body"));
    activate(&store, &skill(), &a, 0, true);

    let first = read(
        tmp.path(),
        OWNER_A,
        "Deploy Helper",
        Some("references/guide.md"),
        Some("task-managed-1"),
    )
    .expect("test: read")
    .expect("test: managed");
    assert_eq!(first.content, "A body");
    assert!(first.pinned);
    assert_eq!(first.pin.revision_id, a.0);

    let b = publish_rev(&store, &skill(), version("B body"));
    activate(&store, &skill(), &b, 1, true);

    let guide = Some("references/guide.md");
    let same_task = read(tmp.path(), OWNER_A, "deploy helper", guide, Some("task-managed-1"))
        .expect("test: read")
        .expect("test: managed");
    assert_eq!(same_task.content, "A body");
    let new_task = read(tmp.path(), OWNER_A, "deploy helper", guide, Some("task-managed-2"))
        .expect("test: read")
        .expect("test: managed");
    assert_eq!(new_task.content, "B body");
    let untracked = read(tmp.path(), OWNER_A, "deploy helper", guide, None)
        .expect("test: read")
        .expect("test: managed");
    assert_eq!((untracked.content.as_str(), untracked.pinned), ("B body", false));
    let default = read(tmp.path(), OWNER_A, "deploy helper", None, None)
        .expect("test: read")
        .expect("test: managed");
    assert!(default.content.contains("Instructions body."));
    assert_eq!(default.resource_path, "SKILL.md");

    for task in ["task-managed-1", "task-managed-2"] {
        release_task_pins(&owner(OWNER_A), task);
    }
}

#[test]
fn managed_reads_refuse_instead_of_falling_back() {
    let (tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A body"));
    activate(&store, &skill(), &a, 0, true);
    let dir = store.root().join("revisions").join(skill().as_str()).join(a.0.as_str());

    for bad in ["../x", "/etc/passwd", "nope.md", "references/../../x"] {
        let err = read(tmp.path(), OWNER_A, "deploy helper", Some(bad), None).expect_err("test: bad resource");
        assert!(matches!(err, LifecycleError::ResourceUnavailable(_)), "{bad}: {err}");
    }
    let err = read_managed_resource(&ManagedReadRequest {
        workspace_dir: tmp.path(),
        owner: &owner(OWNER_A),
        skill_name: "deploy helper",
        resource_path: Some("references/guide.md"),
        task_id: None,
        correlation: Correlation::default(),
        pin_task: false,
        max_bytes: 3,
    })
    .expect_err("test: too large");
    assert!(matches!(err, LifecycleError::ResourceUnavailable(_)), "{err}");

    // A disabled binding is an error, not a fallback to the projection.
    let b = publish_rev(&store, &skill(), version("B body"));
    activate(&store, &skill(), &b, 1, false);
    let err = read(tmp.path(), OWNER_A, "deploy helper", None, None).expect_err("test: disabled");
    assert!(matches!(err, LifecycleError::SkillDisabled(_)), "{err}");
    activate(&store, &skill(), &a, 2, true);

    fs::write(dir.join("bundle/references/guide.md"), "tampered").expect("test: tamper");
    let err = read(tmp.path(), OWNER_A, "deploy helper", None, None).expect_err("test: tampered");
    assert!(matches!(err, LifecycleError::Tampered { .. }), "{err}");
}

#[test]
fn unmanaged_for_this_owner_means_legacy_behaviour() {
    let tmp = TempDir::new().expect("test: tempdir");
    assert!(
        read(tmp.path(), OWNER_A, "deploy helper", None, None)
            .expect("test: read")
            .is_none()
    );
    assert!(!tmp.path().join("self").exists(), "reading must not create a store");
    assert!(
        SkillLifecycleStore::open_existing(tmp.path(), owner(OWNER_A))
            .expect("test: open")
            .is_none()
    );

    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER_A)).expect("test: store");
    let a = publish_rev(&store, &skill(), version("A"));
    activate(&store, &skill(), &a, 0, true);
    assert!(
        read(tmp.path(), OWNER_A, "deploy helper", None, None)
            .expect("test: read")
            .is_some()
    );
    assert!(
        read(tmp.path(), OWNER_A, "not-managed", None, None)
            .expect("test: read")
            .is_none()
    );
    assert!(
        read(tmp.path(), OWNER_B, "deploy helper", None, None)
            .expect("test: read")
            .is_none(),
        "another owner does not see the store"
    );
}

#[test]
fn untrusted_origin_marker_is_reported() {
    let (tmp, store) = setup();
    let mut entries = basic_entries();
    entries.push(entry(
        ".openprx-untrusted-origin.json",
        br#"{"trusted":false,"source":"https://example.invalid/x.git","review_required":true}"#,
    ));
    let id = publish_rev(&store, &skill(), entries);
    activate(&store, &skill(), &id, 0, true);
    let read = read(tmp.path(), OWNER_A, "deploy helper", None, None)
        .expect("test: read")
        .expect("test: managed");
    assert!(read.untrusted_origin);
}

#[test]
fn a_pinned_task_keeps_its_revision_when_the_skill_is_renamed_or_disabled() {
    let (tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A body"));
    activate(&store, &skill(), &a, 0, true);
    let guide = Some("references/guide.md");
    let first = read(tmp.path(), OWNER_A, "Deploy helper", guide, Some("task-rename-1"))
        .expect("test: read")
        .expect("test: managed");
    assert_eq!(first.content, "A body");

    let renamed = publish_rev(
        &store,
        &skill(),
        vec![
            entry("SKILL.md", md("Renamed skill", "Now called something else").as_bytes()),
            entry("references/guide.md", b"B body"),
        ],
    );
    activate(&store, &skill(), &renamed, 1, false);

    // The pinned task still reads A by the name it selected, although the
    // active binding is now a differently named, disabled revision.
    let again = read(tmp.path(), OWNER_A, "deploy helper", guide, Some("task-rename-1"))
        .expect("test: pinned read survives rename and disable")
        .expect("test: managed");
    assert_eq!((again.content.as_str(), again.pinned), ("A body", true));
    // Another task resolves the new binding: the old name is gone, the new one is disabled.
    assert!(
        read(tmp.path(), OWNER_A, "deploy helper", guide, Some("task-rename-2"))
            .expect("test: read")
            .is_none()
    );
    let err = read(tmp.path(), OWNER_A, "renamed skill", guide, Some("task-rename-2")).expect_err("test: disabled");
    assert!(matches!(err, LifecycleError::SkillDisabled(_)), "{err}");
    release_task_pins(&owner(OWNER_A), "task-rename-1");
}

#[test]
fn stray_files_in_the_active_directory_do_not_break_listing() {
    let (_tmp, store) = setup();
    let a = publish_rev(&store, &skill(), version("A"));
    activate(&store, &skill(), &a, 0, true);
    let active = store.root().join("active");
    for stray in [".DS_Store", "notes.txt", "bad name.json", ".hidden.json"] {
        fs::write(active.join(stray), b"x").expect("test: stray file");
    }
    let listed = store.list_active_bindings().expect("test: list");
    assert_eq!(listed.len(), 1);
    assert!(
        store
            .find_binding_by_name("deploy helper")
            .expect("test: find")
            .is_some()
    );
}
