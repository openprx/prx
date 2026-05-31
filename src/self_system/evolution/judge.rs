use crate::providers::traits::Provider;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MIN_PREV_BASELINE: f64 = 0.01;
/// Default minimum overall score required for a judged cycle to pass.
pub const DEFAULT_JUDGE_PASS_THRESHOLD: f64 = 0.6;

/// Runtime configuration for Judge scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeConfig {
    pub model: String,
    pub temperature: f64,
    pub human_sample_rate: f64,
    /// Minimum overall score for a cycle to be considered a pass.
    /// Configurable so the 0.6 cutoff is no longer hard-coded at call sites.
    #[serde(default = "default_pass_threshold")]
    pub pass_threshold: f64,
}

const fn default_pass_threshold() -> f64 {
    DEFAULT_JUDGE_PASS_THRESHOLD
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            model: "mock-judge-v1".to_string(),
            temperature: 0.1,
            human_sample_rate: 0.1,
            pass_threshold: DEFAULT_JUDGE_PASS_THRESHOLD,
        }
    }
}

impl JudgeConfig {
    /// Clamp the configured pass threshold into the valid `[0, 1]` range.
    pub const fn effective_pass_threshold(&self) -> f64 {
        self.pass_threshold.clamp(0.0, 1.0)
    }
}

/// Strict structured score payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredScores {
    pub task_completion: f64,
    pub correctness: f64,
    pub efficiency: f64,
    pub safety_compliance: f64,
    pub reasoning: String,
}

impl StructuredScores {
    pub fn validate(&self) -> Result<()> {
        ensure_score_range("task_completion", self.task_completion)?;
        ensure_score_range("correctness", self.correctness)?;
        ensure_score_range("efficiency", self.efficiency)?;
        ensure_score_range("safety_compliance", self.safety_compliance)?;
        if self.reasoning.trim().is_empty() {
            bail!("reasoning must not be empty");
        }
        Ok(())
    }

    pub fn overall(&self) -> f64 {
        (self.task_completion + self.correctness + self.efficiency + self.safety_compliance) / 4.0
    }
}

/// Judge invocation output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeResult {
    pub experiment_id: String,
    pub task_id: String,
    pub scores: StructuredScores,
    pub raw_output: String,
    pub needs_human_review: bool,
}

/// Rolling Judge drift alert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeDriftAlert {
    pub message: String,
    pub recent_round_means: Vec<f64>,
}

/// Health snapshot after recording a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeHealthReport {
    pub score_distribution: BTreeMap<String, u32>,
    pub round_mean: f64,
    pub drift_alert: Option<JudgeDriftAlert>,
}

/// Maintains recent judge rounds and reports drift signals.
#[derive(Debug, Default)]
pub struct JudgeHealthMonitor {
    recent_round_means: VecDeque<f64>,
}

impl JudgeHealthMonitor {
    pub fn new() -> Self {
        Self {
            recent_round_means: VecDeque::with_capacity(8),
        }
    }

    pub fn record_round(&mut self, results: &[JudgeResult]) -> JudgeHealthReport {
        let round_mean = if results.is_empty() {
            0.0
        } else {
            results.iter().map(|item| item.scores.overall()).sum::<f64>() / results.len() as f64
        };

        if self.recent_round_means.len() >= 8 {
            self.recent_round_means.pop_front();
        }
        self.recent_round_means.push_back(round_mean);

        let distribution = build_distribution(results);
        let drift_alert = self.detect_drift();

        JudgeHealthReport {
            score_distribution: distribution,
            round_mean,
            drift_alert,
        }
    }

    fn detect_drift(&self) -> Option<JudgeDriftAlert> {
        if self.recent_round_means.len() < 4 {
            return None;
        }
        let values = self.recent_round_means.iter().copied().collect::<Vec<_>>();
        // SAFETY: values.len() >= 4 (checked above), so values.len()-4 is valid,
        // and tail has exactly 4 elements so tail[0..3] are always valid.
        #[allow(clippy::indexing_slicing)]
        let tail = &values[values.len() - 4..];

        #[allow(clippy::indexing_slicing)]
        let a = relative_shift(tail[0], tail[1]);
        #[allow(clippy::indexing_slicing)]
        let b = relative_shift(tail[1], tail[2]);
        #[allow(clippy::indexing_slicing)]
        let c = relative_shift(tail[2], tail[3]);
        if a > 0.15 && b > 0.15 && c > 0.15 {
            return Some(JudgeDriftAlert {
                message: "judge score drift exceeded 15% for 3 consecutive rounds".to_string(),
                recent_round_means: tail.to_vec(),
            });
        }
        None
    }
}

fn build_distribution(results: &[JudgeResult]) -> BTreeMap<String, u32> {
    let mut out: BTreeMap<String, u32> = BTreeMap::new();
    for item in results {
        let bucket = score_bucket(item.scores.overall());
        *out.entry(bucket).or_insert(0) += 1;
    }
    out
}

