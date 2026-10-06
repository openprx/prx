use crate::memory::{Memory, MemoryCategory, MemoryEntry};
use crate::self_system::evolution::analyzer::{CandidatePriority, EvolutionCandidate};
use crate::self_system::evolution::config::{EvolutionConfig, EvolutionMode, SharedEvolutionConfig};
use crate::self_system::evolution::engine::{
    CycleResult, EngineCycleInput, EvolutionEngine, FileChange, PendingChange,
};
use crate::self_system::evolution::gate::{EvolutionGate, GateMetrics, GateResult};
use crate::self_system::evolution::judge::{JudgeConfig, JudgeEngine, MockJudgeModel};
use crate::self_system::evolution::memory_compressor::{EmbeddingSimilarityDetector, MemoryCompressor};
use crate::self_system::evolution::record::{ChangeType, DataBasis, EvolutionLayer, EvolutionLog, EvolutionResult};
use crate::self_system::evolution::rollback::RollbackManager;
use crate::self_system::evolution::{
    ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals,
    EvolutionValidation, RiskLevel, ValidationStatus,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

const MUTATION_RATIO: f64 = 0.10;

/// Grace window (days) for L1 soft-deleted conversation memories before they may
/// be physically reclaimed (FIX-P1-11).
const PRUNE_GRACE_DAYS: u32 = 14;

#[derive(Debug, Clone)]
struct MutationPlan {
    key: String,
    before: f64,
    after: f64,
    token_degradation: f64,
}

/// L1 memory evolution loop executor.
pub struct MemoryEvolutionEngine {
    shared_config: SharedEvolutionConfig,
    workspace_root: PathBuf,
    config_path: PathBuf,
    judge: JudgeEngine,
    rollback: RollbackManager,
    memory: Option<Arc<dyn Memory>>,
}

impl MemoryEvolutionEngine {
    pub fn new(shared_config: SharedEvolutionConfig, config_path: impl AsRef<Path>) -> Result<Self> {
        let config_path = config_path.as_ref().to_path_buf();
        let workspace_root = config_path.parent().unwrap_or_else(|| Path::new("."));
        let rollback_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".evolution/rollback/memory");
        let max_versions = shared_config.load_full().rollback.max_versions;

        Ok(Self {
            shared_config,
            workspace_root: workspace_root.to_path_buf(),
            config_path: config_path.clone(),
            judge: JudgeEngine::new(JudgeConfig::default(), std::sync::Arc::new(MockJudgeModel)),
            rollback: RollbackManager::new(workspace_root, &config_path, rollback_dir, max_versions)?,
            memory: None,
        })
    }

    /// Inject the configured application memory backend. Production callers
    /// must use this instead of letting the evolution layer own a local backend.
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    fn select_candidate(&self, candidates: &[EvolutionCandidate]) -> Option<EvolutionCandidate> {
        let mut sorted = candidates.to_vec();
        sorted.sort_by_key(|item| match item.priority {
            CandidatePriority::High => 0,
            CandidatePriority::Medium => 1,
            CandidatePriority::Low => 2,
        });
        sorted.into_iter().next()
    }

    fn build_mutation_plan(&self, cfg: &EvolutionConfig, candidate: &EvolutionCandidate) -> MutationPlan {
        if candidate.target.contains_key("task_type") {
            let choices = [
                ("retrieval.score_weights.recency", cfg.retrieval.score_weights.recency),
                (
                    "retrieval.score_weights.access_freq",
                    cfg.retrieval.score_weights.access_freq,
                ),
                (
                    "retrieval.score_weights.category_weight",
                    cfg.retrieval.score_weights.category_weight,
                ),
                (
                    "retrieval.score_weights.useful_ratio",
                    cfg.retrieval.score_weights.useful_ratio,
                ),
                (
                    "retrieval.score_weights.source_confidence",
                    cfg.retrieval.score_weights.source_confidence,
                ),
            ];
            let index = candidate.evidence_ids.len() % choices.len();
            // SAFETY: index = N % choices.len() is always < choices.len()
            #[allow(clippy::indexing_slicing)]
            let (key, value) = choices[index];
            let after = mutate_numeric(value, candidate, key);
            return MutationPlan {
                key: key.to_string(),
                before: value,
                after,
                token_degradation: 0.0,
            };
        }

        if candidate.target.contains_key("memory_id") {
            let before = cfg.memory.max_tokens as f64;
            let after = mutate_numeric(before, candidate, "memory.max_tokens").round().max(64.0);
            let degradation = ((after - before) / before.max(1.0)).max(0.0);
            return MutationPlan {
                key: "memory.max_tokens".to_string(),
                before,
                after,
                token_degradation: degradation,
            };
        }

        let fusion_choices = [
            ("memory.retrieval_fusion.bm25", cfg.memory.retrieval_fusion.bm25),
            ("memory.retrieval_fusion.vector", cfg.memory.retrieval_fusion.vector),
            ("memory.retrieval_fusion.metadata", cfg.memory.retrieval_fusion.metadata),
        ];
        let index = candidate.evidence_ids.len() % fusion_choices.len();
        // SAFETY: index = N % fusion_choices.len() is always < fusion_choices.len()
        #[allow(clippy::indexing_slicing)]
        let (key, value) = fusion_choices[index];
        MutationPlan {
            key: key.to_string(),
            before: value,
            after: mutate_numeric(value, candidate, key),
            token_degradation: 0.0,
        }
    }

    fn apply_plan_to_config(cfg: &mut EvolutionConfig, plan: &MutationPlan) {
        match plan.key.as_str() {
            "retrieval.score_weights.recency" => cfg.retrieval.score_weights.recency = plan.after,
            "retrieval.score_weights.access_freq" => cfg.retrieval.score_weights.access_freq = plan.after,
            "retrieval.score_weights.category_weight" => cfg.retrieval.score_weights.category_weight = plan.after,
            "retrieval.score_weights.useful_ratio" => cfg.retrieval.score_weights.useful_ratio = plan.after,
            "retrieval.score_weights.source_confidence" => cfg.retrieval.score_weights.source_confidence = plan.after,
            "memory.max_tokens" => cfg.memory.max_tokens = plan.after as usize,
            "memory.retrieval_fusion.bm25" => cfg.memory.retrieval_fusion.bm25 = plan.after,
            "memory.retrieval_fusion.vector" => cfg.memory.retrieval_fusion.vector = plan.after,
            "memory.retrieval_fusion.metadata" => cfg.memory.retrieval_fusion.metadata = plan.after,
            _ => {}
        }
    }
}

