//! Task experience capture (SL03): selection and hydration receipts joined to
//! the task terminal.
//!
//! One record per `(owner, task id)` in `experiences/<experience-id>.json`.
//! It starts `incomplete` and only a terminal join can change that:
//!
//! * a selected skill that was never hydrated is not credited as used;
//! * a hydrated skill is not credited as useful because the task succeeded;
//! * a task without a terminal stays `incomplete`, nothing manufactures one;
//! * the same terminal joined twice is idempotent, a different terminal or
//!   outcome is refused, so a cancellation never becomes a success;
//! * evidence that does not fit (bounds) or arrives after the terminal is
//!   counted and makes the experience ineligible for learning claims.
//!
//! Task, run, segment and child identities are kept as separate fields on each
//! receipt and are never forced equal or inferred from each other.
//!
//! Every update runs under the store's shared lock and the experience's own
//! exclusive lock, and replaces the record atomically.

use super::generation::ensure_dir_synced;
use super::store::{SkillLifecycleStore, check_private, read_record, record_bytes};
use super::types::{LifecycleError, OwnerScope, PinnedSkillRevision, RevisionId, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

pub(super) const EXPERIENCES_DIR: &str = "experiences";
/// Selection receipts kept per experience; further ones are counted as dropped.
pub const MAX_SELECTION_RECEIPTS: usize = 64;
/// Hydration receipts kept per experience; further ones are counted as dropped.
pub const MAX_HYDRATION_RECEIPTS: usize = 256;
const CAPTURE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ID_BYTES: usize = 256;

/// Outcome of a task as reported by its terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceOutcome {
    Success,
    Failure,
    Cancelled,
    /// No terminal has been joined (the initial state).
    Incomplete,
    Blocked,
}

/// Separate correlation identities of one receipt. None is inferred from
/// another.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Correlation {
    pub run_id: Option<String>,
    pub segment_id: Option<String>,
    pub child_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionReceipt {
    pub skill_id: SkillId,
    pub revision_id: RevisionId,
    pub generation: u64,
    pub display_name: String,
    pub correlation: Correlation,
    pub selected_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HydrationReceipt {
    pub skill_id: SkillId,
    pub revision_id: RevisionId,
    pub generation: u64,
    pub resource_path: String,
    /// Hex SHA-256 of the exact bytes returned to the task.
    pub resource_sha256: String,
    pub bytes: u64,
    pub correlation: Correlation,
    pub hydrated_at: DateTime<Utc>,
}

/// Terminal facts joined to an experience.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalJoin {
    pub terminal_receipt_id: String,
    pub outcome: ExperienceOutcome,
    pub joined_at: DateTime<Utc>,
}

/// The persisted record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillExperience {
    pub schema_version: u32,
    /// Stable record id; equals the experience id (`exp-` + hash of owner and task).
    pub record_id: String,
    pub owner_scope: OwnerScope,
    pub task_id: String,
    pub selection_receipts: Vec<SelectionReceipt>,
    pub hydration_receipts: Vec<HydrationReceipt>,
    pub terminal: Option<TerminalJoin>,
    pub outcome: ExperienceOutcome,
    /// Evidence that did not fit the bounds.
    pub dropped_evidence: u32,
    /// Evidence that arrived after the terminal was joined.
    pub late_evidence: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SkillExperience {
    /// Whether the experience may support a learning claim: it has a terminal
    /// and no evidence was dropped or late.
    #[must_use]
    pub const fn learning_eligible(&self) -> bool {
        self.terminal.is_some() && self.dropped_evidence == 0 && self.late_evidence == 0
    }

    /// Skills with at least one hydration receipt: the only skills credited as used.
    #[must_use]
    pub fn used_skills(&self) -> Vec<&SkillId> {
        let mut used: Vec<&SkillId> = Vec::new();
        for receipt in &self.hydration_receipts {
            if !used.contains(&&receipt.skill_id) {
                used.push(&receipt.skill_id);
            }
        }
        used
    }
}

/// Result of joining a terminal.
#[derive(Clone, Debug)]
pub struct TerminalJoinOutcome {
    pub experience: SkillExperience,
    /// `true` when the identical terminal had already been joined.
    pub replayed: bool,
}

/// Deterministic experience id of `(owner, task)`.
#[must_use]
pub fn experience_id(owner: &OwnerScope, task_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"prx-skill-experience\0");
    for part in [owner.as_str(), task_id] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    format!("exp-{}", digest.get(..32).unwrap_or(&digest))
}

