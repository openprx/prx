//! Owner-scoped store of immutable skill revisions.
//!
//! Layout below `<owner workspace>/self/skill-lifecycle/`:
//!
//! ```text
//! schema.json
//! staging/<uuid>/                       same-filesystem staging, renamed into place
//! revisions/<skill-id>/<revision-id>/
//!   revision.json
//!   bundle/...
//! ```
//!
//! The store never opens `brain.db` and never writes outside its root.

use super::bundle::{BUNDLE_FORMAT_VERSION, Bundle};
use super::manifest::{ManifestInfo, validate_manifest, validate_manifest_strict};
use super::types::{
    LifecycleError, NewRevision, OwnerScope, RevisionId, SCHEMA_VERSION, SkillId, SkillRevision, SourceKind,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const SELF_DIR: &str = "self";
const STORE_DIR: &str = "skill-lifecycle";
const SCHEMA_FILE: &str = "schema.json";
const REVISIONS_DIR: &str = "revisions";
pub(super) const STAGING_DIR: &str = "staging";
const REVISION_FILE: &str = "revision.json";
const BUNDLE_DIR: &str = "bundle";
pub(super) const MAX_RECORD_BYTES: u64 = 256 * 1024;

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const PRIVATE_EXEC_MODE: u32 = 0o700;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreSchema {
    schema_version: u32,
    owner_scope: OwnerScope,
    bundle_format_version: u32,
    created_at: DateTime<Utc>,
    /// Unix mode enforced on directories of the store; `None` without mode bits.
    enforced_dir_mode: Option<u32>,
    enforced_file_mode: Option<u32>,
}

/// Result of publishing a revision.
#[derive(Clone, Debug)]
pub struct WriteOutcome {
    pub revision: SkillRevision,
    /// `false` when an identical, verified revision already existed.
    pub created: bool,
    /// Canonical digest of the stored record (see [`record_digest`]).
    pub record_digest: String,
}

/// A revision whose bundle hash, owner scope and manifest were re-verified.
#[derive(Clone, Debug)]
pub struct VerifiedRevision {
    pub revision: SkillRevision,
    /// Canonical digest of the stored record (see [`record_digest`]).
    pub record_digest: String,
    pub bundle: Bundle,
}

/// Version of the record digest encoding.
const RECORD_DIGEST_VERSION: u32 = 1;
const RECORD_DIGEST_DOMAIN: &[u8] = b"prx-skill-record\0";

/// Canonical digest of a revision record: lowercase hex SHA-256 of
/// `b"prx-skill-record\0"`, the `u32` big-endian digest version (1) and the
/// compact `serde_json` serialization of the record (fixed field order, no
/// maps). A digest stored inside the same directory does not authenticate the
/// record, because whoever can edit the record can edit the digest too. The
/// digest is meant to be pinned elsewhere, for example by the SL08 publication
/// journal, and passed to [`SkillLifecycleStore::read_revision`].
pub fn record_digest(revision: &SkillRevision) -> Result<String, LifecycleError> {
    let bytes = serde_json::to_vec(revision).map_err(|e| LifecycleError::InvalidRecord {
        path: PathBuf::from(REVISION_FILE),
        reason: e.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DIGEST_DOMAIN);
    hasher.update(RECORD_DIGEST_VERSION.to_be_bytes());
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Name of the first provenance field in which `new` differs from `record`.
fn provenance_difference(record: &SkillRevision, new: &NewRevision) -> Option<&'static str> {
    if record.parent_revision_id != new.parent_revision_id {
        Some("parent_revision_id")
    } else if record.source_kind != new.source_kind {
        Some("source_kind")
    } else if record.source_commit != new.source_commit {
        Some("source_commit")
    } else if record.proposal_id != new.proposal_id {
        Some("proposal_id")
    } else if record.pattern_ids != new.pattern_ids {
        Some("pattern_ids")
    } else if record.capability_requirements != new.capability_requirements
        && (new.source_kind == SourceKind::Learned
            || !new
                .capability_requirements
                .iter()
                .all(|name| record.capability_requirements.contains(name)))
    {
        // A learned revision must match exactly. For the others the manifests
        // may have added requirements (the record lists the union), and a
        // revision stored before the key existed lists none.
        Some("capability_requirements")
    } else if record.review_receipt_ids != new.review_receipt_ids {
        Some("review_receipt_ids")
    } else if record.validation_receipt_ids != new.validation_receipt_ids {
        Some("validation_receipt_ids")
    } else {
        None
    }
}

/// Handle to one owner's lifecycle store.
#[derive(Debug)]
pub struct SkillLifecycleStore {
    root: PathBuf,
    owner: OwnerScope,
}

impl SkillLifecycleStore {
    /// Opens (creating when absent) the store of `owner` inside an already
    /// verified owner workspace. The caller supplies the workspace directory
    /// resolved by the runtime for that owner; this function additionally
    /// refuses symlinked store components, loosened permissions, a store that
    /// belongs to a different owner scope and unsupported schema versions.
    pub fn open(workspace_dir: &Path, owner: OwnerScope) -> Result<Self, LifecycleError> {
        let workspace = workspace_dir
            .canonicalize()
            .map_err(|e| LifecycleError::WorkspaceUnavailable {
                path: workspace_dir.to_path_buf(),
                reason: e.to_string(),
            })?;
        if !workspace.is_dir() {
            return Err(LifecycleError::WorkspaceUnavailable {
                path: workspace,
                reason: "not a directory".to_string(),
            });
        }
        let self_dir = workspace.join(SELF_DIR);
        ensure_real_dir(&self_dir, false)?;
        let root = self_dir.join(STORE_DIR);
        ensure_real_dir(&root, true)?;
        ensure_real_dir(&root.join(STAGING_DIR), true)?;
        ensure_real_dir(&root.join(REVISIONS_DIR), true)?;
        let store = Self { root, owner };
        store.ensure_schema()?;
        Ok(store)
    }

    /// Opens an existing store without creating anything. `Ok(None)` means the
    /// owner workspace has no lifecycle store (nothing is managed). A store
    /// that exists but belongs to another owner scope is
    /// [`LifecycleError::OwnerMismatch`], and any unsafe component is refused.
    pub fn open_existing(workspace_dir: &Path, owner: OwnerScope) -> Result<Option<Self>, LifecycleError> {
        let workspace = workspace_dir
            .canonicalize()
            .map_err(|e| LifecycleError::WorkspaceUnavailable {
                path: workspace_dir.to_path_buf(),
                reason: e.to_string(),
            })?;
        let self_dir = workspace.join(SELF_DIR);
        let root = self_dir.join(STORE_DIR);
        for path in [&self_dir, &root] {
            match fs::symlink_metadata(path) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        return Err(LifecycleError::UnsafeStorePath {
                            path: path.clone(),
                            reason: "must be a real directory, not a symlink or file".to_string(),
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(LifecycleError::io(path, e)),
            }
        }
        check_private(&root)?;
        let schema = root.join(SCHEMA_FILE);
        if fs::symlink_metadata(&schema).is_err() {
            return Ok(None);
        }
        let store = Self { root, owner };
        store.verify_schema()?;
        Ok(Some(store))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn owner_scope(&self) -> &OwnerScope {
        &self.owner
    }

    fn ensure_schema(&self) -> Result<(), LifecycleError> {
        let path = self.root.join(SCHEMA_FILE);
        if fs::symlink_metadata(&path).is_err() {
            let schema = StoreSchema {
                schema_version: SCHEMA_VERSION,
                owner_scope: self.owner.clone(),
                bundle_format_version: BUNDLE_FORMAT_VERSION,
                created_at: Utc::now(),
                enforced_dir_mode: private_dir_mode(),
                enforced_file_mode: private_file_mode(),
            };
            let bytes = record_bytes(&schema, &path)?;
            let tmp = self
                .root
                .join(STAGING_DIR)
                .join(format!("schema-{}.tmp", uuid::Uuid::new_v4()));
            write_private_file(&tmp, &bytes, false)?;
            // A hard link publishes the complete file atomically and refuses to
            // replace a schema another process created first.
            let linked = fs::hard_link(&tmp, &path);
            if let Err(e) = fs::remove_file(&tmp) {
                tracing::warn!(path = %tmp.display(), error = %e, "failed to remove temporary skill store schema file");
            }
            match linked {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(LifecycleError::io(&path, e)),
            }
        }
        self.verify_schema()
    }

    fn verify_schema(&self) -> Result<(), LifecycleError> {
        let path = self.root.join(SCHEMA_FILE);
        let schema: StoreSchema = read_record(&path)?;
        if schema.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "store",
                found: schema.schema_version,
            });
        }
        if schema.bundle_format_version != BUNDLE_FORMAT_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "bundle format",
                found: schema.bundle_format_version,
            });
        }
        self.check_owner(&schema.owner_scope)?;
        check_private(&path)
    }

    fn check_owner(&self, found: &OwnerScope) -> Result<(), LifecycleError> {
        if *found == self.owner {
            Ok(())
        } else {
            Err(LifecycleError::OwnerMismatch {
                expected: self.owner.to_string(),
                found: found.to_string(),
            })
        }
    }

    pub(super) fn bundle_dir(&self, skill: &SkillId, revision: &RevisionId) -> PathBuf {
        self.revision_dir(skill, revision).join(BUNDLE_DIR)
    }

    /// Reads `revision.json` only (no bundle hashing) and accepts it when its
    /// canonical digest equals `digest`, which an active binding pins. This is
    /// enough to learn the authentic display name without verifying the whole
    /// bundle.
    pub(super) fn pinned_revision_record(
        &self,
        skill: &SkillId,
        revision: &RevisionId,
        digest: &str,
    ) -> Result<SkillRevision, LifecycleError> {
        let path = self.revision_dir(skill, revision).join(REVISION_FILE);
        check_private(&path)?;
        let record: SkillRevision = read_record(&path)?;
        self.check_owner(&record.owner_scope)?;
        if record.skill_id != *skill || record.revision_id != *revision || record_digest(&record)? != digest {
            return Err(LifecycleError::Tampered {
                path,
                reason: "revision record does not match the digest pinned by the binding".to_string(),
            });
        }
        Ok(record)
    }

    pub(super) fn check_owner_scope(&self, found: &OwnerScope) -> Result<(), LifecycleError> {
        self.check_owner(found)
    }

    fn revision_dir(&self, skill: &SkillId, revision: &RevisionId) -> PathBuf {
        self.root
            .join(REVISIONS_DIR)
            .join(skill.as_str())
            .join(revision.as_str())
    }

    /// Publishes `bundle` as an immutable revision of `new.skill_id`.
    ///
    /// The bundle is validated (manifest included), staged on the same
    /// filesystem, re-read and re-hashed, then renamed into place. A revision
    /// that already exists is verified and returned unchanged when `new`
    /// carries identical provenance; different provenance is a
    /// [`LifecycleError::RevisionConflict`]. An existing revision is never
    /// overwritten, and a mismatching one is reported as tampered.
    ///
    /// Staging happens in `staging/`; a crash can leave a stale directory
    /// there, which the SL08 recovery pass is responsible for sweeping.
    ///
    /// Raw store write with no lifecycle policy, `pub(in crate::skills)`;
    /// learned candidates are written solely by `install_candidate`, and test
    /// fixtures elsewhere use `test_support`.
    pub(in crate::skills) fn write_revision(
        &self,
        new: &NewRevision,
        bundle: &Bundle,
    ) -> Result<WriteOutcome, LifecycleError> {
        let info = validate_manifest(bundle)?;
        // A revision that is not learned takes its required tools from its
        // own manifests. A learned one keeps exactly what its baseline had
        // (`install_candidate` copies them): it can neither add nor remove a
        // required tool, which would move the adherence oracle.
        let original = new;
        let mut declared = new.clone();
        if new.source_kind != SourceKind::Learned {
            declared
                .capability_requirements
                .extend(info.required_tools.iter().cloned());
            declared.capability_requirements.sort();
            declared.capability_requirements.dedup();
        }
        let new = &declared;
        let revision_id = bundle.revision_id().clone();
        let final_dir = self.revision_dir(&new.skill_id, &revision_id);
        if fs::symlink_metadata(&final_dir).is_ok() {
            // An idempotent rewrite: the caller's own list must fit the stored
            // record; what the manifests add is not the caller's claim.
            return self.existing_outcome(&final_dir, original, &revision_id);
        }

        if original.source_kind != SourceKind::Learned {
            // A new operator, import or upstream revision may not declare a
            // malformed or mis-shaped `required-tools`; only reading is lenient.
            validate_manifest_strict(bundle)?;
        }

        let skill_dir = self.root.join(REVISIONS_DIR).join(new.skill_id.as_str());
        ensure_real_dir(&skill_dir, true)?;
        sync_dir(&self.root.join(REVISIONS_DIR))?;
        let staged = self.root.join(STAGING_DIR).join(uuid::Uuid::new_v4().to_string());
        // Shared store lock: exclusive startup recovery must not sweep a
        // staging directory while it is being renamed into place.
        let _store_lock = self.lock_store(true, super::publish::PUBLICATION_LOCK_TIMEOUT)?;
        let result = self.stage_and_publish(new, bundle, &info, &revision_id, &staged, &final_dir);
        if staged.exists()
            && let Err(e) = fs::remove_dir_all(&staged)
        {
            tracing::warn!(path = %staged.display(), error = %e, "failed to clean skill revision staging directory");
        }
        result
    }

    /// Verifies an already published revision and checks that the caller's
    /// provenance matches it, so a differing request is never silently dropped.
    fn existing_outcome(
        &self,
        dir: &Path,
        new: &NewRevision,
        revision_id: &RevisionId,
    ) -> Result<WriteOutcome, LifecycleError> {
        let verified = self.verify_dir(dir, &new.skill_id, revision_id)?;
        if let Some(field) = provenance_difference(&verified.revision, new) {
            return Err(LifecycleError::RevisionConflict {
                revision: format!("{}/{revision_id}", new.skill_id),
                field,
            });
        }
        Ok(WriteOutcome {
            revision: verified.revision,
            created: false,
            record_digest: verified.record_digest,
        })
    }

    /// Stages, verifies and renames a revision into `final_dir`. When the
    /// rename loses a race with another writer, the winner is verified and
    /// compared with `new` exactly like a pre-existing revision.
    pub(super) fn stage_and_publish(
        &self,
        new: &NewRevision,
        bundle: &Bundle,
        info: &ManifestInfo,
        revision_id: &RevisionId,
        staged: &Path,
        final_dir: &Path,
    ) -> Result<WriteOutcome, LifecycleError> {
        create_private_dir(staged)?;
        let bundle_dir = staged.join(BUNDLE_DIR);
        create_private_dir(&bundle_dir)?;
        for file in bundle.files() {
            let target = bundle_dir.join(file.path());
            if let Some(parent) = target.parent() {
                create_private_dir_all(parent)?;
            }
            write_private_file(&target, file.bytes(), file.is_executable())?;
        }
        let revision = SkillRevision {
            schema_version: SCHEMA_VERSION,
            record_id: revision_id.to_string(),
            owner_scope: self.owner.clone(),
            skill_id: new.skill_id.clone(),
            revision_id: revision_id.clone(),
            parent_revision_id: new.parent_revision_id.clone(),
            display_name: info.name.clone(),
            description: info.description.clone(),
            semantic_version: info.semantic_version.clone(),
            bundle_manifest_hash: revision_id.digest_hex().to_string(),
            bundle_format_version: BUNDLE_FORMAT_VERSION,
            source_kind: new.source_kind,
            source_commit: new.source_commit.clone(),
            proposal_id: new.proposal_id.clone(),
            pattern_ids: new.pattern_ids.clone(),
            capability_requirements: new.capability_requirements.clone(),
            review_receipt_ids: new.review_receipt_ids.clone(),
            validation_receipt_ids: new.validation_receipt_ids.clone(),
            created_at: Utc::now(),
            recorded_dir_mode: actual_dir_mode(staged)?,
        };
        let record_path = staged.join(REVISION_FILE);
        write_private_file(&record_path, &record_bytes(&revision, &record_path)?, false)?;
        sync_tree_dirs(&bundle_dir)?;
        sync_dir(staged)?;

        // Re-read what was written; a staging bug or disk fault must not
        // publish content that does not match its identity.
        self.verify_dir(staged, &new.skill_id, revision_id)?;

        match fs::rename(staged, final_dir) {
            Ok(()) => {
                if let Some(parent) = final_dir.parent() {
                    sync_dir(parent)?;
                }
                let record_digest = record_digest(&revision)?;
                Ok(WriteOutcome {
                    revision,
                    created: true,
                    record_digest,
                })
            }
            Err(e) => {
                if fs::symlink_metadata(final_dir).is_ok() {
                    // Another writer published the same revision first.
                    self.existing_outcome(final_dir, new, revision_id)
                } else {
                    Err(LifecycleError::io(final_dir, e))
                }
            }
        }
    }

    /// Reads a revision and verifies its record, owner scope, manifest and the
    /// complete bundle hash. With `expected_record_digest` the record's
    /// canonical digest must match too, which authenticates the fields that
    /// are not derived from the bundle (provenance, receipts, timestamps).
    pub fn read_revision(
        &self,
        skill: &SkillId,
        revision: &RevisionId,
        expected_record_digest: Option<&str>,
    ) -> Result<VerifiedRevision, LifecycleError> {
        let dir = self.revision_dir(skill, revision);
        match fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(LifecycleError::RevisionNotFound(format!("{skill}/{revision}")));
            }
            Err(e) => return Err(LifecycleError::io(&dir, e)),
        }
        let verified = self.verify_dir(&dir, skill, revision)?;
        if let Some(expected) = expected_record_digest
            && verified.record_digest != expected
        {
            return Err(LifecycleError::Tampered {
                path: dir.join(REVISION_FILE),
                reason: "record digest does not match the pinned digest".to_string(),
            });
        }
        Ok(verified)
    }

    fn verify_dir(
        &self,
        dir: &Path,
        skill: &SkillId,
        revision: &RevisionId,
    ) -> Result<VerifiedRevision, LifecycleError> {
        let tampered = |reason: String| LifecycleError::Tampered {
            path: dir.to_path_buf(),
            reason,
        };
        let meta = fs::symlink_metadata(dir).map_err(|e| LifecycleError::io(dir, e))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(tampered("revision path is not a real directory".to_string()));
        }
        let canonical = dir.canonicalize().map_err(|e| LifecycleError::io(dir, e))?;
        if !canonical.starts_with(&self.root) {
            return Err(LifecycleError::UnsafeStorePath {
                path: dir.to_path_buf(),
                reason: "revision resolves outside the owner store".to_string(),
            });
        }
        check_private_tree(dir)?;

        let mut names = Vec::new();
        for item in fs::read_dir(dir).map_err(|e| LifecycleError::io(dir, e))? {
            let item = item.map_err(|e| LifecycleError::io(dir, e))?;
            names.push(item.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        if names != [BUNDLE_DIR, REVISION_FILE] {
            return Err(tampered(format!("unexpected revision directory contents {names:?}")));
        }

        let record_path = dir.join(REVISION_FILE);
        let record: SkillRevision = read_record(&record_path)?;
        if record.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "revision",
                found: record.schema_version,
            });
        }
        self.check_owner(&record.owner_scope)?;
        if record.skill_id != *skill
            || record.revision_id != *revision
            || record.record_id != revision.as_str()
            || record.bundle_manifest_hash != revision.digest_hex()
            || record.bundle_format_version != BUNDLE_FORMAT_VERSION
        {
            return Err(tampered("record identity does not match its location".to_string()));
        }

        let bundle = Bundle::from_dir(&dir.join(BUNDLE_DIR)).map_err(|e| tampered(format!("bundle invalid: {e}")))?;
        if bundle.revision_id() != revision {
            return Err(tampered(format!(
                "bundle hash {} does not match revision id {revision}",
                bundle.revision_id()
            )));
        }
        let info = validate_manifest(&bundle).map_err(|e| tampered(format!("manifest invalid: {e}")))?;
        if (&info.name, &info.description, &info.semantic_version)
            != (&record.display_name, &record.description, &record.semantic_version)
        {
            return Err(tampered(
                "record metadata does not match the bundle manifest".to_string(),
            ));
        }
        let record_digest = record_digest(&record)?;
        Ok(VerifiedRevision {
            revision: record,
            record_digest,
            bundle,
        })
    }
}

