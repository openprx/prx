//! Bounded append-only experiment records inside the lifecycle store.
//! Store lock precedes skill lock, exactly as publication and staging do.
use super::{ExperimentError, ExperimentEvent, ExperimentId, ExperimentSpec, ExperimentState, FrozenExperiment};
use crate::skills::lifecycle::generation::ensure_dir_synced;
use crate::skills::lifecycle::store::{check_private, read_record, record_bytes};
use crate::skills::lifecycle::{LifecycleError, SkillLifecycleStore, TransactionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, time::Duration};

// Each of <=64 stages has at most reserve, dispatch, unknown, settle and cleanup.
// Cancellation has a dedicated extra slot: settlement never competes with admission.
const MAX_EVENTS: usize = super::MAX_EXPERIMENT_STAGES * 5 + 1;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentEntry {
    pub sequence: u32,
    pub event_id: String,
    pub experiment_sha256: String,
    pub previous_sha256: String,
    pub recorded_at: DateTime<Utc>,
    pub event: ExperimentEvent,
}
impl ExperimentEntry {
    pub fn digest(&self) -> Result<String, ExperimentError> {
        let bytes = serde_json::to_vec(self).map_err(|error| ExperimentError::InvalidEvidence(error.to_string()))?;
        let mut hash = Sha256::new();
        hash.update(b"prx-instruction-experiment-entry-v1\0");
        hash.update(bytes);
        Ok(hex::encode(hash.finalize()))
    }
}
#[derive(Clone, Debug)]
pub struct ExperimentJournal {
    pub plan: FrozenExperiment,
    pub entries: Vec<ExperimentEntry>,
    pub state: ExperimentState,
    pub head_sha256: String,
}
impl ExperimentJournal {
    pub fn next_sequence(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }
}
impl SkillLifecycleStore {
    fn experiment_dir(&self, id: &ExperimentId) -> PathBuf {
        self.root().join("experiments").join(id.as_str())
    }
    /// Registers immutable inputs. Reusing an identity with different inputs is
    /// refused. Callers must pin the returned digest outside this directory.
    pub fn admit_experiment(
        &self,
        plan: &FrozenExperiment,
        now: DateTime<Utc>,
    ) -> Result<ExperimentJournal, ExperimentError> {
        self.check_owner_scope(&plan.spec().owner_scope)?;
        plan.ensure_live(now)?;
        let _store_lock = self.lock_store(true, LOCK_TIMEOUT)?;
        let _skill_lock = self.lock_skill(&plan.spec().pins.skill_id, LOCK_TIMEOUT)?;
        let live = self.require_live_experiment_admission(plan)?;
        let parent = self.root().join("experiments");
        let directory = self.experiment_dir(&plan.spec().id);
        // Inspect only: unauthenticated/conflicting requests never create a
        // directory or consume launch capacity. Existing components must be safe.
        for component in [&parent, &directory] {
            match fs::symlink_metadata(component) {
                Ok(metadata) => {
                    check_private(component)?;
                    if !metadata.is_dir() || metadata.file_type().is_symlink() {
                        return Err(LifecycleError::UnsafeStorePath {
                            path: component.to_path_buf(),
                            reason: "experiment component is not a real directory".into(),
                        }
                        .into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(LifecycleError::io(component, error).into()),
            }
        }
        let path = directory.join("spec.json");
        let exists = match fs::symlink_metadata(&path) {
            Ok(_) => {
                check_private(&path)?;
                let stored: ExperimentSpec = read_record(&path)?;
                let stored = stored.freeze()?;
                self.verify_experiment_admission(&stored)?;
                if stored.spec().id != plan.spec().id || stored.digest() != plan.digest() {
                    return Err(ExperimentError::Conflict);
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(LifecycleError::io(&path, error).into()),
        };
        if let Some(live) = live {
            let envelope =
                plan.spec().controlled_admission.as_ref().ok_or_else(|| {
                    ExperimentError::InvalidEvidence("controlled admission envelope disappeared".into())
                })?;
            live.claim_store_admission(
                &envelope.pin,
                plan,
                self.admission_workspace()?,
                self.owner_scope().as_str(),
            )
            .map_err(|_| ExperimentError::InvalidEvidence("controlled admission capacity or source refused".into()))?;
        }
        if !exists {
            ensure_dir_synced(self.root(), &parent)?;
            ensure_dir_synced(&parent, &directory)?;
            let bytes = record_bytes(plan.spec(), &path)?;
            self.write_atomic(&plan.spec().pins.skill_id, &path, &bytes)?;
        }
        self.read_experiment(&plan.spec().id, plan.digest())
    }
    /// Replays only a bounded, contiguous, hash-linked journal. The admitted
    /// digest must come from the caller's pin, not from this directory itself.
    pub fn read_experiment(
        &self,
        id: &ExperimentId,
        expected_digest: &str,
    ) -> Result<ExperimentJournal, ExperimentError> {
        check_private(&self.root().join("experiments"))?;
        let directory = self.experiment_dir(id);
        check_private(&directory)?;
        let path = directory.join("spec.json");
        check_private(&path)?;
        let plan = read_record::<ExperimentSpec>(&path)?.freeze()?;
        self.check_owner_scope(&plan.spec().owner_scope)?;
        self.verify_experiment_admission(&plan)?;
        if plan.spec().id != *id || plan.digest() != expected_digest {
            return Err(ExperimentError::Conflict);
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|error| LifecycleError::io(&directory, error))? {
            let entry = entry.map_err(|error| LifecycleError::io(&directory, error))?;
            let name = entry.file_name();
            if name == "spec.json" {
                continue;
            }
            if names.len() >= MAX_EVENTS {
                return Err(ExperimentError::InvalidEvidence("journal record limit exceeded".into()));
            }
            names.push(name);
        }
        names.sort();
        let mut result = ExperimentJournal {
            state: ExperimentState::new(&plan),
            head_sha256: plan.digest().to_string(),
            plan,
            entries: Vec::new(),
        };
        let mut identities = std::collections::BTreeSet::new();
        let mut previous_time = result.plan.spec().created_at;
        for (index, name) in names.into_iter().enumerate() {
            if name != std::ffi::OsStr::new(&format!("{index:04}.json")) {
                return Err(ExperimentError::InvalidEvidence("non-contiguous journal".into()));
            }
            let path = directory.join(name);
            check_private(&path)?;
            let entry: ExperimentEntry = read_record(&path)?;
            TransactionId::parse(&entry.event_id)?;
            if entry.sequence as usize != index
                || entry.previous_sha256 != result.head_sha256
                || entry.experiment_sha256 != result.plan.digest()
                || !identities.insert(entry.event_id.clone())
                || entry.recorded_at < previous_time
            {
                return Err(ExperimentError::InvalidEvidence(
                    "journal binding or chronology mismatch".into(),
                ));
            }
            result.state.apply(&result.plan, &entry.event, entry.recorded_at)?;
            result.head_sha256 = entry.digest()?;
            previous_time = entry.recorded_at;
            result.entries.push(entry);
        }
        Ok(result)
    }
    /// Durable compare-and-append. Identical event-id retries are idempotent;
    /// conflicting retries, stale sequence/head and illegal transitions refuse.
    pub fn append_experiment(
        &self,
        id: &ExperimentId,
        digest: &str,
        expected_sequence: u32,
        expected_head: &str,
        event_id: &str,
        event: ExperimentEvent,
        now: DateTime<Utc>,
    ) -> Result<ExperimentJournal, ExperimentError> {
        TransactionId::parse(event_id)?;
        let _store_lock = self.lock_store(true, LOCK_TIMEOUT)?;
        let initial = self.read_experiment(id, digest)?;
        let _skill_lock = self.lock_skill(&initial.plan.spec().pins.skill_id, LOCK_TIMEOUT)?;
        let mut journal = self.read_experiment(id, digest)?;
        if let Some(old) = journal.entries.iter().find(|entry| entry.event_id == event_id) {
            if old.event == event {
                return Ok(journal);
            }
            return Err(ExperimentError::Conflict);
        }
        if journal.next_sequence() != expected_sequence || journal.head_sha256 != expected_head {
            return Err(ExperimentError::Conflict);
        }
        if journal.entries.len() >= MAX_EVENTS {
            return Err(ExperimentError::InvalidTransition("journal full".into()));
        }
        let previous_time = journal
            .entries
            .last()
            .map_or_else(|| journal.plan.spec().created_at, |entry| entry.recorded_at);
        if now < previous_time {
            return Err(ExperimentError::Expired);
        }
        journal.state.apply(&journal.plan, &event, now)?;
        let entry = ExperimentEntry {
            sequence: expected_sequence,
            event_id: event_id.into(),
            experiment_sha256: digest.into(),
            previous_sha256: journal.head_sha256.clone(),
            recorded_at: now,
            event,
        };
        let path = self.experiment_dir(id).join(format!("{expected_sequence:04}.json"));
        let bytes = record_bytes(&entry, &path)?;
        if bytes.len() as u64 > crate::skills::lifecycle::store::MAX_RECORD_BYTES {
            return Err(ExperimentError::InvalidEvidence(
                "stage record exceeds byte limit".into(),
            ));
        }
        self.write_atomic(&journal.plan.spec().pins.skill_id, &path, &bytes)?;
        journal.head_sha256 = entry.digest()?;
        journal.entries.push(entry);
        Ok(journal)
    }
}