fn check_text(what: &'static str, value: &str) -> Result<(), LifecycleError> {
    if value.is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control) {
        return Err(LifecycleError::InvalidId {
            kind: what,
            reason: format!("must be 1..={MAX_ID_BYTES} bytes without control characters"),
        });
    }
    Ok(())
}

fn check_correlation(correlation: &Correlation) -> Result<(), LifecycleError> {
    for (what, value) in [
        ("run id", &correlation.run_id),
        ("segment id", &correlation.segment_id),
        ("child id", &correlation.child_id),
    ] {
        if let Some(value) = value {
            check_text(what, value)?;
        }
    }
    Ok(())
}

impl SkillLifecycleStore {
    fn experience_path(&self, id: &str) -> PathBuf {
        self.root().join(EXPERIENCES_DIR).join(format!("{id}.json"))
    }

    /// Reads one experience; `None` when the task left no managed-skill evidence.
    pub fn read_experience(&self, task_id: &str) -> Result<Option<SkillExperience>, LifecycleError> {
        check_text("task id", task_id)?;
        let path = self.experience_path(&experience_id(self.owner_scope(), task_id));
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&path, e)),
        }
        check_private(&path)?;
        let record: SkillExperience = read_record(&path)?;
        if record.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "experience",
                found: record.schema_version,
            });
        }
        self.check_owner_scope(&record.owner_scope)?;
        if record.task_id != task_id || record.record_id != experience_id(self.owner_scope(), task_id) {
            return Err(LifecycleError::Tampered {
                path,
                reason: "experience identity does not match its location".to_string(),
            });
        }
        Ok(Some(record))
    }

    /// Records that `task_id` selected the pinned revision. Selection alone
    /// never credits the skill as used.
    pub fn record_selection(
        &self,
        task_id: &str,
        pin: &PinnedSkillRevision,
        correlation: &Correlation,
    ) -> Result<SkillExperience, LifecycleError> {
        check_correlation(correlation)?;
        let receipt = SelectionReceipt {
            skill_id: pin.skill_id.clone(),
            revision_id: pin.revision_id.clone(),
            generation: pin.generation,
            display_name: pin.display_name.clone(),
            correlation: correlation.clone(),
            selected_at: Utc::now(),
        };
        self.update_experience(task_id, |record| {
            if record.terminal.is_some() {
                record.late_evidence = record.late_evidence.saturating_add(1);
            } else if record.selection_receipts.len() >= MAX_SELECTION_RECEIPTS {
                record.dropped_evidence = record.dropped_evidence.saturating_add(1);
            } else {
                record.selection_receipts.push(receipt);
            }
            Ok(None)
        })
        .map(|(record, _)| record)
    }

    /// Records that bytes of the pinned revision were actually handed to the task.
    pub fn record_hydration(
        &self,
        task_id: &str,
        pin: &PinnedSkillRevision,
        resource_path: &str,
        content: &[u8],
        correlation: &Correlation,
    ) -> Result<SkillExperience, LifecycleError> {
        check_correlation(correlation)?;
        check_text("resource path", resource_path)?;
        let receipt = HydrationReceipt {
            skill_id: pin.skill_id.clone(),
            revision_id: pin.revision_id.clone(),
            generation: pin.generation,
            resource_path: resource_path.to_string(),
            resource_sha256: hex::encode(Sha256::digest(content)),
            bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
            correlation: correlation.clone(),
            hydrated_at: Utc::now(),
        };
        self.update_experience(task_id, |record| {
            if record.terminal.is_some() {
                record.late_evidence = record.late_evidence.saturating_add(1);
            } else if record.hydration_receipts.len() >= MAX_HYDRATION_RECEIPTS {
                record.dropped_evidence = record.dropped_evidence.saturating_add(1);
            } else {
                record.hydration_receipts.push(receipt);
            }
            Ok(None)
        })
        .map(|(record, _)| record)
    }

    /// Notes that evidence for `task_id` could not be recorded (for example a
    /// capture failure), so the experience cannot support a learning claim.
    pub fn record_dropped_evidence(&self, task_id: &str) -> Result<SkillExperience, LifecycleError> {
        self.update_experience(task_id, |record| {
            record.dropped_evidence = record.dropped_evidence.saturating_add(1);
            Ok(None)
        })
        .map(|(record, _)| record)
    }

    /// Joins the task terminal. Idempotent for the identical terminal; any other
    /// terminal or outcome after the first is [`LifecycleError::TerminalConflict`].
    /// `ExperienceOutcome::Incomplete` is not a terminal outcome and is refused.
    pub fn join_terminal(
        &self,
        task_id: &str,
        terminal_receipt_id: &str,
        outcome: ExperienceOutcome,
    ) -> Result<TerminalJoinOutcome, LifecycleError> {
        check_text("terminal receipt id", terminal_receipt_id)?;
        if outcome == ExperienceOutcome::Incomplete {
            return Err(LifecycleError::InvalidId {
                kind: "terminal outcome",
                reason: "incomplete is not a terminal outcome".to_string(),
            });
        }
        let terminal_receipt_id = terminal_receipt_id.to_string();
        let (experience, replayed) = self.update_experience(task_id, |record| {
            if let Some(existing) = &record.terminal {
                if existing.terminal_receipt_id == terminal_receipt_id && existing.outcome == outcome {
                    return Ok(Some(true));
                }
                return Err(LifecycleError::TerminalConflict {
                    task_id: record.task_id.clone(),
                });
            }
            record.terminal = Some(TerminalJoin {
                terminal_receipt_id: terminal_receipt_id.clone(),
                outcome,
                joined_at: Utc::now(),
            });
            record.outcome = outcome;
            Ok(Some(false))
        })?;
        Ok(TerminalJoinOutcome {
            experience,
            replayed: replayed.unwrap_or(false),
        })
    }

    /// Read-modify-write of one experience under the store and experience locks.
    /// `change` returns `Some(true)` for "nothing to write" (a replay).
    fn update_experience(
        &self,
        task_id: &str,
        change: impl FnOnce(&mut SkillExperience) -> Result<Option<bool>, LifecycleError>,
    ) -> Result<(SkillExperience, Option<bool>), LifecycleError> {
        check_text("task id", task_id)?;
        ensure_dir_synced(self.root(), &self.root().join(EXPERIENCES_DIR))?;
        let id = experience_id(self.owner_scope(), task_id);
        // Experience ids are valid skill-id strings, so the per-skill lock
        // (and the staging sweep that honours it) protects them as well.
        let lock_id = SkillId::parse(&id)?;
        let _store_lock = self.lock_store(true, CAPTURE_LOCK_TIMEOUT)?;
        let _lock = self.lock_skill(&lock_id, CAPTURE_LOCK_TIMEOUT)?;
        let now = Utc::now();
        let mut record = self.read_experience(task_id)?.unwrap_or_else(|| SkillExperience {
            schema_version: SCHEMA_VERSION,
            record_id: id.clone(),
            owner_scope: self.owner_scope().clone(),
            task_id: task_id.to_string(),
            selection_receipts: Vec::new(),
            hydration_receipts: Vec::new(),
            terminal: None,
            outcome: ExperienceOutcome::Incomplete,
            dropped_evidence: 0,
            late_evidence: 0,
            created_at: now,
            updated_at: now,
        });
        let replay = change(&mut record)?;
        if replay == Some(true) {
            return Ok((record, replay));
        }
        record.updated_at = Utc::now();
        let path = self.experience_path(&id);
        self.write_atomic(&lock_id, &path, &record_bytes(&record, &path)?)?;
        Ok((record, replay))
    }
}

