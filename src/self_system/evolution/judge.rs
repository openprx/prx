use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::hash::{Hash, Hasher};

const MIN_PREV_BASELINE: f64 = 0.01;

/// Runtime configuration for Judge scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeConfig {
    pub model: String,
    pub temperature: f64,
    pub human_sample_rate: f64,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            model: "mock-judge-v1".to_string(),
            temperature: 0.1,
            human_sample_rate: 0.1,
        }
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
            results
                .iter()
                .map(|item| item.scores.overall())
                .sum::<f64>()
                / results.len() as f64
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
        let tail = &values[values.len() - 4..];

        let a = relative_shift(tail[0], tail[1]);
        let b = relative_shift(tail[1], tail[2]);
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
    async fn score(
        &self,
        task_description: &str,
        execution_result: &str,
        config: &JudgeConfig,
    ) -> Result<String>;
}

/// Mock scoring implementation with deterministic heuristics.
pub struct MockJudgeModel;

#[async_trait]
impl JudgeScoringModel for MockJudgeModel {
    async fn score(
        &self,
        task_description: &str,
        execution_result: &str,
        _config: &JudgeConfig,
    ) -> Result<String> {
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

/// Orchestrates judge model calls and health tracking.
pub struct JudgeEngine<M: JudgeScoringModel> {
    config: JudgeConfig,
    model: M,
    health_monitor: JudgeHealthMonitor,
}

impl<M: JudgeScoringModel> JudgeEngine<M> {
    pub fn new(config: JudgeConfig, model: M) -> Self {
        Self {
            config,
            model,
            health_monitor: JudgeHealthMonitor::new(),
        }
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

        let scores = parse_scores(&raw_output)
            .with_context(|| "judge output does not satisfy StructuredScores schema")?;

        Ok(JudgeResult {
            experiment_id: experiment_id.to_string(),
            task_id: task_id.to_string(),
            scores,
            raw_output,
            needs_human_review: should_sample_human_review(
                experiment_id,
                task_id,
                self.config.human_sample_rate,
            ),
        })
    }

    pub fn record_round_and_check_drift(&mut self, results: &[JudgeResult]) -> JudgeHealthReport {
        self.health_monitor.record_round(results)
    }
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
            MockJudgeModel,
        );

        let result = engine
            .judge_task(
                "exp-1",
                "task-1",
                "Complete task",
                "success with safe execution",
            )
            .await
            .unwrap();

        assert!(result.needs_human_review);
        assert!(result.scores.task_completion >= 0.0);
        assert!(result.scores.task_completion <= 1.0);
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
