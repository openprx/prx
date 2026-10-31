#![cfg(test)]

//! FINDING 108.2 and 108.6: the compatibility projection and the caches follow
//! the committed binding, and a failure after the commit is reported as
//! `CommittedRepairRequired` and repaired by recovery.

use super::{exec_entry, md, owner};
use crate::skills::lifecycle::publish::crash_hooks;
use crate::skills::lifecycle::{
    Bundle, BundleEntry, Correlation, LifecycleError, ManagedReadRequest, NewRevision, PublishRequest, RevisionId,
    RollbackRequest, SkillId, SkillLifecycleStore, SourceKind, TransactionId, TransactionKind, TransactionState,
    apply_migration, plan_migration, read_managed_resource, release_task_pins,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const OWNER: &str = "owner:ws:cli:alice";
const NAME: &str = "Deploy helper";

type Rev = (RevisionId, String);

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: skill id")
}

fn setup() -> (TempDir, SkillLifecycleStore) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store opens");
    (tmp, store)
}

fn entries(tag: &str) -> Vec<BundleEntry> {
    vec![
        super::entry("SKILL.md", md(NAME, tag).as_bytes()),
        super::entry("references/guide.md", format!("guide {tag}\r\n").as_bytes()),
        exec_entry("scripts/run.sh", format!("#!/bin/sh\necho {tag}\n").as_bytes()),
    ]
}

fn write(store: &SkillLifecycleStore, id: &SkillId, tag: &str) -> Rev {
    let bundle = Bundle::from_entries(entries(tag)).expect("test: bundle");
    let out = store
        .write_revision(&NewRevision::new(id.clone(), SourceKind::Operator), &bundle)
        .expect("test: write revision");
    (out.revision.revision_id, out.record_digest)
}

fn publish_req(name: &str, id: &SkillId, rev: &Rev, expected: u64, enabled: bool) -> PublishRequest {
    PublishRequest {
        transaction_id: TransactionId::parse(name).expect("test: tx id"),
        skill_id: id.clone(),
        candidate: rev.0.clone(),
        candidate_record_digest: rev.1.clone(),
        expected_generation: expected,
        enabled,
    }
}

fn rollback_req(name: &str, id: &SkillId, target: &RevisionId, expected: u64) -> RollbackRequest {
    RollbackRequest {
        transaction_id: TransactionId::parse(name).expect("test: tx id"),
        skill_id: id.clone(),
        target: target.clone(),
        expected_generation: expected,
    }
}

fn projection(workspace: &Path) -> PathBuf {
    workspace.join("skills/deploy-helper")
}

/// Revision id of the tree currently on disk at `dir`.
fn on_disk(dir: &Path) -> RevisionId {
    Bundle::from_dir(dir)
        .expect("test: projection is a valid bundle")
        .revision_id()
        .clone()
}

fn config_for(workspace: &Path) -> crate::config::Config {
    let quiet = workspace.join("quiet-community");
    fs::create_dir_all(&quiet).expect("test: quiet dir");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config.skills.open_skills_dir = Some(quiet.to_string_lossy().into_owned());
    config.skills.openclaw_skills_dir = Some(quiet.to_string_lossy().into_owned());
    config
}

/// Descriptions the discovery catalog shows for the workspace.
fn discovered(workspace: &Path) -> Vec<(String, String)> {
    crate::skills::catalog_skill_metadata(workspace, &config_for(workspace))
        .into_iter()
        .map(|m| (m.name, m.description))
        .collect()
}

/// Descriptions of the cached skill catalog snapshot.
fn cached_descriptions(workspace: &Path) -> Vec<String> {
    crate::skills::load_skills_with_config(workspace, &config_for(workspace))
        .into_iter()
        .map(|skill| skill.description)
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn leftover_names(workspace: &Path) -> Vec<String> {
    fs::read_dir(workspace.join("skills/.openprx-projection"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn no_leftovers(workspace: &Path) {
    let staging = workspace.join("skills/.openprx-projection");
    let left = fs::read_dir(&staging).map_or(0, Iterator::count);
    assert_eq!(left, 0, "projection staging must be empty");
}

#[test]
fn publish_and_rollback_project_the_committed_bytes_and_discovery_follows() {
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );

    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    assert_eq!(on_disk(&projection(tmp.path())), a.0);
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "alpha-desc".to_string())]);

    store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect("test: publish b");
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "beta-desc".to_string())]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(projection(tmp.path()).join("scripts/run.sh"))
            .expect("test: script")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "the executable bit is part of the revision");
    }

    store
        .rollback(&rollback_req("t3", &skill(), &a.0, 2))
        .expect("test: rollback");
    assert_eq!(on_disk(&projection(tmp.path())), a.0);
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "alpha-desc".to_string())]);
    no_leftovers(tmp.path());
}

#[test]
fn a_disabled_publication_hides_the_skill_from_discovery() {
    let (tmp, store) = setup();
    let a = write(&store, &skill(), "alpha-desc");
    store
        .publish(&publish_req("t1", &skill(), &a, 0, false))
        .expect("test: publish disabled");
    assert_eq!(on_disk(&projection(tmp.path())), a.0);
    assert_eq!(discovered(tmp.path()).len(), 0);
    let b = write(&store, &skill(), "beta-desc");
    store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect("test: publish enabled");
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "beta-desc".to_string())]);
}

