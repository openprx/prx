use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Evolution execution mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionMode {
    /// Emit proposal drafts only; never mutate memory, prompts, config, or strategy files.
    #[default]
    DraftOnly,
    /// Analyze and log recommendations but do not auto-apply.
    Shadow,
    /// Auto-apply allowed evolution changes.
    Auto,
}

impl EvolutionMode {
    pub const fn allows_target_mutation(&self) -> bool {
        matches!(self, Self::Auto)
    }

    pub const fn is_proposal_only(&self) -> bool {
        matches!(self, Self::DraftOnly | Self::Shadow)
    }

    pub const fn requires_judge(&self) -> bool {
        matches!(self, Self::Shadow | Self::Auto)
    }

    pub const fn requires_grant(&self) -> bool {
        matches!(self, Self::Auto)
    }

    pub const fn is_draft_like(&self) -> bool {
        self.is_proposal_only()
    }
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
    pub retention: EvolutionRetentionConfig,
    pub data_thresholds: DataThresholds,
}

impl Default for EvolutionRuntimeConfig {
    fn default() -> Self {
        Self {
            mode: EvolutionMode::default(),
            storage_dir: "self/evolution".to_string(),
            batch_size: 64,
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
    pub gate: EvolutionGateConfig,
    #[serde(default)]
    pub prompt: PromptEvolutionConfig,
    #[serde(default)]
    pub rollback: RollbackConfig,
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

/// Identify retired inputs without loading the main operator configuration.
pub fn removed_evolution_keys(raw: &str) -> Vec<String> {
    let Ok(value) = toml::from_str::<toml::Value>(raw) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for path in ["strategy", "memory.max_tokens", "memory.retrieval_fusion", "retrieval"] {
        if path.split('.').try_fold(&value, |v, part| v.get(part)).is_some() {
            keys.push(path.to_string());
        }
    }
    if let Some(entries) = value
        .get("prompt")
        .and_then(|v| v.get("mutable_files"))
        .and_then(toml::Value::as_array)
    {
        for entry in entries.iter().filter_map(toml::Value::as_str) {
            let managed = entry
                .strip_prefix("skills/")
                .and_then(|path| path.strip_suffix("/SKILL.md"))
                .is_some_and(|name| !name.is_empty() && !name.contains('/'));
            if !managed {
                keys.push(format!("prompt.mutable_files: {entry}"));
            }
        }
    }
    keys
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
        for key in removed_evolution_keys(&content) {
            tracing::warn!(path = %path.display(), key = %key,
                "configuration key is ignored: evolution adopts only through the skills lifecycle");
        }
        Ok(cfg)
    }
}

/// Transactional in-process state for the evolution pipeline.
///
/// This domain object is loaded once by the ConfigGeneration-controlled
/// self-system supervisor. It has no file watcher; only a successful,
/// persisted evolution mutation may swap it during that pinned supervisor
/// generation.
pub type SharedEvolutionConfig = Arc<ArcSwap<EvolutionConfig>>;

/// Create a shared evolution config handle.
pub fn new_shared_evolution_config(initial: EvolutionConfig) -> SharedEvolutionConfig {
    Arc::new(ArcSwap::from_pointee(initial))
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
        assert_eq!(cfg.runtime.mode, EvolutionMode::DraftOnly);
        assert_eq!(cfg.runtime.data_thresholds.decision_log, 200);
        assert_eq!(cfg.runtime.data_thresholds.memory_access, 800);
        assert_eq!(cfg.runtime.data_thresholds.same_failure, 25);
        assert_eq!(cfg.gate.min_improvement, 0.03);
        assert_eq!(cfg.gate.max_regression, -0.05);
        assert_eq!(cfg.gate.max_token_degradation, 0.10);
        assert_eq!(cfg.prompt.human_approval_severity, 3);
        assert_eq!(cfg.prompt.max_rollback_versions, 10);
        assert_eq!(cfg.rollback.max_versions, 10);
        assert_eq!(cfg.rollback.circuit_breaker_threshold, 5);
        assert_eq!(cfg.rollback.cooldown_after_rollback_hours, 24);
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
    }

    #[test]
    fn config_toml_parses_draft_only_mode() {
        let parsed = toml::from_str::<EvolutionConfig>(
            r#"
[runtime]
mode = "draft_only"
            "#,
        )
        .unwrap();

        assert_eq!(parsed.runtime.mode, EvolutionMode::DraftOnly);
        assert!(parsed.runtime.mode.is_proposal_only());
        assert!(!parsed.runtime.mode.requires_judge());
        assert!(!parsed.runtime.mode.requires_grant());
        assert!(!parsed.runtime.mode.allows_target_mutation());
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

        assert_eq!(parsed.runtime.mode, EvolutionMode::DraftOnly);
        assert_eq!(parsed.runtime.retention.hot_days, 30);
        assert_eq!(parsed.runtime.retention.cold_days, 365);
        assert_eq!(parsed.runtime.data_thresholds.memory_access, 800);
        assert_eq!(parsed.gate.min_improvement, 0.08);
        assert_eq!(parsed.gate.max_regression, -0.05);
    }
    #[test]
    fn legacy_keys_load_and_are_reported_once_each() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evolution_config.toml");
        let raw = r#"
[prompt]
mutable_files = ["AGENTS.md", "skills/a/SKILL.md"]
[strategy]
decision_policy_path = "legacy.toml"
[memory]
max_tokens = 4096
[memory.retrieval_fusion]
bm25 = 0.9
[retrieval]
vector_retrieval_threshold = 100
[retrieval.score_weights]
recency = 0.5
"#;
        std::fs::write(&path, raw).unwrap();
        let expected = vec![
            "strategy",
            "memory.max_tokens",
            "memory.retrieval_fusion",
            "retrieval",
            "prompt.mutable_files: AGENTS.md",
        ];
        assert_eq!(removed_evolution_keys(raw), expected);
        let warnings = dir.path().join("warnings.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(std::fs::File::create(&warnings).unwrap())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let cfg = rt.block_on(EvolutionConfig::load_from_path(&path)).unwrap();
            assert_eq!(cfg.runtime.mode, EvolutionMode::DraftOnly);
            assert_eq!(
                cfg.gate.min_improvement,
                EvolutionConfig::default().gate.min_improvement
            );
            assert_eq!(cfg.prompt.mutable_files.len(), 2);
        });
        let log = std::fs::read_to_string(warnings).unwrap();
        assert_eq!(log.lines().count(), 5, "{log}");
        for key in expected {
            let field = format!("key={key}");
            assert_eq!(log.lines().filter(|line| line.ends_with(&field)).count(), 1, "{log}");
        }
        assert!(!log.contains("legacy.toml"));
        assert!(!log.contains("4096"));
    }
}
