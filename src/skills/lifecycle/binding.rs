//! Active bindings, task revision pins and pinned reads (SL02).
//!
//! `active/<skill-id>.json` names the revision a skill currently resolves to.
//! A task that is issued a [`PinnedSkillRevision`] keeps reading that revision
//! after a newer binding is written; revisions are immutable and a read through
//! a pin verifies the bundle hash, owner scope and the pinned record digest.

use super::bundle::{Bundle, BundleFile};
use super::store::{
    SkillLifecycleStore, VerifiedRevision, check_private, ensure_real_dir, read_record, record_bytes, sync_dir,
    write_private_file,
};
use super::types::{
    ActiveSkillBinding, LifecycleError, OwnerScope, PinnedSkillRevision, RevisionId, SCHEMA_VERSION, SkillId,
};
use chrono::Utc;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

const BINDING_EXTENSION: &str = "json";
/// Upper bound on bindings listed for one owner store.
pub const MAX_ACTIVE_BINDINGS: usize = 512;
/// Upper bound on concurrently remembered task pins in this process.
pub const MAX_TASK_PINS: usize = 4096;
const UNTRUSTED_ORIGIN_MARKER: &str = ".openprx-untrusted-origin.json";

impl SkillLifecycleStore {
    fn binding_path(&self, skill: &SkillId) -> PathBuf {
        self.active_dir().join(format!("{skill}.{BINDING_EXTENSION}"))
    }

