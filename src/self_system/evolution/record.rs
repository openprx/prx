use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Memory operation type captured in [`MemoryAccessLog`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    Read,
    Write,
    Update,
    Delete,
    Search,
}

/// Task category for memory/decision/evolution logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Chat,
    ToolCall,
    Planning,
    Evaluation,
    Recovery,
    Other,
}

/// Actor identity for a log entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Agent,
    System,
    Tool,
}

/// Annotation source for usefulness marks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationSource {
    UserFeedback,
    AutoEvaluator,
    HumanReview,
}

/// Decision category for [`DecisionLog`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionType {
    MemoryPolicy,
    ToolSelection,
    RuntimePolicy,
    SafetyPolicy,
    Other,
}

/// Outcome for a decision execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
    Partial,
    RolledBack,
}

/// Evolution layer impacted by a change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionLayer {
    Prompt,
    Policy,
    Tooling,
    Memory,
    Runtime,
}

/// Change operation type captured in [`EvolutionLog`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    Add,
    Update,
    Remove,
    Tune,
    Rollback,
}

/// Result of an evolution change after validation window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionResult {
    Improved,
    Neutral,
    Regressed,
    Rejected,
}

/// Statistical basis for an evolution change proposal.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DataBasis {
    pub sample_count: u32,
    pub time_range_days: u32,
    pub key_metrics: HashMap<String, f64>,
    pub patterns_found: Vec<String>,
}

/// Memory access event record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryAccessLog {
    pub timestamp: String,
    pub experiment_id: String,
    pub trace_id: String,
    pub action: MemoryAction,
    pub memory_id: String,
    pub task_context: String,
    pub task_type: TaskType,
    pub actor: Actor,
    pub was_useful: Option<bool>,
    pub useful_annotation_source: Option<AnnotationSource>,
    pub annotation_confidence: Option<f64>,
    pub tokens_consumed: u32,
}

/// Decision execution record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionLog {
    pub timestamp: String,
    pub experiment_id: String,
    pub trace_id: String,
    pub decision_type: DecisionType,
    pub task_type: TaskType,
    pub risk_level: u8,
    pub actor: Actor,
    pub input_context: String,
    pub action_taken: String,
    pub outcome: Outcome,
    pub tokens_used: u32,
    pub latency_ms: u64,
    pub user_correction: Option<String>,
    pub config_snapshot_hash: String,
}

impl DecisionLog {
    /// Normalize oversized fields to storage constraints.
    pub fn normalize_for_storage(&mut self) {
        self.input_context = truncate_to_chars(&self.input_context, 500);
    }

    /// Build a [`DecisionLog`] and enforce storage constraints.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        timestamp: String,
        experiment_id: String,
        trace_id: String,
        decision_type: DecisionType,
        task_type: TaskType,
        risk_level: u8,
        actor: Actor,
        input_context: String,
        action_taken: String,
        outcome: Outcome,
        tokens_used: u32,
        latency_ms: u64,
        user_correction: Option<String>,
        config_snapshot_hash: String,
    ) -> Self {
        let mut log = Self {
            timestamp,
            experiment_id,
            trace_id,
            decision_type,
            task_type,
            risk_level,
            actor,
            input_context,
            action_taken,
            outcome,
            tokens_used,
            latency_ms,
            user_correction,
            config_snapshot_hash,
        };
        log.normalize_for_storage();
        log
    }
}

/// Evolution change record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionLog {
    pub experiment_id: String,
    pub timestamp: String,
    pub layer: EvolutionLayer,
    pub change_type: ChangeType,
    pub before_value: String,
    pub after_value: String,
    pub trigger_reason: String,
    pub data_basis: DataBasis,
    pub result: Option<EvolutionResult>,
}

fn truncate_to_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

impl Default for MemoryAccessLog {
    fn default() -> Self {
        Self {
            timestamp: String::new(),
            experiment_id: String::new(),
            trace_id: String::new(),
            action: MemoryAction::Read,
            memory_id: String::new(),
            task_context: String::new(),
            task_type: TaskType::Other,
            actor: Actor::System,
            was_useful: None,
            useful_annotation_source: None,
            annotation_confidence: None,
            tokens_consumed: 0,
        }
    }
}

