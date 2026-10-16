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
use super::capture::Correlation;
use super::generation::{ACTIVE_DIR, ActiveBinding};
use super::store::{SkillLifecycleStore, VerifiedRevision};
use super::types::{LifecycleError, OwnerScope, PinnedSkillRevision, SkillId};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Upper bound on bindings listed for one owner store.
pub const MAX_ACTIVE_BINDINGS: usize = 512;
/// Upper bound on concurrently remembered task pins in this process. When the
/// table is full of pins younger than [`ORPHAN_PIN_AGE`], new pins are refused
/// with [`LifecycleError::PinTableFull`]; a live pin is never evicted.
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
            let parsed = name
                .strip_suffix(".json")
                .filter(|_| !name.starts_with('.'))
                .and_then(|stem| SkillId::parse(stem).ok());
            let Some(id) = parsed else {
                tracing::warn!(entry = %entry.path().display(), "ignoring unexpected entry in the active binding directory");
                continue;
            };
            ids.push(id);
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
        self.verify_binding(binding).map(|(pin, _)| pin)
    }

    /// Verifies the revision named by `binding` once and returns both the pin
    /// and the verified revision, so a caller that also reads it does not
    /// hash the bundle a second time.
    pub(super) fn verify_binding(
        &self,
        binding: &ActiveBinding,
    ) -> Result<(PinnedSkillRevision, VerifiedRevision), LifecycleError> {
        let verified = self.read_revision(&binding.skill_id, &binding.revision_id, Some(&binding.record_digest))?;
        let pin = PinnedSkillRevision {
            owner_scope: binding.owner_scope.clone(),
            skill_id: binding.skill_id.clone(),
            revision_id: binding.revision_id.clone(),
            generation: binding.generation,
            display_name: verified.revision.display_name.clone(),
            record_digest: verified.record_digest.clone(),
        };
        Ok((pin, verified))
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

type PinEntries = VecDeque<(PinKey, Arc<PinnedSkillRevision>, Instant)>;

/// A pin older than this is considered orphaned (its task ended without
/// reaching a terminal that releases pins, for example a turn that failed
/// before finalization) and may be reclaimed, but only when the table is full.
pub const ORPHAN_PIN_AGE: Duration = Duration::from_hours(24);

/// Bounded table of the pins issued to tasks. A pin lives until
/// [`release_task_pins`] removes it (the task terminal does, and so does every
/// exit of `finalize_turn`). Live pins are never evicted: when the table is
/// full, only pins older than `orphan_age` are reclaimed, and if none are the
/// new pin is refused, so a running task never silently switches revision.
pub(super) struct PinTable {
    cap: usize,
    orphan_age: Duration,
    entries: Mutex<PinEntries>,
}

impl PinTable {
    pub(super) const fn new(cap: usize, orphan_age: Duration) -> Self {
        Self {
            cap,
            orphan_age,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// Makes room for one pin; `false` when the table is full of live pins.
    fn make_room(&self, entries: &mut PinEntries) -> bool {
        if entries.len() < self.cap {
            return true;
        }
        let before = entries.len();
        entries.retain(|(_, _, issued)| issued.elapsed() < self.orphan_age);
        let reclaimed = before - entries.len();
        if reclaimed > 0 {
            tracing::warn!(reclaimed, "reclaimed orphaned task skill pins");
        }
        entries.len() < self.cap
    }

    fn lookup(entries: &PinEntries, key: &PinKey) -> Option<Arc<PinnedSkillRevision>> {
        entries
            .iter()
            .find(|(existing, _, _)| existing == key)
            .map(|(_, pin, _)| Arc::clone(pin))
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
        let Some(issued) = issue()? else {
            return Ok(None);
        };
        let mut entries = self.entries.lock();
        if let Some(pin) = Self::lookup(&entries, &key) {
            return Ok(Some(pin));
        }
        if !self.make_room(&mut entries) {
            return Err(LifecycleError::PinTableFull(self.cap));
        }
        let pin = Arc::new(issued);
        entries.push_back((key, Arc::clone(&pin), Instant::now()));
        Ok(Some(pin))
    }

    /// The pin this task holds for a skill whose display name equals `name`
    /// (ASCII case-insensitive).
    pub(super) fn find_by_name(
        &self,
        owner: &OwnerScope,
        task_id: &str,
        name: &str,
    ) -> Option<Arc<PinnedSkillRevision>> {
        self.entries
            .lock()
            .iter()
            .find(|(key, pin, _)| {
                key.owner == *owner && key.task_id == task_id && pin.display_name.eq_ignore_ascii_case(name)
            })
            .map(|(_, pin, _)| Arc::clone(pin))
    }

    pub(super) fn release(&self, owner: &OwnerScope, task_id: &str) {
        self.entries
            .lock()
            .retain(|(key, _, _)| !(key.owner == *owner && key.task_id == task_id));
    }
}

static TASK_PINS: PinTable = PinTable::new(MAX_TASK_PINS, ORPHAN_PIN_AGE);

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

/// One managed-skill read.
#[derive(Clone, Debug)]
pub struct ManagedReadRequest<'a> {
    pub workspace_dir: &'a Path,
    pub owner: &'a OwnerScope,
    pub skill_name: &'a str,
    pub resource_path: Option<&'a str>,
    /// The trusted task the read belongs to; hydration is captured for it.
    pub task_id: Option<&'a str>,
    /// Run, segment and child identities of the call, recorded as given.
    pub correlation: Correlation,
    /// Keep the task on the revision it first read (requires `task_id`). The
    /// pins live until [`release_task_pins`] / [`on_task_terminal`].
    pub pin_task: bool,
    pub max_bytes: u64,
}

/// Resolves the skill through the owner's lifecycle store and reads one
/// resource of its revision.
///
/// `Ok(None)` means the skill is not lifecycle-managed for this owner (no
/// store, a store of another owner, or no binding), and the caller keeps the
/// legacy behaviour. A managed skill that is disabled, whose revision is
/// missing or tampered, or whose resource is absent or too large is an error:
/// nothing falls back to another revision. A successful read with a task id
/// records a hydration receipt; a capture failure is logged and marks the
/// experience as having dropped evidence, but never fails the read.
pub fn read_managed_resource(request: &ManagedReadRequest<'_>) -> Result<Option<ManagedSkillRead>, LifecycleError> {
    let ManagedReadRequest {
        workspace_dir,
        owner,
        skill_name,
        resource_path,
        task_id,
        pin_task,
        max_bytes,
        ..
    } = *request;
    let pin_task_id = task_id.filter(|_| pin_task);
    // A task that already holds a pin for this skill name keeps it, even if
    // the active binding was renamed, disabled or removed since.
    let held = pin_task_id.and_then(|task| TASK_PINS.find_by_name(owner, task, skill_name));
    let store = match SkillLifecycleStore::open_existing(workspace_dir, owner.clone()) {
        Ok(Some(store)) => store,
        Ok(None) | Err(LifecycleError::OwnerMismatch { .. }) if held.is_none() => return Ok(None),
        Ok(None) => return Err(LifecycleError::RevisionNotFound(skill_name.to_string())),
        Err(e) => return Err(e),
    };
    let (pin, pinned, verified) = if let Some(pin) = held {
        let verified = store.read_pinned(&pin)?;
        ((*pin).clone(), true, verified)
    } else {
        let Some((binding, _display_name)) = store.find_binding_by_name(skill_name)? else {
            return Ok(None);
        };
        if !binding.enabled {
            return Err(LifecycleError::SkillDisabled(skill_name.to_string()));
        }
        // One verification of the bundle serves both the pin and the read.
        let (issued, verified) = store.verify_binding(&binding)?;
        match pin_task_id {
            Some(task) => {
                let pin = task_pin(owner, task, &binding.skill_id, || Ok(Some(issued)))?
                    .ok_or_else(|| LifecycleError::RevisionNotFound(binding.skill_id.to_string()))?;
                // Another thread may have pinned this task first.
                let verified = if pin.revision_id == verified.revision.revision_id {
                    verified
                } else {
                    store.read_pinned(&pin)?
                };
                ((*pin).clone(), true, verified)
            }
            None => (issued, false, verified),
        }
    };
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
    if let Some(task) = task_id {
        capture_hydration(&store, task, &pin, file.path(), file.bytes(), &request.correlation);
    }
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

fn capture_hydration(
    store: &SkillLifecycleStore,
    task: &str,
    pin: &PinnedSkillRevision,
    path: &str,
    content: &[u8],
    correlation: &Correlation,
) {
    if let Err(error) = store.record_hydration(task, pin, path, content, correlation) {
        tracing::warn!(%error, task, "failed to record a skill hydration receipt");
        // Lock-free, so it still works when the failure was a lock timeout.
        store.mark_dropped_evidence(task);
    }
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
