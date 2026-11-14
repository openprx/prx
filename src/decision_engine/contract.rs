//! Owned observation and structured prediction contracts.
use crate::decision_engine::prediction::{CandidateKey, DecisionAnswer, QuestionId};
use std::collections::BTreeMap;

pub struct IntentInput<'a> {
    pub decision_id: &'a str,
    pub answers: &'a BTreeMap<QuestionId, DecisionAnswer>,
    pub candidates: &'a [CandidateKey],
}
#[derive(Debug, Clone, PartialEq)]
pub enum IntentValue {
    Choice {
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        distribution: Vec<f64>,
        expected: f64,
        confidence: f64,
    },
    Probability(f64),
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntentAnalysis {
    pub decision_id: String,
    pub global: BTreeMap<String, IntentValue>,
    pub candidates: BTreeMap<String, BTreeMap<String, IntentValue>>,
}
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("outcome exceeds the owned observation bounds")]
    OutcomeBounds,
}
/// How a turn ended, as recorded in engine telemetry (P2-R10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnResultClass {
    Completed,
    Failed,
    Cancelled,
    Refused,
}
impl TurnResultClass {
    /// The stored `result` column value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Refused => "refused",
        }
    }
    /// Parse a stored `result` column value.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "refused" => Some(Self::Refused),
            _ => None,
        }
    }
}
/// The bounds every telemetry identifier and tool list obeys.
pub const MAX_DECISION_ID_BYTES: usize = 128;
pub const MAX_OUTCOME_TOOLS: usize = 64;
pub const MAX_TOOL_NAME_BYTES: usize = 128;
/// The largest stored redacted brief and analysis values document.
pub const MAX_ANALYSIS_TEXT_BYTES: usize = 16 * 1024;
/// Private fields enforce bounds before an outcome enters a queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    decision_id: Option<String>,
    tools: Vec<String>,
    result: TurnResultClass,
    cost_micro_usd: Option<u64>,
    duration_ms: u64,
}
impl TurnOutcome {
    pub fn new(
        decision_id: Option<String>,
        tools: Vec<String>,
        result: TurnResultClass,
        cost_micro_usd: Option<u64>,
        duration_ms: u64,
    ) -> Result<Self, EngineError> {
        if tools.len() > MAX_OUTCOME_TOOLS
            || tools.iter().any(|s| s.len() > MAX_TOOL_NAME_BYTES)
            || decision_id
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > MAX_DECISION_ID_BYTES)
            || i64::try_from(duration_ms).is_err()
            || cost_micro_usd.is_some_and(|cost| i64::try_from(cost).is_err())
        {
            return Err(EngineError::OutcomeBounds);
        }
        Ok(Self {
            decision_id,
            tools,
            result,
            cost_micro_usd,
            duration_ms,
        })
    }
    pub fn decision_id(&self) -> Option<&str> {
        self.decision_id.as_deref()
    }
    pub fn tools(&self) -> &[String] {
        &self.tools
    }
    pub const fn result(&self) -> TurnResultClass {
        self.result
    }
    pub const fn cost_micro_usd(&self) -> Option<u64> {
        self.cost_micro_usd
    }
    pub const fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
}
/// One analysis whose brief was sent to the serving model (P2-R10). It holds
/// the redacted brief and typed values only: never user input or backend
/// free text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisRecord {
    decision_id: String,
    created_at_ms: i64,
    brief_redacted: String,
    brief_sha256: String,
    values_json: String,
}
impl AnalysisRecord {
    /// Build the record of `analysis` and the exact redacted `brief` sent,
    /// keeping at most `max_candidates` candidates.
    pub fn new(
        analysis: &IntentAnalysis,
        brief_redacted: String,
        created_at_ms: i64,
        max_candidates: usize,
    ) -> Result<Self, EngineError> {
        use sha2::Digest as _;
        if analysis.decision_id.is_empty()
            || analysis.decision_id.len() > MAX_DECISION_ID_BYTES
            || brief_redacted.len() > MAX_ANALYSIS_TEXT_BYTES
        {
            return Err(EngineError::OutcomeBounds);
        }
        let values_json = serde_json::to_string(&analysis_values(analysis, max_candidates))
            .map_err(|_| EngineError::OutcomeBounds)?;
        if values_json.len() > MAX_ANALYSIS_TEXT_BYTES {
            return Err(EngineError::OutcomeBounds);
        }
        let brief_sha256 = format!("{:x}", sha2::Sha256::digest(brief_redacted.as_bytes()));
        Ok(Self {
            decision_id: analysis.decision_id.clone(),
            created_at_ms,
            brief_redacted,
            brief_sha256,
            values_json,
        })
    }
    /// Rebuild a stored row. The store's CHECK constraints bound the columns.
    pub(crate) const fn from_row(
        decision_id: String,
        created_at_ms: i64,
        brief_redacted: String,
        brief_sha256: String,
        values_json: String,
    ) -> Self {
        Self {
            decision_id,
            created_at_ms,
            brief_redacted,
            brief_sha256,
            values_json,
        }
    }
    pub fn decision_id(&self) -> &str {
        &self.decision_id
    }
    pub const fn created_at_ms(&self) -> i64 {
        self.created_at_ms
    }
    pub fn brief_redacted(&self) -> &str {
        &self.brief_redacted
    }
    pub fn brief_sha256(&self) -> &str {
        &self.brief_sha256
    }
    pub fn values_json(&self) -> &str {
        &self.values_json
    }
}
/// `{"global": {question: value}, "candidates": [{"candidate", "values"}]}`.
fn analysis_values(analysis: &IntentAnalysis, max_candidates: usize) -> serde_json::Value {
    let global: serde_json::Map<String, serde_json::Value> = analysis
        .global
        .iter()
        .map(|(question, value)| (question.clone(), intent_value_json(value)))
        .collect();
    let candidates: Vec<serde_json::Value> = analysis
        .candidates
        .iter()
        .take(max_candidates)
        .map(|(candidate, values)| {
            let values: serde_json::Map<String, serde_json::Value> = values
                .iter()
                .map(|(question, value)| (question.clone(), intent_value_json(value)))
                .collect();
            serde_json::json!({ "candidate": candidate, "values": values })
        })
        .collect();
    serde_json::json!({ "global": global, "candidates": candidates })
}
fn intent_value_json(value: &IntentValue) -> serde_json::Value {
    match value {
        IntentValue::Choice {
            probabilities,
            confidence,
        } => serde_json::json!({ "choice": probabilities, "confidence": confidence }),
        IntentValue::Score {
            distribution,
            expected,
            confidence,
        } => serde_json::json!({ "distribution": distribution, "expected": expected, "confidence": confidence }),
        IntentValue::Probability(probability) => serde_json::json!({ "probability": probability }),
    }
}
/// What the telemetry worker persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryRecord {
    Analysis(AnalysisRecord),
    Outcome(TurnOutcome),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveAdmission {
    Accepted,
    Dropped,
    Disabled,
}
#[async_trait::async_trait]
pub trait DecisionEngine: Send + Sync {
    async fn analyze(&self, input: IntentInput<'_>) -> Result<IntentAnalysis, EngineError>;
    fn observe(&self, outcome: TurnOutcome) -> ObserveAdmission;
    fn identity(&self) -> &'static str;
}
