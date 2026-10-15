//! Task revision pins and pinned reads (SL02) on top of the SL08 active
//! binding.
//!
//! `active/<skill-id>.json` ([`ActiveBinding`], written only by `publish` and
//! `rollback`) names the revision a skill currently resolves to and pins its
//! record digest. A task that is issued a [`PinnedSkillRevision`] keeps
//! reading that revision after a newer binding is published; revisions are
//! immutable and a read through a pin verifies the bundle hash, owner scope and
//! the pinned record digest.

use super::bundle::{Bundle, BundleFile};
use super::generation::{ACTIVE_DIR, ActiveBinding};
use super::store::{SkillLifecycleStore, VerifiedRevision};
use super::types::{LifecycleError, OwnerScope, PinnedSkillRevision, SkillId};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Upper bound on bindings listed for one owner store.
pub const MAX_ACTIVE_BINDINGS: usize = 512;
/// Upper bound on concurrently remembered task pins in this process. When the
/// table is full, new pins are refused with [`LifecycleError::PinTableFull`];
/// a live pin is never evicted.
pub const MAX_TASK_PINS: usize = 4096;
const UNTRUSTED_ORIGIN_MARKER: &str = ".openprx-untrusted-origin.json";

impl SkillLifecycleStore {
    /// Lists every active binding of the store in skill-id order.
    pub fn list_active_bindings(&self) -> Result<Vec<ActiveBinding>, LifecycleError> {
        let dir = self.root().join(ACTIVE_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LifecycleError::io(&dir, e)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(".json")
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
            if let Some(binding) = self.active_binding(&id)? {
                bindings.push(binding);
            }
        }
        Ok(bindings)
    }

    /// Finds the binding whose revision display name equals `name` (ASCII
    /// case-insensitive, like the catalog). The display name is read from the
    /// revision record authenticated by the binding's pinned digest. Two
    /// skills sharing a name are an error rather than a silent choice.
    pub fn find_binding_by_name(&self, name: &str) -> Result<Option<(ActiveBinding, String)>, LifecycleError> {
        let mut found: Option<(ActiveBinding, String)> = None;
        for binding in self.list_active_bindings()? {
            let record =
                self.pinned_revision_record(&binding.skill_id, &binding.revision_id, &binding.record_digest)?;
            if record.display_name.eq_ignore_ascii_case(name) {
                if found.is_some() {
                    return Err(LifecycleError::AmbiguousSkillName(name.to_string()));
                }
                found = Some((binding, record.display_name));
            }
        }
        Ok(found)
    }

    /// Pins the revision named by `binding`, verifying it against the pinned
    /// record digest. `display_name` is the authenticated name of that revision.
    pub fn pin_binding(&self, binding: &ActiveBinding) -> Result<PinnedSkillRevision, LifecycleError> {
        let verified = self.read_revision(&binding.skill_id, &binding.revision_id, Some(&binding.record_digest))?;
        Ok(PinnedSkillRevision {
            owner_scope: binding.owner_scope.clone(),
            skill_id: binding.skill_id.clone(),
            revision_id: binding.revision_id.clone(),
            generation: binding.generation,
            display_name: verified.revision.display_name,
            record_digest: verified.record_digest,
        })
    }

    /// Resolves the active binding of `skill` into a pin; `None` when the skill
    /// has none.
    pub fn pin_active(&self, skill: &SkillId) -> Result<Option<PinnedSkillRevision>, LifecycleError> {
        self.active_binding(skill)?
            .map_or(Ok(None), |binding| self.pin_binding(&binding).map(Some))
    }

    /// Reads the pinned revision, verifying its bundle hash, owner scope and
    /// the digest recorded in the pin. Never follows a newer binding.
    pub fn read_pinned(&self, pin: &PinnedSkillRevision) -> Result<VerifiedRevision, LifecycleError> {
        self.check_owner_scope(&pin.owner_scope)?;
        self.read_revision(&pin.skill_id, &pin.revision_id, Some(&pin.record_digest))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PinKey {
    owner: OwnerScope,
    task_id: String,
    skill: SkillId,
}

type PinEntries = VecDeque<(PinKey, Arc<PinnedSkillRevision>)>;

/// Bounded table of the pins issued to tasks. A pin lives until
/// [`release_task_pins`] removes it; SL03 calls that at the task terminal.
/// Nothing is ever evicted: when the table is full a new pin is refused, so a
/// running task never silently switches revision.
pub(super) struct PinTable {
    cap: usize,
    entries: Mutex<PinEntries>,
}

impl PinTable {
    pub(super) const fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    fn lookup(entries: &PinEntries, key: &PinKey) -> Option<Arc<PinnedSkillRevision>> {
        entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, pin)| Arc::clone(pin))
    }

    pub(super) fn pin(
        &self,
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
        let existing = Self::lookup(&self.entries.lock(), &key);
        if let Some(pin) = existing {
            return Ok(Some(pin));
        }
        if self.entries.lock().len() >= self.cap {
            return Err(LifecycleError::PinTableFull(self.cap));
        }
        let Some(issued) = issue()? else {
            return Ok(None);
        };
        let mut entries = self.entries.lock();
        if let Some(pin) = Self::lookup(&entries, &key) {
            return Ok(Some(pin));
        }
        if entries.len() >= self.cap {
            return Err(LifecycleError::PinTableFull(self.cap));
        }
        let pin = Arc::new(issued);
        entries.push_back((key, Arc::clone(&pin)));
        Ok(Some(pin))
    }

    pub(super) fn release(&self, owner: &OwnerScope, task_id: &str) {
        self.entries
            .lock()
            .retain(|(key, _)| !(key.owner == *owner && key.task_id == task_id));
    }
}

static TASK_PINS: PinTable = PinTable::new(MAX_TASK_PINS);

/// Returns the pin of `task_id` for `skill`, issuing it with `issue` on first
/// use. Later calls return the same pin even if the active binding moved.
pub fn task_pin(
    owner: &OwnerScope,
    task_id: &str,
    skill: &SkillId,
    issue: impl FnOnce() -> Result<Option<PinnedSkillRevision>, LifecycleError>,
) -> Result<Option<Arc<PinnedSkillRevision>>, LifecycleError> {
    TASK_PINS.pin(owner, task_id, skill, issue)
}

/// Forgets every pin of one task. Call at the task terminal.
pub fn release_task_pins(owner: &OwnerScope, task_id: &str) {
    TASK_PINS.release(owner, task_id);
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
    let Some((binding, _display_name)) = store.find_binding_by_name(skill_name)? else {
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