#[async_trait]
impl EvolutionEngine for MemoryEvolutionEngine {
    fn name(&self) -> &'static str {
        "memory_evolution_engine"
    }

    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Memory
    }

    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
        let started_at = Utc::now().to_rfc3339();
        let fitness_trend = input.fitness_trend.clone().unwrap_or_default();
        let cycle_id = if input.cycle_id.is_empty() {
            Uuid::now_v7().to_string()
        } else {
            input.cycle_id
        };

        let current = self.shared_config.load_full();
        let mode = current.runtime.mode.clone();
        let candidate = self
            .select_candidate(&input.analyzer_candidates)
            .unwrap_or_else(default_candidate);
        let mut next_cfg = current.as_ref().clone();
        let plan = self.build_mutation_plan(&next_cfg, &candidate);
        Self::apply_plan_to_config(&mut next_cfg, &plan);
        let serialized = toml::to_string_pretty(&next_cfg)?;

        let proposal = EvolutionProposal {
            id: Uuid::now_v7().to_string(),
            summary: format!("Mutate {} by ±10%", plan.key),
            rationale: format!(
                "selected by analyzer priority {:?} with single-parameter mutation",
                candidate.priority
            ),
            risk_level: RiskLevel::Low,
            target: ChangeTarget::ConfigFile {
                path: self.config_path.to_string_lossy().to_string(),
            },
            operation: ChangeOperation::Write {
                content: serialized.clone(),
            },
        };

        let judge_result = self
            .judge
            .judge_task(
                &proposal.id,
                &cycle_id,
                &proposal.summary,
                "memory mutation evaluation success",
            )
            .await?;

        let gate_metrics = GateMetrics {
            average_improvement: judge_result.scores.overall() * 0.1,
            holdout_regression: -((1.0 - judge_result.scores.safety_compliance) * 0.04),
            token_degradation: plan.token_degradation,
        };
        let gate = EvolutionGate::from_evolution_config(current.as_ref());
        let gate_result = gate.evaluate(&candidate, &gate_metrics);

        let mut pending: Option<Arc<dyn PendingChange>> = None;
        let mut outcome = CycleOutcome::NoAction;
        let mut notes = "shadow mode: recommendation only".to_string();

        match mode {
            EvolutionMode::DraftOnly | EvolutionMode::Shadow => {}
            // A refused gate is a refusal whatever judged it.
            EvolutionMode::Auto if !matches!(gate_result, GateResult::Passed) => {
                outcome = CycleOutcome::Failed;
                notes = "gate rejected mutation".to_string();
            }
            // D-035: the engine's verdict comes from a degraded (mock) judge,
            // which never applies a config write: the mutation is deferred.
            EvolutionMode::Auto if self.judge.is_degraded() => {
                notes = "judge degraded (mock scorer): mutation deferred, config not written".to_string();
            }
            EvolutionMode::Auto => {
                // D-046: proposed only; the pipeline prunes, writes and
                // installs it after an accepted verdict.
                pending = Some(Arc::new(MemoryConfigChange {
                    file: FileChange::new(
                        self.workspace_root.clone(),
                        self.config_path.clone(),
                        serialized.clone(),
                        self.rollback.clone(),
                    ),
                    memory: self.memory.clone(),
                    workspace_root: self.workspace_root.clone(),
                    shared_config: Arc::clone(&self.shared_config),
                    next: Arc::new(next_cfg),
                }));
                notes = "gate passed; config written only after an accepted verdict".to_string();
            }
        }
        let key_metrics = HashMap::from([
            ("average_improvement".to_string(), gate_metrics.average_improvement),
            ("token_degradation".to_string(), gate_metrics.token_degradation),
        ]);

        let evolution_log = EvolutionLog {
            experiment_id: proposal.id.clone(),
            timestamp: Utc::now().to_rfc3339(),
            layer: EvolutionLayer::Memory,
            change_type: ChangeType::Tune,
            before_value: format!("{}={:.6}", plan.key, plan.before),
            after_value: format!("{}={:.6}", plan.key, plan.after),
            trigger_reason: candidate.suggested_value.clone(),
            data_basis: DataBasis {
                sample_count: candidate.evidence_ids.len() as u32,
                time_range_days: candidate.backfill_after_days,
                key_metrics,
                patterns_found: vec![candidate.current_value.clone()],
            },
            result: if matches!(gate_result, GateResult::Passed) {
                Some(EvolutionResult::Neutral)
            } else {
                Some(EvolutionResult::Rejected)
            },
        };

        // The pipeline records the cycle, once, under its experiment id.

        let cycle = EvolutionCycle {
            id: cycle_id,
            started_at,
            finished_at: Utc::now().to_rfc3339(),
            signals: EvolutionSignals {
                memory_count: candidate.evidence_ids.len(),
                health_components: 1,
                health_error_components: 0,
                cron_runs: 0,
                cron_failure_ratio: 0.0,
            },
            trend: fitness_trend.clone(),
            proposal: Some(proposal.clone()),
            validation: EvolutionValidation {
                status: ValidationStatus::Skipped,
                before_score: fitness_trend.latest_score,
                after_score: None,
                delta: None,
                notes: format!("{notes}; fitness validation pending a later closed window"),
            },
            outcome,
            alert: match gate_result {
                GateResult::Passed => None,
                GateResult::Rejected(ref rejection) => {
                    Some(format!("gate_rejected:{}:{}", rejection.reason, rejection.details))
                }
            },
            errors: Vec::new(),
        };

        Ok(CycleResult {
            layer: EvolutionLayer::Memory,
            proposal: Some(proposal),
            cycle,
            evolution_log: Some(evolution_log),
            needs_human_approval: false,
            shadow_mode: mode.is_draft_like(),
            pending,
        })
    }
}