/// Key an experience and its pins are filed under: the run id when there is
/// one (a terminal belongs to a run; a recurring task reuses its task id across
/// runs), else the task id. `None` when neither is present or usable.
#[must_use]
pub fn task_key(task_id: Option<&str>, run_id: Option<&str>) -> Option<String> {
    [run_id, task_id]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

/// What the task terminal owner reports.
#[derive(Clone, Debug)]
pub struct TaskTerminal<'a> {
    pub task_id: &'a str,
    pub terminal_receipt_id: &'a str,
    pub outcome: ExperienceOutcome,
}

/// The one call-in for a task terminal: joins the terminal to the task's
/// experience (when the owner has a lifecycle store) and releases the task's
/// revision pins. Pins are released even when the join fails, so a capture
/// problem can never leak them. Blocking; async callers use `spawn_blocking`.
pub fn on_task_terminal(
    workspace_dir: &std::path::Path,
    owner: &OwnerScope,
    terminal: &TaskTerminal<'_>,
) -> Result<Option<TerminalJoinOutcome>, LifecycleError> {
    let joined = match SkillLifecycleStore::open_existing(workspace_dir, owner.clone()) {
        Ok(Some(store)) => {
            // A task that never touched a managed skill has no experience and
            // needs none; do not create an empty record for it.
            match store.read_experience(terminal.task_id) {
                Ok(Some(_)) => store
                    .join_terminal(terminal.task_id, terminal.terminal_receipt_id, terminal.outcome)
                    .map(Some),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            }
        }
        Ok(None) | Err(LifecycleError::OwnerMismatch { .. }) => Ok(None),
        Err(e) => Err(e),
    };
    super::binding::release_task_pins(owner, terminal.task_id);
    joined
}
