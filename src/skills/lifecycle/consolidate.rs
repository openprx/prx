//! Bounded consolidation of experiences into knowledge patterns (SL04).
//!
//! `consolidate_pattern` validates every evidence reference against the stored
//! experiences (it must exist, belong to this owner, have a joined terminal and
//! no dropped or late evidence, for skill-related classes have actually
//! hydrated the skill, and for guidance classes not have ended in success),
//! keeps contradicting evidence, and never writes to an experience. Updates run
//! under the store's shared lock and one knowledge lock; pattern files are
//! immutable and the index is derived from them.
//!
//! Group identity: a pattern revises an earlier one only when task family,
//! failure class, skill, scope and the normalised statement are all the same.
//! A different statement is a different lesson and a separate group, so it
//! never inherits another lesson's evidence. Inside a group, applicability and
//! exceptions are merged (union) with the earlier pattern's, never silently
//! replaced, and evidence carried over that stopped being eligible is moved to
//! `withdrawn_evidence` instead of blocking the group.
//!
//! The index lists only the active pattern of each group (bounded; history is
//! reached through `supersedes`). A crash between the pattern write and the
//! index write leaves an unindexed pattern file; every consolidation and every
//! rebuild adopts such files before doing anything else, resolving two
//! successors of one predecessor to a single active pattern (the later
//! `created_at`, ties by pattern id).

use super::capture::{ExperienceOutcome, is_experience_id};
use super::generation::ensure_dir_synced;
use super::knowledge::{
    ConfidenceBasis, FailureClass, KnowledgePattern, MAX_CAPABILITIES, MAX_ITEM_BYTES, MAX_LIST_ITEMS, MAX_NAME_BYTES,
    MAX_PATTERN_EVIDENCE, MAX_STATEMENT_BYTES, PatternId, PatternIndex, PatternIndexEntry, PatternScope,
};
use super::store::{MAX_RECORD_BYTES, SkillLifecycleStore, check_private, read_record, record_bytes};
use super::types::{LifecycleError, SCHEMA_VERSION, SkillId};
use chrono::Utc;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

pub(super) const PATTERNS_DIR: &str = "patterns";
const INDEX_FILE: &str = "index.json";
/// Reserved lock identity (see `SkillId::reserved`): no skill id can equal it.
const KNOWLEDGE_LOCK_ID: &str = "_knowledge-patterns";
const KNOWLEDGE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Pattern files (history included) one consolidation or rebuild will scan.
/// Pattern retention is SL09; past this a typed error asks for it.
const MAX_PATTERN_FILES: usize = 4096;

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

/// The reserved lock identity named `name`.
pub(super) fn reserved_lock(name: &str) -> Result<SkillId, LifecycleError> {
    SkillId::reserved(name).ok_or_else(|| invalid(format!("{name} is not a reserved lock name")))
}

fn check(what: &str, value: &str, max: usize, multiline: bool) -> Result<(), LifecycleError> {
    if value.trim().is_empty()
        || value.len() > max
        || value.chars().any(|c| c.is_control() && !(multiline && c == '\n'))
    {
        return Err(invalid(format!(
            "{what} must be 1..={max} bytes without control characters"
        )));
    }
    Ok(())
}

/// An identifier-like field: no control characters at all, newline included.
pub(super) fn check_ident(what: &str, value: &str, max: usize) -> Result<(), LifecycleError> {
    check(what, value, max, false)
}

/// Free text: may span lines, never contains other control characters.
pub(super) fn check_text(what: &str, value: &str, max: usize) -> Result<(), LifecycleError> {
    check(what, value, max, true)
}

pub(super) fn check_evidence_id(id: &str) -> Result<(), LifecycleError> {
    if is_experience_id(id) {
        Ok(())
    } else {
        Err(invalid(format!(
            "evidence id {id:?} must be exp- followed by 32 lowercase hex digits"
        )))
    }
}