    /// Reads and validates the binding of `skill`; `None` when it has none.
    pub fn read_binding(&self, skill: &SkillId) -> Result<Option<ActiveSkillBinding>, LifecycleError> {
        let path = self.binding_path(skill);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&path, e)),
        }
        check_private(&path)?;
        let binding: ActiveSkillBinding = read_record(&path)?;
        if binding.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "binding",
                found: binding.schema_version,
            });
        }
        self.check_owner_scope(&binding.owner_scope)?;
        if binding.skill_id != *skill || binding.record_id != binding_record_id(skill) || binding.generation == 0 {
            return Err(LifecycleError::InvalidRecord {
                path,
                reason: "binding identity does not match its location".to_string(),
            });
        }
        Ok(Some(binding))
    }

    /// Lists every binding of the store in skill-id order.
    pub fn list_bindings(&self) -> Result<Vec<ActiveSkillBinding>, LifecycleError> {
        let dir = self.active_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LifecycleError::io(&dir, e)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem =
                name.strip_suffix(&format!(".{BINDING_EXTENSION}"))
                    .ok_or_else(|| LifecycleError::InvalidRecord {
                        path: entry.path(),
                        reason: "unexpected entry in the active binding directory".to_string(),
                    })?;
            ids.push(SkillId::parse(stem)?);
            if ids.len() > MAX_ACTIVE_BINDINGS {
                return Err(LifecycleError::InvalidRecord {
                    path: dir,
                    reason: format!("more than {MAX_ACTIVE_BINDINGS} active bindings"),
                });
            }
        }
        ids.sort();
        let mut bindings = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(binding) = self.read_binding(&id)? {
                bindings.push(binding);
            }
        }
        Ok(bindings)
    }

    /// Finds the binding whose display name equals `name` (ASCII
    /// case-insensitive, like the catalog). Two skills sharing a name are an
    /// error rather than a silent choice.
    pub fn find_binding_by_name(&self, name: &str) -> Result<Option<ActiveSkillBinding>, LifecycleError> {
        let mut found: Option<ActiveSkillBinding> = None;
        for binding in self.list_bindings()? {
            if binding.display_name.eq_ignore_ascii_case(name) {
                if found.is_some() {
                    return Err(LifecycleError::AmbiguousSkillName(name.to_string()));
                }
                found = Some(binding);
            }
        }
        Ok(found)
    }

    /// Points `skill` at `revision`. `expected_generation` is `None` to create
    /// the first binding (generation 1) and `Some(n)` to replace generation `n`
    /// (new generation `n + 1`); anything else is a
    /// [`LifecycleError::GenerationConflict`]. The revision must verify and
    /// belong to `skill`. This is a single-writer primitive: the cross-process
    /// lock, journal and authorization of publication are SL08.
    pub fn write_binding(
        &self,
        skill: &SkillId,
        revision: &RevisionId,
        enabled: bool,
        expected_generation: Option<u64>,
    ) -> Result<ActiveSkillBinding, LifecycleError> {
        let verified = self.read_revision(skill, revision, None)?;
        let current = self.read_binding(skill)?;
        let conflict = |actual: Option<u64>| LifecycleError::GenerationConflict {
            skill: skill.to_string(),
            expected: expected_generation,
            actual,
        };
        let generation = match (&current, expected_generation) {
            (None, None) => 1,
            (Some(current), Some(expected)) if current.generation == expected => expected
                .checked_add(1)
                .ok_or_else(|| conflict(Some(current.generation)))?,
            (current, _) => return Err(conflict(current.as_ref().map(|c| c.generation))),
        };
        let binding = ActiveSkillBinding {
            schema_version: SCHEMA_VERSION,
            record_id: binding_record_id(skill),
            owner_scope: self.owner_scope().clone(),
            skill_id: skill.clone(),
            revision_id: revision.clone(),
            generation,
            enabled,
            display_name: verified.revision.display_name,
            created_at: Utc::now(),
        };
        let active = self.active_dir();
        ensure_real_dir(&active, true)?;
        let path = self.binding_path(skill);
        let tmp = self.staging_dir().join(format!("binding-{}.tmp", uuid::Uuid::new_v4()));
        write_private_file(&tmp, &record_bytes(&binding, &path)?, false)?;
        let published = if current.is_none() {
            // A hard link never replaces a binding another writer created first.
            fs::hard_link(&tmp, &path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    conflict(None)
                } else {
                    LifecycleError::io(&path, e)
                }
            })
        } else {
            fs::rename(&tmp, &path).map_err(|e| LifecycleError::io(&path, e))
        };
        if tmp.exists()
            && let Err(e) = fs::remove_file(&tmp)
        {
            tracing::warn!(path = %tmp.display(), error = %e, "failed to remove temporary binding file");
        }
        published?;
        sync_dir(&active)?;
        Ok(binding)
    }

    /// Resolves the active binding of `skill` into a pin. `None` when the skill
    /// has no binding. The revision is verified, and the binding must agree
    /// with the revision record.
    pub fn pin_active(&self, skill: &SkillId) -> Result<Option<PinnedSkillRevision>, LifecycleError> {
        let Some(binding) = self.read_binding(skill)? else {
            return Ok(None);
        };
        self.pin_binding(&binding).map(Some)
    }

    /// Pins the revision named by `binding`.
    pub fn pin_binding(&self, binding: &ActiveSkillBinding) -> Result<PinnedSkillRevision, LifecycleError> {
        let verified = self.read_revision(&binding.skill_id, &binding.revision_id, None)?;
        if verified.revision.display_name != binding.display_name {
            return Err(LifecycleError::Tampered {
                path: self.binding_path(&binding.skill_id),
                reason: "binding display name does not match its revision".to_string(),
            });
        }
        Ok(PinnedSkillRevision {
            owner_scope: binding.owner_scope.clone(),
            skill_id: binding.skill_id.clone(),
            revision_id: binding.revision_id.clone(),
            generation: binding.generation,
            display_name: binding.display_name.clone(),
            record_digest: verified.record_digest,
        })
    }

    /// Reads the pinned revision, verifying its bundle hash, owner scope and
    /// the digest recorded in the pin. Never follows a newer binding.
    pub fn read_pinned(&self, pin: &PinnedSkillRevision) -> Result<VerifiedRevision, LifecycleError> {
        self.check_owner_scope(&pin.owner_scope)?;
        self.read_revision(&pin.skill_id, &pin.revision_id, Some(&pin.record_digest))
    }
}

