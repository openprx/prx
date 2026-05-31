//! Typed `EvolutionProposalDraft` model and supporting enums.
//!
//! FIX-P0-03 (#9): Before this module the self-evolution subsystem wrote
//! evolution proposal rows with hand-rolled SQL `params!` (see
//! `xin/evolution.rs`), with no type safety and no shared CRUD contract across
//! memory backends. This module introduces a single typed representation that
//! both the deterministic `DraftEvolutionScheduler` and the L1 memory evolution
//! engine (FIX-P1-11) use, persisted through the `Memory` trait CRUD methods.
//!
//! Field layout follows design doc
//! `task/prx/design/d01-evolution-proposal-draft-schema-2026-05-28.md` §2.1.

// Several types here carry `f32`/`serde_json::Value` (e.g. JudgeVerdict.confidence,
// ProposedChange.new_value), so `Eq` cannot be derived; `PartialEq` is intentional
// for test assertions and equality checks.
#![allow(clippy::derive_partial_eq_without_eq)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::self_system::evolution::config::EvolutionMode;

/// A self-evolution change proposal awaiting judge/apply/rollback.
///
/// Semantically distinct from `MemoryDraft` (a user memory write proposal):
/// an `EvolutionProposalDraft` proposes a change to memory, prompt, strategy,
/// or config resources produced by the self-evolution loops.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvolutionProposalDraft {
    /// Immutable identifier (ULID/UUIDv7 with an `evo-` prefix).
    pub draft_id: String,
    /// Owner principal id; derived from runtime envelope, never LLM-forgeable.
    pub owner_id: String,
    /// Dispatching principal, e.g. `"self_system"` or `"xin:scheduler"`.
    pub principal_id: String,
    pub workspace_id: String,
    pub topic_id: Option<String>,
    /// Bound `task_runs.run_id` for traceability.
    pub task_id: Option<String>,

    pub source_message_event_ids: Vec<i64>,
    pub source_memory_event_ids: Vec<i64>,
    /// `sha256(canonical_content)` per source, used as the dedup/anti-ABA anchor.
    pub evidence_hashes: Vec<String>,

    pub target_resource: EvolutionTargetResource,
    pub proposed_change: ProposedChange,
    pub risk_level: RiskLevel,
    pub mode: EvolutionMode,

    pub created_at: DateTime<Utc>,
    /// `"xin:scheduler"` | `"self_system:l1|l2|l3"`.
    pub created_by_runtime: String,

    pub judge_verdict: Option<JudgeVerdict>,
    pub applied_at: Option<DateTime<Utc>>,
    pub applied_by: Option<String>,
    pub rollback_anchor: Option<RollbackAnchor>,
}

impl EvolutionProposalDraft {
    /// True once a terminal apply has happened (used for rollback eligibility).
    #[must_use]
    pub const fn is_applied(&self) -> bool {
        self.applied_at.is_some()
    }

    /// True when a judge verdict is already recorded (重判 guard).
    #[must_use]
    pub const fn is_judged(&self) -> bool {
        self.judge_verdict.is_some()
    }
}

/// The resource a proposal intends to mutate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvolutionTargetResource {
    /// Semantic memory entry; `scope` reuses the `MemoryVisibility` string form.
    SemanticMemory {
        memory_id: String,
        scope: String,
    },
    /// Prompt file under the workspace allowlist subtree.
    PromptFile {
        rel_path: String,
    },
    StrategyFile {
        rel_path: String,
    },
    /// Config fragment such as `"evolution.runtime"`.
    ConfigFragment {
        section: String,
    },
}

/// The concrete change a proposal would apply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProposedChange {
    /// Forget a semantic memory key (L1 redundancy pruning, FIX-P1-11).
    MemoryForget {
        reason: String,
    },
    /// Replace a memory value; `diff_hash` = `sha256(canonical_json)`.
    MemoryUpdate {
        new_value: serde_json::Value,
        diff_hash: String,
    },
    /// Replace prompt content; apply verifies `sha256(current) == old_hash` (ABA guard).
    PromptReplace {
        old_hash: String,
        new_content: String,
    },
    StrategyReplace {
        old_hash: String,
        new_content: String,
    },
    /// RFC 6902 JSON patch over a config fragment.
    ConfigPatch {
        json_patch: serde_json::Value,
    },
}

/// Coarse blast-radius classification used by the judge matrix.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    /// Canonical lowercase form matching the SQL `CHECK` constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// Parse the canonical lowercase form; unknown values fail explicitly.
    pub fn try_from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "critical" => Ok(Self::Critical),
            other => anyhow::bail!("unknown evolution proposal risk_level '{other}'"),
        }
    }
}

/// Verdict produced by an `EvolutionJudge`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum JudgeVerdict {
    Approved {
        judge_id: String,
        confidence: f32,
        reasoning: String,
    },
    Rejected {
        judge_id: String,
        reasoning: String,
    },
    NeedsHumanReview {
        reason: String,
    },
}

