use super::{basic_entries, entry, md, owner};
use crate::skills::lifecycle::{
    Bundle, BundleEntry, LifecycleError, NewRevision, OwnerScope, RevisionId, SkillId, SkillLifecycleStore, SourceKind,
    read_managed_resource, release_task_pins, task_pin,
};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const OWNER_A: &str = "owner:ws:cli:alice";
const OWNER_B: &str = "owner:ws:cli:bob";
const LIMIT: u64 = 64 * 1024;

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

fn setup() -> (TempDir, SkillLifecycleStore) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER_A)).expect("test: store");
    (tmp, store)
}

fn publish(store: &SkillLifecycleStore, entries: Vec<BundleEntry>) -> RevisionId {
    let bundle = Bundle::from_entries(entries).expect("test: bundle");
    store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write")
        .revision
        .revision_id
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
    read_managed_resource(workspace, &owner(who), name, resource, task, LIMIT)
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
    for odd in ["...", "--", "\u{4e2d}\u{6587}", &"x".repeat(128), "_leading"] {
        let id = SkillId::from_legacy_dir(odd).expect("test: odd names still map");
        assert!(SkillId::parse(id.as_str()).is_ok(), "{odd}");
    }
}

#[test]
fn bindings_are_generation_checked() {
    let (_tmp, store) = setup();
    let a = publish(&store, version("A"));
    let b = publish(&store, version("B"));
    assert!(store.read_binding(&skill()).expect("test: read").is_none());

    let first = store.write_binding(&skill(), &a, true, None).expect("test: create");
    assert_eq!(first.generation, 1);
    assert_eq!(first.display_name, "Deploy helper");
    let err = store
        .write_binding(&skill(), &b, true, None)
        .expect_err("test: create over existing");
    assert!(
        matches!(err, LifecycleError::GenerationConflict { actual: Some(1), .. }),
        "{err}"
    );
    let err = store
        .write_binding(&skill(), &b, true, Some(7))
        .expect_err("test: stale generation");
    assert!(
        matches!(err, LifecycleError::GenerationConflict { actual: Some(1), .. }),
        "{err}"
    );
    let second = store.write_binding(&skill(), &b, true, Some(1)).expect("test: replace");
    assert_eq!(second.generation, 2);
    assert_eq!(store.read_binding(&skill()).expect("test: read"), Some(second));
    assert_eq!(store.list_bindings().expect("test: list").len(), 1);
}

#[test]
fn a_binding_needs_a_verified_revision_of_that_skill() {
    let (_tmp, store) = setup();
    let a = publish(&store, version("A"));
    let other = SkillId::parse("other-skill").expect("test: id");
    let err = store
        .write_binding(&other, &a, true, None)
        .expect_err("test: foreign revision");
    assert!(matches!(err, LifecycleError::RevisionNotFound(_)), "{err}");
    let missing = RevisionId::parse(&format!("r1-{}", "0".repeat(64))).expect("test: id");
    assert!(store.write_binding(&skill(), &missing, true, None).is_err());
}

