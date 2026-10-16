//! Bounded consolidation of experiences into knowledge patterns (SL04).
//!
//! `consolidate_pattern` validates every evidence reference against the stored
//! experiences (it must exist, belong to this owner, have a joined terminal and
//! no dropped or late evidence, and, for skill-related classes, have actually
//! hydrated the skill), merges only inside an identical scope, keeps
//! contradicting evidence, and never writes to an experience. Updates run under
//! the store's shared lock and one knowledge lock; pattern files are immutable
//! and the index is derived from them.

use super::generation::ensure_dir_synced;
use super::knowledge::{
    ConfidenceBasis, FailureClass, KnowledgePattern, MAX_CAPABILITIES, MAX_ITEM_BYTES, MAX_LIST_ITEMS, MAX_NAME_BYTES,
    MAX_PATTERN_EVIDENCE, MAX_STATEMENT_BYTES, PatternId, PatternIndex, PatternIndexEntry, PatternScope,
};
use super::store::{SkillLifecycleStore, check_private, read_record, record_bytes};
use super::types::{LifecycleError, SCHEMA_VERSION, SkillId};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

pub(super) const PATTERNS_DIR: &str = "patterns";
const INDEX_FILE: &str = "index.json";
const KNOWLEDGE_LOCK_ID: &str = "knowledge-patterns";
const KNOWLEDGE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// A request to record or extend a pattern.
#[derive(Clone, Debug)]
pub struct ConsolidationRequest {
    pub task_family: String,
    pub failure_class: FailureClass,
    pub skill_id: Option<SkillId>,
    pub scope: PatternScope,
    pub statement: String,
    pub applicability: Vec<String>,
    pub exceptions: Vec<String>,
    /// Experience ids that support the statement.
    pub evidence: Vec<String>,
    /// Experience ids that contradict it.
    pub counterevidence: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ConsolidationOutcome {
    pub pattern: KnowledgePattern,
    /// `false` when the identical pattern already existed.
    pub created: bool,
}

fn invalid(reason: impl Into<String>) -> LifecycleError {
    LifecycleError::InvalidKnowledge(reason.into())
}

pub(super) fn check_field(what: &str, value: &str, max: usize) -> Result<(), LifecycleError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(|c| c.is_control() && c != '\n') {
        return Err(invalid(format!(
            "{what} must be 1..={max} bytes without control characters"
        )));
    }
    Ok(())
}

fn check_list(what: &str, items: &[String]) -> Result<(), LifecycleError> {
    if items.len() > MAX_LIST_ITEMS {
        return Err(invalid(format!("{what} has more than {MAX_LIST_ITEMS} items")));
    }
    items
        .iter()
        .try_for_each(|item| check_field(what, item, MAX_ITEM_BYTES))
}

fn sorted_unique(items: &[String]) -> Vec<String> {
    items.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect()
}

impl ConsolidationRequest {
    fn validate(&self) -> Result<(), LifecycleError> {
        check_field("task family", &self.task_family, MAX_NAME_BYTES)?;
        check_field("statement", &self.statement, MAX_STATEMENT_BYTES)?;
        check_field("environment", &self.scope.environment, MAX_NAME_BYTES)?;
        if self.scope.capabilities.len() > MAX_CAPABILITIES {
            return Err(invalid(format!("more than {MAX_CAPABILITIES} capabilities")));
        }
        self.scope
            .capabilities
            .iter()
            .try_for_each(|capability| check_field("capability", capability, MAX_NAME_BYTES))?;
        check_list("applicability", &self.applicability)?;
        check_list("exception", &self.exceptions)?;
        if self.evidence.is_empty() {
            return Err(invalid("a pattern needs at least one supporting experience"));
        }
        if self.evidence.len() > MAX_PATTERN_EVIDENCE || self.counterevidence.len() > MAX_PATTERN_EVIDENCE {
            return Err(invalid(format!("more than {MAX_PATTERN_EVIDENCE} evidence references")));
        }
        if self.skill_id.is_none()
            && matches!(
                self.failure_class,
                FailureClass::MissingGuidance | FailureClass::GuidanceNotFollowed | FailureClass::WrongSkillRetrieved
            )
        {
            return Err(invalid("a skill-related failure class needs the skill it is about"));
        }
        Ok(())
    }