/// Restoration anchor written when a proposal is applied in `Auto` mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RollbackAnchor {
    /// Soft-deleted memory snapshot id (L1 trash, FIX-P1-11).
    MemorySnapshot {
        snapshot_id: String,
    },
    FileBackup {
        backup_path: PathBuf,
        sha256: String,
    },
    ConfigBackup {
        snapshot_id: String,
    },
}

/// Status transition target for `update_evolution_proposal_status`.
///
/// Each variant maps to one canonical `evolution_proposal_events.event_type`
/// and the column(s) it mutates on `evolution_proposals`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProposalStatusUpdate {
    /// Record a judge verdict (`proposal.judged`). Rejects re-judging.
    Judged { verdict: JudgeVerdict },
    /// Mark applied with a rollback anchor (`proposal.applied`).
    Applied {
        applied_by: String,
        rollback_anchor: RollbackAnchor,
    },
    /// Mark rolled back, clearing applied state (`proposal.rollback`).
    RolledBack,
}

/// Filter for `list_evolution_proposals`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProposalFilter {
    pub workspace_id: Option<String>,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub mode: Option<EvolutionMode>,
    pub judged: Option<bool>,
    pub applied: Option<bool>,
    pub since: Option<DateTime<Utc>>,
    /// `0` = no limit; callers must sanity-check before calling.
    pub limit: usize,
}

/// Owned, JSON-encoded column values for one `evolution_proposals` row.
///
/// This is the backend-neutral wire form: both SQLite (`params!`) and Postgres
/// (`&[&dyn ToSql]`) bind these exact strings/options, so the two backends stay
/// byte-equal for the JSON columns. Construct with [`ProposalRowValues::encode`]
/// and rebuild a typed draft with [`ProposalRowValues::decode`].
pub struct ProposalRowValues {
    pub draft_id: String,
    pub owner_id: String,
    pub principal_id: String,
    pub workspace_id: String,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub source_message_event_ids_json: String,
    pub source_memory_event_ids_json: String,
    pub evidence_hashes_json: String,
    pub target_resource_json: String,
    pub proposed_change_json: String,
    pub risk_level: String,
    pub mode: String,
    pub created_at: DateTime<Utc>,
    pub created_by_runtime: String,
    pub judge_verdict_json: Option<String>,
    pub applied_at: Option<DateTime<Utc>>,
    pub applied_by: Option<String>,
    pub rollback_anchor_json: Option<String>,
}

impl ProposalRowValues {
    /// Encode a typed draft into JSON column values, failing on serialization error.
    pub fn encode(draft: &EvolutionProposalDraft) -> anyhow::Result<Self> {
        let judge_verdict_json = match &draft.judge_verdict {
            Some(verdict) => Some(serde_json::to_string(verdict)?),
            None => None,
        };
        let rollback_anchor_json = match &draft.rollback_anchor {
            Some(anchor) => Some(serde_json::to_string(anchor)?),
            None => None,
        };
        Ok(Self {
            draft_id: draft.draft_id.clone(),
            owner_id: draft.owner_id.clone(),
            principal_id: draft.principal_id.clone(),
            workspace_id: draft.workspace_id.clone(),
            topic_id: draft.topic_id.clone(),
            task_id: draft.task_id.clone(),
            source_message_event_ids_json: serde_json::to_string(&draft.source_message_event_ids)?,
            source_memory_event_ids_json: serde_json::to_string(&draft.source_memory_event_ids)?,
            evidence_hashes_json: serde_json::to_string(&draft.evidence_hashes)?,
            target_resource_json: serde_json::to_string(&draft.target_resource)?,
            proposed_change_json: serde_json::to_string(&draft.proposed_change)?,
            risk_level: draft.risk_level.as_str().to_string(),
            mode: mode_to_db(&draft.mode).to_string(),
            created_at: draft.created_at,
            created_by_runtime: draft.created_by_runtime.clone(),
            judge_verdict_json,
            applied_at: draft.applied_at,
            applied_by: draft.applied_by.clone(),
            rollback_anchor_json,
        })
    }

    /// Decode a typed draft from JSON column values, failing on parse error.
    #[allow(clippy::too_many_arguments)]
    pub fn decode(
        draft_id: String,
        owner_id: String,
        principal_id: String,
        workspace_id: String,
        topic_id: Option<String>,
        task_id: Option<String>,
        source_message_event_ids_json: &str,
        source_memory_event_ids_json: &str,
        evidence_hashes_json: &str,
        target_resource_json: &str,
        proposed_change_json: &str,
        risk_level: &str,
        mode: &str,
        created_at: DateTime<Utc>,
        created_by_runtime: String,
        judge_verdict_json: Option<&str>,
        applied_at: Option<DateTime<Utc>>,
        applied_by: Option<String>,
        rollback_anchor_json: Option<&str>,
    ) -> anyhow::Result<EvolutionProposalDraft> {
        let judge_verdict = match judge_verdict_json {
            Some(raw) => Some(serde_json::from_str::<JudgeVerdict>(raw)?),
            None => None,
        };
        let rollback_anchor = match rollback_anchor_json {
            Some(raw) => Some(serde_json::from_str::<RollbackAnchor>(raw)?),
            None => None,
        };
        Ok(EvolutionProposalDraft {
            draft_id,
            owner_id,
            principal_id,
            workspace_id,
            topic_id,
            task_id,
            source_message_event_ids: serde_json::from_str(source_message_event_ids_json)?,
            source_memory_event_ids: serde_json::from_str(source_memory_event_ids_json)?,
            evidence_hashes: serde_json::from_str(evidence_hashes_json)?,
            target_resource: serde_json::from_str(target_resource_json)?,
            proposed_change: serde_json::from_str(proposed_change_json)?,
            risk_level: RiskLevel::try_from_db_str(risk_level)?,
            mode: mode_from_db(mode)?,
            created_at,
            created_by_runtime,
            judge_verdict,
            applied_at,
            applied_by,
            rollback_anchor,
        })
    }
}

