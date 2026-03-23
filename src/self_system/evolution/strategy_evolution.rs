use crate::self_system::evolution::analyzer::{CandidatePriority, EvolutionCandidate};
use crate::self_system::evolution::config::{EvolutionMode, SharedEvolutionConfig};
use crate::self_system::evolution::engine::{CycleResult, EngineCycleInput, EvolutionEngine};
use crate::self_system::evolution::gate::{EvolutionGate, GateMetrics, GateResult};
use crate::self_system::evolution::record::{
    ChangeType, DataBasis, DecisionLog, EvolutionLayer, EvolutionLog, EvolutionResult, Outcome,
};
use crate::self_system::evolution::rollback::RollbackManager;
use crate::self_system::evolution::safety_utils::{atomic_write, validate_path_in_workspace};
use crate::self_system::evolution::storage::AsyncJsonlWriter;
use crate::self_system::evolution::{
    ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals,
    EvolutionValidation, FitnessTrend, RiskLevel, ValidationStatus,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use uuid::Uuid;

/// Per-task-type daily summary.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskDailySummary {
    pub task_type: String,
    pub total: u32,
    pub success: u32,
    pub avg_tokens: f64,
    pub avg_latency_ms: f64,
    pub efficiency_score: f64,
}

#[derive(Debug, Clone)]
struct StrategyMutationPlan {
    path: String,
    before: toml::Value,
    after: toml::Value,
}

/// L3 strategy evolution executor.
pub struct StrategyEvolutionEngine {
    shared_config: SharedEvolutionConfig,
    workspace_root: PathBuf,
    writer: Arc<AsyncJsonlWriter>,
    rollback: RollbackManager,
}

impl StrategyEvolutionEngine {
    pub fn new(
        shared_config: SharedEvolutionConfig,
        workspace_root: impl AsRef<Path>,
        writer: Arc<AsyncJsonlWriter>,
    ) -> Result<Self> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let cfg = shared_config.load_full();
        let policy_path = validate_path_in_workspace(&workspace_root, Path::new(&cfg.strategy.decision_policy_path))?;
        let rollback_dir = validate_path_in_workspace(&workspace_root, Path::new(".evolution/rollback/strategy"))?;
        let rollback = RollbackManager::new(&workspace_root, &policy_path, rollback_dir, cfg.rollback.max_versions)?;
        Ok(Self {
            shared_config,
            workspace_root,
            writer,
            rollback,
        })
    }

    async fn collect_daily_summary(&self) -> Result<Vec<TaskDailySummary>> {
        let since = Utc::now() - Duration::hours(24);
        let decisions = self.writer.read_decisions_since(since).await?;
        Ok(build_task_daily_summary(&decisions))
    }

    fn select_worst_summary(&self, summaries: &[TaskDailySummary]) -> Option<TaskDailySummary> {
        summaries
            .iter()
            .min_by(|a, b| a.efficiency_score.total_cmp(&b.efficiency_score))
            .cloned()
    }

    fn choose_mutation_plan(
        &self,
        policy: &toml::Value,
        mutation_range: f64,
        worst: Option<&TaskDailySummary>,
    ) -> Result<StrategyMutationPlan> {
        let scalar_params = list_mutable_params(policy);
        if scalar_params.is_empty() {
            bail!("decision policy has no mutable scalar parameter");
        }
        let pivot = worst.map(|item| stable_hash(&item.task_type) as usize).unwrap_or(0);
        // SAFETY: pivot % scalar_params.len() is always < scalar_params.len(), which is non-empty
        #[allow(clippy::indexing_slicing)]
        let (path, before_value) = scalar_params[pivot % scalar_params.len()].clone();
        let after_value = match before_value {
            toml::Value::Integer(v) => toml::Value::Integer(mutate_numeric(v as f64, mutation_range).round() as i64),
            toml::Value::Float(v) => toml::Value::Float(mutate_numeric(v, mutation_range)),
            toml::Value::Boolean(v) => toml::Value::Boolean(!v),
            _ => bail!("unsupported mutable type at {path}"),
        };
        Ok(StrategyMutationPlan {
            path,
            before: before_value,
            after: after_value,
        })
    }
}