    /// The same request with its lists normalised (scope capabilities, evidence).
    fn normalised(&self) -> Self {
        let mut normalised = self.clone();
        normalised.scope.capabilities = sorted_unique(&self.scope.capabilities);
        normalised.evidence = sorted_unique(&self.evidence);
        normalised.counterevidence = sorted_unique(&self.counterevidence);
        normalised
    }
}

fn group_matches(entry: &PatternIndexEntry, request: &ConsolidationRequest) -> bool {
    entry.task_family == request.task_family
        && entry.failure_class == request.failure_class
        && entry.skill_id == request.skill_id
        && entry.scope == request.scope
}

fn pattern_digest(request: &ConsolidationRequest, evidence: &[String], counter: &[String], owner: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"prx-knowledge-pattern\0");
    let skill = request.skill_id.as_ref().map(SkillId::as_str).unwrap_or_default();
    let class = format!("{:?}", request.failure_class);
    let parts: Vec<&str> = [
        owner,
        &request.task_family,
        &class,
        skill,
        &request.scope.environment,
        &request.statement,
    ]
    .into_iter()
    .chain(request.scope.capabilities.iter().map(String::as_str))
    .chain(["|applicability"])
    .chain(request.applicability.iter().map(String::as_str))
    .chain(["|exceptions"])
    .chain(request.exceptions.iter().map(String::as_str))
    .chain(["|evidence"])
    .chain(evidence.iter().map(String::as_str))
    .chain(["|counter"])
    .chain(counter.iter().map(String::as_str))
    .collect();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hex::encode(hasher.finalize())
}

impl SkillLifecycleStore {
    fn patterns_dir(&self) -> PathBuf {
        self.root().join(PATTERNS_DIR)
    }

    fn pattern_path(&self, id: &PatternId) -> PathBuf {
        self.patterns_dir().join(format!("{id}.json"))
    }

