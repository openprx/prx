//! Private immutable adopted history and a bounded owner/family/skill index.
//! Only the historical-proof owner may mint a verified record. This is not a
//! publisher, a memory backend or a second accounting ledger.
use super::context::{LessonCompatibility, LessonRef};
use crate::decision::{DecisionKey, KeyDomain};
use crate::skills::lifecycle::generation::ensure_dir_synced;
use crate::skills::lifecycle::store::{check_private, record_bytes};
use crate::skills::lifecycle::{LifecycleError, OwnerScope, RevisionId, SkillId, SkillLifecycleStore};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) const RECORD_CAP: u64 = 16 * 1024;
pub(super) const INDEX_CAP: u64 = 32 * 1024;
pub(super) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub(super) fn refusal(message: &str) -> LifecycleError {
    LifecycleError::ProposalRefused(message.into())
}
pub(super) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A deliberately small source projection, never case/oracle/output evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LessonEdit {
    pub op: String,
    pub old: Option<String>,
    pub new: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SupportedGuidance {
    pub edits: Vec<LessonEdit>,
    pub description: Option<String>,
}
impl SupportedGuidance {
    pub fn render(&self) -> Result<String, LifecycleError> {
        if self.edits.len() > 2 || (self.edits.is_empty() && self.description.is_none()) {
            return Err(refusal("unsupported adopted diff shape"));
        }
        for edit in &self.edits {
            if !matches!(edit.op.as_str(), "replace" | "append")
                || (edit.op == "replace" && edit.old.as_ref().is_none_or(String::is_empty))
                || edit.new.is_empty()
            {
                return Err(refusal("unsupported adopted edit"));
            }
        }
        let bytes = serde_json::to_vec(self).map_err(|_| refusal("guidance encoding failed"))?;
        if !bytes.is_ascii() || bytes.len() > 512 {
            return Err(refusal("adopted guidance exceeds the supported ASCII item bound"));
        }
        // Do not allow quoted source to create a formatter boundary or token tag.
        let rendered = String::from_utf8(bytes)
            .map_err(|_| refusal("guidance is not UTF-8"))?
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace("BEGIN_PRX_LESSON", "BEGIN\\u005fPRX\\u005fLESSON")
            .replace("END_PRX_LESSON", "END\\u005fPRX\\u005fLESSON");
        if rendered.len() > 512 {
            return Err(refusal("escaped adopted guidance exceeds its item bound"));
        }
        Ok(rendered)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdoptionRecord {
    pub schema_version: u32,
    pub owner_scope: OwnerScope,
    pub adoption_id: String,
    pub skill_id: SkillId,
    pub task_family: String,
    pub proposal_id: String,
    pub publication_tx: String,
    pub origin_baseline: RevisionId,
    pub adopted_revision: RevisionId,
    pub adopted_record_sha256: String,
    pub adopted_bundle_bytes: u64,
    pub experiment_id: String,
    pub experiment_sha256: String,
    pub gate_review_pin: serde_json::Value,
    pub evidence_pin_sha256: String,
    pub compatibility: LessonCompatibility,
    pub excluded_roots: Vec<String>,
    pub source: SupportedGuidance,
    pub root_cost_evidence_sha256: Option<String>,
    pub committed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}
impl AdoptionRecord {
    pub fn reference(&self) -> Result<LessonRef, LifecycleError> {
        Ok(LessonRef {
            adoption_id: self.adoption_id.clone(),
            adoption_sha256: digest(&serde_json::to_vec(self).map_err(|_| refusal("adoption encoding failed"))?),
            publication_tx: self.publication_tx.clone(),
            adopted_revision: self.adopted_revision.clone(),
            adopted_record_sha256: self.adopted_record_sha256.clone(),
        })
    }
}
/// Constructed only after exact publication and D opaque history checks.
pub(super) struct VerifiedAdoptionRecord(pub(super) AdoptionRecord);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IndexEntry {
    pub reference: LessonRef,
    pub retired: bool,
    pub invalidated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdoptionIndex {
    pub schema_version: u32,
    pub owner_scope: OwnerScope,
    pub task_family: String,
    pub skill_id: SkillId,
    pub entries: Vec<IndexEntry>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sealed<T> {
    value: T,
    sha256: String,
    mac: String,
}

pub(super) fn read_bounded_bytes(path: &Path, cap: u64) -> Result<Vec<u8>, LifecycleError> {
    check_private(path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| LifecycleError::io(path, error))?;
    let metadata = file.metadata().map_err(|error| LifecycleError::io(path, error))?;
    if !metadata.is_file() || metadata.len() > cap {
        return Err(refusal("adoption record exceeds bounded regular-file input"));
    }
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| LifecycleError::io(path, error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
        return Err(refusal("adoption input changed beyond its read bound"));
    }
    Ok(bytes)
}
impl SkillLifecycleStore {
    fn adoption_key(&self) -> Result<std::sync::Arc<DecisionKey>, LifecycleError> {
        let workspace = self
            .root()
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| refusal("adoption store has no owner workspace"))?;
        DecisionKey::load_existing(workspace).map_err(|_| refusal("existing adoption evidence key is unavailable"))
    }
    pub(super) fn adoption_directory(&self) -> PathBuf {
        self.root().join("adoptions")
    }
    pub(super) fn adoption_record_path(&self, id: &str) -> Result<PathBuf, LifecycleError> {
        let suffix = id
            .strip_prefix("adopt-")
            .ok_or_else(|| refusal("invalid adoption id"))?;
        if !valid_digest(suffix) {
            return Err(refusal("invalid adoption id"));
        }
        Ok(self.adoption_directory().join("records").join(format!("{id}.json")))
    }
    pub(super) fn adoption_index_path(&self, family: &str, skill: &SkillId) -> Result<PathBuf, LifecycleError> {
        if family.is_empty() || family.len() > 128 || family.chars().any(char::is_control) {
            return Err(refusal("invalid lesson family"));
        }
        Ok(self
            .adoption_directory()
            .join("indexes")
            .join(format!("{}-{}.json", digest(family.as_bytes()), skill)))
    }
    pub(super) fn read_adoption_sealed<T: DeserializeOwned + Serialize>(
        &self,
        bytes: &[u8],
        domain: &[u8],
    ) -> Result<(T, String), LifecycleError> {
        let sealed: Sealed<T> =
            serde_json::from_slice(bytes).map_err(|_| refusal("invalid sealed adoption evidence"))?;
        let payload = serde_json::to_vec(&sealed.value).map_err(|_| refusal("adoption evidence encoding failed"))?;
        let sha256 = digest(&payload);
        let key = self.adoption_key()?;
        if sealed.sha256 != sha256
            || sealed.mac
                != hex::encode(key.hmac(
                    KeyDomain::TerminalClaim,
                    &[domain, self.owner_scope().as_str().as_bytes(), &payload],
                ))
        {
            return Err(refusal("adoption evidence seal changed"));
        }
        Ok((sealed.value, sha256))
    }
    fn adoption_sealed_bytes<T: Serialize>(
        &self,
        value: &T,
        path: &Path,
        domain: &[u8],
    ) -> Result<Vec<u8>, LifecycleError> {
        let payload = serde_json::to_vec(value).map_err(|_| refusal("adoption evidence encoding failed"))?;
        let key = self.adoption_key()?;
        let sealed = Sealed {
            value,
            sha256: digest(&payload),
            mac: hex::encode(key.hmac(
                KeyDomain::TerminalClaim,
                &[domain, self.owner_scope().as_str().as_bytes(), &payload],
            )),
        };
        record_bytes(&sealed, path)
    }
    pub(super) fn decode_adoption(
        &self,
        bytes: &[u8],
        reference: &LessonRef,
    ) -> Result<AdoptionRecord, LifecycleError> {
        let (record, hash): (AdoptionRecord, _) = self.read_adoption_sealed(bytes, b"prx-adopted-history-v1")?;
        if hash != reference.adoption_sha256
            || record.schema_version != 1
            || record.owner_scope != *self.owner_scope()
            || record.reference()? != *reference
            || record.expires_at <= record.committed_at
        {
            return Err(refusal("adoption evidence belongs to another origin"));
        }
        record.source.render()?;
        Ok(record)
    }
    pub(super) fn read_adoption_index_locked(
        &self,
        family: &str,
        skill: &SkillId,
    ) -> Result<Option<(AdoptionIndex, Vec<u8>)>, LifecycleError> {
        let path = self.adoption_index_path(family, skill)?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(LifecycleError::io(&path, error)),
            _ => {}
        }
        let bytes = read_bounded_bytes(&path, INDEX_CAP)?;
        let (index, _): (AdoptionIndex, _) = self.read_adoption_sealed(&bytes, b"prx-adoption-index-v1")?;
        if index.schema_version != 1
            || index.owner_scope != *self.owner_scope()
            || index.task_family != family
            || index.skill_id != *skill
            || index.entries.windows(2).any(|pair| match pair {
                [a, b] => a.reference.adoption_id >= b.reference.adoption_id,
                _ => false,
            })
        {
            return Err(refusal("adoption index owner, family or canonical order changed"));
        }
        Ok(Some((index, bytes)))
    }
    pub(super) fn persist_verified_adoption(
        &self,
        verified: &VerifiedAdoptionRecord,
    ) -> Result<LessonRef, LifecycleError> {
        let record = &verified.0;
        if record.owner_scope != *self.owner_scope() || record.schema_version != 1 {
            return Err(refusal("adoption owner mismatch"));
        }
        let reference = record.reference()?;
        let _store = self.lock_store(true, Duration::from_secs(5))?;
        let _skill = self.lock_skill(&record.skill_id, Duration::from_secs(5))?;
        let root = self.adoption_directory();
        ensure_dir_synced(self.root(), &root)?;
        for name in ["records", "indexes"] {
            ensure_dir_synced(&root, &root.join(name))?;
        }
        let path = self.adoption_record_path(&record.adoption_id)?;
        let bytes = self.adoption_sealed_bytes(record, &path, b"prx-adopted-history-v1")?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > RECORD_CAP {
            return Err(refusal("adoption exceeds record capacity"));
        }
        if fs::symlink_metadata(&path).is_ok() {
            self.decode_adoption(&read_bounded_bytes(&path, RECORD_CAP)?, &reference)?;
        } else {
            self.write_atomic(&record.skill_id, &path, &bytes)?;
        }
        let mut index = self
            .read_adoption_index_locked(&record.task_family, &record.skill_id)?
            .map_or_else(
                || AdoptionIndex {
                    schema_version: 1,
                    owner_scope: self.owner_scope().clone(),
                    task_family: record.task_family.clone(),
                    skill_id: record.skill_id.clone(),
                    entries: Vec::new(),
                },
                |(index, _)| index,
            );
        if let Some(existing) = index
            .entries
            .iter()
            .find(|entry| entry.reference.adoption_id == reference.adoption_id)
        {
            if existing.reference != reference {
                return Err(refusal("conflicting adoption index pin"));
            }
            return Ok(reference);
        }
        index.entries.push(IndexEntry {
            reference: reference.clone(),
            retired: false,
            invalidated: false,
        });
        index
            .entries
            .sort_by(|a, b| a.reference.adoption_id.cmp(&b.reference.adoption_id));
        let path = self.adoption_index_path(&record.task_family, &record.skill_id)?;
        let bytes = self.adoption_sealed_bytes(&index, &path, b"prx-adoption-index-v1")?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > INDEX_CAP {
            return Err(refusal("adoption partition exceeds its bounded index capacity"));
        }
        self.write_atomic(&record.skill_id, &path, &bytes)?;
        Ok(reference)
    }
}