pub(super) fn record_bytes<T: Serialize>(value: &T, path: &Path) -> Result<Vec<u8>, LifecycleError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| LifecycleError::InvalidRecord {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(super) fn read_record<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, LifecycleError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).map_err(|e| LifecycleError::io(path, e))?;
    let meta = file.metadata().map_err(|e| LifecycleError::io(path, e))?;
    if !meta.is_file() || meta.len() > MAX_RECORD_BYTES {
        return Err(LifecycleError::InvalidRecord {
            path: path.to_path_buf(),
            reason: "not a regular file within the record size limit".to_string(),
        });
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| LifecycleError::io(path, e))?;
    serde_json::from_slice(&bytes).map_err(|e| LifecycleError::InvalidRecord {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Requires `path` to be a real (non-symlink) directory, creating it with
/// owner-only permissions when absent. With `enforce_private`, the directory
/// must already be owner-only.
pub(super) fn ensure_real_dir(path: &Path, enforce_private: bool) -> Result<(), LifecycleError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(LifecycleError::UnsafeStorePath {
                    path: path.to_path_buf(),
                    reason: "must be a real directory, not a symlink or file".to_string(),
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match create_private_dir(path) {
            Ok(()) => {}
            Err(LifecycleError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        },
        Err(e) => return Err(LifecycleError::io(path, e)),
    }
    if enforce_private {
        check_private(path)?;
    }
    Ok(())
}

#[cfg(unix)]
const fn private_dir_mode() -> Option<u32> {
    Some(PRIVATE_DIR_MODE)
}
#[cfg(not(unix))]
const fn private_dir_mode() -> Option<u32> {
    None
}
#[cfg(unix)]
const fn private_file_mode() -> Option<u32> {
    Some(PRIVATE_FILE_MODE)
}
#[cfg(not(unix))]
const fn private_file_mode() -> Option<u32> {
    None
}

fn create_private_dir(path: &Path) -> Result<(), LifecycleError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(PRIVATE_DIR_MODE);
    }
    builder.create(path).map_err(|e| LifecycleError::io(path, e))
}

fn create_private_dir_all(path: &Path) -> Result<(), LifecycleError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(PRIVATE_DIR_MODE);
    }
    builder.create(path).map_err(|e| LifecycleError::io(path, e))
}