#[test]
fn legacy_skills_without_a_binding_are_never_touched() {
    let (tmp, store) = setup();
    let legacy = tmp.path().join("skills/legacy-tool");
    fs::create_dir_all(&legacy).expect("test: legacy dir");
    fs::write(legacy.join("SKILL.md"), md("Legacy", "legacy desc")).expect("test: legacy skill");
    let before = fs::read(legacy.join("SKILL.md")).expect("test: read");
    let a = write(&store, &skill(), "alpha-desc");
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish");
    store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(fs::read(legacy.join("SKILL.md")).expect("test: read"), before);
    assert_eq!(fs::read_dir(&legacy).expect("test: dir").count(), 1);
}

#[test]
fn an_imported_legacy_directory_is_replaced_in_place_not_duplicated() {
    let tmp = TempDir::new().expect("test: tempdir");
    let dir = tmp.path().join("skills/Deploy Helper");
    fs::create_dir_all(&dir).expect("test: legacy dir");
    fs::write(dir.join("SKILL.md"), md(NAME, "legacy-desc")).expect("test: legacy skill");
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let plan = plan_migration(tmp.path(), Some(&store)).expect("test: plan");
    let report = apply_migration(&store, tmp.path(), &plan);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        fs::read_to_string(dir.join("SKILL.md")).expect("test: read"),
        md(NAME, "legacy-desc")
    );

    let id = SkillId::from_legacy_dir("Deploy Helper").expect("test: legacy id");
    let next = write(&store, &id, "next-desc");
    store
        .publish(&publish_req("t1", &id, &next, 1, true))
        .expect("test: publish");
    assert_eq!(on_disk(&dir), next.0);
    assert_eq!(
        fs::read_dir(tmp.path().join("skills"))
            .expect("test: skills")
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| !e.file_name().to_string_lossy().starts_with('.'))
            })
            .count(),
        1,
        "no second directory for the same skill"
    );
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "next-desc".to_string())]);
}

#[test]
fn a_projection_write_failure_after_commit_is_reported_and_repaired_by_recovery() {
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");

    crash_hooks::arm("projection_staged");
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: injected projection failure");
    let LifecycleError::CommittedRepairRequired { transaction_id, reason } = &err else {
        panic!("test: expected CommittedRepairRequired, got {err}");
    };
    assert_eq!(transaction_id, "t2");
    assert!(reason.contains("injected crash point"), "{reason}");

    // The binding and journal are committed; the readers still see one whole old tree.
    let bound = store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!((bound.generation, &bound.revision_id), (2, &b.0));
    assert_eq!(on_disk(&projection(tmp.path())), a.0, "never a mixed or partial tree");
    no_leftovers(tmp.path());

    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.projections.repaired, [skill()]);
    assert_eq!(report.projections.failed.len(), 0);
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "beta-desc".to_string())]);

    let again = store.recover(Duration::ZERO).expect("test: second recover");
    assert!(again.projections.repaired.is_empty(), "recovery is idempotent");
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
}

#[test]
fn a_rollback_projection_failure_is_repaired_by_retrying_the_same_transaction() {
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect("test: publish b");

    crash_hooks::arm("projection_staged");
    let err = store
        .rollback(&rollback_req("t3", &skill(), &a.0, 2))
        .expect_err("test: injected projection failure");
    assert!(matches!(err, LifecycleError::CommittedRepairRequired { .. }), "{err}");
    assert_eq!(on_disk(&projection(tmp.path())), b.0);

    let retried = store
        .rollback(&rollback_req("t3", &skill(), &a.0, 2))
        .expect("test: retry repairs");
    assert!(retried.replayed);
    assert_eq!(on_disk(&projection(tmp.path())), a.0);
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "alpha-desc".to_string())]);
}

#[test]
fn a_crash_between_the_commit_record_and_the_projection_is_repaired_at_startup() {
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    crash_hooks::arm("after_committed");
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: crash");
    assert!(matches!(err, LifecycleError::CommittedRepairRequired { .. }), "{err}");
    drop(store);
    let restarted = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: reopen");
    assert_eq!(on_disk(&projection(tmp.path())), a.0);
    let report = restarted.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.projections.repaired, [skill()]);
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
}

#[test]
fn recovery_repairs_a_missing_projection_and_sweeps_leftover_trees() {
    let (tmp, store) = setup();
    let a = write(&store, &skill(), "alpha-desc");
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    fs::remove_dir_all(projection(tmp.path())).expect("test: lose the projection");
    let leftover = tmp.path().join(format!(
        "skills/.openprx-projection/deploy-helper.{}.new",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&leftover).expect("test: leftover tree");
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.projections.repaired, [skill()]);
    assert_eq!(report.swept_staging, 1);
    assert_eq!(on_disk(&projection(tmp.path())), a.0);
    no_leftovers(tmp.path());
}

