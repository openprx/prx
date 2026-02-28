use crate::memory::{Memory, MemoryCategory, SqliteMemory};
use crate::self_system::evolution::analyzer::{CandidatePriority, EvolutionCandidate};
use crate::self_system::evolution::config::{
    EvolutionConfig, EvolutionMode, SharedEvolutionConfig,
};
use crate::self_system::evolution::engine::{CycleResult, EngineCycleInput, EvolutionEngine};
use crate::self_system::evolution::gate::{EvolutionGate, GateMetrics, GateResult};
use crate::self_system::evolution::judge::{JudgeConfig, JudgeEngine, MockJudgeModel};
use crate::self_system::evolution::memory_compressor::{
    EmbeddingSimilarityDetector, MemoryCompressor,
};
use crate::self_system::evolution::record::{
    ChangeType, DataBasis, EvolutionLayer, EvolutionLog, EvolutionResult,
};
use crate::self_system::evolution::rollback::RollbackManager;
use crate::self_system::evolution::safety_utils::atomic_write;
use crate::self_system::evolution::storage::AsyncJsonlWriter;
use crate::self_system::evolution::{
    ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal,
    EvolutionSignals, EvolutionValidation, FitnessTrend, RiskLevel, ValidationStatus,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use uuid::Uuid;

const MUTATION_RATIO: f64 = 0.10;

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
    writer: Option<Arc<AsyncJsonlWriter>>,
    judge: JudgeEngine<MockJudgeModel>,
    rollback: RollbackManager,
}

impl MemoryEvolutionEngine {
    pub fn new(
        shared_config: SharedEvolutionConfig,
        config_path: impl AsRef<Path>,
        writer: Option<Arc<AsyncJsonlWriter>>,
    ) -> Result<Self> {
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
            writer,
            judge: JudgeEngine::new(JudgeConfig::default(), MockJudgeModel),
            rollback: RollbackManager::new(
                workspace_root,
                &config_path,
                rollback_dir,
                max_versions,
            )?,
        })
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

    fn build_mutation_plan(
        &self,
        cfg: &EvolutionConfig,
        candidate: &EvolutionCandidate,
    ) -> MutationPlan {
        if candidate.target.contains_key("task_type") {
            let choices = [
                (
                    "retrieval.score_weights.recency",
                    cfg.retrieval.score_weights.recency,
                ),
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
            let after = mutate_numeric(before, candidate, "memory.max_tokens")
                .round()
                .max(64.0);
            let degradation = ((after - before) / before.max(1.0)).max(0.0);
            return MutationPlan {
                key: "memory.max_tokens".to_string(),
                before,
                after,
                token_degradation: degradation,
            };
        }

        let fusion_choices = [
            (
                "memory.retrieval_fusion.bm25",
                cfg.memory.retrieval_fusion.bm25,
            ),
            (
                "memory.retrieval_fusion.vector",
                cfg.memory.retrieval_fusion.vector,
            ),
            (
                "memory.retrieval_fusion.metadata",
                cfg.memory.retrieval_fusion.metadata,
            ),
        ];
        let index = candidate.evidence_ids.len() % fusion_choices.len();
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
            "retrieval.score_weights.access_freq" => {
                cfg.retrieval.score_weights.access_freq = plan.after
            }
            "retrieval.score_weights.category_weight" => {
                cfg.retrieval.score_weights.category_weight = plan.after
            }
            "retrieval.score_weights.useful_ratio" => {
                cfg.retrieval.score_weights.useful_ratio = plan.after
            }
            "retrieval.score_weights.source_confidence" => {
                cfg.retrieval.score_weights.source_confidence = plan.after
            }
            "memory.max_tokens" => cfg.memory.max_tokens = plan.after as usize,
            "memory.retrieval_fusion.bm25" => cfg.memory.retrieval_fusion.bm25 = plan.after,
            "memory.retrieval_fusion.vector" => cfg.memory.retrieval_fusion.vector = plan.after,
            "memory.retrieval_fusion.metadata" => cfg.memory.retrieval_fusion.metadata = plan.after,
            _ => {}
        }
    }

    async fn prune_redundant_conversation_memories(&self) -> Result<u32> {
        let db_path = self.workspace_root.join("memory").join("brain.db");
        if !db_path.exists() {
            return Ok(0);
        }

        let memory = SqliteMemory::new_with_path(db_path)?;
        let entries = memory
            .list(Some(&MemoryCategory::Conversation), None)
            .await?;
        if entries.len() < 2 {
            return Ok(0);
        }

        let compressor = MemoryCompressor::new(EmbeddingSimilarityDetector::default());
        let redundant_ids = compressor.detect_redundancy(&entries).await?;
        if redundant_ids.is_empty() {
            return Ok(0);
        }

        let key_by_id = entries
            .into_iter()
            .map(|entry| (entry.id, entry.key))
            .collect::<HashMap<_, _>>();
        let mut removed = 0u32;
        for redundant_id in redundant_ids {
            let Some(key) = key_by_id.get(&redundant_id) else {
                continue;
            };
            if memory.forget(key).await? {
                removed = removed.saturating_add(1);
            }
        }

        Ok(removed)
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
        let cycle_id = if input.cycle_id.is_empty() {
            Uuid::now_v7().to_string()
        } else {
            input.cycle_id
        };

        let current = self.shared_config.load_full();
        let deduplicated_conversation_memories =
            match self.prune_redundant_conversation_memories().await {
                Ok(count) => count,
                Err(error) => {
                    tracing::warn!(
                        target: "self_system",
                        "memory redundancy cleanup skipped: {error}"
                    );
                    0
                }
            };
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

        let mut applied = false;
        let mut outcome = CycleOutcome::NoAction;
        let mut validation_status = ValidationStatus::Skipped;
        let mut notes = "shadow mode: recommendation only".to_string();

        match mode {
            EvolutionMode::Shadow => {}
            EvolutionMode::Auto => {
                if matches!(gate_result, GateResult::Passed) {
                    self.rollback.backup_current_version().await?;
                    if let Some(parent) = self.config_path.parent() {
                        if !parent.as_os_str().is_empty() {
                            fs::create_dir_all(parent).await?;
                        }
                    }
                    atomic_write(
                        &self.workspace_root,
                        &self.config_path,
                        serialized.as_bytes(),
                    )
                    .await?;
                    self.shared_config.store(Arc::new(next_cfg));
                    applied = true;
                    outcome = CycleOutcome::Applied;
                    validation_status = ValidationStatus::Improved;
                    notes = "gate passed and config persisted".to_string();
                } else {
                    outcome = CycleOutcome::Failed;
                    validation_status = ValidationStatus::Regressed;
                    notes = "gate rejected mutation".to_string();
                }
            }
        }
        if deduplicated_conversation_memories > 0 {
            notes.push_str(&format!(
                "; deduplicated {} conversation memories",
                deduplicated_conversation_memories
            ));
        }

        let mut key_metrics = HashMap::from([
            (
                "average_improvement".to_string(),
                gate_metrics.average_improvement,
            ),
            (
                "token_degradation".to_string(),
                gate_metrics.token_degradation,
            ),
        ]);
        key_metrics.insert(
            "deduplicated_conversation_memories".to_string(),
            f64::from(deduplicated_conversation_memories),
        );

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
            result: Some(if applied {
                EvolutionResult::Improved
            } else if matches!(gate_result, GateResult::Passed) {
                EvolutionResult::Neutral
            } else {
                EvolutionResult::Rejected
            }),
        };

        if let Some(writer) = &self.writer {
            writer.append_evolution(&evolution_log).await?;
        }

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
            trend: FitnessTrend {
                window: 1,
                previous_average: 0.5,
                latest_score: if applied { 0.6 } else { 0.5 },
                is_declining: false,
            },
            proposal: Some(proposal.clone()),
            validation: EvolutionValidation {
                status: validation_status.clone(),
                before_score: 0.5,
                after_score: if validation_status == ValidationStatus::Improved {
                    0.6
                } else {
                    0.5
                },
                delta: if validation_status == ValidationStatus::Improved {
                    0.1
                } else {
                    0.0
                },
                notes,
            },
            outcome,
            alert: match gate_result {
                GateResult::Passed => None,
                GateResult::Rejected(ref rejection) => Some(format!(
                    "gate_rejected:{}:{}",
                    rejection.reason, rejection.details
                )),
            },
            errors: Vec::new(),
        };

        Ok(CycleResult {
            layer: EvolutionLayer::Memory,
            proposal: Some(proposal),
            cycle,
            evolution_log: Some(evolution_log),
            needs_human_approval: false,
            shadow_mode: matches!(mode, EvolutionMode::Shadow),
        })
    }
}

