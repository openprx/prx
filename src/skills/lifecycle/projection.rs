//! Compatibility projection of lifecycle-managed skills (spec 7 and 8.3 steps
//! 7-8).
//!
//! `active/<skill-id>.json` is the authority; `skills/<directory>/` is a plain
//! directory copy of the active revision's bundle so that existing consumers
//! (the discovery catalog, the legacy loader) keep working. After every
//! binding commit, publish and rollback call [`SkillLifecycleStore::refresh_projection`]
//! under the skill lock, and the startup recovery sweep calls
//! [`SkillLifecycleStore::repair_projections`] for any projection a crash or a
//! failure left behind a committed binding.
//!
//! The copy is immutable and replaced as a whole: the new tree is written and
//! fsynced under `skills/.openprx-projection/`, which is on the same file
//! system as `skills/`, then renamed into place. On Linux the swap is one
//! `renameat2(RENAME_EXCHANGE)`, so a reader sees the old tree or the new one.
//! Elsewhere the old tree is renamed away first, which leaves a short window
//! in which the skill is absent (never half-written); the sweep restores a
//! projection that is absent and keeps the `.old` tree of any skill whose
//! repair failed. No symlink is created or followed.
//!
//! A first publication refuses to replace an existing `skills/<dir>/` that is
//! not its own revision (an unmanaged tree). After that the bound directory
//! belongs to the lifecycle: later generations and rollbacks overwrite it, and
//! recovery rewrites it when it differs from the active binding and resets its
//! disabled marker to the binding's `enabled` state on every pass (see
//! `sync_disabled_marker`).
//!
//! Only skills that have an active binding are touched. A legacy skill with no
//! binding is never read or written here.

use super::bundle::Bundle;
use super::generation::{ActiveBinding, SkillLock};
use super::store::{SkillLifecycleStore, sync_dir, sync_tree_dirs};
use super::types::{LifecycleError, RevisionId, SkillId};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Hidden directory below `skills/` used for staging and replaced trees.
pub(super) const PROJECTION_STAGING_DIR: &str = ".openprx-projection";
/// Marker directory of the legacy loader (`skills/.openprx-disabled/<dir>`).
const DISABLED_MARKER_DIR: &str = ".openprx-disabled";
/// Upper bound on `skills/` entries scanned to find a projection directory.
const MAX_PROJECTION_SCAN: usize = 1024;
const UUID_LEN: usize = 36;
const NEW_SUFFIX: &str = ".new";
const OLD_SUFFIX: &str = ".old";

/// What [`SkillLifecycleStore::refresh_projection`] found and did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProjectionChange {
    /// The directory already held exactly the committed revision.
    Unchanged,
    /// The directory was written (first projection, or replaced).
    Written,
}

/// Outcome of the projection pass of the recovery sweep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionRepair {
    /// Skills whose projection was missing or stale and was rewritten.
    pub repaired: Vec<SkillId>,
    /// Skills that could not be repaired, with the reason. The next sweep or a
    /// retry of the committed transaction tries again.
    pub failed: Vec<(SkillId, String)>,
    /// Set when the active bindings could not be listed at all, so no skill
    /// was examined. The sweep then keeps every replaced (`.old`) tree.
    pub listing_error: Option<String>,
    /// Skills whose previous projection tree (`.old`) must be kept because
    /// their repair failed.
    pub(super) keep: Vec<SkillId>,
}