#[cfg(unix)]
#[test]
fn a_symlinked_projection_path_is_refused_before_the_commit_and_never_followed() {
    let (tmp, store) = setup();
    let outside = TempDir::new().expect("test: outside");
    fs::write(outside.path().join("SKILL.md"), b"outside").expect("test: outside file");
    fs::create_dir_all(tmp.path().join("skills")).expect("test: skills");
    std::os::unix::fs::symlink(outside.path(), projection(tmp.path())).expect("test: symlink");
    let a = write(&store, &skill(), "alpha-desc");
    let err = store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect_err("test: refused");
    assert!(matches!(err, LifecycleError::UnsafeStorePath { .. }), "{err}");
    assert!(store.active_binding(&skill()).expect("test: binding").is_none());
    assert_eq!(
        fs::read(outside.path().join("SKILL.md")).expect("test: read"),
        b"outside"
    );
    no_leftovers(tmp.path());
}

fn managed_read(workspace: &Path, task: &str) -> String {
    let owner = owner(OWNER);
    let read = read_managed_resource(&ManagedReadRequest {
        workspace_dir: workspace,
        owner: &owner,
        skill_name: NAME,
        resource_path: None,
        task_id: Some(task),
        correlation: Correlation::default(),
        pin_task: true,
        max_bytes: 64 * 1024,
    })
    .expect("test: managed read")
    .expect("test: managed skill");
    read.content
}

/// A task that is already pinned to revision `a` keeps reading it while `b` is
/// published and the process dies at `point`; recovery then decides cleanly.
fn pinned_task_across_a_crash(point: &'static str, task: &str) -> (TempDir, SkillLifecycleStore, Rev, Rev) {
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    assert!(
        managed_read(tmp.path(), task).contains("alpha-desc"),
        "the task is pinned to a"
    );

    crash_hooks::arm(point);
    store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: injected crash");
    assert!(
        managed_read(tmp.path(), task).contains("alpha-desc"),
        "pin holds through the crash"
    );
    (tmp, store, a, b)
}

#[test]
fn a_pinned_task_keeps_its_revision_when_the_publish_crashes_before_the_swap() {
    let task = format!("task-{}", uuid::Uuid::new_v4());
    let (tmp, store, a, _b) = pinned_task_across_a_crash("after_prepared", &task);
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.aborted, [TransactionId::parse("t2").expect("test: tx")]);
    assert!(
        report.projections.repaired.is_empty(),
        "nothing was committed, nothing to repair"
    );
    assert_eq!(on_disk(&projection(tmp.path())), a.0);
    assert!(managed_read(tmp.path(), &task).contains("alpha-desc"));
    let fresh = format!("task-{}", uuid::Uuid::new_v4());
    assert!(managed_read(tmp.path(), &fresh).contains("alpha-desc"));
    release_task_pins(&owner(OWNER), &task);
    release_task_pins(&owner(OWNER), &fresh);
}

#[test]
fn a_pinned_task_keeps_its_revision_when_the_publish_crashes_after_the_swap() {
    let task = format!("task-{}", uuid::Uuid::new_v4());
    let (tmp, store, _a, b) = pinned_task_across_a_crash("after_binding", &task);
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.committed, [TransactionId::parse("t2").expect("test: tx")]);
    assert_eq!(report.projections.repaired, [skill()]);
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "beta-desc".to_string())]);
    // The in-flight task still reads its pinned revision; a new task reads the committed one.
    assert!(managed_read(tmp.path(), &task).contains("alpha-desc"));
    let fresh = format!("task-{}", uuid::Uuid::new_v4());
    assert!(managed_read(tmp.path(), &fresh).contains("beta-desc"));
    release_task_pins(&owner(OWNER), &task);
    release_task_pins(&owner(OWNER), &fresh);
}

#[test]
fn a_pinned_task_survives_a_projection_failure_and_its_repair() {
    let task = format!("task-{}", uuid::Uuid::new_v4());
    let (tmp, store, _a, b) = pinned_task_across_a_crash("projection_staged", &task);
    let bound = store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(bound.revision_id, b.0);
    store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    assert!(managed_read(tmp.path(), &task).contains("alpha-desc"));
    release_task_pins(&owner(OWNER), &task);
}

#[test]
fn an_unmanaged_directory_survives_and_the_first_publication_is_refused() {
    let (tmp, store) = setup();
    let mine = projection(tmp.path());
    fs::create_dir_all(&mine).expect("test: user dir");
    fs::write(mine.join("SKILL.md"), b"my own skill").expect("test: user file");
    let a = write(&store, &skill(), "alpha-desc");
    let err = store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect_err("test: refused");
    assert!(matches!(err, LifecycleError::UnsafeStorePath { .. }), "{err}");
    assert_eq!(fs::read(mine.join("SKILL.md")).expect("test: read"), b"my own skill");
    assert_eq!(fs::read_dir(&mine).expect("test: dir").count(), 1);
    assert!(store.active_binding(&skill()).expect("test: binding").is_none());
    let tx = TransactionId::parse("t1").expect("test: tx");
    assert!(
        store.journal_entry(&tx).expect("test: journal").is_none(),
        "no mutation"
    );
}

