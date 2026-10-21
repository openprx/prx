#![cfg(test)]

use super::{md, owner};
use crate::skills::lifecycle::NewRevision;
use crate::skills::lifecycle::{
    Bundle, Correlation, ManagedReadRequest, MigrationAction, MigrationEntry, SkillId, SkillLifecycleStore, SourceKind,
    apply_migration, plan_migration, read_managed_resource,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const OWNER: &str = "owner:ws:cli:alice";

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("test: parent")).expect("test: mkdir");
    fs::write(path, bytes).expect("test: write");
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("test: read_dir") {
            let entry = entry.expect("test: entry");
            let path = entry.path();
            let kind = entry.file_type().expect("test: type");
            if kind.is_dir() {
                walk(&path, out);
            } else if kind.is_symlink() {
                out.insert(path, b"<symlink>".to_vec());
            } else {
                out.insert(path.clone(), fs::read(&path).expect("test: read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

fn workspace() -> TempDir {
    let tmp = TempDir::new().expect("test: tempdir");
    let skills = tmp.path().join("skills");
    write(&skills.join("alpha/SKILL.md"), md("Alpha", "First skill").as_bytes());
    write(&skills.join("alpha/references/note.md"), b"note\r\n");
    write(&skills.join("beta/SKILL.md"), md("Beta", "Second skill").as_bytes());
    write(
        &skills.join("beta/.openprx-untrusted-origin.json"),
        br#"{"trusted":false,"source":"https://example.invalid/beta.git","review_required":true}"#,
    );
    write(&skills.join("gamma/SKILL.md"), md("Gamma", "Disabled skill").as_bytes());
    write(&skills.join(".openprx-disabled/gamma"), b"");
    write(
        &skills.join("toml-only/SKILL.toml"),
        b"[skill]\nname='t'\ndescription='d'\n",
    );
    write(
        &skills.join("conflict/SKILL.md"),
        md("Conflict", "From markdown").as_bytes(),
    );
    write(
        &skills.join("conflict/SKILL.toml"),
        b"[skill]\nname='Conflict'\ndescription='From toml'\n",
    );
    write(
        &skills.join("dup/SKILL.md"),
        md("alpha", "Same name as alpha").as_bytes(),
    );
    write(&skills.join("README.md"), b"not a skill");
    tmp
}

fn find<'a>(entries: &'a [MigrationEntry], directory: &str) -> &'a MigrationEntry {
    entries
        .iter()
        .find(|entry| entry.directory == directory)
        .expect("test: entry planned")
}

#[test]
fn dry_run_classifies_everything_and_changes_nothing() {
    let tmp = workspace();
    let outside = TempDir::new().expect("test: tempdir");
    write(&outside.path().join("SKILL.md"), md("Shared", "External").as_bytes());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("skills/shared")).expect("test: symlink");
        write(
            &tmp.path().join("skills/inner/SKILL.md"),
            md("Inner", "Has a link").as_bytes(),
        );
        std::os::unix::fs::symlink("SKILL.md", tmp.path().join("skills/inner/link.md")).expect("test: symlink");
    }
    let before = snapshot(tmp.path());
    let plan = plan_migration(tmp.path(), None).expect("test: plan");
    assert_eq!(snapshot(tmp.path()), before, "a dry run must not touch the workspace");
    assert!(!tmp.path().join("self").exists());

    let dirs: Vec<&str> = plan.entries.iter().map(|e| e.directory.as_str()).collect();
    assert!(!dirs.contains(&"README.md") && !dirs.contains(&".openprx-disabled"));
    assert!(matches!(
        find(&plan.entries, "alpha").action,
        MigrationAction::Import { enabled: true, .. }
    ));
    assert!(matches!(
        find(&plan.entries, "beta").action,
        MigrationAction::Import { enabled: true, .. }
    ));
    assert!(matches!(
        find(&plan.entries, "gamma").action,
        MigrationAction::Import { enabled: false, .. }
    ));
    for refused in ["toml-only", "conflict", "dup"] {
        assert!(
            matches!(find(&plan.entries, refused).action, MigrationAction::Refused { .. }),
            "{refused}"
        );
    }
    #[cfg(unix)]
    for refused in ["shared", "inner"] {
        assert!(
            matches!(find(&plan.entries, refused).action, MigrationAction::Refused { .. }),
            "{refused}"
        );
    }
    assert_eq!(
        find(&plan.entries, "alpha").skill_id,
        Some(SkillId::from_legacy_dir("alpha").expect("test: id"))
    );
}

