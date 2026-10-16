//! Knowledge patterns (SL04): evidence-linked, revisable statements derived
//! from task experiences.
//!
//! Experiences stay append-only and are never modified by consolidation; a
//! pattern only references them by id. A pattern is immutable once written; a
//! revision is a new pattern that `supersedes` the old one. Contradicting
//! evidence is kept in `counterevidence_ids`, never dropped.
//!
//! Layout inside the owner store: `patterns/<pattern-id>.json` and the derived
//! `patterns/index.json` (rebuildable from the pattern files).

use super::types::{LifecycleError, OwnerScope, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Evidence references kept per pattern (evidence and counterevidence each).
pub const MAX_PATTERN_EVIDENCE: usize = 256;
/// Distinct tasks a pattern needs before it may back a skill-revision proposal.
pub const MIN_DISTINCT_TASKS_FOR_PROPOSAL: usize = 2;
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
}

/// What a pattern of a class may be used for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    /// Procedural guidance; may back a skill revision.
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
            Self::MissingGuidance => PatternKind::Guidance,
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
    /// Experience ids that contradict it. Never removed by consolidation.
    pub counterevidence_ids: Vec<String>,
    /// Patterns this one replaces.
    pub supersedes: Vec<PatternId>,
    pub confidence_basis: ConfidenceBasis,
    pub created_at: DateTime<Utc>,
}

impl KnowledgePattern {
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
    /// Set when a later pattern superseded this one.
    pub superseded_by: Option<PatternId>,
}

/// Derived listing of patterns; rebuildable from the pattern files.
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