pub(super) fn write_private_file(path: &Path, bytes: &[u8], executable: bool) -> Result<(), LifecycleError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable {
            PRIVATE_EXEC_MODE
        } else {
            PRIVATE_FILE_MODE
        });
    }
    #[cfg(not(unix))]
    let _ = executable;
    let mut file = options.open(path).map_err(|e| LifecycleError::io(path, e))?;
    file.write_all(bytes).map_err(|e| LifecycleError::io(path, e))?;
    file.sync_all().map_err(|e| LifecycleError::io(path, e))
}

/// Syncs `path` and every directory below it.
pub(super) fn sync_tree_dirs(path: &Path) -> Result<(), LifecycleError> {
    for item in fs::read_dir(path).map_err(|e| LifecycleError::io(path, e))? {
        let item = item.map_err(|e| LifecycleError::io(path, e))?;
        if item
            .file_type()
            .map_err(|e| LifecycleError::io(item.path(), e))?
            .is_dir()
        {
            sync_tree_dirs(&item.path())?;
        }
    }
    sync_dir(path)
}

pub(super) fn sync_dir(path: &Path) -> Result<(), LifecycleError> {
    fs::File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| LifecycleError::io(path, e))
}

#[cfg(unix)]
fn actual_dir_mode(path: &Path) -> Result<Option<u32>, LifecycleError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path).map_err(|e| LifecycleError::io(path, e))?;
    Ok(Some(meta.permissions().mode() & 0o777))
}
#[cfg(not(unix))]
fn actual_dir_mode(_path: &Path) -> Result<Option<u32>, LifecycleError> {
    Ok(None)
}