fn check_list(what: &str, items: &[String]) -> Result<(), LifecycleError> {
    if items.len() > MAX_LIST_ITEMS {
        return Err(invalid(format!("{what} has more than {MAX_LIST_ITEMS} items")));
    }
    items.iter().try_for_each(|item| check_text(what, item, MAX_ITEM_BYTES))
}

pub(super) fn sorted_unique(items: &[String]) -> Vec<String> {
    items.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect()
}

fn is_canonical(items: &[String]) -> bool {
    items.iter().zip(items.iter().skip(1)).all(|(a, b)| a < b)
}

impl ConsolidationRequest {
    fn validate(&self) -> Result<(), LifecycleError> {
        check_ident("task family", &self.task_family, MAX_NAME_BYTES)?;
        check_text("statement", &self.statement, MAX_STATEMENT_BYTES)?;
        check_ident("environment", &self.scope.environment, MAX_NAME_BYTES)?;
        if self.scope.capabilities.len() > MAX_CAPABILITIES {
            return Err(invalid(format!("more than {MAX_CAPABILITIES} capabilities")));
        }
        self.scope
            .capabilities
            .iter()
            .try_for_each(|capability| check_ident("capability", capability, MAX_NAME_BYTES))?;
        check_list("applicability", &self.applicability)?;
        check_list("exception", &self.exceptions)?;
        if self.evidence.is_empty() {
            return Err(invalid("a pattern needs at least one supporting experience"));
        }
        if self.evidence.len() > MAX_PATTERN_EVIDENCE || self.counterevidence.len() > MAX_PATTERN_EVIDENCE {
            return Err(invalid(format!("more than {MAX_PATTERN_EVIDENCE} evidence references")));
        }
        self.evidence
            .iter()
            .chain(&self.counterevidence)
            .try_for_each(|id| check_evidence_id(id))?;
        if self.skill_id.is_none() && needs_skill(self.failure_class) {
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
        normalised.applicability = sorted_unique(&self.applicability);
        normalised.exceptions = sorted_unique(&self.exceptions);
        normalised
    }
}

const fn needs_skill(class: FailureClass) -> bool {
    matches!(
        class,
        FailureClass::MissingGuidance | FailureClass::GuidanceNotFollowed | FailureClass::WrongSkillRetrieved
    )
}

/// What an experience has to satisfy to serve as evidence.
pub(super) struct EvidenceUse<'a> {
    pub skill: Option<&'a SkillId>,
    /// The skill must have been hydrated in the experience.
    pub requires_hydration: bool,
    /// An experience that ended in success cannot support a failure of guidance.
    pub rejects_success: bool,
}

/// Whether the error says the experience cannot serve as evidence (as opposed
/// to a store failure that must propagate).
pub(super) const fn is_unusable(error: &LifecycleError) -> bool {
    matches!(
        error,
        LifecycleError::EvidenceNotUsable { .. } | LifecycleError::InvalidId { .. }
    )
}