#[test]
fn forged_or_misplaced_bindings_are_refused() {
    let (_tmp, store) = setup();
    let a = publish(&store, version("A"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind");
    let path = store.root().join("active").join("deploy-helper.json");
    let text = fs::read_to_string(&path).expect("test: record");
    fs::write(&path, text.replace(OWNER_A, OWNER_B)).expect("test: forge owner");
    assert!(matches!(
        store.read_binding(&skill()).expect_err("test: forged owner"),
        LifecycleError::OwnerMismatch { .. }
    ));
    fs::write(&path, text.replace("binding-deploy-helper", "binding-other")).expect("test: forge id");
    assert!(matches!(
        store.read_binding(&skill()).expect_err("test: forged id"),
        LifecycleError::InvalidRecord { .. }
    ));
    fs::write(&path, text.replace("Deploy helper", "Renamed")).expect("test: forge name");
    let binding = store
        .read_binding(&skill())
        .expect("test: parses")
        .expect("test: present");
    assert!(matches!(
        store.pin_binding(&binding).expect_err("test: name mismatch"),
        LifecycleError::Tampered { .. }
    ));
}

#[test]
fn name_lookup_is_case_insensitive_and_rejects_ambiguity() {
    let (_tmp, store) = setup();
    let a = publish(&store, version("A"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind");
    assert!(
        store
            .find_binding_by_name("DEPLOY HELPER")
            .expect("test: find")
            .is_some()
    );
    assert!(store.find_binding_by_name("unknown").expect("test: find").is_none());

    let other = SkillId::parse("second").expect("test: id");
    let bundle = Bundle::from_entries(version("A")).expect("test: bundle");
    store
        .write_revision(&NewRevision::new(other.clone(), SourceKind::Operator), &bundle)
        .expect("test: write");
    store
        .write_binding(&other, bundle.revision_id(), true, None)
        .expect("test: bind");
    assert!(matches!(
        store
            .find_binding_by_name("deploy helper")
            .expect_err("test: ambiguous"),
        LifecycleError::AmbiguousSkillName(_)
    ));
}

#[test]
fn a_pinned_task_reads_a_after_b_is_published() {
    let (_tmp, store) = setup();
    let a = publish(&store, version("revision A body"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind A");
    let pin_a = store.pin_active(&skill()).expect("test: pin").expect("test: bound");

    let b = publish(&store, version("revision B body"));
    store
        .write_binding(&skill(), &b, true, Some(1))
        .expect("test: publish B");

    let pinned = store.read_pinned(&pin_a).expect("test: pinned read");
    assert_eq!(pinned.revision.revision_id, a);
    let guide = pinned.bundle.file("references/guide.md").expect("test: guide");
    assert_eq!(guide.bytes(), b"revision A body");
    let fresh = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    assert_eq!(fresh.revision_id, b);
    assert_eq!(fresh.generation, 2);
}

#[test]
fn pinned_reads_fail_when_the_pinned_revision_changed_or_vanished() {
    let (_tmp, store) = setup();
    let a = publish(&store, version("A"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind");
    let pin = store.pin_active(&skill()).expect("test: pin").expect("test: bound");
    let dir = store.root().join("revisions").join(skill().as_str()).join(a.as_str());

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
    let a = publish(&store, version("A"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind");
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
    let a = publish(&store, version("A"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind");
    let who = owner(OWNER_A);
    let issue = || store.pin_active(&skill());
    let first = task_pin(&who, "task-pin-hold", &skill(), issue)
        .expect("test: issue")
        .expect("test: pin");
    assert_eq!(first.revision_id, a);

    let b = publish(&store, version("B"));
    store
        .write_binding(&skill(), &b, true, Some(1))
        .expect("test: publish B");
    let again = task_pin(&who, "task-pin-hold", &skill(), issue)
        .expect("test: reuse")
        .expect("test: pin");
    assert_eq!(again.revision_id, a, "the task keeps revision A");
    let other_task = task_pin(&who, "task-pin-other", &skill(), issue)
        .expect("test: issue")
        .expect("test: pin");
    assert_eq!(other_task.revision_id, b, "a new task resolves the new binding");

    release_task_pins(&who, "task-pin-hold");
    let after = task_pin(&who, "task-pin-hold", &skill(), issue)
        .expect("test: reissue")
        .expect("test: pin");
    assert_eq!(after.revision_id, b);
    release_task_pins(&who, "task-pin-hold");
    release_task_pins(&who, "task-pin-other");
}

#[test]
fn managed_reads_pin_per_task_and_verify_resources() {
    let (tmp, store) = setup();
    let a = publish(&store, version("A body"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind");

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
    assert_eq!(first.pin.revision_id, a);

    let b = publish(&store, version("B body"));
    store
        .write_binding(&skill(), &b, true, Some(1))
        .expect("test: publish B");

    let same_task = read(
        tmp.path(),
        OWNER_A,
        "deploy helper",
        Some("references/guide.md"),
        Some("task-managed-1"),
    )
    .expect("test: read")
    .expect("test: managed");
    assert_eq!(same_task.content, "A body");
    let new_task = read(
        tmp.path(),
        OWNER_A,
        "deploy helper",
        Some("references/guide.md"),
        Some("task-managed-2"),
    )
    .expect("test: read")
    .expect("test: managed");
    assert_eq!(new_task.content, "B body");
    let untracked = read(tmp.path(), OWNER_A, "deploy helper", Some("references/guide.md"), None)
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
    let a = publish(&store, version("A body"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind");
    let dir = store.root().join("revisions").join(skill().as_str()).join(a.as_str());

    for bad in ["../x", "/etc/passwd", "nope.md", "references/../../x"] {
        let err = read(tmp.path(), OWNER_A, "deploy helper", Some(bad), None).expect_err("test: bad resource");
        assert!(matches!(err, LifecycleError::ResourceUnavailable(_)), "{bad}: {err}");
    }
    let err = read_managed_resource(
        tmp.path(),
        &owner(OWNER_A),
        "deploy helper",
        Some("references/guide.md"),
        None,
        3,
    )
    .expect_err("test: too large");
    assert!(matches!(err, LifecycleError::ResourceUnavailable(_)), "{err}");

    // A disabled binding is an error, not a fallback to the projection.
    store
        .write_binding(&skill(), &a, false, Some(1))
        .expect("test: disable");
    let err = read(tmp.path(), OWNER_A, "deploy helper", None, None).expect_err("test: disabled");
    assert!(matches!(err, LifecycleError::SkillDisabled(_)), "{err}");
    store.write_binding(&skill(), &a, true, Some(2)).expect("test: enable");

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
    let a = publish(&store, version("A"));
    store.write_binding(&skill(), &a, true, None).expect("test: bind");
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
    let id = publish(&store, entries);
    store.write_binding(&skill(), &id, true, None).expect("test: bind");
    let read = read(tmp.path(), OWNER_A, "deploy helper", None, None)
        .expect("test: read")
        .expect("test: managed");
    assert!(read.untrusted_origin);
}