fn score_bucket(value: f64) -> String {
    if value < 0.2 {
        "0.0-0.2".to_string()
    } else if value < 0.4 {
        "0.2-0.4".to_string()
    } else if value < 0.6 {
        "0.4-0.6".to_string()
    } else if value < 0.8 {
        "0.6-0.8".to_string()
    } else {
        "0.8-1.0".to_string()
    }
}

/// LLM scoring interface. Current default can use mock implementation.
#[async_trait]
pub trait JudgeScoringModel: Send + Sync {
    async fn score(&self, task_description: &str, execution_result: &str, config: &JudgeConfig) -> Result<String>;
}

/// Mock scoring implementation with deterministic heuristics.
pub struct MockJudgeModel;

#[async_trait]
impl JudgeScoringModel for MockJudgeModel {
    async fn score(&self, task_description: &str, execution_result: &str, _config: &JudgeConfig) -> Result<String> {
        let text = format!(
            "{} {}",
            task_description.to_ascii_lowercase(),
            execution_result.to_ascii_lowercase()
        );

        let success_hint = text.contains("success") || text.contains("done");
        let safety_hint = !text.contains("unsafe") && !text.contains("violation");
        let efficiency_hint = !text.contains("slow") && !text.contains("timeout");

        let scores = StructuredScores {
            task_completion: if success_hint { 0.9 } else { 0.5 },
            correctness: if success_hint { 0.85 } else { 0.45 },
            efficiency: if efficiency_hint { 0.8 } else { 0.4 },
            safety_compliance: if safety_hint { 0.9 } else { 0.2 },
            reasoning: "mock-judge heuristic scoring".to_string(),
        };
        Ok(serde_json::to_string(&scores)?)
    }
}

/// Real LLM-backed scoring implementation.
///
/// Calls a configured [`Provider`] to obtain a structured score. The model is asked to
/// reply with the strict `StructuredScores` JSON schema. Provider/credential access is
/// owned by the caller that constructs this judge; the evolution subsystem never reaches
/// into the memory layer for it.
pub struct ModelJudge {
    provider: Arc<dyn Provider>,
}

impl ModelJudge {
    pub const fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }

    fn build_prompt(task_description: &str, execution_result: &str) -> String {
        format!(
            "You are an evolution proposal verifier. Score the executed change strictly.\n\
             Reply with ONLY a JSON object matching this schema (all scores in [0,1]):\n\
             {{\"task_completion\":<f64>,\"correctness\":<f64>,\"efficiency\":<f64>,\
             \"safety_compliance\":<f64>,\"reasoning\":\"<non-empty>\"}}\n\
             Task: {task_description}\n\
             Execution result: {execution_result}\n"
        )
    }

    fn extract_json(raw: &str) -> &str {
        // Models may wrap JSON in prose or code fences; isolate the outermost object.
        match (raw.find('{'), raw.rfind('}')) {
            (Some(start), Some(end)) if end >= start => &raw[start..=end],
            _ => raw,
        }
    }
}

#[async_trait]
impl JudgeScoringModel for ModelJudge {
    async fn score(&self, task_description: &str, execution_result: &str, config: &JudgeConfig) -> Result<String> {
        let prompt = Self::build_prompt(task_description, execution_result);
        let raw = self
            .provider
            .simple_chat(&prompt, &config.model, config.temperature)
            .await
            .context("model judge provider call failed")?;
        Ok(Self::extract_json(&raw).to_string())
    }
}

/// Orchestrates judge model calls and health tracking.
///
/// Holds the scoring model behind `Arc<dyn JudgeScoringModel>` so the implementation can
/// be swapped (mock / real model) at runtime without changing the engine type.
pub struct JudgeEngine {
    config: JudgeConfig,
    model: Arc<dyn JudgeScoringModel>,
    review_queue_path: Option<PathBuf>,
    health_monitor: JudgeHealthMonitor,
}

impl JudgeEngine {
    pub fn new(config: JudgeConfig, model: Arc<dyn JudgeScoringModel>) -> Self {
        Self {
            config,
            model,
            review_queue_path: None,
            health_monitor: JudgeHealthMonitor::new(),
        }
    }

    /// Set the destination file for human-review queue records.
    pub fn with_review_queue(mut self, path: impl Into<PathBuf>) -> Self {
        self.review_queue_path = Some(path.into());
        self
    }

    /// Effective (clamped) pass threshold for downstream rollback decisions.
    pub const fn pass_threshold(&self) -> f64 {
        self.config.effective_pass_threshold()
    }