#[test]
fn apply_imports_with_preserved_bytes_markers_and_enable_state() {
    let tmp = workspace();
    let skills_before = snapshot(&tmp.path().join("skills"));
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let plan = plan_migration(tmp.path(), Some(&store)).expect("test: plan");
    let report = apply_migration(&store, tmp.path(), &plan);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.imported.len(), 3);
    assert_eq!(
        snapshot(&tmp.path().join("skills")),
        skills_before,
        "the legacy projection is never rewritten"
    );

    let alpha = SkillId::from_legacy_dir("alpha").expect("test: id");
    let binding = store
        .active_binding(&alpha)
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!((binding.generation, binding.enabled), (1, true));
    let verified = store
        .read_revision(&alpha, &binding.revision_id, None)
        .expect("test: revision");
    let legacy = Bundle::from_dir(&tmp.path().join("skills/alpha")).expect("test: legacy bundle");
    assert_eq!(verified.revision.revision_id, *legacy.revision_id());
    let note = verified.bundle.file("references/note.md").expect("test: note");
    assert_eq!(note.bytes(), b"note\r\n");
    assert_eq!(verified.revision.source_kind, SourceKind::Operator);

    let beta = SkillId::from_legacy_dir("beta").expect("test: id");
    let beta_binding = store
        .active_binding(&beta)
        .expect("test: binding")
        .expect("test: bound");
    let beta_rev = store
        .read_revision(&beta, &beta_binding.revision_id, None)
        .expect("test: revision");
    assert_eq!(beta_rev.revision.source_kind, SourceKind::Upstream);
    assert!(beta_rev.bundle.file(".openprx-untrusted-origin.json").is_some());

    let gamma = SkillId::from_legacy_dir("gamma").expect("test: id");
    assert!(
        !store
            .active_binding(&gamma)
            .expect("test: binding")
            .expect("test: bound")
            .enabled
    );
    assert!(
        store
            .active_binding(&SkillId::from_legacy_dir("conflict").expect("test: id"))
            .expect("test: binding")
            .is_none()
    );
}

#[test]
fn apply_is_idempotent_and_drift_is_reported() {
    let tmp = workspace();
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let plan = plan_migration(tmp.path(), Some(&store)).expect("test: plan");
    apply_migration(&store, tmp.path(), &plan);

    let again = plan_migration(tmp.path(), Some(&store)).expect("test: replan");
    assert!(matches!(
        find(&again.entries, "alpha").action,
        MigrationAction::AlreadyImported { .. }
    ));
    let report = apply_migration(&store, tmp.path(), &again);
    assert!(report.imported.is_empty() && report.failed.is_empty());

    write(&tmp.path().join("skills/alpha/references/note.md"), b"edited later");
    let drift = plan_migration(tmp.path(), Some(&store)).expect("test: replan");
    assert!(matches!(
        find(&drift.entries, "alpha").action,
        MigrationAction::Drifted { .. }
    ));
    let report = apply_migration(&store, tmp.path(), &drift);
    assert!(
        report.imported.is_empty(),
        "drift is reported, never imported over the binding"
    );
}

#[test]
fn apply_refuses_a_directory_that_changed_since_the_plan() {
    let tmp = workspace();
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let plan = plan_migration(tmp.path(), Some(&store)).expect("test: plan");
    write(
        &tmp.path().join("skills/alpha/references/note.md"),
        b"changed after the plan",
    );
    let report = apply_migration(&store, tmp.path(), &plan);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed.first().map(|f| f.0.as_str()), Some("alpha"));
    assert_eq!(report.imported.len(), 2);
    assert!(
        store
            .active_binding(&SkillId::from_legacy_dir("alpha").expect("test: id"))
            .expect("test: binding")
            .is_none()
    );
}

#[test]
fn imported_skills_read_like_the_legacy_directory() {
    let tmp = workspace();
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let plan = plan_migration(tmp.path(), Some(&store)).expect("test: plan");
    apply_migration(&store, tmp.path(), &plan);
    for (resource, legacy) in [
        (None, "alpha/SKILL.md"),
        (Some("references/note.md"), "alpha/references/note.md"),
    ] {
        let managed = read_managed_resource(&ManagedReadRequest {
            workspace_dir: tmp.path(),
            owner: &owner(OWNER),
            skill_name: "alpha",
            resource_path: resource,
            task_id: None,
            correlation: Correlation::default(),
            pin_task: false,
            max_bytes: 64 * 1024,
        })
        .expect("test: read")
        .expect("test: managed");
        let expected = fs::read_to_string(tmp.path().join("skills").join(legacy)).expect("test: legacy");
        assert_eq!(managed.content, expected);
    }
}