/// Build and persist an `EvolutionProposalDraft` for an L1 memory-forget
/// candidate (FIX-P1-11). Uses the typed CRUD path (FIX-P0-03) rather than
/// raw SQL so the proposal is type-checked and auditable.
async fn record_forget_proposal(
    memory: &dyn Memory,
    workspace_id: &str,
    memory_key: &str,
    memory_event_id: String,
) -> Result<()> {
    use crate::self_system::evolution::proposal::{
        EvolutionProposalDraft, EvolutionTargetResource, ProposedChange, RiskLevel as ProposalRiskLevel,
    };

    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::default();
    hasher.update(memory_key.as_bytes());
    let evidence_hash = format!("{:x}", hasher.finalize());

    let draft = EvolutionProposalDraft {
        draft_id: format!("evo-{}", Uuid::now_v7()),
        owner_id: "self_system".to_string(),
        principal_id: "self_system:l1".to_string(),
        workspace_id: workspace_id.to_string(),
        topic_id: None,
        task_id: None,
        source_message_event_ids: Vec::new(),
        source_memory_event_ids: Vec::new(),
        evidence_hashes: vec![evidence_hash],
        target_resource: EvolutionTargetResource::SemanticMemory {
            memory_id: memory_key.to_string(),
            scope: "workspace".to_string(),
        },
        proposed_change: ProposedChange::MemoryForget {
            reason: format!("redundant conversation memory (candidate id {memory_event_id})"),
        },
        risk_level: ProposalRiskLevel::Low,
        mode: EvolutionMode::Auto,
        created_at: Utc::now(),
        created_by_runtime: "self_system:l1".to_string(),
        judge_verdict: None,
        applied_at: None,
        applied_by: None,
        rollback_anchor: None,
    };
    memory.create_evolution_proposal(draft).await?;
    Ok(())
}