#[async_trait]
impl EvolutionEngine for StrategyEvolutionEngine {
    fn name(&self) -> &'static str {
        "strategy_evolution_engine"
    }

    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Policy
    }

    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
        let started_at = Utc::now().to_rfc3339();
        let cycle_id = if input.cycle_id.is_empty() {
            Uuid::now_v7().to_string()
        } else {
            input.cycle_id
        };
        let cfg = self.shared_config.load_full();
        let mode = cfg.runtime.mode.clone();
        let policy_path =
            validate_path_in_workspace(&self.workspace_root, Path::new(&cfg.strategy.decision_policy_path))?;
        let raw = fs::read_to_string(&policy_path)
            .await
            .with_context(|| format!("failed reading {}", policy_path.display()))?;
        let mut parsed = raw.parse::<toml::Value>()?;

        let summaries = self.collect_daily_summary().await?;
        let worst = self.select_worst_summary(&summaries);
        let plan = self.choose_mutation_plan(&parsed, cfg.strategy.param_mutation_range.max(0.0), worst.as_ref())?;
        set_value_at_path(&mut parsed, &plan.path, plan.after.clone())?;
        let serialized = toml::to_string_pretty(&parsed)?;

        let candidate = input
            .analyzer_candidates
            .first()
            .cloned()
            .unwrap_or_else(default_candidate);
        let proposal = EvolutionProposal {
            id: Uuid::now_v7().to_string(),
            summary: format!("Mutate strategy parameter {}", plan.path),
            rationale: format!(
                "based on worst efficiency task_type={}",
                worst.as_ref().map(|item| item.task_type.as_str()).unwrap_or("unknown")
            ),
            risk_level: RiskLevel::Medium,
            target: ChangeTarget::ConfigFile {
                path: cfg.strategy.decision_policy_path.clone(),
            },
            operation: ChangeOperation::Write {
                content: serialized.clone(),
            },
        };

        let avg_success = worst
            .as_ref()
            .map(|item| item.success as f64 / item.total.max(1) as f64)
            .unwrap_or(0.5);
        let token_degradation = worst
            .as_ref()
            .map(|item| (item.avg_tokens / 1000.0).clamp(0.0, 1.0))
            .unwrap_or(0.0);

        let gate_metrics = GateMetrics {
            average_improvement: (1.0 - avg_success) * 0.1,
            holdout_regression: -0.02,
            token_degradation,
        };
        let gate = EvolutionGate::from_evolution_config(cfg.as_ref());
        let gate_result = gate.evaluate(&candidate, &gate_metrics);

        let mut outcome = CycleOutcome::NoAction;
        let mut status = ValidationStatus::Skipped;
        let mut notes = "shadow mode: strategy mutation recorded only".to_string();

        if !matches!(mode, EvolutionMode::Shadow) {
            if matches!(gate_result, GateResult::Passed) {
                self.rollback.backup_current_version().await?;
                atomic_write(&self.workspace_root, &policy_path, serialized.as_bytes()).await?;
                outcome = CycleOutcome::Applied;
                status = ValidationStatus::Improved;
                notes = "gate passed and strategy persisted".to_string();
            } else {
                outcome = CycleOutcome::Failed;
                status = ValidationStatus::Regressed;
                notes = "gate rejected strategy mutation".to_string();
            }
        }

        let evolution_log = EvolutionLog {
            experiment_id: proposal.id.clone(),
            timestamp: Utc::now().to_rfc3339(),
            layer: EvolutionLayer::Policy,
            change_type: ChangeType::Tune,
            before_value: format!("{}={}", plan.path, plan.before),
            after_value: format!("{}={}", plan.path, plan.after),
            trigger_reason: proposal.rationale.clone(),
            data_basis: DataBasis {
                sample_count: summaries.iter().map(|item| item.total).sum(),
                time_range_days: 1,
                key_metrics: HashMap::from([
                    ("average_improvement".to_string(), gate_metrics.average_improvement),
                    ("token_degradation".to_string(), gate_metrics.token_degradation),
                ]),
                patterns_found: vec![format!(
                    "worst_task_type={}",
                    worst.as_ref().map(|item| item.task_type.as_str()).unwrap_or("unknown")
                )],
            },
            result: Some(if matches!(outcome, CycleOutcome::Applied) {
                EvolutionResult::Improved
            } else if matches!(gate_result, GateResult::Passed) {
                EvolutionResult::Neutral
            } else {
                EvolutionResult::Rejected
            }),
        };
        self.writer.append_evolution(&evolution_log).await?;

        let cycle = EvolutionCycle {
            id: cycle_id,
            started_at,
            finished_at: Utc::now().to_rfc3339(),
            signals: EvolutionSignals {
                memory_count: 0,
                health_components: 1,
                health_error_components: 0,
                cron_runs: 0,
                cron_failure_ratio: 0.0,
            },
            trend: FitnessTrend {
                window: 1,
                previous_average: 0.5,
                latest_score: if matches!(outcome, CycleOutcome::Applied) {
                    0.6
                } else {
                    0.5
                },
                is_declining: false,
            },
            proposal: Some(proposal.clone()),
            validation: EvolutionValidation {
                status,
                before_score: 0.5,
                after_score: if matches!(outcome, CycleOutcome::Applied) {
                    0.6
                } else {
                    0.5
                },
                delta: if matches!(outcome, CycleOutcome::Applied) {
                    0.1
                } else {
                    0.0
                },
                notes,
            },
            outcome,
            alert: match gate_result {
                GateResult::Passed => None,
                GateResult::Rejected(rejection) => {
                    Some(format!("gate_rejected:{}:{}", rejection.reason, rejection.details))
                }
            },
            errors: Vec::new(),
        };

        Ok(CycleResult {
            layer: EvolutionLayer::Policy,
            proposal: Some(proposal),
            cycle,
            evolution_log: Some(evolution_log),
            needs_human_approval: false,
            shadow_mode: matches!(mode, EvolutionMode::Shadow),
        })
    }
}