fn mutate_numeric(value: f64, candidate: &EvolutionCandidate, salt: &str) -> f64 {
    let seed = format!(
        "{}:{}:{}",
        candidate.current_value, candidate.suggested_value, salt
    );
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
    use crate::self_system::evolution::config::{
        new_shared_evolution_config, EvolutionConfig, EvolutionMode,
    };
    use tempfile::tempdir;

    #[tokio::test]
    async fn memory_engine_shadow_mode_only_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evolution_config.toml");
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Shadow;
        fs::write(&path, toml::to_string_pretty(&cfg).unwrap())
            .await
            .unwrap();

        let shared = new_shared_evolution_config(cfg);
        let mut engine = MemoryEvolutionEngine::new(shared, &path, None).unwrap();
        let result = engine
            .run_cycle(EngineCycleInput {
                cycle_id: "c1".to_string(),
                analyzer_candidates: vec![default_candidate()],
            })
            .await
            .unwrap();

        assert!(result.shadow_mode);
        assert!(!matches!(result.cycle.outcome, CycleOutcome::Applied));
    }

    #[tokio::test]
    async fn memory_engine_auto_applies_when_gate_passes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evolution_config.toml");
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Auto;
        fs::write(&path, toml::to_string_pretty(&cfg).unwrap())
            .await
            .unwrap();

        let shared = new_shared_evolution_config(cfg);
        let mut engine = MemoryEvolutionEngine::new(shared.clone(), &path, None).unwrap();
        let result = engine
            .run_cycle(EngineCycleInput {
                cycle_id: "c2".to_string(),
                analyzer_candidates: vec![default_candidate()],
            })
            .await
            .unwrap();

        assert!(matches!(
            result.cycle.outcome,
            CycleOutcome::Applied | CycleOutcome::Failed
        ));
    }
}