fn unsafe_path(path: &Path, reason: &str) -> LifecycleError {
    LifecycleError::UnsafeStorePath {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

impl SkillLifecycleStore {
    /// The owner workspace this store lives in (`<workspace>/self/skill-lifecycle`).
    fn workspace_dir(&self) -> Result<&Path, LifecycleError> {
        self.root()
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| unsafe_path(self.root(), "the store root has no workspace parent"))
    }

    /// Directory below `skills_root` that projects `skill`: the legacy
    /// directory the skill was imported from when there is one, otherwise a
    /// directory named after the skill id.
    fn projection_dir_name(&self, skills_root: &Path, skill: &SkillId) -> Result<String, LifecycleError> {
        let entries = match fs::read_dir(skills_root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(skill.to_string()),
            Err(e) => return Err(LifecycleError::io(skills_root, e)),
        };
        let mut scanned = 0_usize;
        for entry in entries {
            let entry = entry.map_err(|e| LifecycleError::io(skills_root, e))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            scanned = scanned.saturating_add(1);
            if scanned > MAX_PROJECTION_SCAN {
                return Err(LifecycleError::InvalidRecord {
                    path: skills_root.to_path_buf(),
                    reason: format!("more than {MAX_PROJECTION_SCAN} skill directories"),
                });
            }
            if SkillId::from_legacy_dir(&name).is_ok_and(|id| &id == skill) {
                return Ok(name);
            }
        }
        Ok(skill.to_string())
    }

    /// Verifies, before the binding commit, that the projection of a
    /// transition can be written: `skills/` and its staging directory are real
    /// directories, the directory scan stays within its cap, and a first
    /// publication does not land on a tree the skill does not own. Nothing is
    /// swapped; a refusal here leaves no mutation.
    pub(super) fn precheck_projection(
        &self,
        skill: &SkillId,
        candidate: &RevisionId,
        has_predecessor: bool,
    ) -> Result<(), LifecycleError> {
        let (_, skills_root, _) = self.projection_roots()?;
        let directory = self.projection_dir_name(&skills_root, skill)?;
        if !has_predecessor {
            refuse_unmanaged(&skills_root.join(directory), candidate)?;
        }
        Ok(())
    }

    /// Workspace, `skills/` and `skills/.openprx-projection/`, each ensured to
    /// be a real directory (never a symlink).
    fn projection_roots(&self) -> Result<(&Path, PathBuf, PathBuf), LifecycleError> {
        let workspace = self.workspace_dir()?;
        let skills_root = crate::skills::skills_dir(workspace);
        ensure_plain_dir(&skills_root)?;
        let staging_root = skills_root.join(PROJECTION_STAGING_DIR);
        ensure_plain_dir(&staging_root)?;
        Ok((workspace, skills_root, staging_root))
    }

    /// Makes `skills/<directory>/` hold exactly the bytes of the binding's
    /// revision, syncs the disabled marker with the binding and drops the
    /// cached catalog snapshots of the workspace. Idempotent; the caller holds
    /// the skill lock. The revision is verified against the binding's pinned
    /// record digest first, so a tampered revision is never projected.
    pub(super) fn refresh_projection(
        &self,
        lock: &SkillLock,
        binding: &ActiveBinding,
    ) -> Result<ProjectionChange, LifecycleError> {
        if lock.skill() != &binding.skill_id {
            return Err(unsafe_path(
                self.root(),
                "publication lock belongs to a different skill",
            ));
        }
        let verified = self.read_revision(&binding.skill_id, &binding.revision_id, Some(&binding.record_digest))?;
        let (workspace, skills_root, staging_root) = self.projection_roots()?;
        let directory = self.projection_dir_name(&skills_root, &binding.skill_id)?;
        let target = skills_root.join(&directory);

        let change = if projection_is_current(&target, &verified.bundle) {
            ProjectionChange::Unchanged
        } else {
            // Defence for the pre-commit refusal: the first generation of a
            // skill never replaces a tree that is not its own revision.
            if binding.generation <= 1 {
                refuse_unmanaged(&target, &binding.revision_id)?;
            }
            let stage = staging_root.join(format!("{}.{}{NEW_SUFFIX}", binding.skill_id, uuid::Uuid::new_v4()));
            let staged = write_tree(&stage, &verified.bundle).and_then(|()| {
                crash_point!("projection_staged");
                swap_in(&stage, &target, &staging_root, &binding.skill_id)
            });
            if let Err(e) = staged {
                remove_tree(&stage);
                return Err(e);
            }
            sync_dir(&skills_root)?;
            ProjectionChange::Written
        };
        sync_disabled_marker(&skills_root, &directory, binding.enabled)?;
        crash_point!("projection_written");
        // Cached catalog snapshots carry descriptions and their embeddings.
        // The process-level embedding map is keyed by the description text, so
        // a changed description can never be served a stale vector.
        crate::skills::invalidate_skill_catalog(workspace);
        Ok(change)
    }

    /// Recovery pass: for every active binding, rewrites a projection that is
    /// missing or differs from the committed revision, and resets the
    /// `skills/.openprx-disabled/<dir>` marker of every bound skill to the
    /// binding's `enabled` state (a gateway toggle of a bound skill does not
    /// survive recovery). Bounded by [`super::binding::MAX_ACTIVE_BINDINGS`]
    /// and idempotent. It never fails as a whole: every problem becomes an
    /// entry of [`ProjectionRepair::failed`] and the pass continues.
    pub(super) fn repair_projections(&self, lock_timeout: Duration) -> ProjectionRepair {
        let mut report = ProjectionRepair::default();
        let bindings = match self.list_active_bindings() {
            Ok(bindings) => bindings,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list active skill bindings for projection repair");
                report.listing_error = Some(e.to_string());
                return report;
            }
        };
        for listed in bindings {
            let skill = listed.skill_id;
            let outcome = self.lock_skill(&skill, lock_timeout).and_then(|lock| {
                // Re-read under the lock: a writer may have moved the binding.
                self.active_binding(&skill)?
                    .map(|binding| self.refresh_projection(&lock, &binding))
                    .transpose()
            });
            match outcome {
                Ok(Some(ProjectionChange::Written)) => report.repaired.push(skill),
                Ok(Some(ProjectionChange::Unchanged) | None) => {}
                Err(e) => {
                    tracing::warn!(skill = %skill, error = %e, "failed to repair a skill projection");
                    report.failed.push((skill.clone(), e.to_string()));
                    report.keep.push(skill);
                }
            }
        }
        report
    }

    /// Removes staged (`.new`) and replaced (`.old`) projection trees older
    /// than `min_age` whose skill lock is free. The `.old` tree of a skill in
    /// `repair.keep` is kept (its repair failed, so it may be the only copy),
    /// and so is every `.old` tree when the bindings could not be listed.
    /// Never fails: problems are logged and the entry is left for the next
    /// sweep. Returns how many were removed.
    pub(super) fn sweep_projection_staging(&self, min_age: Duration, repair: &ProjectionRepair) -> usize {
        let keep = repair.keep.as_slice();
        let keep_all_old = repair.listing_error.is_some();
        let staging_root = match self.workspace_dir() {
            Ok(workspace) => crate::skills::skills_dir(workspace).join(PROJECTION_STAGING_DIR),
            Err(e) => {
                tracing::warn!(error = %e, "cannot locate the skill projection staging directory");
                return 0;
            }
        };
        let entries = match fs::read_dir(&staging_root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
            Err(e) => {
                tracing::warn!(path = %staging_root.display(), error = %e, "cannot read the skill projection staging directory");
                return 0;
            }
        };
        let mut removed = 0_usize;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(path = %staging_root.display(), error = %e, "cannot read a skill projection staging entry");
                    continue;
                }
            };
            let path = entry.path();
            let age = match fs::symlink_metadata(&path).and_then(|meta| meta.modified()) {
                Ok(modified) => SystemTime::now().duration_since(modified).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "cannot read the age of a skill projection leftover; treating it as new");
                    continue;
                }
            };
            if age < min_age {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let owner = staged_tree_skill(&name);
            let is_old = name.ends_with(OLD_SUFFIX);
            if is_old && (keep_all_old || owner.as_ref().is_some_and(|skill| keep.contains(skill))) {
                continue;
            }
            let _skill_lock = match owner {
                Some(skill) => match self.lock_skill(&skill, Duration::ZERO) {
                    Ok(lock) => Some(lock),
                    Err(LifecycleError::LockTimeout { .. }) => continue,
                    Err(e) => {
                        tracing::warn!(skill = %skill, error = %e, "cannot lock a skill to sweep its projection leftover");
                        continue;
                    }
                },
                None => None,
            };
            let result = if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match result {
                Ok(()) => removed = removed.saturating_add(1),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to sweep a skill projection leftover");
                }
            }
        }
        removed
    }
}

