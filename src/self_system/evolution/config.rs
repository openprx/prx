use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::time::{Duration, Instant};

/// Evolution execution mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionMode {
    /// Analyze and log recommendations but do not auto-apply.
    #[default]
    Shadow,
    /// Auto-apply allowed evolution changes.
    Auto,
}

/// Data thresholds used to gate evolution actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DataThresholds {
    pub decision_log: u32,
    pub memory_access: u32,
    pub same_failure: u32,
}

impl Default for DataThresholds {
    fn default() -> Self {
        Self {
            decision_log: 200,
            memory_access: 800,
            same_failure: 25,
        }
    }
}

/// Tiered retention parameters in days.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionRetentionConfig {
    pub hot_days: u32,
    pub warm_days: u32,
    pub cold_days: u32,
}

impl Default for EvolutionRetentionConfig {
    fn default() -> Self {
        Self {
            hot_days: 30,
            warm_days: 90,
            cold_days: 180,
        }
    }
}

/// Runtime storage and refresh behavior for evolution records.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionRuntimeConfig {
    pub mode: EvolutionMode,
    pub storage_dir: String,
    pub batch_size: usize,
    pub poll_interval_secs: u64,
    pub retention: EvolutionRetentionConfig,
    pub data_thresholds: DataThresholds,
}

impl Default for EvolutionRuntimeConfig {
    fn default() -> Self {
        Self {
            mode: EvolutionMode::default(),
            storage_dir: "self/evolution".to_string(),
            batch_size: 64,
            poll_interval_secs: 3,
            retention: EvolutionRetentionConfig::default(),
            data_thresholds: DataThresholds::default(),
        }
    }
}

/// Top-level config for `evolution_config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvolutionConfig {
    #[serde(default)]
    pub runtime: EvolutionRuntimeConfig,
    #[serde(default)]
    pub retrieval: EvolutionRetrievalConfig,
    #[serde(default)]
    pub gate: EvolutionGateConfig,
    #[serde(default)]
    pub memory: MemoryEvolutionConfig,
    #[serde(default)]
    pub prompt: PromptEvolutionConfig,
    #[serde(default)]
    pub strategy: StrategyEvolutionConfig,
    #[serde(default)]
    pub rollback: RollbackConfig,
}

/// L1 memory evolution tuning config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryEvolutionConfig {
    pub max_tokens: usize,
    #[serde(default)]
    pub retrieval_fusion: RetrievalFusionWeights,
}

impl Default for MemoryEvolutionConfig {
    fn default() -> Self {
        Self {
            max_tokens: 2_048,
            retrieval_fusion: RetrievalFusionWeights::default(),
        }
    }
}

/// Fusion weights used for retrieval signal merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalFusionWeights {
    pub bm25: f64,
    pub vector: f64,
    pub metadata: f64,
}

impl Default for RetrievalFusionWeights {
    fn default() -> Self {
        Self {
            bm25: 0.4,
            vector: 0.4,
            metadata: 0.2,
        }
    }
}

/// L2 prompt evolution guardrails and rollout policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptEvolutionConfig {
    #[serde(default)]
    pub mutable_files: Vec<String>,
    #[serde(default)]
    pub immutable_files: Vec<String>,
    pub human_approval_severity: u8,
    pub max_rollback_versions: usize,
    #[serde(default = "default_blocked_keywords")]
    pub blocked_keywords: Vec<String>,
}

impl Default for PromptEvolutionConfig {
    fn default() -> Self {
        Self {
            mutable_files: Vec::new(),
            immutable_files: Vec::new(),
            human_approval_severity: 3,
            max_rollback_versions: 10,
            blocked_keywords: default_blocked_keywords(),
        }
    }
}

fn default_blocked_keywords() -> Vec<String> {
    vec![
        "disable safety".to_string(),
        "bypass policy".to_string(),
        "ignore security".to_string(),
        "remove guardrail".to_string(),
    ]
}

/// L3 strategy evolution config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyEvolutionConfig {
    pub decision_policy_path: String,
    pub param_mutation_range: f64,
}

impl Default for StrategyEvolutionConfig {
    fn default() -> Self {
        Self {
            decision_policy_path: "decision_policy.toml".to_string(),
            param_mutation_range: 0.15,
        }
    }
}

/// Rollback and circuit-breaker controls.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RollbackConfig {
    pub max_versions: usize,
    pub circuit_breaker_threshold: u32,
    pub cooldown_after_rollback_hours: u64,
}

impl Default for RollbackConfig {
    fn default() -> Self {
        Self {
            max_versions: 10,
            circuit_breaker_threshold: 5,
            cooldown_after_rollback_hours: 24,
        }
    }
}

/// Gate thresholds for allowing evolution changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionGateConfig {
    /// Minimum average improvement required to pass gate.
    pub min_improvement: f64,
    /// Maximum allowed holdout regression (typically negative).
    pub max_regression: f64,
    /// Maximum allowed token degradation.
    pub max_token_degradation: f64,
}

