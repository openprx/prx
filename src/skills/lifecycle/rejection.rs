//! Rejection history (SL04): proposals that were rejected stay on record, and
//! a materially equivalent repeat is suppressed until new evidence or a changed
//! environment justifies another attempt.
//!
//! Records live in `rejections/<record-id>.json` and are immutable. Nothing in
//! publication, rollback or disable touches them: a rejected proposal's history
//! survives every later change of the active revision.

use super::consolidate::{
    EvidenceUse, check_evidence_id, check_ident, check_text, is_canonical, is_unusable, reserved_lock, sorted_unique,
};
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
/// Reserved lock identity (see `SkillId::reserved`): no skill id can equal it.
const REJECTION_LOCK_ID: &str = "_knowledge-rejections";
/// Rejection files one listing will read; past this a typed error.
const MAX_REJECTION_FILES: usize = 1024;
const MAX_MATERIAL_TEXTS: usize = 16;
const MAX_MATERIAL_BYTES: usize = 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

const NO_REQUIREMENTS: EvidenceUse<'static> = EvidenceUse {
    skill: None,
    requires_hydration: false,
    rejects_success: false,
    uncovered: false,
};

/// What a proposal would do to its skill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalOperation {
    Create,
    Revise,
    Disable,
}

/// Digest of the material content of a proposal: the tokens of the given
/// texts (the changed instruction text). Only layout is normalised: case,
/// whitespace, markdown markers (heading, list and quote prefixes, emphasis
/// marks around a word) and sentence punctuation after a word. Punctuation
/// inside a token and everything inside a code span is content (`x != y`,
/// `1.5s`, `rm -rf ./build` stay distinct from `x == y`, `15s`, `rm rf build`).
#[must_use]
pub fn material_digest(texts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"prx-proposal-material-v2\0");
    for text in texts {
        let tokens = material_tokens(text);
        hasher.update(u64::try_from(tokens.len()).unwrap_or(u64::MAX).to_be_bytes());
        for token in tokens {
            hasher.update(u64::try_from(token.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(token.as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

fn is_line_marker(token: &str) -> bool {
    matches!(token, "-" | "*" | "+" | ">")
        || (!token.is_empty() && token.len() <= 6 && token.bytes().all(|b| b == b'#'))
        || token
            .strip_suffix(['.', ')'])
            .is_some_and(|n| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
}

fn material_tokens(text: &str) -> Vec<String> {
    const MARKS: &[char] = &['*', '_', '~'];
    const SENTENCE: &[char] = &['.', ',', ';', ':', '!', '?'];
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_is_code = false;
    let mut in_code = false;
    let mut first_on_line = true;
    let mut flush = |current: &mut String, is_code: bool, first_on_line: &mut bool| {
        if current.is_empty() {
            return;
        }
        let raw = std::mem::take(current);
        if is_code {
            tokens.push(raw);
        } else if !(*first_on_line && is_line_marker(&raw)) {
            let trimmed = raw
                .trim_start_matches(MARKS)
                .trim_end_matches(|c| MARKS.contains(&c) || SENTENCE.contains(&c))
                .trim_end_matches(MARKS);
            if !trimmed.is_empty() {
                tokens.push(trimmed.to_string());
            }
        }
        *first_on_line = false;
    };
    for c in text.nfc().flat_map(char::to_lowercase) {
        if c == '`' {
            flush(&mut current, current_is_code, &mut first_on_line);
            in_code = !in_code;
        } else if c.is_whitespace() {
            flush(&mut current, current_is_code, &mut first_on_line);
            if c == '\n' && !in_code {
                first_on_line = true;
            }
        } else {
            if current.is_empty() {
                current_is_code = in_code;
            }
            current.push(c);
        }
    }
    flush(&mut current, current_is_code, &mut first_on_line);
    tokens
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
    /// The changed instruction texts of the proposal. The store derives the
    /// material fingerprint from them; callers do not supply one.
    pub material: Vec<String>,
    pub pattern_ids: Vec<PatternId>,
    pub evidence_ids: Vec<String>,
    pub environment_binding: String,
    pub reasons: Vec<String>,
}

fn check_material(texts: &[&str]) -> Result<String, LifecycleError> {
    if texts.is_empty() || texts.len() > MAX_MATERIAL_TEXTS || texts.iter().any(|t| t.len() > MAX_MATERIAL_BYTES) {
        return Err(LifecycleError::InvalidKnowledge(format!(
            "a proposal needs 1..={MAX_MATERIAL_TEXTS} material texts of at most {MAX_MATERIAL_BYTES} bytes"
        )));
    }
    if texts.iter().all(|t| material_tokens(t).is_empty()) {
        return Err(LifecycleError::InvalidKnowledge(
            "the proposal material has no content".to_string(),
        ));
    }
    Ok(material_digest(texts))
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
        check_ident("proposal id", &self.proposal_id, MAX_NAME_BYTES)?;
        check_ident("environment binding", &self.environment_binding, MAX_NAME_BYTES)?;
        if self.reasons.is_empty() || self.reasons.len() > MAX_LIST_ITEMS {
            return Err(LifecycleError::InvalidKnowledge(
                "a rejection needs between 1 and 16 reasons".to_string(),
            ));
        }
        self.reasons
            .iter()
            .try_for_each(|reason| check_text("reason", reason, MAX_ITEM_BYTES))?;
        if self.evidence_ids.len() > MAX_PATTERN_EVIDENCE || self.pattern_ids.len() > MAX_PATTERN_EVIDENCE {
            return Err(LifecycleError::InvalidKnowledge("too many references".to_string()));
        }
        self.evidence_ids.iter().try_for_each(|id| check_evidence_id(id))
    }
}

/// Bounds and canonical form of a stored record, as `record_rejection` writes it.
fn verify_record(record: &RejectionRecord) -> Result<(), LifecycleError> {
    check_ident("proposal id", &record.proposal_id, MAX_NAME_BYTES)?;
    check_ident("environment binding", &record.environment_binding, MAX_NAME_BYTES)?;
    let invalid = |reason: &str| LifecycleError::InvalidKnowledge(reason.to_string());
    if record.fingerprint.len() != 64
        || !record
            .fingerprint
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("fingerprint must be 64 lowercase hex digits"));
    }
    if record.reasons.is_empty() || record.reasons.len() > MAX_LIST_ITEMS {
        return Err(invalid("a rejection has between 1 and 16 reasons"));
    }
    record
        .reasons
        .iter()
        .try_for_each(|reason| check_text("reason", reason, MAX_ITEM_BYTES))?;
    if record.evidence_ids.len() > MAX_PATTERN_EVIDENCE || record.pattern_ids.len() > MAX_PATTERN_EVIDENCE {
        return Err(invalid("too many references"));
    }
    record.evidence_ids.iter().try_for_each(|id| check_evidence_id(id))?;
    if !is_canonical(&record.reasons) || !is_canonical(&record.evidence_ids) || !is_canonical(&record.pattern_ids) {
        return Err(invalid("lists must be sorted and free of duplicates"));
    }
    Ok(())
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
        let material: Vec<&str> = new.material.iter().map(String::as_str).collect();
        let fingerprint = check_material(&material)?;
        let evidence = sorted_unique(&new.evidence_ids);
        // Evidence that does not exist, is not this owner's, or is not
        // eligible cannot be recorded: it would later pose as "already known".
        for id in &evidence {
            self.usable_evidence(id, &NO_REQUIREMENTS)?;
        }
        let mut pattern_ids = new.pattern_ids.clone();
        pattern_ids.sort();
        pattern_ids.dedup();
        for pattern in &pattern_ids {
            if self.read_pattern(pattern)?.is_none() {
                return Err(LifecycleError::InvalidKnowledge(format!(
                    "rejection references unknown pattern {pattern}"
                )));
            }
        }
        let dir = self.rejections_dir();
        ensure_dir_synced(self.root(), &dir)?;
        let lock_id = reserved_lock(REJECTION_LOCK_ID)?;
        let _store_lock = self.lock_store(true, LOCK_TIMEOUT)?;
        let _lock = self.lock_skill(&lock_id, LOCK_TIMEOUT)?;
        let id = record_id_of(&new.proposal_id, self.owner_scope());
        let path = dir.join(format!("{id}.json"));
        let record = RejectionRecord {
            schema_version: SCHEMA_VERSION,
            record_id: id.clone(),
            owner_scope: self.owner_scope().clone(),
            proposal_id: new.proposal_id.clone(),
            operation: new.operation,
            skill_id: new.skill_id.clone(),
            fingerprint,
            pattern_ids,
            evidence_ids: evidence,
            environment_binding: new.environment_binding.clone(),
            reasons: sorted_unique(&new.reasons),
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
        verify_record(&record).map_err(|e| LifecycleError::Tampered {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
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
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json").filter(|id| id.starts_with("rej-")) else {
                continue;
            };
            if files.len() >= MAX_REJECTION_FILES {
                return Err(LifecycleError::KnowledgeLimit {
                    what: "rejection files",
                    limit: MAX_REJECTION_FILES,
                });
            }
            files.push((entry.path(), id.to_string()));
        }
        let mut records = Vec::new();
        for (path, id) in files {
            if let Some(record) = self.read_rejection_file(&path, &id)?
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
    /// one: same skill, operation, material fingerprint (derived here from
    /// `material`) and environment, and no new evidence. Evidence counts as new
    /// only when it exists, is this owner's, is eligible, did not end in
    /// success, hydrated this skill, and neither the earlier rejections nor
    /// the patterns they cite already had it; made-up, unrelated or ineligible
    /// ids are ignored.
    /// Returns the rejection that suppresses the attempt.
    pub fn suppressing_rejection(
        &self,
        skill: &SkillId,
        operation: ProposalOperation,
        material: &[&str],
        evidence_ids: &[String],
        environment_binding: &str,
    ) -> Result<Option<RejectionRecord>, LifecycleError> {
        let fingerprint = check_material(material)?;
        let matching: Vec<RejectionRecord> = self
            .rejections_for(skill)?
            .into_iter()
            .filter(|r| {
                r.operation == operation && r.fingerprint == fingerprint && r.environment_binding == environment_binding
            })
            .collect();
        self.first_unlifted(skill, operation, matching, evidence_ids)
    }

    /// Like [`Self::suppressing_rejection`], for the moment before a proposal
    /// is generated, when there is no material to fingerprint yet: an attempt
    /// from the same patterns in the same environment as a rejected one, with
    /// no new evidence, would be answered by the same model with the same
    /// input and is suppressed before any model call is paid for. A pattern
    /// that gained evidence is a new immutable pattern id, so it never matches.
    pub(super) fn suppressing_attempt(
        &self,
        skill: &SkillId,
        operation: ProposalOperation,
        pattern_ids: &[PatternId],
        evidence_ids: &[String],
        environment_binding: &str,
    ) -> Result<Option<RejectionRecord>, LifecycleError> {
        let mut wanted = pattern_ids.to_vec();
        wanted.sort();
        wanted.dedup();
        let matching: Vec<RejectionRecord> = self
            .rejections_for(skill)?
            .into_iter()
            .filter(|r| {
                r.operation == operation && r.environment_binding == environment_binding && r.pattern_ids == wanted
            })
            .collect();
        self.first_unlifted(skill, operation, matching, evidence_ids)
    }

    /// The first of `matching` when no evidence in `evidence_ids` lifts the
    /// suppression, `None` when `matching` is empty or new evidence exists.
    fn first_unlifted(
        &self,
        skill: &SkillId,
        operation: ProposalOperation,
        matching: Vec<RejectionRecord>,
        evidence_ids: &[String],
    ) -> Result<Option<RejectionRecord>, LifecycleError> {
        if matching.is_empty() {
            return Ok(None);
        }
        let mut known: BTreeSet<String> = matching.iter().flat_map(|r| r.evidence_ids.iter().cloned()).collect();
        // Evidence the rejected attempt's own patterns rest on is known too.
        for pattern_id in matching.iter().flat_map(|r| r.pattern_ids.iter()) {
            if let Some(pattern) = self.read_pattern(pattern_id)? {
                known.extend(pattern.evidence_ids);
                known.extend(pattern.counterevidence_ids);
            }
        }
        // Anything beyond that lifts suppression only if it hydrated this
        // skill, did not end in success, and is eligible. For a new skill there
        // is nothing to hydrate: new evidence is eligible work no skill covered.
        let same_skill = if operation == ProposalOperation::Create {
            EvidenceUse {
                skill: None,
                requires_hydration: false,
                rejects_success: false,
                uncovered: true,
            }
        } else {
            EvidenceUse {
                skill: Some(skill),
                requires_hydration: true,
                rejects_success: true,
                uncovered: false,
            }
        };
        for id in evidence_ids {
            if known.contains(id) {
                continue;
            }
            match self.usable_evidence(id, &same_skill) {
                Ok(_) => return Ok(None),
                Err(e) if is_unusable(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(matching.into_iter().next())
    }
}