async fn prune_redundant_conversation_memories(memory: &dyn Memory, workspace_root: &Path) -> Result<u32> {
    let entries = memory.list(Some(&MemoryCategory::Conversation), None).await?;
    if entries.len() < 2 {
        return Ok(0);
    }

    // Near-duplicates are compared only within one owner: two callers who
    // wrote the same thing each keep their own row, and one owner's row is
    // never trashed as a copy of another's. A row whose owner the backend
    // cannot report — written after the owner lookup — waits for the next
    // cycle rather than join any owner's group.
    let owners = memory.list_owner_ids(&MemoryCategory::Conversation).await?;
    let key_by_id = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry.key.clone()))
        .collect::<HashMap<_, _>>();
    let mut groups: HashMap<Option<String>, Vec<MemoryEntry>> = HashMap::new();
    for entry in entries {
        let owner = match &owners {
            None => None,
            Some(owners) => match owners.get(&entry.id) {
                Some(owner) => owner.clone(),
                None => continue,
            },
        };
        groups.entry(owner).or_default().push(entry);
    }

    let compressor = MemoryCompressor::new(EmbeddingSimilarityDetector::default());
    let mut redundant_ids = Vec::new();
    for group in groups.values().filter(|group| group.len() >= 2) {
        redundant_ids.extend(compressor.detect_redundancy(group).await?);
    }
    if redundant_ids.is_empty() {
        return Ok(0);
    }

    let workspace_id = workspace_root.to_string_lossy().to_string();
    let mut removed = 0u32;
    for redundant_id in redundant_ids {
        let Some(key) = key_by_id.get(&redundant_id) else {
            continue;
        };
        // FIX-P1-11: Auto mode must NOT physically delete. Produce an
        // EvolutionProposalDraft recording the forget intent, then soft-delete
        // into trash with a 14-day grace window so the change is reversible
        // and auditable instead of an irreversible `forget`.
        if let Err(error) = record_forget_proposal(memory, &workspace_id, key, redundant_id).await {
            tracing::warn!(
                target: "self_system",
                key = %key,
                "failed to record forget proposal; skipping trash to stay reversible: {error}"
            );
            continue;
        }
        match memory
            .move_to_trash(key, "redundant conversation memory", PRUNE_GRACE_DAYS)
            .await
        {
            Ok(Some(_)) => removed = removed.saturating_add(1),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    target: "self_system",
                    key = %key,
                    "move_to_trash failed: {error}"
                );
            }
        }
    }

    Ok(removed)
}

/// The memory layer's pending change (D-046): write the tuned evolution
/// config, install it, then move redundant conversation memories to trash,
/// only after an accepted verdict.
struct MemoryConfigChange {
    file: FileChange,
    memory: Option<Arc<dyn Memory>>,
    workspace_root: PathBuf,
    shared_config: SharedEvolutionConfig,
    next: Arc<EvolutionConfig>,
}

