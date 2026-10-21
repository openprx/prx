//! Knowledge patterns (SL04): evidence-linked, revisable statements derived
//! from task experiences.
//!
//! Experiences stay append-only and are never modified by consolidation; a
//! pattern only references them by id. A pattern is immutable once written; a
//! revision is a new pattern that `supersedes` the old one. Contradicting
//! evidence is kept in `counterevidence_ids`, never dropped.
//!
//! Layout inside the owner store: `patterns/<pattern-id>.json` for active
//! patterns (and crash leftovers), `patterns/history/` for superseded ones,
//! `patterns/quarantine/` for unreadable or forged files, and the derived
//! `patterns/index.json` (rebuildable from the pattern files).

use super::types::{LifecycleError, OwnerScope, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use unicode_normalization::UnicodeNormalization;

/// Evidence references kept per pattern (evidence and counterevidence each).
pub const MAX_PATTERN_EVIDENCE: usize = 256;
/// Distinct root tasks a pattern needs before it may back a skill-revision or
/// new-skill proposal.
pub const MIN_DISTINCT_TASKS_FOR_PROPOSAL: usize = 2;
/// Withdrawn evidence ids a pattern lists; the rest is only counted.
pub(super) const MAX_WITHDRAWN_KEPT: usize = 64;
pub(super) const MAX_STATEMENT_BYTES: usize = 2000;
pub(super) const MAX_LIST_ITEMS: usize = 16;
pub(super) const MAX_ITEM_BYTES: usize = 500;
pub(super) const MAX_NAME_BYTES: usize = 128;
pub(super) const MAX_CAPABILITIES: usize = 32;

/// Why a task did not go as intended, per the failure table of spec section 10.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The guidance was wrong or missing: the only class that can lead to a
    /// skill revision proposal.
    MissingGuidance,
    /// The right skill was loaded but not followed: examine trigger, hydration
    /// and adherence; the content is preserved.
    GuidanceNotFollowed,
    /// A provider or tool was unavailable: an infrastructure blocker, no
    /// invented workflow cure.
    InfrastructureUnavailable,
    /// A permission was refused: the refusal is retained, no bypass is taught.
    PermissionRefusal,
    /// The runtime aborted or cleanup failed: an engineering defect, not a
    /// prompt problem.
    RuntimeDefect,
    /// The contract or oracle conflicts: independent contract review first.
    ConflictingContract,
    /// The wrong skill was retrieved: improve metadata or retrieval, evaluated
    /// separately.
    WrongSkillRetrieved,
    /// Repeated work that no managed skill covered: the zero-baseline path. Its
    /// evidence is completed root tasks (the runtime records a completed turn
    /// as a success and has no task-outcome check) that read no managed skill.
    /// Such a pattern is about no skill yet (`skill_id` is `None`), and the only
    /// thing it may lead to is a proposal for a NEW instruction skill.
    MissingSkill,
}

/// What a pattern of a class may be used for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    /// Procedural guidance; may back a skill revision or, when no skill covers
    /// the work, a new instruction skill.
    Guidance,
    /// An adherence lapse of correct content.
    Adherence,
    /// A retrieval or metadata problem.
    Retrieval,
    /// A blocker, refusal, defect or contract conflict: recorded, never turned
    /// into a workflow rule.
    Blocker,
}

impl FailureClass {
    #[must_use]
    pub const fn kind(self) -> PatternKind {
        match self {
            Self::MissingGuidance | Self::MissingSkill => PatternKind::Guidance,
            Self::GuidanceNotFollowed => PatternKind::Adherence,
            Self::WrongSkillRetrieved => PatternKind::Retrieval,
            Self::InfrastructureUnavailable
            | Self::PermissionRefusal
            | Self::RuntimeDefect
            | Self::ConflictingContract => PatternKind::Blocker,
        }
    }

    /// Only wrong or missing guidance may lead to a skill revision.
    #[must_use]
    pub const fn allows_skill_revision(self) -> bool {
        matches!(self, Self::MissingGuidance)
    }

    /// Only work that no managed skill covered may lead to a new skill.
    #[must_use]
    pub const fn allows_new_skill(self) -> bool {
        matches!(self, Self::MissingSkill)
    }
}

/// Facts a caller supplies about one failed or incomplete task. The runtime
/// source of these facts (terminal and outcome classification) is a later
/// hook; the classifier itself only decides from what it is given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FailureFacts {
    pub contract_or_oracle_conflict: bool,
    pub runtime_abort_or_cleanup_failure: bool,
    pub provider_or_tool_unavailable: bool,
    pub permission_refused: bool,
    /// The skill that was needed was not the one selected.
    pub wrong_skill_selected: bool,
    /// The needed skill was loaded and its guidance was applicable and correct,
    /// but the task did not follow it.
    pub correct_guidance_not_followed: bool,
    /// The needed guidance was absent from, or wrong in, the loaded skill.
    pub guidance_missing_or_wrong: bool,
    /// The work recurs and no managed skill covered it at all.
    pub no_skill_covers_the_work: bool,
}