/// Whether the loaded pattern content is internally consistent.
fn verify_content(pattern: &KnowledgePattern) -> Result<(), LifecycleError> {
    check_ident("task family", &pattern.task_family, MAX_NAME_BYTES)?;
    check_text("statement", &pattern.statement, MAX_STATEMENT_BYTES)?;
    check_ident("environment", &pattern.scope.environment, MAX_NAME_BYTES)?;
    if pattern.scope.capabilities.len() > MAX_CAPABILITIES {
        return Err(invalid("too many capabilities"));
    }
    pattern
        .scope
        .capabilities
        .iter()
        .try_for_each(|capability| check_ident("capability", capability, MAX_NAME_BYTES))?;
    check_list("applicability", &pattern.applicability)?;
    check_list("exception", &pattern.exceptions)?;
    let lists = [
        &pattern.scope.capabilities,
        &pattern.applicability,
        &pattern.exceptions,
        &pattern.evidence_ids,
        &pattern.counterevidence_ids,
        &pattern.withdrawn_evidence,
    ];
    if !lists.iter().all(|list| is_canonical(list)) {
        return Err(invalid("lists must be sorted and free of duplicates"));
    }
    for list in [
        &pattern.evidence_ids,
        &pattern.counterevidence_ids,
        &pattern.withdrawn_evidence,
    ] {
        if list.len() > MAX_PATTERN_EVIDENCE {
            return Err(invalid("too many evidence references"));
        }
        list.iter().try_for_each(|id| check_evidence_id(id))?;
    }
    if pattern.evidence_ids.is_empty() {
        return Err(invalid("a pattern needs supporting evidence"));
    }
    let all: BTreeSet<&String> = pattern
        .evidence_ids
        .iter()
        .chain(&pattern.counterevidence_ids)
        .chain(&pattern.withdrawn_evidence)
        .collect();
    if all.len() != pattern.evidence_ids.len() + pattern.counterevidence_ids.len() + pattern.withdrawn_evidence.len() {
        return Err(invalid("an experience appears in more than one evidence list"));
    }
    if pattern.kind != pattern.failure_class.kind() {
        return Err(invalid("pattern kind does not match its failure class"));
    }
    if pattern.skill_id.is_none() && needs_skill(pattern.failure_class) {
        return Err(invalid("a skill-related pattern needs its skill"));
    }
    if pattern.supersedes.len() > 1 {
        return Err(invalid("a pattern supersedes at most one pattern"));
    }
    let basis = &pattern.confidence_basis;
    if basis.evidence_count != pattern.evidence_ids.len()
        || basis.counterevidence_count != pattern.counterevidence_ids.len()
        || !(1..=basis.evidence_count).contains(&basis.distinct_tasks)
        || basis.counter_distinct_tasks > basis.counterevidence_count
        || (basis.counterevidence_count > 0 && basis.counter_distinct_tasks == 0)
    {
        return Err(invalid("confidence basis does not match the evidence lists"));
    }
    Ok(())
}

impl SkillLifecycleStore {
    fn patterns_dir(&self) -> PathBuf {
        self.root().join(PATTERNS_DIR)
    }

    fn pattern_path(&self, id: &PatternId) -> PathBuf {
        self.patterns_dir().join(format!("{id}.json"))
    }