/// Creates `path` when missing; refuses a symlink or a non-directory.
fn ensure_plain_dir(path: &Path) -> Result<(), LifecycleError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            Err(unsafe_path(path, "must be a real directory, not a symlink or file"))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(LifecycleError::io(path, e)),
        },
        Err(e) => Err(LifecycleError::io(path, e)),
    }
}

/// Refuses to project onto an existing tree that is not the candidate's own
/// revision: such a tree belongs to the operator, not to the lifecycle.
fn refuse_unmanaged(target: &Path, candidate: &RevisionId) -> Result<(), LifecycleError> {
    match fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(LifecycleError::io(target, e)),
        Ok(meta) => {
            if meta.is_dir() && Bundle::from_dir(target).is_ok_and(|found| found.revision_id() == candidate) {
                Ok(())
            } else {
                Err(unsafe_path(
                    target,
                    "an unmanaged skill already occupies this directory; it is never replaced by a first publication",
                ))
            }
        }
    }
}

/// Skill of a `<skill-id>.<uuid>.new|.old` leftover, `None` for other names.
fn staged_tree_skill(name: &str) -> Option<SkillId> {
    let stem = name
        .strip_suffix(NEW_SUFFIX)
        .or_else(|| name.strip_suffix(OLD_SUFFIX))?;
    let skill = stem.get(..stem.len().checked_sub(UUID_LEN + 1)?)?;
    SkillId::parse(skill).ok().or_else(|| SkillId::reserved(skill))
}