#[test]
fn the_projection_itself_refuses_an_unmanaged_tree_for_a_first_generation() {
    let (tmp, store) = setup();
    let a = write(&store, &skill(), "alpha-desc");
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish");
    fs::remove_dir_all(projection(tmp.path())).expect("test: drop projection");
    fs::create_dir_all(projection(tmp.path())).expect("test: user dir");
    fs::write(projection(tmp.path()).join("SKILL.md"), b"mine").expect("test: user file");
    let binding = store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    let lock = store.lock_skill(&skill(), Duration::from_secs(5)).expect("test: lock");
    let err = store.refresh_projection(&lock, &binding).expect_err("test: refused");
    assert!(matches!(err, LifecycleError::UnsafeStorePath { .. }), "{err}");
    drop(lock);
    assert_eq!(
        fs::read(projection(tmp.path()).join("SKILL.md")).expect("test: read"),
        b"mine"
    );
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.projections.failed.len(), 1);
    assert_eq!(
        fs::read(projection(tmp.path()).join("SKILL.md")).expect("test: read"),
        b"mine"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_skills_directory_is_refused_before_the_commit() {
    let (tmp, store) = setup();
    let outside = TempDir::new().expect("test: outside");
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("skills")).expect("test: symlink");
    let a = write(&store, &skill(), "alpha-desc");
    let err = store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect_err("test: refused");
    assert!(matches!(err, LifecycleError::UnsafeStorePath { .. }), "{err}");
    assert!(store.active_binding(&skill()).expect("test: binding").is_none());
    assert_eq!(fs::read_dir(outside.path()).expect("test: outside").count(), 0);
}

#[test]
fn the_directory_cap_ignores_dot_entries_and_refuses_before_the_commit() {
    let (tmp, store) = setup();
    let skills = tmp.path().join("skills");
    fs::create_dir_all(&skills).expect("test: skills");
    for n in 0..1100 {
        fs::create_dir(skills.join(format!(".hidden-{n}"))).expect("test: dot dir");
    }
    let a = write(&store, &skill(), "alpha-desc");
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: dot entries do not count");
    for n in 0..1030 {
        fs::create_dir(skills.join(format!("other-{n}"))).expect("test: plain dir");
    }
    let b = write(&store, &skill(), "beta-desc");
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: over the cap");
    assert!(matches!(err, LifecycleError::InvalidRecord { .. }), "{err}");
    let bound = store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(
        (bound.generation, &bound.revision_id),
        (1, &a.0),
        "refused before the commit"
    );
}

/// Platforms that still rename an existing projection aside. macOS refuses
/// instead; see the macOS tests at the bottom of this module.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_two_rename_fallback_leaves_the_skill_absent_and_recovery_restores_it() {
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    crash_hooks::force_two_step(true);
    crash_hooks::arm("projection_swapped_out");
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: crash between the renames");
    assert!(matches!(err, LifecycleError::CommittedRepairRequired { .. }), "{err}");
    assert!(
        !projection(tmp.path()).exists(),
        "the skill is absent, not half-written"
    );
    let left = leftover_names(tmp.path());
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left.iter().all(|n| n.ends_with(".old")), "{left:?}");

    let report = store.recover(Duration::ZERO).expect("test: recover");
    crash_hooks::force_two_step(false);
    assert_eq!(report.projections.repaired, [skill()]);
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    no_leftovers(tmp.path());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_failed_repair_keeps_the_previous_tree_until_a_repair_succeeds() {
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    crash_hooks::force_two_step(true);
    crash_hooks::arm("projection_swapped_out");
    store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: crash between the renames");
    // Something in the way makes the repair fail.
    fs::write(projection(tmp.path()), b"in the way").expect("test: blocker");
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.projections.failed.len(), 1);
    assert_eq!(report.swept_staging, 0, "nothing is swept for a failed skill");
    let left = leftover_names(tmp.path());
    assert_eq!(left.len(), 1, "the previous tree survives: {left:?}");

    fs::remove_file(projection(tmp.path())).expect("test: remove blocker");
    let report = store.recover(Duration::ZERO).expect("test: recover again");
    crash_hooks::force_two_step(false);
    assert_eq!(report.projections.repaired, [skill()]);
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    no_leftovers(tmp.path());
}

#[test]
fn a_crash_after_the_swap_leaves_a_stale_cache_that_recovery_invalidates() {
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    assert_eq!(cached_descriptions(tmp.path()), ["alpha-desc"]);
    crash_hooks::arm("projection_written");
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: crash before the cache is invalidated");
    assert!(matches!(err, LifecycleError::CommittedRepairRequired { .. }), "{err}");
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    assert_eq!(
        cached_descriptions(tmp.path()),
        ["alpha-desc"],
        "the snapshot is stale until repair"
    );
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert!(report.projections.repaired.is_empty(), "the tree was already current");
    assert_eq!(cached_descriptions(tmp.path()), ["beta-desc"]);
}