    /// Reads and verifies one pattern; `None` when it does not exist. The
    /// content must be within bounds, internally consistent, and hash to the
    /// pattern id (a forged or hand-edited file fails closed).
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
        let tampered = |reason: String| LifecycleError::Tampered {
            path: path.clone(),
            reason,
        };
        if pattern.pattern_id != *id || pattern.record_id != id.as_str() {
            return Err(tampered("pattern identity does not match its location".to_string()));
        }
        verify_content(&pattern).map_err(|e| tampered(e.to_string()))?;
        if PatternId::from_digest(&pattern.content_digest()) != *id {
            return Err(tampered("pattern content does not match its id".to_string()));
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

    /// The active pattern of every group.
    pub fn active_patterns(&self) -> Result<Vec<KnowledgePattern>, LifecycleError> {
        let mut patterns = Vec::new();
        for entry in self.pattern_index()?.entries {
            patterns.push(
                self.read_pattern(&entry.pattern_id)?
                    .ok_or_else(|| invalid(format!("indexed pattern {} is missing", entry.pattern_id)))?,
            );
        }
        Ok(patterns)
    }

    /// Loads an evidence experience, checks that it may support (or contradict)
    /// a pattern about `usage.skill`, and returns the key its task counts under:
    /// the parent run when the experience has one, else its run, else the task,
    /// so sub-agent runs of one root task are one task.
    pub(super) fn usable_evidence(&self, id: &str, usage: &EvidenceUse<'_>) -> Result<String, LifecycleError> {
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
        if usage.rejects_success
            && experience
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.outcome == ExperienceOutcome::Success)
        {
            return Err(unusable(
                "the task ended in success, so it shows no failure of the guidance",
            ));
        }
        if usage.requires_hydration
            && let Some(skill) = usage.skill
            && !experience.used_skills().contains(&skill)
        {
            return Err(unusable("the skill was never hydrated in this experience"));
        }
        let correlation = &experience.correlation;
        Ok(
            match correlation.parent_run_id.as_ref().or(correlation.run_id.as_ref()) {
                Some(run) => format!("run:{run}"),
                None => format!("task:{}", experience.task_id),
            },
        )
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
        let lock_id = reserved_lock(KNOWLEDGE_LOCK_ID)?;
        let _store_lock = self.lock_store(true, KNOWLEDGE_LOCK_TIMEOUT)?;
        let _lock = self.lock_skill(&lock_id, KNOWLEDGE_LOCK_TIMEOUT)?;

        let mut index = self.pattern_index()?;
        let dirty = self.adopt_unindexed(&mut index)?;
        let group = PatternIndexEntry {
            pattern_id: PatternId::from_digest(""),
            task_family: request.task_family.clone(),
            failure_class: request.failure_class,
            skill_id: request.skill_id.clone(),
            scope: request.scope.clone(),
            statement_key: super::knowledge::statement_key(&request.statement),
        };
        let active: Vec<&PatternIndexEntry> = index.entries.iter().filter(|entry| entry.same_group(&group)).collect();
        if active.len() > 1 {
            return Err(invalid("more than one active pattern in a group; rebuild the index"));
        }
        let previous = match active.first() {
            Some(entry) => Some(self.read_pattern(&entry.pattern_id)?.ok_or_else(|| {
                invalid(format!(
                    "indexed pattern {} is missing; rebuild the index",
                    entry.pattern_id
                ))
            })?),
            None => None,
        };

        let (support_use, counter_use) = (
            EvidenceUse {
                skill: request.skill_id.as_ref(),
                requires_hydration: matches!(
                    request.failure_class,
                    FailureClass::MissingGuidance | FailureClass::GuidanceNotFollowed
                ),
                rejects_success: matches!(
                    request.failure_class,
                    FailureClass::MissingGuidance | FailureClass::GuidanceNotFollowed
                ),
            },
            EvidenceUse {
                skill: request.skill_id.as_ref(),
                requires_hydration: false,
                rejects_success: false,
            },
        );
        // Requested evidence must be usable now. Evidence carried over from
        // the previous pattern is re-read too; if it stopped being eligible
        // it is withdrawn (kept for audit) instead of blocking the group.
        let mut tasks = BTreeSet::new();
        let mut counter_tasks = BTreeSet::new();
        let mut evidence: BTreeSet<String> = BTreeSet::new();
        let mut counter: BTreeSet<String> = BTreeSet::new();
        let mut withdrawn: BTreeSet<String> = BTreeSet::new();
        for id in &request.evidence {
            tasks.insert(self.usable_evidence(id, &support_use)?);
            evidence.insert(id.clone());
        }
        for id in &request.counterevidence {
            counter_tasks.insert(self.usable_evidence(id, &counter_use)?);
            counter.insert(id.clone());
        }
        if let Some(previous) = &previous {
            withdrawn.extend(previous.withdrawn_evidence.iter().cloned());
            let carried = previous
                .evidence_ids
                .iter()
                .map(|id| (id, true))
                .chain(previous.counterevidence_ids.iter().map(|id| (id, false)));
            for (id, supports) in carried {
                if evidence.contains(id) || counter.contains(id) {
                    if evidence.contains(id) != supports {
                        return Err(invalid(
                            "new evidence contradicts the existing pattern's own evidence list",
                        ));
                    }
                    continue;
                }
                let usage = if supports { &support_use } else { &counter_use };
                match self.usable_evidence(id, usage) {
                    Ok(task) if supports => {
                        tasks.insert(task);
                        evidence.insert(id.clone());
                    }
                    Ok(task) => {
                        counter_tasks.insert(task);
                        counter.insert(id.clone());
                    }
                    Err(e) if is_unusable(&e) => {
                        tracing::warn!(evidence = %id, error = %e, "carried-over evidence withdrawn from pattern");
                        withdrawn.insert(id.clone());
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        withdrawn.retain(|id| !evidence.contains(id) && !counter.contains(id));
        if evidence.len() > MAX_PATTERN_EVIDENCE || counter.len() > MAX_PATTERN_EVIDENCE {
            return Err(invalid(format!(
                "pattern evidence would exceed {MAX_PATTERN_EVIDENCE} references"
            )));
        }
        let merge = |old: Option<&Vec<String>>, new: &[String]| -> Vec<String> {
            sorted_unique(
                &old.map(|o| o.iter().chain(new).cloned().collect::<Vec<_>>())
                    .unwrap_or_else(|| new.to_vec()),
            )
        };
        let applicability = merge(previous.as_ref().map(|p| &p.applicability), &request.applicability);
        let exceptions = merge(previous.as_ref().map(|p| &p.exceptions), &request.exceptions);
        check_list("applicability", &applicability)?;
        check_list("exception", &exceptions)?;

        let evidence: Vec<String> = evidence.into_iter().collect();
        let counter: Vec<String> = counter.into_iter().collect();
        let mut pattern = KnowledgePattern {
            schema_version: SCHEMA_VERSION,
            record_id: String::new(),
            owner_scope: self.owner_scope().clone(),
            pattern_id: PatternId::from_digest(""),
            task_family: request.task_family.clone(),
            failure_class: request.failure_class,
            kind: request.failure_class.kind(),
            skill_id: request.skill_id.clone(),
            scope: request.scope.clone(),
            // An equivalent statement keeps the earlier wording, so a
            // re-worded repeat is the same pattern.
            statement: previous
                .as_ref()
                .map_or_else(|| request.statement.clone(), |p| p.statement.clone()),
            applicability,
            exceptions,
            confidence_basis: ConfidenceBasis {
                evidence_count: evidence.len(),
                counterevidence_count: counter.len(),
                distinct_tasks: tasks.len(),
                counter_distinct_tasks: counter_tasks.len(),
            },
            evidence_ids: evidence,
            counterevidence_ids: counter,
            withdrawn_evidence: withdrawn.into_iter().collect(),
            supersedes: previous.iter().map(|p| p.pattern_id.clone()).collect(),
            created_at: Utc::now(),
        };
        let pattern_id = PatternId::from_digest(&pattern.content_digest());
        pattern.record_id = pattern_id.to_string();
        pattern.pattern_id = pattern_id.clone();

        // Unchanged content: the active pattern already is this pattern.
        if previous.as_ref().is_some_and(|p| p.pattern_id == pattern_id) {
            if dirty {
                self.write_pattern_index(&lock_id, &mut index)?;
            }
            return Ok(ConsolidationOutcome {
                pattern: previous.unwrap_or(pattern),
                created: false,
            });
        }
        // Size the new index before anything is written: a refusal leaves no
        // orphan pattern file behind.
        let mut next = index.clone();
        next.entries.retain(|entry| !entry.same_group(&group));
        next.entries.push(PatternIndexEntry::of(&pattern));
        let index_file = self.index_bytes(&mut next)?;
        let created = match self.read_pattern(&pattern_id)? {
            Some(_) => false,
            None => {
                let path = self.pattern_path(&pattern_id);
                self.write_atomic(&lock_id, &path, &record_bytes(&pattern, &path)?)?;
                true
            }
        };
        self.write_atomic(&lock_id, &self.patterns_dir().join(INDEX_FILE), &index_file)?;
        Ok(ConsolidationOutcome { pattern, created })
    }

    /// Serialises the index, refusing one past the record cap that readers
    /// enforce, so a written index can always be read back.
    fn index_bytes(&self, index: &mut PatternIndex) -> Result<Vec<u8>, LifecycleError> {
        index.updated_at = Utc::now();
        let path = self.patterns_dir().join(INDEX_FILE);
        let bytes = record_bytes(index, &path)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RECORD_BYTES {
            return Err(LifecycleError::KnowledgeLimit {
                what: "active patterns in the index (bytes)",
                limit: usize::try_from(MAX_RECORD_BYTES).unwrap_or(usize::MAX),
            });
        }
        Ok(bytes)
    }

    fn write_pattern_index(&self, lock_id: &SkillId, index: &mut PatternIndex) -> Result<(), LifecycleError> {
        let bytes = self.index_bytes(index)?;
        self.write_atomic(lock_id, &self.patterns_dir().join(INDEX_FILE), &bytes)
    }

    /// Adopts pattern files the index does not list (left by a crash between
    /// the pattern write and the index write, or by a lost index) and resolves
    /// them against the index so each group has exactly one active pattern: a
    /// successor replaces its predecessor; of two unrelated candidates for one
    /// group the later `created_at` (ties: larger id) wins. Returns whether the
    /// index changed. Unreadable or forged files are skipped with a warning.
    fn adopt_unindexed(&self, index: &mut PatternIndex) -> Result<bool, LifecycleError> {
        let dir = self.patterns_dir();
        let indexed: BTreeSet<PatternId> = index.entries.iter().map(|entry| entry.pattern_id.clone()).collect();
        let mut candidates = Vec::new();
        let mut seen = 0usize;
        for item in fs::read_dir(&dir).map_err(|e| LifecycleError::io(&dir, e))? {
            let item = item.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = item.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json").filter(|id| id.starts_with("pat-")) else {
                continue;
            };
            seen += 1;
            if seen > MAX_PATTERN_FILES {
                return Err(LifecycleError::KnowledgeLimit {
                    what: "pattern files (retention is required)",
                    limit: MAX_PATTERN_FILES,
                });
            }
            let Ok(id) = PatternId::parse(id) else {
                continue;
            };
            if indexed.contains(&id) {
                continue;
            }
            match self.read_pattern(&id) {
                Ok(Some(pattern)) => candidates.push(pattern),
                Ok(None) => {}
                Err(e) => tracing::warn!(pattern = %id, error = %e, "skipping unreadable pattern file"),
            }
        }
        candidates.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.pattern_id.cmp(&b.pattern_id))
        });
        let mut changed = false;
        for candidate in candidates {
            let entry = PatternIndexEntry::of(&candidate);
            let Some(slot) = index.entries.iter_mut().find(|e| e.same_group(&entry)) else {
                index.entries.push(entry);
                changed = true;
                continue;
            };
            let current_id = slot.pattern_id.clone();
            let wins = if candidate.supersedes.contains(&current_id) {
                true
            } else {
                let current = self
                    .read_pattern(&current_id)?
                    .ok_or_else(|| invalid(format!("indexed pattern {current_id} is missing")))?;
                !current.supersedes.contains(&candidate.pattern_id)
                    && (candidate.created_at, &candidate.pattern_id) > (current.created_at, &current.pattern_id)
            };
            if wins {
                *slot = entry;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Rebuilds `patterns/index.json` from the pattern files (the index is only
    /// a derived listing of the active pattern of each group). Returns the
    /// number of active patterns indexed.
    pub fn rebuild_pattern_index(&self) -> Result<usize, LifecycleError> {
        let dir = self.patterns_dir();
        ensure_dir_synced(self.root(), &dir)?;
        let lock_id = reserved_lock(KNOWLEDGE_LOCK_ID)?;
        let _store_lock = self.lock_store(true, KNOWLEDGE_LOCK_TIMEOUT)?;
        let _lock = self.lock_skill(&lock_id, KNOWLEDGE_LOCK_TIMEOUT)?;
        let mut index = PatternIndex::empty(self.owner_scope());
        self.adopt_unindexed(&mut index)?;
        self.write_pattern_index(&lock_id, &mut index)?;
        Ok(index.entries.len())
    }
}