fn import_all(tmp: &TempDir) -> (SkillLifecycleStore, crate::skills::lifecycle::MigrationPlan) {
    let store = SkillLifecycleStore::open(tmp.path(), owner(OWNER)).expect("test: store");
    let plan = plan_migration(tmp.path(), Some(&store)).expect("test: plan");
    (store, plan)
}

#[test]
fn reapplying_the_same_plan_after_commit_replays() {
    let tmp = workspace();
    let (store, plan) = import_all(&tmp);
    let first = apply_migration(&store, tmp.path(), &plan);
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    let again = apply_migration(&store, tmp.path(), &plan);
    assert!(again.failed.is_empty(), "a replay is not a failure: {:?}", again.failed);
    assert_eq!(again.imported, first.imported);
    let alpha = SkillId::from_legacy_dir("alpha").expect("test: id");
    let binding = store
        .active_binding(&alpha)
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(binding.generation, 1, "the replay must not publish a second generation");
}

#[test]
fn a_plan_made_before_someone_else_published_is_stale() {
    let tmp = workspace();
    let (store, plan) = import_all(&tmp);
    let alpha = SkillId::from_legacy_dir("alpha").expect("test: id");
    let other = Bundle::from_entries(vec![super::entry(
        "SKILL.md",
        md("Alpha", "Published elsewhere").as_bytes(),
    )])
    .expect("test: bundle");
    let written = store
        .write_revision(&NewRevision::new(alpha.clone(), SourceKind::Learned), &other)
        .expect("test: write");
    // The operator removed the legacy directory before the other publication;
    // a first publication never replaces a different, unmanaged tree.
    fs::remove_dir_all(tmp.path().join("skills/alpha")).expect("test: remove legacy dir");
    super::activate(
        &store,
        &alpha,
        &(written.revision.revision_id.clone(), written.record_digest),
        0,
        true,
    );

    // The legacy directory is back as planned (the other publication projected
    // into its own directory), so only the generation check can refuse.
    write(
        &tmp.path().join("skills/alpha/SKILL.md"),
        md("Alpha", "First skill").as_bytes(),
    );
    write(&tmp.path().join("skills/alpha/references/note.md"), b"note\r\n");
    let report = apply_migration(&store, tmp.path(), &plan);
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert_eq!(report.failed.first().map(|f| f.0.as_str()), Some("alpha"));
    assert!(
        report.failed.first().is_some_and(|f| f.1.contains("generation")),
        "{:?}",
        report.failed
    );
    let binding = store
        .active_binding(&alpha)
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(
        binding.revision_id, written.revision.revision_id,
        "their revision is preserved"
    );
    assert_eq!(binding.generation, 1);
}

#[test]
fn an_import_aborted_by_a_crash_is_retried_with_a_new_attempt() {
    use crate::skills::lifecycle::publish::crash_hooks;
    let tmp = workspace();
    let (store, plan) = import_all(&tmp);
    crash_hooks::arm("after_prepared");
    let crashed = apply_migration(&store, tmp.path(), &plan);
    crash_hooks::disarm();
    assert_eq!(crashed.failed.len(), 1, "{:?}", crashed.failed);
    assert_eq!(crashed.failed.first().map(|f| f.0.as_str()), Some("alpha"));
    let alpha = SkillId::from_legacy_dir("alpha").expect("test: id");
    assert!(store.active_binding(&alpha).expect("test: binding").is_none());

    // The first attempt's id is stuck PREPARED/ABORTED; a retry derives the next one.
    let (store, plan) = import_all(&tmp);
    assert!(matches!(
        find(&plan.entries, "alpha").action,
        MigrationAction::Import { .. }
    ));
    let retry = apply_migration(&store, tmp.path(), &plan);
    assert!(retry.failed.is_empty(), "{:?}", retry.failed);
    assert_eq!(retry.imported.len(), 1);
    let binding = store
        .active_binding(&alpha)
        .expect("test: binding")
        .expect("test: bound");
    assert_eq!(binding.generation, 1);
}
