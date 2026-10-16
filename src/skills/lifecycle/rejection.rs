//! Rejection history (SL04): proposals that were rejected stay on record, and
//! a materially equivalent repeat is suppressed until new evidence or a changed
//! environment justifies another attempt.
//!
//! Records live in `rejections/<record-id>.json` and are immutable. Nothing in
//! publication, rollback or disable touches them: a rejected proposal's history
//! survives every later change of the active revision.

use super::consolidate::check_field;
use super::generation::ensure_dir_synced;
use super::knowledge::{MAX_ITEM_BYTES, MAX_LIST_ITEMS, MAX_NAME_BYTES, MAX_PATTERN_EVIDENCE, PatternId};
use super::store::{SkillLifecycleStore, check_private, read_record, record_bytes};
use super::types::{LifecycleError, OwnerScope, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

pub(super) const REJECTIONS_DIR: &str = "rejections";
const REJECTION_LOCK_ID: &str = "knowledge-rejections";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// What a proposal would do to its skill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalOperation {
    Create,
    Revise,
    Disable,
}

/// Digest of the material content of a proposal: the words of the given texts
/// (the changed instruction text), ignoring case, whitespace, punctuation and
/// markdown decoration, so a re-worded layout of the same change is the same
/// proposal while a different instruction is not.
#[must_use]
pub fn material_digest(texts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"prx-proposal-material\0");
    for text in texts {
        let normalised: String = text
            .nfc()
            .flat_map(char::to_lowercase)
            .filter(|c| c.is_alphanumeric())
            .collect();
        hasher.update(u64::try_from(normalised.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(normalised.as_bytes());
    }
    hex::encode(hasher.finalize())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectionRecord {
    pub schema_version: u32,
    /// `rej-` + 32 hex digits derived from the proposal id.
    pub record_id: String,
    pub owner_scope: OwnerScope,
    pub proposal_id: String,
    pub operation: ProposalOperation,
    pub skill_id: SkillId,
    pub fingerprint: String,
    pub pattern_ids: Vec<PatternId>,
    pub evidence_ids: Vec<String>,
    /// Digest of the model, tool and configuration bindings the attempt used.
    pub environment_binding: String,
    pub reasons: Vec<String>,
    pub rejected_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct NewRejection {
    pub proposal_id: String,
    pub operation: ProposalOperation,
    pub skill_id: SkillId,
    pub fingerprint: String,
    pub pattern_ids: Vec<PatternId>,
    pub evidence_ids: Vec<String>,
    pub environment_binding: String,
    pub reasons: Vec<String>,
}

fn record_id_of(proposal_id: &str, owner: &OwnerScope) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"prx-rejection\0");
    for part in [owner.as_str(), proposal_id] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    format!("rej-{}", digest.get(..32).unwrap_or(&digest))
}

impl NewRejection {
    fn validate(&self) -> Result<(), LifecycleError> {
        check_field("proposal id", &self.proposal_id, MAX_NAME_BYTES)?;
        check_field("fingerprint", &self.fingerprint, MAX_NAME_BYTES)?;
        check_field("environment binding", &self.environment_binding, MAX_NAME_BYTES)?;
        if self.reasons.is_empty() || self.reasons.len() > MAX_LIST_ITEMS {
            return Err(LifecycleError::InvalidKnowledge(
                "a rejection needs between 1 and 16 reasons".to_string(),
            ));
        }
        self.reasons
            .iter()
            .try_for_each(|reason| check_field("reason", reason, MAX_ITEM_BYTES))?;
        if self.evidence_ids.len() > MAX_PATTERN_EVIDENCE || self.pattern_ids.len() > MAX_PATTERN_EVIDENCE {
            return Err(LifecycleError::InvalidKnowledge("too many references".to_string()));
        }
        self.evidence_ids
            .iter()
            .try_for_each(|id| check_field("evidence id", id, MAX_NAME_BYTES))
    }
}

impl SkillLifecycleStore {
    fn rejections_dir(&self) -> PathBuf {
        self.root().join(REJECTIONS_DIR)
    }

    /// Records a rejected proposal. Idempotent for the same proposal id and
    /// content; the same proposal id with different content is refused (a
    /// record is never overwritten). Every referenced pattern must exist.
    pub fn record_rejection(&self, new: &NewRejection) -> Result<RejectionRecord, LifecycleError> {
        new.validate()?;
        for pattern in &new.pattern_ids {
            if self.read_pattern(pattern)?.is_none() {
                return Err(LifecycleError::InvalidKnowledge(format!(
                    "rejection references unknown pattern {pattern}"
                )));
            }
        }
        let dir = self.rejections_dir();
        ensure_dir_synced(self.root(), &dir)?;
        let lock_id = SkillId::parse(REJECTION_LOCK_ID)?;
        let _store_lock = self.lock_store(true, LOCK_TIMEOUT)?;
        let _lock = self.lock_skill(&lock_id, LOCK_TIMEOUT)?;
        let id = record_id_of(&new.proposal_id, self.owner_scope());
        let path = dir.join(format!("{id}.json"));
        let mut evidence = new.evidence_ids.clone();
        evidence.sort();
        evidence.dedup();
        let record = RejectionRecord {
            schema_version: SCHEMA_VERSION,
            record_id: id.clone(),
            owner_scope: self.owner_scope().clone(),
            proposal_id: new.proposal_id.clone(),
            operation: new.operation,
            skill_id: new.skill_id.clone(),
            fingerprint: new.fingerprint.clone(),
            pattern_ids: new.pattern_ids.clone(),
            evidence_ids: evidence,
            environment_binding: new.environment_binding.clone(),
            reasons: new.reasons.clone(),
            rejected_at: Utc::now(),
        };
        if let Some(existing) = self.read_rejection_file(&path, &id)? {
            let same = (
                &existing.operation,
                &existing.skill_id,
                &existing.fingerprint,
                &existing.pattern_ids,
                &existing.evidence_ids,
                &existing.environment_binding,
                &existing.reasons,
            ) == (
                &record.operation,
                &record.skill_id,
                &record.fingerprint,
                &record.pattern_ids,
                &record.evidence_ids,
                &record.environment_binding,
                &record.reasons,
            );
            return if same {
                Ok(existing)
            } else {
                Err(LifecycleError::InvalidKnowledge(format!(
                    "proposal {} is already recorded as rejected with different content",
                    new.proposal_id
                )))
            };
        }
        self.write_atomic(&lock_id, &path, &record_bytes(&record, &path)?)?;
        Ok(record)
    }

    fn read_rejection_file(&self, path: &std::path::Path, id: &str) -> Result<Option<RejectionRecord>, LifecycleError> {
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(path, e)),
        }
        check_private(path)?;
        let record: RejectionRecord = read_record(path)?;
        if record.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "rejection",
                found: record.schema_version,
            });
        }
        self.check_owner_scope(&record.owner_scope)?;
        if record.record_id != id || record_id_of(&record.proposal_id, self.owner_scope()) != id {
            return Err(LifecycleError::Tampered {
                path: path.to_path_buf(),
                reason: "rejection identity does not match its location".to_string(),
            });
        }
        Ok(Some(record))
    }

    /// Every rejection of `skill`, oldest first.
    pub fn rejections_for(&self, skill: &SkillId) -> Result<Vec<RejectionRecord>, LifecycleError> {
        let dir = self.rejections_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LifecycleError::io(&dir, e)),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json").filter(|id| id.starts_with("rej-")) else {
                continue;
            };
            if let Some(record) = self.read_rejection_file(&entry.path(), id)?
                && record.skill_id == *skill
            {
                records.push(record);
            }
        }
        records.sort_by(|a, b| {
            a.rejected_at
                .cmp(&b.rejected_at)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });
        Ok(records)
    }

    /// Whether a new attempt is a materially equivalent repeat of a rejected
    /// one: same skill, operation, fingerprint and environment, and no evidence
    /// the earlier attempts did not already have. Returns the rejection that
    /// suppresses it.
    pub fn suppressing_rejection(
        &self,
        skill: &SkillId,
        operation: ProposalOperation,
        fingerprint: &str,
        evidence_ids: &[String],
        environment_binding: &str,
    ) -> Result<Option<RejectionRecord>, LifecycleError> {
        let matching: Vec<RejectionRecord> = self
            .rejections_for(skill)?
            .into_iter()
            .filter(|r| {
                r.operation == operation && r.fingerprint == fingerprint && r.environment_binding == environment_binding
            })
            .collect();
        let known: BTreeSet<&String> = matching.iter().flat_map(|r| r.evidence_ids.iter()).collect();
        if matching.is_empty() || evidence_ids.iter().any(|id| !known.contains(id)) {
            return Ok(None);
        }
        Ok(matching.into_iter().next())
    }
}