/// Verifies actual owner-only permissions and ownership of one entry.
#[cfg(unix)]
pub(super) fn check_private(path: &Path) -> Result<(), LifecycleError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = fs::symlink_metadata(path).map_err(|e| LifecycleError::io(path, e))?;
    if meta.file_type().is_symlink() {
        return Err(LifecycleError::UnsafeStorePath {
            path: path.to_path_buf(),
            reason: "symlink".to_string(),
        });
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(LifecycleError::PermissionsTooOpen {
            path: path.to_path_buf(),
            mode,
        });
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(LifecycleError::UnsafeStorePath {
            path: path.to_path_buf(),
            reason: "not owned by the current user".to_string(),
        });
    }
    Ok(())
}
#[cfg(not(unix))]
pub(super) fn check_private(_path: &Path) -> Result<(), LifecycleError> {
    Ok(())
}

fn check_private_tree(path: &Path) -> Result<(), LifecycleError> {
    check_private(path)?;
    let meta = fs::symlink_metadata(path).map_err(|e| LifecycleError::io(path, e))?;
    if meta.is_dir() {
        for item in fs::read_dir(path).map_err(|e| LifecycleError::io(path, e))? {
            let item = item.map_err(|e| LifecycleError::io(path, e))?;
            check_private_tree(&item.path())?;
        }
    }
    Ok(())
}
