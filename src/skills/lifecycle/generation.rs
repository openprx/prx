//! Active generation binding with process-safe compare-and-swap.
//!
//! `active/<skill-id>.json` is the authority for which revision of a skill is
//! live. It is replaced only while holding the skill's advisory file lock
//! (`locks/<skill-id>.lock`, `flock` semantics: released by the kernel when the
//! holder dies, honoured across threads and processes), after re-reading the
//! current generation under that lock, and by an atomic rename of a fully
//! written and fsynced temporary file. Readers never need the lock: they see
//! either the old or the new complete file.

use super::journal::TransactionId;
use super::store::{
    STAGING_DIR, SkillLifecycleStore, check_private, ensure_real_dir, read_record, record_bytes, sync_dir,
    write_private_file,
};
use super::types::{LifecycleError, OwnerScope, RevisionId, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) const ACTIVE_DIR: &str = "active";
pub(super) const LOCKS_DIR: &str = "locks";
const LOCK_POLL: Duration = Duration::from_millis(5);

/// The authoritative active revision of one skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveBinding {
    pub schema_version: u32,
    pub owner_scope: OwnerScope,
    pub skill_id: SkillId,
    pub revision_id: RevisionId,
    /// Pinned `record_digest()` of the active revision.
    pub record_digest: String,
    /// Increases by one on every publish and rollback; never rewound.
    pub generation: u64,
    /// Journal transaction that produced this binding.
    pub transaction_id: TransactionId,
    pub updated_at: DateTime<Utc>,
}

/// Proof that the skill's publication lock is held; released on drop.
#[derive(Debug)]
pub(super) struct SkillLock {
    file: fs::File,
    skill: SkillId,
}

impl SkillLock {
    pub(super) const fn skill(&self) -> &SkillId {
        &self.skill
    }
}

impl Drop for SkillLock {
    fn drop(&mut self) {
        if let Err(e) = FileExt::unlock(&self.file) {
            tracing::warn!(skill = %self.skill, error = %e, "failed to release skill publication lock");
        }
    }
}

/// Creates `dir` below `root` when missing and syncs `root` so the entry is
/// durable; refuses symlinks and loose permissions.
pub(super) fn ensure_dir_synced(root: &Path, dir: &Path) -> Result<(), LifecycleError> {
    let existed = fs::symlink_metadata(dir).is_ok();
    ensure_real_dir(dir, true)?;
    if !existed {
        sync_dir(root)?;
    }
    Ok(())
}

impl SkillLifecycleStore {
    fn binding_path(&self, skill: &SkillId) -> PathBuf {
        self.root().join(ACTIVE_DIR).join(format!("{}.json", skill.as_str()))
    }

    /// Current binding of `skill`, `None` when nothing was ever published.
    pub fn active_binding(&self, skill: &SkillId) -> Result<Option<ActiveBinding>, LifecycleError> {
        let path = self.binding_path(skill);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&path, e)),
        }
        check_private(&path)?;
        let binding: ActiveBinding = read_record(&path)?;
        if binding.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "active binding",
                found: binding.schema_version,
            });
        }
        if binding.owner_scope != *self.owner_scope() {
            return Err(LifecycleError::OwnerMismatch {
                expected: self.owner_scope().to_string(),
                found: binding.owner_scope.to_string(),
            });
        }
        if binding.skill_id != *skill {
            return Err(LifecycleError::Tampered {
                path,
                reason: "binding skill id does not match its file name".to_string(),
            });
        }
        Ok(Some(binding))
    }

    /// Generation of the active binding; 0 when nothing was published.
    pub(super) fn current_generation(&self, skill: &SkillId) -> Result<u64, LifecycleError> {
        Ok(self.active_binding(skill)?.map_or(0, |b| b.generation))
    }

    pub(super) fn ensure_publication_dirs(&self) -> Result<(), LifecycleError> {
        for name in [ACTIVE_DIR, LOCKS_DIR, super::journal::PUBLICATION_DIR] {
            ensure_dir_synced(self.root(), &self.root().join(name))?;
        }
        Ok(())
    }

    /// Takes the exclusive publication lock of `skill`, waiting up to `timeout`.
    pub(super) fn lock_skill(&self, skill: &SkillId, timeout: Duration) -> Result<SkillLock, LifecycleError> {
        let locks = self.root().join(LOCKS_DIR);
        ensure_dir_synced(self.root(), &locks)?;
        let path = locks.join(format!("{}.lock", skill.as_str()));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).map_err(|e| LifecycleError::io(&path, e))?;
        check_private(&path)?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    return Ok(SkillLock {
                        file,
                        skill: skill.clone(),
                    });
                }
                Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                    if Instant::now() >= deadline {
                        return Err(LifecycleError::LockTimeout { path });
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(e) => return Err(LifecycleError::io(&path, e)),
            }
        }
    }

    /// Writes `bytes` to a fresh fsynced temporary file in `staging/`.
    fn stage_file(&self, bytes: &[u8]) -> Result<PathBuf, LifecycleError> {
        let tmp = self
            .root()
            .join(STAGING_DIR)
            .join(format!("atomic-{}.tmp", uuid::Uuid::new_v4()));
        write_private_file(&tmp, bytes, false)?;
        Ok(tmp)
    }

    /// Atomically renames a staged file onto `target` and syncs its directory.
    fn commit_staged(&self, tmp: &Path, target: &Path) -> Result<(), LifecycleError> {
        if let Err(e) = fs::rename(tmp, target) {
            if let Err(rm) = fs::remove_file(tmp) {
                tracing::warn!(path = %tmp.display(), error = %rm, "failed to remove staged file after rename failure");
            }
            return Err(LifecycleError::io(target, e));
        }
        target.parent().map_or(Ok(()), sync_dir)
    }

    /// Durable atomic replacement of `target` with `bytes`.
    pub(super) fn write_atomic(&self, target: &Path, bytes: &[u8]) -> Result<(), LifecycleError> {
        let tmp = self.stage_file(bytes)?;
        self.commit_staged(&tmp, target)
    }

    /// Compare-and-swap of the active binding. Under `lock`, the current
    /// generation must equal `expected_generation`; otherwise
    /// [`LifecycleError::StaleGeneration`] is returned and nothing is written.
    pub(super) fn cas_binding(
        &self,
        lock: &SkillLock,
        new: &ActiveBinding,
        expected_generation: u64,
    ) -> Result<(), LifecycleError> {
        if lock.skill() != &new.skill_id {
            return Err(LifecycleError::UnsafeStorePath {
                path: self.binding_path(&new.skill_id),
                reason: "publication lock belongs to a different skill".to_string(),
            });
        }
        let actual = self.current_generation(&new.skill_id)?;
        if actual != expected_generation || expected_generation.checked_add(1) != Some(new.generation) {
            return Err(LifecycleError::StaleGeneration {
                skill: new.skill_id.to_string(),
                expected: expected_generation,
                actual,
            });
        }
        let path = self.binding_path(&new.skill_id);
        let tmp = self.stage_file(&record_bytes(new, &path)?)?;
        crash_point!("binding_staged");
        self.commit_staged(&tmp, &path)
    }
}