#[test]
fn one_unreadable_binding_does_not_abort_recovery_or_lose_the_report() {
    let (tmp, store) = setup();
    let a = write(&store, &skill(), "alpha-desc");
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    let b = write(&store, &skill(), "beta-desc");
    crash_hooks::arm("after_prepared");
    store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: crash");
    let bad = store.root().join("active/broken-skill.json");
    fs::write(&bad, b"{ not json").expect("test: corrupt binding");
    let leftover = tmp.path().join(format!(
        "skills/.openprx-projection/deploy-helper.{}.new",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&leftover).expect("test: leftover tree");
    let old = tmp.path().join(format!(
        "skills/.openprx-projection/deploy-helper.{}.old",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&old).expect("test: replaced tree");
    let report = store
        .recover(Duration::ZERO)
        .expect("test: recover still returns its report");
    assert_eq!(report.aborted, [TransactionId::parse("t2").expect("test: tx")]);
    assert!(
        report.projections.listing_error.is_some(),
        "the listing failure is reported"
    );
    assert_eq!(report.projections.failed.len(), 0);
    assert_eq!(report.swept_staging, 1, "only the .new leftover goes");
    assert!(!leftover.exists());
    assert!(
        old.exists(),
        "a .old tree may be the only copy while bindings are unreadable"
    );
}

#[test]
fn recovery_resets_the_disabled_marker_of_a_bound_skill_to_the_binding() {
    let (tmp, store) = setup();
    let a = write(&store, &skill(), "alpha-desc");
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish");
    let marker = tmp.path().join("skills/.openprx-disabled/deploy-helper");
    fs::create_dir_all(marker.parent().expect("test: parent")).expect("test: marker dir");
    fs::write(&marker, b"").expect("test: gateway toggle");
    assert_eq!(discovered(tmp.path()).len(), 0, "the toggle hides the skill");
    store.recover(Duration::ZERO).expect("test: recover");
    assert!(!marker.exists(), "the binding is enabled, so the marker is cleared");
    assert_eq!(discovered(tmp.path()), [(NAME.to_string(), "alpha-desc".to_string())]);
}

/// On supported filesystems the second publish must bypass the crash seam in
/// the two-rename gap. This exercises the real OS exchange, not a mock.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
#[test]
fn atomic_exchange_keeps_the_projection_present_during_replacement() {
    let (tmp, store) = setup();
    let a = write(&store, &skill(), "alpha-desc");
    let b = write(&store, &skill(), "beta-desc");
    store.publish(&publish_req("atomic-a", &skill(), &a, 0, true)).unwrap();
    crash_hooks::arm("projection_swapped_out");
    let result = store.publish(&publish_req("atomic-b", &skill(), &b, 1, true));
    crash_hooks::disarm();
    result.expect("the native atomic exchange must not enter the two-rename gap");
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    no_leftovers(tmp.path());
}

/// Clears projection test seams on drop, including when an assertion fails.
struct ExchangeSeamGuard;

impl ExchangeSeamGuard {
    fn arm() -> Self {
        crash_hooks::inject_exchange_errno(0);
        crash_hooks::force_two_step(false);
        crash_hooks::disarm();
        Self
    }
}

impl Drop for ExchangeSeamGuard {
    fn drop(&mut self) {
        crash_hooks::inject_exchange_errno(0);
        crash_hooks::force_two_step(false);
        crash_hooks::disarm();
    }
}

fn repair_required(err: LifecycleError) -> (String, String) {
    match err {
        LifecycleError::CommittedRepairRequired { transaction_id, reason } => (transaction_id, reason),
        other => panic!("test: expected CommittedRepairRequired, got {other}"),
    }
}

fn live_binding(store: &SkillLifecycleStore) -> crate::skills::lifecycle::generation::ActiveBinding {
    store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound")
}

#[cfg(target_os = "macos")]
fn committed_transactions(store: &SkillLifecycleStore) -> Vec<TransactionId> {
    store
        .journal_entries_for(&skill())
        .expect("test: journal")
        .into_iter()
        .filter(|entry| entry.state == TransactionState::Committed)
        .map(|entry| entry.transaction_id)
        .collect()
}

#[cfg(unix)]
fn directory_identity(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path).expect("test: projection metadata");
    (meta.dev(), meta.ino())
}

#[cfg(target_os = "macos")]
const fn raw_exchange_errno(errno: rustix::io::Errno) -> i32 {
    errno.raw_os_error()
}

#[test]
fn first_publication_into_an_absent_target_still_renames_once() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let a = write(&store, &skill(), "alpha-desc");
    crash_hooks::inject_exchange_errno(libc_einval());
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: an absent target is one rename");
    assert_eq!(on_disk(&projection(tmp.path())), a.0);
    assert!(projection(tmp.path()).is_dir(), "the first projection is present");
    let bound = live_binding(&store);
    assert_eq!((bound.generation, bound.revision_id), (1, a.0));
    no_leftovers(tmp.path());
}

const fn libc_einval() -> i32 {
    rustix::io::Errno::INVAL.raw_os_error()
}

