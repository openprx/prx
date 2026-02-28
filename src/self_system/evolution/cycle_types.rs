use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ChangeTarget {
    ConfigFile { path: String },
    CronFile { path: String },
    WorkspaceFile { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum ChangeOperation {
    Append { content: String },
    Replace { from: String, to: String },
    Write { content: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionProposal {
    pub id: String,
    pub summary: String,
    pub rationale: String,
    pub risk_level: RiskLevel,
    pub target: ChangeTarget,
    pub operation: ChangeOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionSignals {
    pub memory_count: usize,
    pub health_components: usize,
    pub health_error_components: usize,
    pub cron_runs: usize,
    pub cron_failure_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessTrend {
    pub window: usize,
    pub previous_average: f64,
    pub latest_score: f64,
    pub is_declining: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    Improved,
    Unchanged,
    Regressed,
    Skipped,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CycleOutcome {
    Applied,
    Paused,
    Halted,
    NoAction,
    ApprovalRequired,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionValidation {
    pub status: ValidationStatus,
    pub before_score: f64,
    pub after_score: f64,
    pub delta: f64,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionCycle {
    pub id: String,
    pub started_at: String,
    pub finished_at: String,
    pub signals: EvolutionSignals,
    pub trend: FitnessTrend,
    pub proposal: Option<EvolutionProposal>,
    pub validation: EvolutionValidation,
    pub outcome: CycleOutcome,
    pub alert: Option<String>,
    pub errors: Vec<String>,
}