impl Default for EvolutionGateConfig {
    fn default() -> Self {
        Self {
            min_improvement: 0.03,
            max_regression: -0.05,
            max_token_degradation: 0.10,
        }
    }
}

/// Scoring weights for evolution-aware memory retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalScoreWeights {
    pub recency: f64,
    pub access_freq: f64,
    pub category_weight: f64,
    pub useful_ratio: f64,
    pub source_confidence: f64,
}

impl Default for RetrievalScoreWeights {
    fn default() -> Self {
        Self {
            recency: 0.30,
            access_freq: 0.20,
            category_weight: 0.15,
            useful_ratio: 0.20,
            source_confidence: 0.15,
        }
    }
}

/// Retrieval tuning knobs used by memory evolution pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionRetrievalConfig {
    pub vector_retrieval_threshold: usize,
    #[serde(default)]
    pub score_weights: RetrievalScoreWeights,
}

impl Default for EvolutionRetrievalConfig {
    fn default() -> Self {
        Self {
            vector_retrieval_threshold: 2_000,
            score_weights: RetrievalScoreWeights::default(),
        }
    }
}

impl EvolutionConfig {
    /// Load and parse `evolution_config.toml` from disk.
    pub async fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read evolution config: {}", path.display()))?;
        let cfg = toml::from_str::<Self>(&content)
            .with_context(|| format!("failed to parse evolution config: {}", path.display()))?;
        Ok(cfg)
    }
}

/// Shared lock-free handle for runtime evolution config.
pub type SharedEvolutionConfig = Arc<ArcSwap<EvolutionConfig>>;

/// Create a shared evolution config handle.
pub fn new_shared_evolution_config(initial: EvolutionConfig) -> SharedEvolutionConfig {
    Arc::new(ArcSwap::from_pointee(initial))
}

/// Runtime polling hot-reload manager for `evolution_config.toml`.
pub struct EvolutionRuntimeConfigManager {
    _handle: tokio::task::JoinHandle<()>,
    reload_version: Arc<AtomicU64>,
    shared: SharedEvolutionConfig,
}

impl EvolutionRuntimeConfigManager {
    /// Spawn a background poller that reloads config when file content changes.
    pub fn spawn(config_path: PathBuf, initial: EvolutionConfig) -> Self {
        let shared = new_shared_evolution_config(initial.clone());
        let poll_interval = Duration::from_secs(initial.runtime.poll_interval_secs.max(1));
        let reload_version = Arc::new(AtomicU64::new(0));
        let version_ref = Arc::clone(&reload_version);
        let shared_ref = Arc::clone(&shared);
        let initial_hash = match std::fs::read(&config_path) {
            Ok(data) => Some(hash_bytes(&data)),
            Err(err) => {
                tracing::debug!(
                    path = %config_path.display(),
                    error = %err,
                    "failed to read initial evolution config hash; waiting for first successful poll"
                );
                None
            }
        };

        let handle = tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval_at(Instant::now() + poll_interval, poll_interval);
            let mut last_hash = initial_hash;

            loop {
                ticker.tick().await;
                let Ok(content) = tokio::fs::read(&config_path).await else {
                    tracing::debug!(
                        path = %config_path.display(),
                        "skipping evolution config poll because file is currently unreadable"
                    );
                    continue;
                };
                let current_hash = hash_bytes(&content);
                if last_hash.as_ref().is_some_and(|h| h == &current_hash) {
                    continue;
                }

                let raw = match std::str::from_utf8(&content) {
                    Ok(raw) => raw,
                    Err(err) => {
                        tracing::warn!(
                            path = %config_path.display(),
                            error = %err,
                            "skipping evolution config reload due to invalid utf-8 content"
                        );
                        continue;
                    }
                };
                match toml::from_str::<EvolutionConfig>(raw) {
                    Ok(config) => {
                        shared_ref.store(Arc::new(config));
                        last_hash = Some(current_hash);
                        version_ref.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %config_path.display(),
                            error = %err,
                            "skipping evolution config reload due to parse error"
                        );
                    }
                }
            }
        });

        Self {
            _handle: handle,
            reload_version,
            shared,
        }
    }

    /// Number of successful runtime reloads.
    pub fn reload_version(&self) -> u64 {
        self.reload_version.load(Ordering::Relaxed)
    }

    /// Shared config state.
    pub fn shared(&self) -> SharedEvolutionConfig {
        Arc::clone(&self.shared)
    }
}