#[cfg(unix)]
#[test]
fn symlink_and_foreign_projection_paths_stay_refused() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let occupied = projection(tmp.path());
    fs::create_dir_all(occupied.parent().expect("test: skills parent")).expect("test: skills");
    fs::write(&occupied, b"foreign-file").expect("test: foreign file");
    let a = write(&store, &skill(), "alpha-desc");
    let err = store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect_err("test: a file is not a projection directory");
    assert!(matches!(err, LifecycleError::UnsafeStorePath { .. }), "{err}");
    assert_eq!(fs::read(&occupied).expect("test: read"), b"foreign-file");
    assert!(store.active_binding(&skill()).expect("test: binding").is_none());
    assert!(
        store
            .journal_entry(&TransactionId::parse("t1").expect("test: tx"))
            .expect("test: journal")
            .is_none()
    );

    let (tmp, store) = setup();
    let outside = TempDir::new().expect("test: outside");
    fs::write(outside.path().join("SKILL.md"), b"foreign-skill").expect("test: foreign skill");
    fs::create_dir_all(tmp.path().join("skills")).expect("test: skills");
    std::os::unix::fs::symlink(outside.path(), projection(tmp.path())).expect("test: symlink");
    let a = write(&store, &skill(), "alpha-desc");
    crash_hooks::inject_exchange_errno(libc_einval());
    let err = store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect_err("test: a foreign symlink is refused");
    assert!(matches!(err, LifecycleError::UnsafeStorePath { .. }), "{err}");
    assert!(store.active_binding(&skill()).expect("test: binding").is_none());
    assert_eq!(
        fs::read(outside.path().join("SKILL.md")).expect("test: read"),
        b"foreign-skill"
    );

    let (tmp, store) = setup();
    let outside = TempDir::new().expect("test: outside");
    fs::write(outside.path().join("SKILL.md"), b"foreign-skill").expect("test: foreign skill");
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    fs::remove_dir_all(projection(tmp.path())).expect("test: drop projection");
    std::os::unix::fs::symlink(outside.path(), projection(tmp.path())).expect("test: symlink");
    crash_hooks::inject_exchange_errno(libc_einval());
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: replacement of a symlink is refused");
    let (transaction_id, reason) = repair_required(err);
    assert_eq!(transaction_id, "t2");
    assert!(reason.contains("symlink"), "{reason}");
    assert!(
        fs::symlink_metadata(projection(tmp.path()))
            .expect("test: meta")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(outside.path().join("SKILL.md")).expect("test: read"),
        b"foreign-skill"
    );
    let bound = live_binding(&store);
    assert_eq!((bound.generation, bound.transaction_id.as_str()), (2, "t2"));
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
#[test]
fn supported_exchange_keeps_the_target_present_for_a_concurrent_reader() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;

    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("reader-a", &skill(), &a, 0, true))
        .expect("test: publish a");
    let dir = projection(tmp.path());
    let old_revision = a.0;
    let new_revision = b.0.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_reader = Arc::clone(&stop);
    let (ready_tx, ready_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut absent = false;
        let mut unreadable = false;
        let mut unexpected = false;
        let mut samples = 0_usize;
        let mut complete = 0_usize;
        let mut announced = false;
        while !stop_reader.load(Ordering::Acquire) {
            samples = samples.saturating_add(1);
            // Directory exchange is atomic. A path walk can still fail between
            // the swap and deletion of the previous tree; that is not absence.
            match fs::symlink_metadata(&dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => absent = true,
                Err(_) => unreadable = true,
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => absent = true,
                Ok(_) => {
                    if let Ok(bundle) = Bundle::from_dir(&dir) {
                        complete = complete.saturating_add(1);
                        let id = bundle.revision_id();
                        if id != &old_revision && id != &new_revision {
                            unexpected = true;
                        }
                    }
                }
            }
            if !announced {
                announced = true;
                let _ = ready_tx.send(());
            }
        }
        (absent, unreadable, unexpected, samples, complete)
    });
    ready_rx.recv().expect("test: reader observed the projection");
    store
        .publish(&publish_req("reader-b", &skill(), &b, 1, true))
        .expect("test: native exchange publishes");
    stop.store(true, Ordering::Release);
    let (absent, unreadable, unexpected, samples, complete) = reader.join().expect("test: reader joins");
    assert!(samples > 1, "the reader kept sampling through the replacement");
    assert!(complete > 0, "the reader never saw a complete projection");
    assert!(!absent, "the projection directory was absent during replacement");
    assert!(
        !unreadable,
        "the projection path could not be stated during replacement"
    );
    assert!(!unexpected, "the reader saw a revision other than the old or new tree");
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    no_leftovers(tmp.path());
}

#[cfg(target_os = "macos")]
#[test]
fn unsupported_exchange_errnos_refuse_before_the_old_tree_moves() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let a = write(&store, &skill(), "alpha-desc");
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    let path = projection(tmp.path());
    let identity = directory_identity(&path);
    let bytes = fs::read(path.join("SKILL.md")).expect("test: read");
    let classes = [
        rustix::io::Errno::INVAL,
        rustix::io::Errno::NOSYS,
        rustix::io::Errno::NOTSUP,
    ];
    let mut expected_generation = 1_u64;
    for (index, errno) in classes.into_iter().enumerate() {
        let rev = write(&store, &skill(), &format!("next-{index}"));
        let tx = format!("tx-errno-{index}");
        crash_hooks::inject_exchange_errno(raw_exchange_errno(errno));
        let err = store
            .publish(&publish_req(&tx, &skill(), &rev, expected_generation, true))
            .expect_err("test: unsupported exchange is refused");
        let (transaction_id, reason) = repair_required(err);
        assert_eq!(transaction_id, tx);
        assert!(reason.contains("was not moved"), "{errno:?} {reason}");
        assert!(path.is_dir(), "the projection stayed present for {errno:?}");
        assert_eq!(directory_identity(&path), identity, "{errno:?} moved the directory");
        assert_eq!(fs::read(path.join("SKILL.md")).expect("test: read"), bytes);
        assert_eq!(on_disk(&path), a.0, "{errno:?} changed the projected revision");
        let bound = live_binding(&store);
        assert_eq!(bound.generation, expected_generation.saturating_add(1));
        assert_eq!(bound.revision_id, rev.0);
        assert_eq!(bound.transaction_id.as_str(), tx);
        no_leftovers(tmp.path());
        expected_generation = expected_generation.saturating_add(1);
        crash_hooks::inject_exchange_errno(0);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_non_unsupported_exchange_error_leaves_the_existing_projection_unmoved() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    let path = projection(tmp.path());
    let identity = directory_identity(&path);
    crash_hooks::inject_exchange_errno(raw_exchange_errno(rustix::io::Errno::IO));
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: exchange i/o error");
    let (transaction_id, reason) = repair_required(err);
    assert_eq!(transaction_id, "t2");
    assert!(!reason.contains("was not moved"), "{reason}");
    assert_eq!(directory_identity(&path), identity);
    assert_eq!(on_disk(&path), a.0);
    assert!(path.is_dir());
    no_leftovers(tmp.path());
}