impl std::fmt::Debug for MemoryConfigChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryConfigChange")
            .field("file", &self.file)
            .field("prunes_memory", &self.memory.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl PendingChange for MemoryConfigChange {
    async fn apply(&self) -> Result<()> {
        // R-PMMCj G-3: write and install the config first; memories are moved
        // to trash only once the change itself is in place. A failed prune
        // is a warning, never a failed change.
        self.file.apply().await?;
        self.shared_config.store(Arc::clone(&self.next));
        if let Some(memory) = self.memory.as_ref() {
            match prune_redundant_conversation_memories(memory.as_ref(), &self.workspace_root).await {
                Ok(count) if count > 0 => tracing::info!(
                    target: "self_system",
                    deduplicated_conversation_memories = count,
                    "memory evolution moved redundant conversation memories to trash"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(target: "self_system", "memory redundancy cleanup skipped: {error}"),
            }
        }
        Ok(())
    }
}

fn mutate_numeric(value: f64, candidate: &EvolutionCandidate, salt: &str) -> f64 {
    let seed = format!("{}:{}:{}", candidate.current_value, candidate.suggested_value, salt);
    let dir = if stable_hash(&seed) % 2 == 0 {
        1.0 + MUTATION_RATIO
    } else {
        1.0 - MUTATION_RATIO
    };
    (value * dir).max(0.0001)
}

fn stable_hash(value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn default_candidate() -> EvolutionCandidate {
    let mut target = std::collections::BTreeMap::new();
    target.insert("task_type".to_string(), "planning".to_string());
    EvolutionCandidate {
        target,
        current_value: "fallback".to_string(),
        suggested_value: "tune_retrieval_weights".to_string(),
        evidence_ids: vec!["fallback".to_string()],
        priority: CandidatePriority::Medium,
        backfill_after_days: 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::config::{EvolutionConfig, EvolutionMode, new_shared_evolution_config};
    use tempfile::tempdir;
    use tokio::fs;

    #[tokio::test]
    async fn memory_engine_shadow_mode_only_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evolution_config.toml");
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Shadow;
        fs::write(&path, toml::to_string_pretty(&cfg).unwrap()).await.unwrap();

        let shared = new_shared_evolution_config(cfg);
        let mut engine = MemoryEvolutionEngine::new(shared, &path).unwrap();
        let result = engine
            .run_cycle(EngineCycleInput {
                cycle_id: "c1".to_string(),
                analyzer_candidates: vec![default_candidate()],
                fitness_trend: None,
            })
            .await
            .unwrap();

        assert!(result.shadow_mode);
        assert!(!matches!(result.cycle.outcome, CycleOutcome::Applied));
    }

    /// D-035 / R-PMMCf §4: the engine's own judge is the mock, so a passing
    /// gate on its verdict must not write the config in Auto mode.
    #[tokio::test]
    async fn memory_engine_auto_defers_on_a_mock_verdict() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evolution_config.toml");
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Auto;
        let original = toml::to_string_pretty(&cfg).unwrap();
        fs::write(&path, &original).await.unwrap();

        let shared = new_shared_evolution_config(cfg);
        let mut engine = MemoryEvolutionEngine::new(shared.clone(), &path).unwrap();
        let result = engine
            .run_cycle(EngineCycleInput {
                cycle_id: "c2".to_string(),
                analyzer_candidates: vec![default_candidate()],
                fitness_trend: None,
            })
            .await
            .unwrap();

        assert!(
            !matches!(result.cycle.outcome, CycleOutcome::Applied),
            "the memory engine applied on its own mock verdict"
        );
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            original,
            "the config file changed"
        );
        assert!(
            result.cycle.validation.notes.contains("judge"),
            "{}",
            result.cycle.validation.notes
        );
    }

    #[test]
    fn production_memory_evolution_uses_injected_backend() {
        let engine_source = include_str!("memory_evolution.rs");
        let production = engine_source.split_once("#[cfg(test)]").unwrap().0;
        assert!(production.contains("memory: Option<Arc<dyn Memory>>"));
        assert!(production.contains("move_to_trash"));
        assert!(!production.contains("SqliteMemory"));
        assert!(!production.contains("brain.db"));

        let daemon_source = include_str!("../../daemon/mod.rs");
        let scheduler_builder = daemon_source
            .split_once("async fn build_evolution_scheduler")
            .unwrap()
            .1;
        assert!(scheduler_builder.contains("create_memory_with_storage_and_routes_with_acl"));
        assert!(scheduler_builder.contains("with_memory(evolution_memory)"));
    }

    /// R-PMMCj G-3: when the config write fails, no memory has been moved to
    /// trash: the change is either applied (write, then prune) or not at all.
    #[tokio::test]
    async fn a_failed_config_write_trashes_no_memory() {
        let dir = tempdir().unwrap();
        let memory = Arc::new(crate::memory::sqlite::SqliteMemory::new(dir.path()).unwrap());
        let text = "The deploy window for the payments service is Tuesday at 14:00 UTC.";
        for key in ["note-one", "note-two"] {
            let context = crate::memory::MemoryWriteContext {
                channel: Some("webhook".to_string()),
                sender_id: Some("alice".to_string()),
                ..crate::memory::MemoryWriteContext::default()
            };
            memory
                .store_with_context(key, text, MemoryCategory::Conversation, None, Some(&context))
                .await
                .unwrap();
        }
        // The config path is a directory, so the write fails.
        let config_path = dir.path().join("evolution_config.toml");
        fs::create_dir_all(&config_path).await.unwrap();
        let shared = new_shared_evolution_config(EvolutionConfig::default());
        let change = MemoryConfigChange {
            file: FileChange::new(
                dir.path().to_path_buf(),
                config_path.clone(),
                "tuned = true\n".to_string(),
                RollbackManager::new(
                    dir.path(),
                    &config_path,
                    dir.path().join(".evolution/rollback/memory"),
                    5,
                )
                .unwrap(),
            ),
            memory: Some(memory.clone() as Arc<dyn Memory>),
            workspace_root: dir.path().to_path_buf(),
            shared_config: Arc::clone(&shared),
            next: Arc::new(EvolutionConfig::default()),
        };
        assert!(change.apply().await.is_err(), "the write fails");
        // The prune records a forget proposal before each trash move.
        let system = crate::memory::MemoryPrincipal {
            require_owner_match: false,
            workspace_id: dir.path().to_string_lossy().to_string(),
            agent_id: Some("self_system".to_string()),
            persona_id: None,
            session_key: None,
            channel: None,
            sender: None,
            owner_id: Some("self_system".to_string()),
            legacy_session_key: None,
        };
        let forgets = memory
            .list_evolution_proposals(
                &system,
                crate::self_system::evolution::ProposalFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            forgets.is_empty(),
            "a memory was trashed although the change was not applied: {forgets:?}"
        );
        // Sanity: the same memories are pruned once the change can apply.
        assert_eq!(
            prune_redundant_conversation_memories(memory.as_ref(), dir.path())
                .await
                .unwrap(),
            1
        );
    }

    /// R-MCPBh §4 residual: a non-isolated network caller's `conversation`
    /// rows carry its owner, and deduplication compared them workspace-wide,
    /// so one owner's row was trashed as a near-copy of another's. Within one
    /// owner the near-copy is still pruned.
    #[tokio::test]
    async fn conversation_memories_are_deduplicated_only_within_one_owner() {
        let dir = tempdir().unwrap();
        let memory = Arc::new(crate::memory::sqlite::SqliteMemory::new(dir.path()).unwrap());
        let text = "The deploy window for the payments service is Tuesday at 14:00 UTC.";
        let store = |key: &'static str, owner: &'static str| {
            let memory = Arc::clone(&memory);
            async move {
                let context = crate::memory::MemoryWriteContext {
                    channel: Some("webhook".to_string()),
                    sender_id: Some(owner.to_string()),
                    ..crate::memory::MemoryWriteContext::default()
                };
                memory
                    .store_with_context(key, text, MemoryCategory::Conversation, None, Some(&context))
                    .await
                    .unwrap();
            }
        };
        store("alice-note", "alice").await;
        store("bob-note", "bob").await;

        assert_eq!(
            prune_redundant_conversation_memories(memory.as_ref(), dir.path())
                .await
                .unwrap(),
            0,
            "two owners' identical rows are not duplicates of each other"
        );

        store("bob-copy", "bob").await;
        assert_eq!(
            prune_redundant_conversation_memories(memory.as_ref(), dir.path())
                .await
                .unwrap(),
            1,
            "bob's own near-copy is still pruned"
        );
    }
}