fn hash_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn load_from_path_parses_threshold_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evolution_config.toml");
        tokio::fs::write(&path, "").await.unwrap();

        let cfg = EvolutionConfig::load_from_path(&path).await.unwrap();
        assert_eq!(cfg.runtime.mode, EvolutionMode::Shadow);
        assert_eq!(cfg.runtime.data_thresholds.decision_log, 200);
        assert_eq!(cfg.runtime.data_thresholds.memory_access, 800);
        assert_eq!(cfg.runtime.data_thresholds.same_failure, 25);
        assert_eq!(cfg.retrieval.vector_retrieval_threshold, 2_000);
        assert_eq!(cfg.gate.min_improvement, 0.03);
        assert_eq!(cfg.gate.max_regression, -0.05);
        assert_eq!(cfg.gate.max_token_degradation, 0.10);
        assert_eq!(cfg.retrieval.score_weights.recency, 0.30);
        assert_eq!(cfg.retrieval.score_weights.access_freq, 0.20);
        assert_eq!(cfg.retrieval.score_weights.category_weight, 0.15);
        assert_eq!(cfg.retrieval.score_weights.useful_ratio, 0.20);
        assert_eq!(cfg.retrieval.score_weights.source_confidence, 0.15);
        assert_eq!(cfg.memory.max_tokens, 2_048);
        assert_eq!(cfg.memory.retrieval_fusion.bm25, 0.4);
        assert_eq!(cfg.memory.retrieval_fusion.vector, 0.4);
        assert_eq!(cfg.memory.retrieval_fusion.metadata, 0.2);
        assert_eq!(cfg.prompt.human_approval_severity, 3);
        assert_eq!(cfg.prompt.max_rollback_versions, 10);
        assert_eq!(cfg.strategy.decision_policy_path, "decision_policy.toml");
        assert_eq!(cfg.strategy.param_mutation_range, 0.15);
        assert_eq!(cfg.rollback.max_versions, 10);
        assert_eq!(cfg.rollback.circuit_breaker_threshold, 5);
        assert_eq!(cfg.rollback.cooldown_after_rollback_hours, 24);
    }

    #[tokio::test]
    async fn manager_hot_reloads_on_content_change() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evolution_config.toml");
        tokio::fs::write(
            &path,
            r#"
[runtime]
mode = "shadow"
storage_dir = "self/evolution"
batch_size = 32
poll_interval_secs = 1

[runtime.retention]
hot_days = 30
warm_days = 90
cold_days = 180

[runtime.data_thresholds]
decision_log = 200
memory_access = 800
same_failure = 25
"#,
        )
        .await
        .unwrap();

        let initial = EvolutionConfig::load_from_path(&path).await.unwrap();
        let manager = EvolutionRuntimeConfigManager::spawn(path.clone(), initial);

        tokio::fs::write(
            &path,
            r#"
[runtime]
mode = "auto"
storage_dir = "self/evolution"
batch_size = 64
poll_interval_secs = 1

[runtime.retention]
hot_days = 30
warm_days = 90
cold_days = 180

[runtime.data_thresholds]
decision_log = 200
memory_access = 800
same_failure = 25
"#,
        )
        .await
        .unwrap();

        let mut reloaded = false;
        for _ in 0..10 {
            if manager.reload_version() >= 1 {
                reloaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        assert!(reloaded);
        assert_eq!(
            manager.shared().load_full().runtime.mode,
            EvolutionMode::Auto
        );
        assert_eq!(manager.shared().load_full().runtime.batch_size, 64);
    }

    #[test]
    fn config_toml_parses_with_partial_nested_tables() {
        let parsed = toml::from_str::<EvolutionConfig>(
            r#"
[runtime]
mode = "auto"

[runtime.retention]
hot_days = 7

[retrieval]
vector_retrieval_threshold = 123

[memory]
max_tokens = 4096
            "#,
        )
        .unwrap();

        assert_eq!(parsed.runtime.mode, EvolutionMode::Auto);
        assert_eq!(parsed.runtime.storage_dir, "self/evolution");
        assert_eq!(parsed.runtime.batch_size, 64);
        assert_eq!(parsed.runtime.retention.hot_days, 7);
        assert_eq!(parsed.runtime.retention.warm_days, 90);
        assert_eq!(parsed.runtime.data_thresholds.decision_log, 200);
        assert_eq!(parsed.retrieval.vector_retrieval_threshold, 123);
        assert_eq!(parsed.retrieval.score_weights.recency, 0.30);
        assert_eq!(parsed.memory.max_tokens, 4096);
        assert_eq!(parsed.memory.retrieval_fusion.bm25, 0.4);
    }

    #[test]
    fn config_json_parses_with_partial_nested_objects() {
        let parsed = serde_json::from_str::<EvolutionConfig>(
            r#"{
  "runtime": {
    "retention": { "cold_days": 365 }
  },
  "gate": {
    "min_improvement": 0.08
  }
}"#,
        )
        .unwrap();

        assert_eq!(parsed.runtime.mode, EvolutionMode::Shadow);
        assert_eq!(parsed.runtime.retention.hot_days, 30);
        assert_eq!(parsed.runtime.retention.cold_days, 365);
        assert_eq!(parsed.runtime.data_thresholds.memory_access, 800);
        assert_eq!(parsed.gate.min_improvement, 0.08);
        assert_eq!(parsed.gate.max_regression, -0.05);
    }
}