#[cfg(target_os = "macos")]
#[test]
fn forced_two_step_on_macos_refuses_before_the_old_tree_moves() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    let path = projection(tmp.path());
    let identity = directory_identity(&path);
    crash_hooks::force_two_step(true);
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: the macOS seam does not rename");
    let (transaction_id, reason) = repair_required(err);
    assert_eq!(transaction_id, "t2");
    assert!(reason.contains("was not moved"), "{reason}");
    assert!(path.is_dir());
    assert_eq!(directory_identity(&path), identity);
    assert_eq!(on_disk(&path), a.0);
    no_leftovers(tmp.path());
    let bound = live_binding(&store);
    assert_eq!((bound.generation, bound.transaction_id.as_str()), (2, "t2"));
}

#[test]
fn a_crash_after_cas_reports_committed_repair_and_keeps_the_old_tree() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    let path = projection(tmp.path());
    #[cfg(unix)]
    let identity = directory_identity(&path);
    crash_hooks::arm("after_committed");
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: crash after the journal commit");
    let (transaction_id, reason) = repair_required(err);
    assert_eq!(transaction_id, "t2");
    assert!(reason.contains("injected crash point"), "{reason}");
    assert!(path.is_dir(), "the crash leaves the old projection present");
    #[cfg(unix)]
    assert_eq!(directory_identity(&path), identity);
    assert_eq!(on_disk(&path), a.0);
    let bound = live_binding(&store);
    assert_eq!(
        (bound.generation, &bound.revision_id, bound.transaction_id.as_str()),
        (2, &b.0, "t2")
    );
    let entry = store
        .journal_entry(&TransactionId::parse("t2").expect("test: tx"))
        .expect("test: journal")
        .expect("test: entry");
    assert_eq!(entry.state, TransactionState::Committed);
    assert_eq!(entry.new_generation, 2);
    no_leftovers(tmp.path());
}