/// `true` when `target` is a real directory whose content hashes to the
/// bundle's revision (a missing, symlinked or differing tree is not current).
fn projection_is_current(target: &Path, bundle: &Bundle) -> bool {
    fs::symlink_metadata(target).is_ok_and(|meta| meta.is_dir())
        && Bundle::from_dir(target).is_ok_and(|found| found.revision_id() == bundle.revision_id())
}

/// Writes the bundle below a fresh `stage` directory and fsyncs files and
/// directories.
fn write_tree(stage: &Path, bundle: &Bundle) -> Result<(), LifecycleError> {
    fs::create_dir(stage).map_err(|e| LifecycleError::io(stage, e))?;
    for file in bundle.files() {
        let path = stage.join(file.path());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| LifecycleError::io(parent, e))?;
        }
        write_file(&path, file.bytes(), file.is_executable())?;
    }
    sync_tree_dirs(stage)
}

fn write_file(path: &Path, bytes: &[u8], executable: bool) -> Result<(), LifecycleError> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| LifecycleError::io(path, e))?;
    file.write_all(bytes).map_err(|e| LifecycleError::io(path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| LifecycleError::io(path, e))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    file.sync_all().map_err(|e| LifecycleError::io(path, e))
}

/// Moves the staged tree onto `target`, replacing what is there. The previous
/// tree ends up under `staging_root` as `.old` and is removed best-effort.
fn swap_in(stage: &Path, target: &Path, staging_root: &Path, skill: &SkillId) -> Result<(), LifecycleError> {
    match fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fs::rename(stage, target).map_err(|e| LifecycleError::io(target, e));
        }
        Err(e) => return Err(LifecycleError::io(target, e)),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(unsafe_path(
                target,
                "the projection path must be a real directory, not a symlink or file",
            ));
        }
        Ok(_) => {}
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        use rustix::io::Errno;
        if !two_step_forced() {
            match renameat_with(CWD, stage, CWD, target, RenameFlags::EXCHANGE) {
                Ok(()) => {
                    // `stage` now names the previous tree.
                    remove_tree(stage);
                    return Ok(());
                }
                Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {
                    tracing::warn!(
                        "renameat2 exchange is unsupported here; replacing the skill projection in two steps"
                    );
                }
                Err(errno) => return Err(LifecycleError::io(target, std::io::Error::from(errno))),
            }
        }
    }
    let old = staging_root.join(format!("{skill}.{}{OLD_SUFFIX}", uuid::Uuid::new_v4()));
    fs::rename(target, &old).map_err(|e| LifecycleError::io(target, e))?;
    crash_point!("projection_swapped_out");
    if let Err(e) = fs::rename(stage, target) {
        // Put the previous tree back so the skill does not disappear.
        if let Err(restore) = fs::rename(&old, target) {
            tracing::warn!(error = %restore, path = %target.display(), "failed to restore the previous skill projection");
        }
        return Err(LifecycleError::io(target, e));
    }
    remove_tree(&old);
    Ok(())
}

/// Test seam: forces the two-rename fallback so it runs on Linux too.
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
fn two_step_forced() -> bool {
    super::publish::crash_hooks::two_step_forced()
}

#[cfg(all(not(test), any(target_os = "linux", target_os = "android")))]
const fn two_step_forced() -> bool {
    false
}

fn remove_tree(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "failed to remove a skill projection leftover"),
    }
}

/// Creates or removes `skills/.openprx-disabled/<directory>` so discovery
/// agrees with the binding's enabled state.
fn sync_disabled_marker(skills_root: &Path, directory: &str, enabled: bool) -> Result<(), LifecycleError> {
    let marker_dir = skills_root.join(DISABLED_MARKER_DIR);
    let marker = marker_dir.join(directory);
    if enabled {
        return match fs::remove_file(&marker) {
            Ok(()) => sync_dir(&marker_dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(LifecycleError::io(&marker, e)),
        };
    }
    if marker.is_file() {
        return Ok(());
    }
    fs::create_dir_all(&marker_dir).map_err(|e| LifecycleError::io(&marker_dir, e))?;
    fs::write(&marker, b"").map_err(|e| LifecycleError::io(&marker, e))?;
    sync_dir(&marker_dir)
}