    pub async fn judge_task(
        &self,
        experiment_id: &str,
        task_id: &str,
        task_description: &str,
        execution_result: &str,
    ) -> Result<JudgeResult> {
        let raw_output = self
            .model
            .score(task_description, execution_result, &self.config)
            .await?;

        let scores =
            parse_scores(&raw_output).with_context(|| "judge output does not satisfy StructuredScores schema")?;

        let needs_human_review = should_sample_human_review(experiment_id, task_id, self.config.human_sample_rate)
            || scores.overall() < self.config.effective_pass_threshold();

        let result = JudgeResult {
            experiment_id: experiment_id.to_string(),
            task_id: task_id.to_string(),
            scores,
            raw_output,
            needs_human_review,
        };

        if needs_human_review {
            if let Err(err) = self.append_review_queue(&result).await {
                tracing::warn!(
                    error = %err,
                    experiment_id = %result.experiment_id,
                    "failed to append judge human-review queue record"
                );
            }
        }

        Ok(result)
    }

    /// Append a compact record to `judge_review_queue.jsonl` for later human triage.
    ///
    /// Persists only score metadata and experiment/task identifiers — never the raw
    /// model output — to avoid leaking content into a shared queue file.
    async fn append_review_queue(&self, result: &JudgeResult) -> Result<()> {
        let Some(path) = self.review_queue_path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let record = JudgeReviewQueueRecord {
            experiment_id: result.experiment_id.clone(),
            task_id: result.task_id.clone(),
            overall_score: result.scores.overall(),
            task_completion: result.scores.task_completion,
            correctness: result.scores.correctness,
            efficiency: result.scores.efficiency,
            safety_compliance: result.scores.safety_compliance,
            pass_threshold: self.config.effective_pass_threshold(),
            queued_at: chrono::Utc::now().to_rfc3339(),
        };
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        append_line(path, &line).await
    }

    pub fn record_round_and_check_drift(&mut self, results: &[JudgeResult]) -> JudgeHealthReport {
        self.health_monitor.record_round(results)
    }
}

/// Append-only record for the human-review queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeReviewQueueRecord {
    pub experiment_id: String,
    pub task_id: String,
    pub overall_score: f64,
    pub task_completion: f64,
    pub correctness: f64,
    pub efficiency: f64,
    pub safety_compliance: f64,
    pub pass_threshold: f64,
    pub queued_at: String,
}