#[cfg(target_os = "macos")]
#[test]
fn repeated_repair_on_unsupported_exchange_is_idempotent() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    let path = projection(tmp.path());
    let identity = directory_identity(&path);
    crash_hooks::inject_exchange_errno(raw_exchange_errno(rustix::io::Errno::NOSYS));
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: refused");
    assert_eq!(repair_required(err).0, "t2");
    let retained = tmp.path().join(format!(
        "skills/.openprx-projection/deploy-helper.{}.old",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&retained).expect("test: retained tree");
    fs::write(retained.join("SKILL.md"), b"retained-old").expect("test: retained bytes");
    for _ in 0..3 {
        let report = store.recover(Duration::ZERO).expect("test: recover");
        assert!(report.projections.repaired.is_empty(), "repair must not republish");
        assert_eq!(report.projections.failed.len(), 1);
        let (failed_skill, _) = report.projections.failed.first().expect("test: failed repair");
        assert_eq!(failed_skill, &skill());
        assert!(path.is_dir(), "recovery left the projection absent");
        assert_eq!(directory_identity(&path), identity);
        assert_eq!(on_disk(&path), a.0);
        assert_eq!(
            fs::read(retained.join("SKILL.md")).expect("test: retained"),
            b"retained-old"
        );
        let bound = live_binding(&store);
        assert_eq!(
            (bound.generation, bound.transaction_id.as_str(), bound.revision_id),
            (2, "t2", b.0.clone())
        );
        assert_eq!(committed_transactions(&store).len(), 2, "repair advanced a generation");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn replaying_a_committed_request_keeps_its_transaction_and_repair_status() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    let path = projection(tmp.path());
    let identity = directory_identity(&path);
    crash_hooks::inject_exchange_errno(raw_exchange_errno(rustix::io::Errno::NOTSUP));
    let request = publish_req("t2", &skill(), &b, 1, true);
    let first = store.publish(&request).expect_err("test: first attempt");
    let (first_id, first_reason) = repair_required(first);
    let second = store.publish(&request).expect_err("test: replay");
    let (second_id, second_reason) = repair_required(second);
    assert_eq!(first_id, "t2");
    assert_eq!(second_id, "t2");
    assert!(first_reason.contains("was not moved"), "{first_reason}");
    assert!(second_reason.contains("was not moved"), "{second_reason}");
    let bound = live_binding(&store);
    assert_eq!(
        (bound.generation, bound.transaction_id.as_str(), bound.revision_id),
        (2, "t2", b.0)
    );
    assert_eq!(
        committed_transactions(&store),
        [
            TransactionId::parse("t1").expect("test: tx"),
            TransactionId::parse("t2").expect("test: tx")
        ]
    );
    assert_eq!(directory_identity(&path), identity);
    assert_eq!(on_disk(&path), a.0);
    assert!(path.is_dir());
}

#[cfg(target_os = "macos")]
#[test]
fn explicit_rollback_after_a_refused_replacement_uses_a_new_generation() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let (a, b, c) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
        write(&store, &skill(), "gamma-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    let path = projection(tmp.path());
    let identity = directory_identity(&path);
    crash_hooks::inject_exchange_errno(raw_exchange_errno(rustix::io::Errno::INVAL));
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: replacement refused");
    assert_eq!(repair_required(err).0, "t2");

    // The projection already holds A, so rollback to A reconciles without a swap.
    let rolled = store
        .rollback(&rollback_req("t3", &skill(), &a.0, 2))
        .expect("test: explicit rollback");
    assert!(!rolled.replayed);
    assert_eq!(rolled.entry.kind, TransactionKind::Rollback);
    assert_eq!(rolled.entry.new_generation, 3);
    assert_eq!(rolled.entry.candidate_revision_id, a.0);
    assert_eq!(on_disk(&path), a.0);
    assert_eq!(directory_identity(&path), identity);

    let err = store
        .publish(&publish_req("t4", &skill(), &c, 3, true))
        .expect_err("test: later replacement still refused");
    assert_eq!(repair_required(err).0, "t4");
    assert_eq!(on_disk(&path), a.0);
    assert_eq!(directory_identity(&path), identity);
    let bound = live_binding(&store);
    assert_eq!((bound.generation, bound.revision_id), (4, c.0));

    let err = store
        .rollback(&rollback_req("t5", &skill(), &b.0, 4))
        .expect_err("test: rollback onto a different tree is still repair");
    let (transaction_id, reason) = repair_required(err);
    assert_eq!(transaction_id, "t5");
    assert!(reason.contains("was not moved"), "{reason}");
    assert_eq!(on_disk(&path), a.0);
    assert_eq!(directory_identity(&path), identity);
    assert!(path.is_dir());
    let bound = live_binding(&store);
    assert_eq!(
        (bound.generation, bound.revision_id, bound.transaction_id.as_str()),
        (5, b.0.clone(), "t5")
    );

    let original = store
        .journal_entry(&TransactionId::parse("t2").expect("test: tx"))
        .expect("test: journal")
        .expect("test: t2");
    assert_eq!(original.state, TransactionState::Committed);
    assert_eq!(original.kind, TransactionKind::Publish);
    assert_eq!(original.new_generation, 2);
    assert_eq!(original.candidate_revision_id, b.0);
    let rollback = store
        .journal_entry(&TransactionId::parse("t5").expect("test: tx"))
        .expect("test: journal")
        .expect("test: t5");
    assert_eq!(rollback.state, TransactionState::Committed);
    assert_eq!(rollback.kind, TransactionKind::Rollback);
    assert_eq!(rollback.new_generation, 5);
}

/// T13 and T14 evidence for platforms whose replacement can still rename the
/// old tree aside. macOS does not compile this path.
#[cfg(not(target_os = "macos"))]
#[test]
fn historic_two_step_crash_and_failed_repair_remain_where_the_fallback_remains() {
    let _guard = ExchangeSeamGuard::arm();
    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    crash_hooks::force_two_step(true);
    crash_hooks::arm("projection_swapped_out");
    let err = store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: crash between the renames");
    assert!(matches!(err, LifecycleError::CommittedRepairRequired { .. }), "{err}");
    assert!(
        !projection(tmp.path()).exists(),
        "the skill is absent, not half-written"
    );
    let left = leftover_names(tmp.path());
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left.iter().all(|name| name.ends_with(".old")), "{left:?}");
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.projections.repaired, [skill()]);
    assert_eq!(on_disk(&projection(tmp.path())), b.0);

    let (tmp, store) = setup();
    let (a, b) = (
        write(&store, &skill(), "alpha-desc"),
        write(&store, &skill(), "beta-desc"),
    );
    store
        .publish(&publish_req("t1", &skill(), &a, 0, true))
        .expect("test: publish a");
    crash_hooks::force_two_step(true);
    crash_hooks::arm("projection_swapped_out");
    store
        .publish(&publish_req("t2", &skill(), &b, 1, true))
        .expect_err("test: crash between the renames");
    fs::write(projection(tmp.path()), b"in the way").expect("test: blocker");
    let report = store.recover(Duration::ZERO).expect("test: recover");
    assert_eq!(report.projections.failed.len(), 1);
    assert_eq!(report.swept_staging, 0, "nothing is swept for a failed skill");
    assert_eq!(leftover_names(tmp.path()).len(), 1, "the previous tree survives");
    fs::remove_file(projection(tmp.path())).expect("test: remove blocker");
    let report = store.recover(Duration::ZERO).expect("test: recover again");
    assert_eq!(report.projections.repaired, [skill()]);
    assert_eq!(on_disk(&projection(tmp.path())), b.0);
    no_leftovers(tmp.path());
}