impl Default for DecisionLog {
    fn default() -> Self {
        Self {
            timestamp: String::new(),
            experiment_id: String::new(),
            trace_id: String::new(),
            decision_type: DecisionType::Other,
            task_type: TaskType::Other,
            risk_level: 0,
            actor: Actor::System,
            input_context: String::new(),
            action_taken: String::new(),
            outcome: Outcome::Partial,
            tokens_used: 0,
            latency_ms: 0,
            user_correction: None,
            config_snapshot_hash: String::new(),
        }
    }
}

impl Default for EvolutionLog {
    fn default() -> Self {
        Self {
            experiment_id: String::new(),
            timestamp: String::new(),
            layer: EvolutionLayer::Runtime,
            change_type: ChangeType::Update,
            before_value: String::new(),
            after_value: String::new(),
            trigger_reason: String::new(),
            data_basis: DataBasis::default(),
            result: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_log_normalize_truncates_input_context() {
        let input = "a".repeat(700);
        let mut log = DecisionLog {
            timestamp: "2026-02-24T00:00:00Z".into(),
            experiment_id: "exp".into(),
            trace_id: "trace".into(),
            decision_type: DecisionType::RuntimePolicy,
            task_type: TaskType::Planning,
            risk_level: 2,
            actor: Actor::Agent,
            input_context: input,
            action_taken: "apply".into(),
            outcome: Outcome::Success,
            tokens_used: 128,
            latency_ms: 50,
            user_correction: None,
            config_snapshot_hash: "hash".into(),
        };

        log.normalize_for_storage();
        assert_eq!(log.input_context.chars().count(), 500);
    }

    #[test]
    fn decision_log_new_applies_storage_limits() {
        let input = "x".repeat(501);
        let log = DecisionLog::new(
            "2026-02-24T00:00:00Z".into(),
            "exp".into(),
            "trace".into(),
            DecisionType::ToolSelection,
            TaskType::ToolCall,
            1,
            Actor::Agent,
            input,
            "run".into(),
            Outcome::Partial,
            100,
            10,
            None,
            "hash".into(),
        );
        assert_eq!(log.input_context.chars().count(), 500);
    }

    #[test]
    fn memory_access_log_parses_legacy_json_with_missing_fields() {
        let raw = r#"{"timestamp":"2026-02-24T00:00:00Z","memory_id":"m1"}"#;
        let parsed = serde_json::from_str::<MemoryAccessLog>(raw).unwrap();
        assert_eq!(parsed.timestamp, "2026-02-24T00:00:00Z");
        assert_eq!(parsed.memory_id, "m1");
        assert_eq!(parsed.action, MemoryAction::Read);
        assert_eq!(parsed.task_type, TaskType::Other);
        assert_eq!(parsed.tokens_consumed, 0);
    }

    #[test]
    fn decision_log_parses_legacy_json_with_missing_fields() {
        let raw = r#"{"timestamp":"2026-02-24T00:00:00Z","action_taken":"noop"}"#;
        let parsed = serde_json::from_str::<DecisionLog>(raw).unwrap();
        assert_eq!(parsed.timestamp, "2026-02-24T00:00:00Z");
        assert_eq!(parsed.action_taken, "noop");
        assert_eq!(parsed.decision_type, DecisionType::Other);
        assert_eq!(parsed.outcome, Outcome::Partial);
        assert_eq!(parsed.tokens_used, 0);
    }

    #[test]
    fn evolution_log_parses_legacy_json_with_missing_fields() {
        let raw = r#"{"timestamp":"2026-02-24T00:00:00Z","trigger_reason":"legacy"}"#;
        let parsed = serde_json::from_str::<EvolutionLog>(raw).unwrap();
        assert_eq!(parsed.timestamp, "2026-02-24T00:00:00Z");
        assert_eq!(parsed.trigger_reason, "legacy");
        assert_eq!(parsed.layer, EvolutionLayer::Runtime);
        assert_eq!(parsed.change_type, ChangeType::Update);
        assert_eq!(parsed.data_basis.sample_count, 0);
    }
}
