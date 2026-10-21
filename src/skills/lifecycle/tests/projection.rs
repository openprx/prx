#![cfg(test)]

//! FINDING 108.2 and 108.6: the compatibility projection and the caches follow
//! the committed binding, and a failure after the commit is reported as
//! `CommittedRepairRequired` and repaired by recovery.

use super::{exec_entry, md, owner};
use crate::skills::lifecycle::publish::crash_hooks;
use crate::skills::lifecycle::{
    Bundle, BundleEntry, Correlation, LifecycleError, ManagedReadRequest, NewRevision, PublishRequest, RevisionId,
    RollbackRequest, SkillId, SkillLifecycleStore, SourceKind, TransactionId, apply_migration, plan_migration,
    read_managed_resource, release_task_pins,
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