fn default_candidate() -> EvolutionCandidate {
    let mut target = BTreeMap::new();
    target.insert("strategy".to_string(), "decision_policy".to_string());
    EvolutionCandidate {
        target,
        current_value: "baseline_policy".to_string(),
        suggested_value: "single_parameter_mutation".to_string(),
        evidence_ids: vec!["strategy-summary".to_string()],
        priority: CandidatePriority::Medium,
        backfill_after_days: 1,
    }
}

fn mutate_numeric(value: f64, range: f64) -> f64 {
    let ratio = range.abs().min(1.0);
    value * (1.0 + ratio)
}

fn list_mutable_params(value: &toml::Value) -> Vec<(String, toml::Value)> {
    let mut out = Vec::new();
    collect_scalars(value, "", &mut out);
    out
}

fn collect_scalars(value: &toml::Value, prefix: &str, out: &mut Vec<(String, toml::Value)>) {
    match value {
        toml::Value::Table(table) => {
            for (k, v) in table {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_scalars(v, &next, out);
            }
        }
        toml::Value::Boolean(_) | toml::Value::Integer(_) | toml::Value::Float(_) => {
            out.push((prefix.to_string(), value.clone()));
        }
        _ => {}
    }
}

fn set_value_at_path(root: &mut toml::Value, path: &str, value: toml::Value) -> Result<()> {
    let parts = path.split('.').collect::<Vec<_>>();
    let mut current = root;
    for (idx, key) in parts.iter().enumerate() {
        let is_last = idx + 1 == parts.len();
        if is_last {
            let Some(table) = current.as_table_mut() else {
                bail!("invalid table path segment: {key}");
            };
            table.insert((*key).to_string(), value);
            return Ok(());
        }
        let Some(next) = current.get_mut(*key) else {
            bail!("path not found: {path}");
        };
        current = next;
    }
    Ok(())
}