    /// Reads and verifies one pattern; `None` when it does not exist.
    pub fn read_pattern(&self, id: &PatternId) -> Result<Option<KnowledgePattern>, LifecycleError> {
        let path = self.pattern_path(id);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&path, e)),
        }
        check_private(&path)?;
        let pattern: KnowledgePattern = read_record(&path)?;
        if pattern.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "pattern",
                found: pattern.schema_version,
            });
        }
        self.check_owner_scope(&pattern.owner_scope)?;
        if pattern.pattern_id != *id || pattern.record_id != id.as_str() {
            return Err(LifecycleError::Tampered {
                path,
                reason: "pattern identity does not match its location".to_string(),
            });
        }
        Ok(Some(pattern))
    }

    /// The derived index; an empty one when no pattern was recorded yet.
    pub fn pattern_index(&self) -> Result<PatternIndex, LifecycleError> {
        let path = self.patterns_dir().join(INDEX_FILE);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PatternIndex::empty(self.owner_scope())),
            Err(e) => return Err(LifecycleError::io(&path, e)),
        }
        check_private(&path)?;
        let index: PatternIndex = read_record(&path)?;
        if index.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "pattern index",
                found: index.schema_version,
            });
        }
        self.check_owner_scope(&index.owner_scope)?;
        Ok(index)
    }

    /// Patterns that no later pattern superseded.
    pub fn active_patterns(&self) -> Result<Vec<KnowledgePattern>, LifecycleError> {
        let mut patterns = Vec::new();
        for entry in self
            .pattern_index()?
            .entries
            .into_iter()
            .filter(|e| e.superseded_by.is_none())
        {
            patterns.push(
                self.read_pattern(&entry.pattern_id)?
                    .ok_or_else(|| invalid(format!("indexed pattern {} is missing", entry.pattern_id)))?,
            );
        }
        Ok(patterns)
    }

    /// Loads an evidence experience and checks that it may support (or
    /// contradict) a pattern about `skill`.
    fn usable_evidence(
        &self,
        id: &str,
        skill: Option<&SkillId>,
        requires_hydration: bool,
    ) -> Result<String, LifecycleError> {
        let unusable = |reason: &str| LifecycleError::EvidenceNotUsable {
            id: id.to_string(),
            reason: reason.to_string(),
        };
        let experience = self
            .read_experience_by_id(id)?
            .ok_or_else(|| unusable("no such experience"))?;
        if !experience.learning_eligible() {
            return Err(unusable("no joined terminal, or evidence was dropped or late"));
        }
        if requires_hydration
            && let Some(skill) = skill
            && !experience.used_skills().contains(&skill)
        {
            return Err(unusable("the skill was never hydrated in this experience"));
        }
        Ok(experience.task_id)
    }

    /// Records a pattern, or extends the active pattern of the same group with
    /// new evidence by writing a superseding one. Idempotent for an identical
    /// request. Blocking (file locks); async callers use `spawn_blocking`.
    pub fn consolidate_pattern(&self, request: &ConsolidationRequest) -> Result<ConsolidationOutcome, LifecycleError> {
        request.validate()?;
        let request = request.normalised();
        if request.evidence.iter().any(|id| request.counterevidence.contains(id)) {
            return Err(invalid("an experience cannot both support and contradict a pattern"));
        }
        let dir = self.patterns_dir();
        ensure_dir_synced(self.root(), &dir)?;
        let lock_id = SkillId::parse(KNOWLEDGE_LOCK_ID)?;
        let _store_lock = self.lock_store(true, KNOWLEDGE_LOCK_TIMEOUT)?;
        let _lock = self.lock_skill(&lock_id, KNOWLEDGE_LOCK_TIMEOUT)?;

        let mut index = self.pattern_index()?;
        let active: Vec<&PatternIndexEntry> = index
            .entries
            .iter()
            .filter(|entry| entry.superseded_by.is_none() && group_matches(entry, &request))
            .collect();
        if active.len() > 1 {
            return Err(invalid("more than one active pattern in a group; rebuild the index"));
        }
        let previous = match active.first() {
            Some(entry) => self.read_pattern(&entry.pattern_id)?,
            None => None,
        };

        // Evidence of the new request must be usable; evidence carried over
        // from the previous pattern is re-read too, so a pattern never rests on
        // an experience that stopped being eligible.
        let requires_hydration = matches!(
            request.failure_class,
            FailureClass::MissingGuidance | FailureClass::GuidanceNotFollowed
        );
        let mut evidence: BTreeSet<String> = request.evidence.iter().cloned().collect();
        let mut counter: BTreeSet<String> = request.counterevidence.iter().cloned().collect();
        if let Some(previous) = &previous {
            evidence.extend(previous.evidence_ids.iter().cloned());
            counter.extend(previous.counterevidence_ids.iter().cloned());
        }
        if evidence.len() > MAX_PATTERN_EVIDENCE || counter.len() > MAX_PATTERN_EVIDENCE {
            return Err(invalid(format!(
                "pattern evidence would exceed {MAX_PATTERN_EVIDENCE} references"
            )));
        }
        if evidence.intersection(&counter).next().is_some() {
            return Err(invalid(
                "new evidence contradicts the existing pattern's own evidence list",
            ));
        }
        let mut tasks = BTreeSet::new();
        let mut counter_tasks = BTreeSet::new();
        for id in &evidence {
            tasks.insert(self.usable_evidence(id, request.skill_id.as_ref(), requires_hydration)?);
        }
        for id in &counter {
            counter_tasks.insert(self.usable_evidence(id, request.skill_id.as_ref(), false)?);
        }
        let evidence: Vec<String> = evidence.into_iter().collect();
        let counter: Vec<String> = counter.into_iter().collect();

        let digest = pattern_digest(&request, &evidence, &counter, self.owner_scope().as_str());
        let pattern_id = PatternId::from_digest(&digest);
        let pattern = KnowledgePattern {
            schema_version: SCHEMA_VERSION,
            record_id: pattern_id.to_string(),
            owner_scope: self.owner_scope().clone(),
            pattern_id: pattern_id.clone(),
            task_family: request.task_family.clone(),
            failure_class: request.failure_class,
            kind: request.failure_class.kind(),
            skill_id: request.skill_id.clone(),
            scope: request.scope.clone(),
            statement: request.statement.clone(),
            applicability: request.applicability.clone(),
            exceptions: request.exceptions.clone(),
            confidence_basis: ConfidenceBasis {
                evidence_count: evidence.len(),
                counterevidence_count: counter.len(),
                distinct_tasks: tasks.len(),
                counter_distinct_tasks: counter_tasks.len(),
            },
            evidence_ids: evidence,
            counterevidence_ids: counter,
            supersedes: previous.iter().map(|p| p.pattern_id.clone()).collect(),
            created_at: Utc::now(),
        };

        // An identical pattern needs no write, but still repairs a missing
        // index entry left by an interrupted earlier call.
        let (pattern, created) = if let Some(existing) = self.read_pattern(&pattern_id)? {
            (existing, false)
        } else {
            let path = self.pattern_path(&pattern_id);
            self.write_atomic(&lock_id, &path, &record_bytes(&pattern, &path)?)?;
            (pattern, true)
        };
        let known = index.entries.iter().any(|entry| entry.pattern_id == pattern.pattern_id);
        if !known {
            for entry in &mut index.entries {
                if pattern.supersedes.contains(&entry.pattern_id) {
                    entry.superseded_by = Some(pattern.pattern_id.clone());
                }
            }
            index.entries.push(PatternIndexEntry {
                pattern_id: pattern.pattern_id.clone(),
                task_family: pattern.task_family.clone(),
                failure_class: pattern.failure_class,
                skill_id: pattern.skill_id.clone(),
                scope: pattern.scope.clone(),
                superseded_by: None,
            });
            self.write_pattern_index(&lock_id, &mut index)?;
        }
        Ok(ConsolidationOutcome { pattern, created })
    }

    fn write_pattern_index(&self, lock_id: &SkillId, index: &mut PatternIndex) -> Result<(), LifecycleError> {
        index.updated_at = Utc::now();
        let path = self.patterns_dir().join(INDEX_FILE);
        self.write_atomic(lock_id, &path, &record_bytes(index, &path)?)
    }

    /// Rebuilds `patterns/index.json` from the pattern files (the index is only
    /// a derived listing). Returns the number of patterns indexed.
    pub fn rebuild_pattern_index(&self) -> Result<usize, LifecycleError> {
        let dir = self.patterns_dir();
        ensure_dir_synced(self.root(), &dir)?;
        let lock_id = SkillId::parse(KNOWLEDGE_LOCK_ID)?;
        let _store_lock = self.lock_store(true, KNOWLEDGE_LOCK_TIMEOUT)?;
        let _lock = self.lock_skill(&lock_id, KNOWLEDGE_LOCK_TIMEOUT)?;
        let mut patterns = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| LifecycleError::io(&dir, e))? {
            let entry = entry.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json").filter(|id| id.starts_with("pat-")) else {
                continue;
            };
            let id = PatternId::parse(id)?;
            patterns.push(
                self.read_pattern(&id)?
                    .ok_or_else(|| invalid(format!("pattern {id} vanished during the rebuild")))?,
            );
        }
        patterns.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.pattern_id.cmp(&b.pattern_id))
        });
        let mut index = PatternIndex::empty(self.owner_scope());
        for pattern in &patterns {
            let superseded_by = patterns
                .iter()
                .find(|later| later.supersedes.contains(&pattern.pattern_id))
                .map(|later| later.pattern_id.clone());
            index.entries.push(PatternIndexEntry {
                pattern_id: pattern.pattern_id.clone(),
                task_family: pattern.task_family.clone(),
                failure_class: pattern.failure_class,
                skill_id: pattern.skill_id.clone(),
                scope: pattern.scope.clone(),
                superseded_by,
            });
        }
        self.write_pattern_index(&lock_id, &mut index)?;
        Ok(index.entries.len())
    }
}