async fn append_line(path: &Path, line: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("failed to open judge review queue: {}", path.display()))?;
    file.write_all(line.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

fn parse_scores(raw_output: &str) -> Result<StructuredScores> {
    let scores = serde_json::from_str::<StructuredScores>(raw_output)?;
    scores.validate()?;
    Ok(scores)
}

fn ensure_score_range(name: &str, score: f64) -> Result<()> {
    if !(0.0..=1.0).contains(&score) {
        bail!("{name} must be within [0, 1], got {score}");
    }
    Ok(())
}

fn should_sample_human_review(experiment_id: &str, task_id: &str, sample_rate: f64) -> bool {
    let sample_rate = sample_rate.clamp(0.0, 1.0);
    if sample_rate <= 0.0 {
        return false;
    }
    if sample_rate >= 1.0 {
        return true;
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    experiment_id.hash(&mut hasher);
    task_id.hash(&mut hasher);
    let sample = (hasher.finish() % 10_000) as f64 / 10_000.0;
    sample < sample_rate
}

fn relative_shift(previous: f64, current: f64) -> f64 {
    (current - previous).abs() / previous.abs().max(MIN_PREV_BASELINE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_scores_validation_enforces_range_and_reasoning() {
        let good = StructuredScores {
            task_completion: 0.9,
            correctness: 0.9,
            efficiency: 0.8,
            safety_compliance: 1.0,
            reasoning: "ok".into(),
        };
        assert!(good.validate().is_ok());

        let bad = StructuredScores {
            task_completion: 1.2,
            correctness: 0.9,
            efficiency: 0.8,
            safety_compliance: 1.0,
            reasoning: "ok".into(),
        };
        assert!(bad.validate().is_err());
    }

    #[tokio::test]
    async fn judge_engine_returns_structured_result_with_sampling_mark() {
        let engine = JudgeEngine::new(
            JudgeConfig {
                human_sample_rate: 1.0,
                ..JudgeConfig::default()
            },
            Arc::new(MockJudgeModel),
        );

        let result = engine
            .judge_task("exp-1", "task-1", "Complete task", "success with safe execution")
            .await
            .unwrap();

        assert!(result.needs_human_review);
        assert!(result.scores.task_completion >= 0.0);
        assert!(result.scores.task_completion <= 1.0);
    }

    #[test]
    fn judge_config_pass_threshold_is_configurable_and_clamped() {
        // Default keeps the historical 0.6 cutoff.
        assert_eq!(
            JudgeConfig::default().effective_pass_threshold(),
            DEFAULT_JUDGE_PASS_THRESHOLD
        );

        // Custom in-range value is honored.
        let cfg = JudgeConfig {
            pass_threshold: 0.8,
            ..JudgeConfig::default()
        };
        assert_eq!(cfg.effective_pass_threshold(), 0.8);

        // Out-of-range values are clamped into [0, 1].
        let low = JudgeConfig {
            pass_threshold: -1.0,
            ..JudgeConfig::default()
        };
        assert_eq!(low.effective_pass_threshold(), 0.0);
        let high = JudgeConfig {
            pass_threshold: 5.0,
            ..JudgeConfig::default()
        };
        assert_eq!(high.effective_pass_threshold(), 1.0);

        // pass_threshold is parseable from config and defaults when absent.
        let parsed: JudgeConfig =
            serde_json::from_str(r#"{"model":"m","temperature":0.1,"human_sample_rate":0.0}"#).unwrap();
        assert_eq!(parsed.pass_threshold, DEFAULT_JUDGE_PASS_THRESHOLD);
        let parsed2: JudgeConfig =
            serde_json::from_str(r#"{"model":"m","temperature":0.1,"human_sample_rate":0.0,"pass_threshold":0.42}"#)
                .unwrap();
        assert_eq!(parsed2.pass_threshold, 0.42);
    }

    struct StubProvider;
    #[async_trait]
    impl Provider for StubProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(r#"{"task_completion":0.9,"correctness":0.9,"efficiency":0.8,"safety_compliance":0.95,"reasoning":"ok"}"#.to_string())
        }
    }

    struct WrappingProvider;
    #[async_trait]
    impl Provider for WrappingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("```json\n{\"task_completion\":0.7,\"correctness\":0.7,\"efficiency\":0.7,\"safety_compliance\":0.7,\"reasoning\":\"r\"}\n```".to_string())
        }
    }

    #[test]
    fn model_judge_can_be_instantiated_as_scoring_model() {
        // Construction must yield a usable `Arc<dyn JudgeScoringModel>`.
        let model: Arc<dyn JudgeScoringModel> = Arc::new(ModelJudge::new(Arc::new(StubProvider)));
        let _engine = JudgeEngine::new(JudgeConfig::default(), model);
    }

    #[tokio::test]
    async fn model_judge_scores_via_provider_and_extracts_json() {
        let engine = JudgeEngine::new(
            JudgeConfig::default(),
            Arc::new(ModelJudge::new(Arc::new(WrappingProvider))),
        );
        let result = engine.judge_task("e", "t", "desc", "exec").await.unwrap();
        assert!((result.scores.overall() - 0.7).abs() < 1e-9);
    }

    #[tokio::test]
    async fn low_score_triggers_review_and_appends_queue_file() {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("judge_review_queue.jsonl");
        // human_sample_rate=0 so only the threshold path can flag review.
        let engine = JudgeEngine::new(
            JudgeConfig {
                human_sample_rate: 0.0,
                pass_threshold: 0.9,
                ..JudgeConfig::default()
            },
            Arc::new(MockJudgeModel),
        )
        .with_review_queue(&queue);

        // "slow"/"timeout"/"unsafe" absent but no success hint => mid scores < 0.9 threshold.
        let result = engine
            .judge_task("exp-q", "task-q", "do work", "result pending")
            .await
            .unwrap();
        assert!(result.needs_human_review);
        let written = tokio::fs::read_to_string(&queue).await.unwrap();
        assert!(written.contains("\"experiment_id\":\"exp-q\""));
        assert!(written.contains("\"pass_threshold\":0.9"));
    }

    #[test]
    fn health_monitor_detects_three_round_consecutive_drift() {
        let mk = |score: f64| JudgeResult {
            experiment_id: "exp".into(),
            task_id: "task".into(),
            scores: StructuredScores {
                task_completion: score,
                correctness: score,
                efficiency: score,
                safety_compliance: score,
                reasoning: "ok".into(),
            },
            raw_output: "{}".into(),
            needs_human_review: false,
        };

        let mut monitor = JudgeHealthMonitor::new();
        let _ = monitor.record_round(&[mk(0.20)]);
        let _ = monitor.record_round(&[mk(0.35)]);
        let _ = monitor.record_round(&[mk(0.55)]);
        let report = monitor.record_round(&[mk(0.80)]);
        assert!(report.drift_alert.is_some());
    }

    #[test]
    fn parse_scores_rejects_invalid_schema() {
        let err = parse_scores("{\"task_completion\":2.0}").unwrap_err();
        assert!(err.to_string().contains("missing field") || err.to_string().contains("within"));
    }

    #[test]
    fn sample_rate_bounds_are_respected() {
        assert!(!should_sample_human_review("e", "t", 0.0));
        assert!(should_sample_human_review("e", "t", 1.0));
    }
}