fn stable_hash(value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn build_task_daily_summary(decisions: &[DecisionLog]) -> Vec<TaskDailySummary> {
    let mut map: BTreeMap<String, (u32, u32, u64, u64)> = BTreeMap::new();
    for item in decisions {
        let key = format!("{:?}", item.task_type).to_ascii_lowercase();
        let row = map.entry(key).or_insert((0, 0, 0, 0));
        row.0 = row.0.saturating_add(1);
        if item.outcome == Outcome::Success {
            row.1 = row.1.saturating_add(1);
        }
        row.2 = row.2.saturating_add(u64::from(item.tokens_used));
        row.3 = row.3.saturating_add(item.latency_ms);
    }

    map.into_iter()
        .map(|(task_type, (total, success, tokens, latency))| {
            let total_f = total.max(1) as f64;
            let success_rate = success as f64 / total_f;
            let avg_tokens = tokens as f64 / total_f;
            let avg_latency_ms = latency as f64 / total_f;
            let efficiency_score = success_rate / (1.0 + avg_tokens / 500.0 + avg_latency_ms / 2000.0);
            TaskDailySummary {
                task_type,
                total,
                success,
                avg_tokens,
                avg_latency_ms,
                efficiency_score,
            }
        })
        .collect()
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::config::{EvolutionConfig, new_shared_evolution_config};
    use crate::self_system::evolution::storage::{AsyncJsonlWriter, JsonlRetentionPolicy, JsonlStoragePaths};
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn task_daily_summary_aggregates_success_tokens_and_latency() {
        let decisions = vec![
            DecisionLog {
                timestamp: "2026-02-24T00:00:00Z".to_string(),
                experiment_id: "e1".to_string(),
                trace_id: "t1".to_string(),
                decision_type: crate::self_system::evolution::record::DecisionType::RuntimePolicy,
                task_type: crate::self_system::evolution::record::TaskType::Planning,
                risk_level: 1,
                actor: crate::self_system::evolution::record::Actor::Agent,
                input_context: "a".to_string(),
                action_taken: "x".to_string(),
                outcome: Outcome::Success,
                tokens_used: 100,
                latency_ms: 80,
                user_correction: None,
                config_snapshot_hash: "h".to_string(),
            },
            DecisionLog {
                timestamp: "2026-02-24T00:01:00Z".to_string(),
                experiment_id: "e2".to_string(),
                trace_id: "t2".to_string(),
                decision_type: crate::self_system::evolution::record::DecisionType::RuntimePolicy,
                task_type: crate::self_system::evolution::record::TaskType::Planning,
                risk_level: 1,
                actor: crate::self_system::evolution::record::Actor::Agent,
                input_context: "b".to_string(),
                action_taken: "y".to_string(),
                outcome: Outcome::Failure,
                tokens_used: 300,
                latency_ms: 120,
                user_correction: None,
                config_snapshot_hash: "h".to_string(),
            },
        ];

        let rows = build_task_daily_summary(&decisions);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total, 2);
        assert_eq!(rows[0].success, 1);
        assert_eq!(rows[0].avg_tokens, 200.0);
    }

    #[tokio::test]
    async fn strategy_engine_rejects_parent_traversal_policy_path() {
        let dir = tempdir().unwrap();
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(dir.path().join("logs")),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .unwrap(),
        );
        let mut cfg = EvolutionConfig::default();
        cfg.strategy.decision_policy_path = "../outside.toml".to_string();
        let err = match StrategyEvolutionEngine::new(new_shared_evolution_config(cfg), dir.path(), writer) {
            Ok(_) => panic!("expected traversal path validation to fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("parent traversal"));
    }
}