fn binding_record_id(skill: &SkillId) -> String {
    format!("binding-{skill}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PinKey {
    owner: OwnerScope,
    task_id: String,
    skill: SkillId,
}

type PinEntries = VecDeque<(PinKey, Arc<PinnedSkillRevision>)>;

/// Bounded process-wide memory of the pins issued to tasks. When the bound is
/// reached the oldest pin is forgotten (with a warning), so a very old task
/// that reads again is re-pinned to the then-active revision; runtimes should
/// call [`release_task_pins`] at the task terminal.
static TASK_PINS: LazyLock<Mutex<PinEntries>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

fn lookup_pin(entries: &PinEntries, key: &PinKey) -> Option<Arc<PinnedSkillRevision>> {
    entries
        .iter()
        .find(|(existing, _)| existing == key)
        .map(|(_, pin)| Arc::clone(pin))
}

/// Returns the pin of `task_id` for `skill`, issuing it with `issue` on first
/// use. Later calls return the same pin even if the active binding moved.
pub fn task_pin(
    owner: &OwnerScope,
    task_id: &str,
    skill: &SkillId,
    issue: impl FnOnce() -> Result<Option<PinnedSkillRevision>, LifecycleError>,
) -> Result<Option<Arc<PinnedSkillRevision>>, LifecycleError> {
    let key = PinKey {
        owner: owner.clone(),
        task_id: task_id.to_string(),
        skill: skill.clone(),
    };
    let existing = lookup_pin(&TASK_PINS.lock(), &key);
    if let Some(pin) = existing {
        return Ok(Some(pin));
    }
    let Some(issued) = issue()? else {
        return Ok(None);
    };
    let mut entries = TASK_PINS.lock();
    if let Some(pin) = lookup_pin(&entries, &key) {
        return Ok(Some(pin));
    }
    if entries.len() >= MAX_TASK_PINS {
        entries.pop_front();
        tracing::warn!("task skill pin table is full; the oldest pin was dropped");
    }
    let pin = Arc::new(issued);
    entries.push_back((key, Arc::clone(&pin)));
    Ok(Some(pin))
}

/// Forgets every pin of one task. Call at the task terminal.
pub fn release_task_pins(owner: &OwnerScope, task_id: &str) {
    TASK_PINS
        .lock()
        .retain(|(key, _)| !(key.owner == *owner && key.task_id == task_id));
}

/// One verified resource read from a pinned or active revision.
#[derive(Clone, Debug)]
pub struct ManagedSkillRead {
    pub pin: PinnedSkillRevision,
    /// Whether the revision came from a task pin (`true`) or was resolved from
    /// the active binding for this single read.
    pub pinned: bool,
    pub resource_path: String,
    pub bundle_dir: PathBuf,
    pub content: String,
    /// The bundle carries an untrusted-origin marker.
    pub untrusted_origin: bool,
}

/// Resolves `skill_name` through the owner's lifecycle store and reads one
/// resource of its revision.
///
/// `Ok(None)` means the skill is not lifecycle-managed for this owner (no
/// store, a store of another owner, or no binding), and the caller keeps the
/// legacy behaviour. A managed skill that is disabled, whose revision is
/// missing or tampered, or whose resource is absent or too large is an error:
/// nothing falls back to another revision.
pub fn read_managed_resource(
    workspace_dir: &Path,
    owner: &OwnerScope,
    skill_name: &str,
    resource_path: Option<&str>,
    task_id: Option<&str>,
    max_bytes: u64,
) -> Result<Option<ManagedSkillRead>, LifecycleError> {
    let store = match SkillLifecycleStore::open_existing(workspace_dir, owner.clone()) {
        Ok(Some(store)) => store,
        Ok(None) | Err(LifecycleError::OwnerMismatch { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some(binding) = store.find_binding_by_name(skill_name)? else {
        return Ok(None);
    };
    if !binding.enabled {
        return Err(LifecycleError::SkillDisabled(skill_name.to_string()));
    }
    let (pin, pinned) = match task_id {
        Some(task) => {
            let pin = task_pin(owner, task, &binding.skill_id, || store.pin_binding(&binding).map(Some))?
                .ok_or_else(|| LifecycleError::RevisionNotFound(binding.skill_id.to_string()))?;
            ((*pin).clone(), true)
        }
        None => (store.pin_binding(&binding)?, false),
    };
    let verified = store.read_pinned(&pin)?;
    let file = select_resource(&verified.bundle, resource_path)?;
    let size = u64::try_from(file.bytes().len()).unwrap_or(u64::MAX);
    if size > max_bytes {
        return Err(LifecycleError::ResourceUnavailable(format!(
            "{} exceeds the {max_bytes}-byte limit",
            file.path()
        )));
    }
    let content = String::from_utf8(file.bytes().to_vec())
        .map_err(|_| LifecycleError::ResourceUnavailable(format!("{} is not valid UTF-8", file.path())))?;
    let untrusted_origin = verified.bundle.file(UNTRUSTED_ORIGIN_MARKER).is_some();
    Ok(Some(ManagedSkillRead {
        resource_path: file.path().to_string(),
        bundle_dir: store.bundle_dir(&pin.skill_id, &pin.revision_id),
        pin,
        pinned,
        content,
        untrusted_origin,
    }))
}

/// Default resource follows the catalog: SKILL.toml when present, else SKILL.md.
fn select_resource<'a>(bundle: &'a Bundle, resource_path: Option<&str>) -> Result<&'a BundleFile, LifecycleError> {
    let wanted = match resource_path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => super::bundle::normalize_relative_path(Path::new(path))
            .map_err(|e| LifecycleError::ResourceUnavailable(e.to_string()))?,
        None if bundle.file("SKILL.toml").is_some() => "SKILL.toml".to_string(),
        None => "SKILL.md".to_string(),
    };
    bundle
        .file(&wanted)
        .ok_or_else(|| LifecycleError::ResourceUnavailable(format!("{wanted} is not part of the revision")))
}