/// Canonical lowercase form of an `EvolutionMode` for the SQL `CHECK` constraint.
#[must_use]
pub const fn mode_to_db(mode: &EvolutionMode) -> &'static str {
    match mode {
        EvolutionMode::DraftOnly => "draft_only",
        EvolutionMode::Shadow => "shadow",
        EvolutionMode::Auto => "auto",
    }
}

/// Parse the canonical lowercase mode form; unknown values fail explicitly.
pub fn mode_from_db(value: &str) -> anyhow::Result<EvolutionMode> {
    match value {
        "draft_only" => Ok(EvolutionMode::DraftOnly),
        "shadow" => Ok(EvolutionMode::Shadow),
        "auto" => Ok(EvolutionMode::Auto),
        other => anyhow::bail!("unknown evolution proposal mode '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_draft() -> EvolutionProposalDraft {
        EvolutionProposalDraft {
            draft_id: "evo-test-1".to_string(),
            owner_id: "self_system".to_string(),
            principal_id: "xin:scheduler".to_string(),
            workspace_id: "ws-1".to_string(),
            topic_id: Some("topic-1".to_string()),
            task_id: Some("run-7".to_string()),
            source_message_event_ids: vec![1, 2, 3],
            source_memory_event_ids: vec![4, 5],
            evidence_hashes: vec!["hash-a".to_string(), "hash-b".to_string()],
            target_resource: EvolutionTargetResource::SemanticMemory {
                memory_id: "conversation:abc".to_string(),
                scope: "workspace".to_string(),
            },
            proposed_change: ProposedChange::MemoryForget {
                reason: "redundant conversation memory".to_string(),
            },
            risk_level: RiskLevel::Low,
            mode: EvolutionMode::Auto,
            created_at: DateTime::parse_from_rfc3339("2026-05-28T00:00:00Z")
                .expect("test: valid rfc3339")
                .with_timezone(&Utc),
            created_by_runtime: "self_system:l1".to_string(),
            judge_verdict: Some(JudgeVerdict::Approved {
                judge_id: "mock".to_string(),
                confidence: 0.7,
                reasoning: "2 evidence chunks".to_string(),
            }),
            applied_at: None,
            applied_by: None,
            rollback_anchor: None,
        }
    }

    #[test]
    fn proposal_draft_roundtrips_through_json() {
        let draft = sample_draft();
        let json = serde_json::to_string(&draft).expect("test: serialize");
        let parsed: EvolutionProposalDraft = serde_json::from_str(&json).expect("test: deserialize");
        assert_eq!(draft, parsed);
    }

    #[test]
    fn target_resource_tagged_enum_serializes_with_kind() {
        let value = serde_json::to_value(EvolutionTargetResource::PromptFile {
            rel_path: "prompts/system.md".to_string(),
        })
        .expect("test: serialize");
        assert_eq!(
            value.get("kind").and_then(serde_json::Value::as_str),
            Some("prompt_file")
        );
        assert_eq!(
            value.get("rel_path").and_then(serde_json::Value::as_str),
            Some("prompts/system.md")
        );
    }

    #[test]
    fn risk_level_db_str_roundtrips() {
        for level in [RiskLevel::Low, RiskLevel::Medium, RiskLevel::High, RiskLevel::Critical] {
            let parsed = RiskLevel::try_from_db_str(level.as_str()).expect("test: parse risk");
            assert_eq!(level, parsed);
        }
        assert!(RiskLevel::try_from_db_str("bogus").is_err());
    }

    #[test]
    fn mode_db_str_roundtrips() {
        for mode in [EvolutionMode::DraftOnly, EvolutionMode::Shadow, EvolutionMode::Auto] {
            let parsed = mode_from_db(mode_to_db(&mode)).expect("test: parse mode");
            assert_eq!(mode, parsed);
        }
        assert!(mode_from_db("bogus").is_err());
    }

    #[test]
    fn applied_and_judged_flags_reflect_state() {
        let mut draft = sample_draft();
        assert!(draft.is_judged());
        assert!(!draft.is_applied());
        draft.applied_at = Some(Utc::now());
        assert!(draft.is_applied());
    }
}