/// Classifies a failure. The order puts causes that are least attributable to
/// skill content first, so an infrastructure, permission or contract problem is
/// never read as a skill defect even when guidance also looks incomplete.
/// `None` means the facts do not support any class: nothing is learned.
#[must_use]
pub const fn classify_failure(facts: &FailureFacts) -> Option<FailureClass> {
    if facts.contract_or_oracle_conflict {
        Some(FailureClass::ConflictingContract)
    } else if facts.runtime_abort_or_cleanup_failure {
        Some(FailureClass::RuntimeDefect)
    } else if facts.provider_or_tool_unavailable {
        Some(FailureClass::InfrastructureUnavailable)
    } else if facts.permission_refused {
        Some(FailureClass::PermissionRefusal)
    } else if facts.wrong_skill_selected {
        Some(FailureClass::WrongSkillRetrieved)
    } else if facts.correct_guidance_not_followed {
        Some(FailureClass::GuidanceNotFollowed)
    } else if facts.guidance_missing_or_wrong {
        Some(FailureClass::MissingGuidance)
    } else if facts.no_skill_covers_the_work {
        Some(FailureClass::MissingSkill)
    } else {
        None
    }
}

/// Identity of a pattern: `pat-` followed by 32 lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PatternId(String);

impl PatternId {
    pub fn parse(value: &str) -> Result<Self, LifecycleError> {
        let valid = value.strip_prefix("pat-").is_some_and(|digest| {
            digest.len() == 32 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        if valid {
            Ok(Self(value.to_string()))
        } else {
            Err(LifecycleError::InvalidId {
                kind: "pattern id",
                reason: "must be pat- followed by 32 lowercase hex digits".to_string(),
            })
        }
    }

    pub(super) fn from_digest(digest_hex: &str) -> Self {
        Self(format!("pat-{}", digest_hex.get(..32).unwrap_or(digest_hex)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PatternId {
    type Error = LifecycleError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<PatternId> for String {
    fn from(value: PatternId) -> Self {
        value.0
    }
}

impl fmt::Display for PatternId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Environment and capability scope a pattern is valid in. Patterns merge only
/// inside an identical scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternScope {
    pub environment: String,
    /// Sorted and de-duplicated.
    pub capabilities: Vec<String>,
}

/// How well supported a pattern is; derived from the evidence, recorded for
/// audit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidenceBasis {
    pub evidence_count: usize,
    pub counterevidence_count: usize,
    pub distinct_tasks: usize,
    pub counter_distinct_tasks: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgePattern {
    pub schema_version: u32,
    /// Stable record id; equals the pattern id.
    pub record_id: String,
    pub owner_scope: OwnerScope,
    pub pattern_id: PatternId,
    pub task_family: String,
    pub failure_class: FailureClass,
    pub kind: PatternKind,
    /// The skill the pattern is about, when it is about one.
    pub skill_id: Option<SkillId>,
    pub scope: PatternScope,
    pub statement: String,
    pub applicability: Vec<String>,
    pub exceptions: Vec<String>,
    /// Experience ids that support the statement.
    pub evidence_ids: Vec<String>,
    /// Experience ids that contradict it. Only removed by consolidation when
    /// the experience stopped being eligible; then it moves to
    /// `withdrawn_evidence`.
    pub counterevidence_ids: Vec<String>,
    /// Evidence (either side) carried over from an earlier pattern that later
    /// became ineligible (dropped, deleted by retention). Kept for audit,
    /// cumulative, sorted and de-duplicated; it never counts as support.
    pub withdrawn_evidence: Vec<String>,
    /// How many experiences were ever withdrawn from this pattern's lineage.
    /// `withdrawn_evidence` keeps at most [`MAX_WITHDRAWN_KEPT`] of them; the
    /// count stays exact.
    pub withdrawn_count: usize,
    /// Patterns this one replaces.
    pub supersedes: Vec<PatternId>,
    /// Position in the lineage: 1 for a first pattern, predecessor + 1 for a
    /// successor. The higher generation is the active one of a fork.
    pub generation: u64,
    pub confidence_basis: ConfidenceBasis,
    pub created_at: DateTime<Utc>,
}

impl KnowledgePattern {
    /// Digest of the pattern's identity: its payload, its lineage position and
    /// the pattern it supersedes. The pattern id is derived from it and
    /// re-derived on every read.
    #[must_use]
    pub fn content_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"prx-knowledge-pattern-v3\0");
        let payload = self.payload_digest();
        let generation = self.generation.to_string();
        for part in [payload.as_str(), generation.as_str()]
            .into_iter()
            .chain(self.supersedes.iter().map(PatternId::as_str))
        {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        hex::encode(hasher.finalize())
    }

    /// Digest of what the pattern says (statement, scope, evidence, ...),
    /// without lineage. Equal payloads mean a request changed nothing.
    #[must_use]
    pub fn payload_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"prx-knowledge-payload\0");
        let class = format!("{:?}", self.failure_class);
        let basis = [
            self.confidence_basis.evidence_count,
            self.confidence_basis.counterevidence_count,
            self.confidence_basis.distinct_tasks,
            self.confidence_basis.counter_distinct_tasks,
        ]
        .map(|n| n.to_string());
        let withdrawn_count = [self.withdrawn_count.to_string()];
        let lists: [(&str, &[String]); 8] = [
            ("capabilities", &self.scope.capabilities),
            ("applicability", &self.applicability),
            ("exceptions", &self.exceptions),
            ("evidence", &self.evidence_ids),
            ("counter", &self.counterevidence_ids),
            ("withdrawn", &self.withdrawn_evidence),
            ("basis", &basis),
            ("withdrawn-count", &withdrawn_count),
        ];
        let skill = self.skill_id.as_ref().map(SkillId::as_str).unwrap_or_default();
        let mut feed = |part: &str| {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        };
        for part in [
            self.owner_scope.as_str(),
            &self.task_family,
            &class,
            skill,
            &self.scope.environment,
            &self.statement,
        ] {
            feed(part);
        }
        for (name, items) in lists {
            feed(name);
            feed(&items.len().to_string());
            for item in items {
                feed(item);
            }
        }
        hex::encode(hasher.finalize())
    }

    /// Whether the pattern may back a proposal for a NEW skill: it records
    /// repeated work no skill covered (it is about no skill), supported by
    /// enough distinct root tasks, and the contradicting evidence does not
    /// reach the supporting evidence.
    #[must_use]
    pub const fn usable_for_new_skill(&self) -> bool {
        self.failure_class.allows_new_skill()
            && self.skill_id.is_none()
            && self.confidence_basis.distinct_tasks >= MIN_DISTINCT_TASKS_FOR_PROPOSAL
            && self.confidence_basis.counter_distinct_tasks < self.confidence_basis.distinct_tasks
    }

    /// Whether the pattern may back a skill revision proposal: it is guidance
    /// about a skill, supported by enough distinct tasks, and the contradicting
    /// evidence does not reach the supporting evidence. A one-off incident, a
    /// blocker, an adherence lapse or a retrieval problem never qualifies.
    #[must_use]
    pub const fn usable_for_proposal(&self) -> bool {
        self.failure_class.allows_skill_revision()
            && self.skill_id.is_some()
            && self.confidence_basis.distinct_tasks >= MIN_DISTINCT_TASKS_FOR_PROPOSAL
            && self.confidence_basis.counter_distinct_tasks < self.confidence_basis.distinct_tasks
    }
}

/// One line of `patterns/index.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternIndexEntry {
    pub pattern_id: PatternId,
    pub task_family: String,
    pub failure_class: FailureClass,
    pub skill_id: Option<SkillId>,
    pub scope: PatternScope,
    /// Digest of the normalised statement: two patterns of one group are
    /// revisions of the same lesson; different lessons are different groups.
    pub statement_key: String,
}

impl PatternIndexEntry {
    pub(super) fn of(pattern: &KnowledgePattern) -> Self {
        Self {
            pattern_id: pattern.pattern_id.clone(),
            task_family: pattern.task_family.clone(),
            failure_class: pattern.failure_class,
            skill_id: pattern.skill_id.clone(),
            scope: pattern.scope.clone(),
            statement_key: statement_key(&pattern.statement),
        }
    }

    /// Whether both describe the same lesson in the same place.
    pub(super) fn same_group(&self, other: &Self) -> bool {
        self.task_family == other.task_family
            && self.failure_class == other.failure_class
            && self.skill_id == other.skill_id
            && self.scope == other.scope
            && self.statement_key == other.statement_key
    }
}

/// Group identity of a statement: NFC, lowercase, whitespace collapsed.
/// Punctuation and wording stay, so a different lesson is a different group.
#[must_use]
pub(super) fn statement_key(statement: &str) -> String {
    let normalised: String = statement
        .nfc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let digest = hex::encode(Sha256::digest(normalised.as_bytes()));
    digest.get(..32).unwrap_or(&digest).to_string()
}

/// Derived listing of the active patterns (one per group); rebuildable from
/// the pattern files. History is not listed: it is reached through `supersedes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternIndex {
    pub schema_version: u32,
    pub owner_scope: OwnerScope,
    pub entries: Vec<PatternIndexEntry>,
    pub updated_at: DateTime<Utc>,
}

impl PatternIndex {
    pub(super) fn empty(owner: &OwnerScope) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            owner_scope: owner.clone(),
            entries: Vec::new(),
            updated_at: Utc::now(),
        }
    }
}
