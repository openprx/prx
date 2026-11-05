//! Owned observation and structured prediction contracts.
use crate::decision::{CandidateKey, DecisionAnswer, QuestionId};
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
/// Private fields enforce bounds before an outcome enters a queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    decision_id: Option<String>,
    tools: Vec<String>,
    succeeded: bool,
}
impl TurnOutcome {
    pub fn new(decision_id: Option<String>, tools: Vec<String>, succeeded: bool) -> Result<Self, EngineError> {
        if tools.len() > 64
            || tools.iter().any(|s| s.len() > 128)
            || decision_id.as_ref().is_some_and(|s| s.len() > 128)
        {
            return Err(EngineError::OutcomeBounds);
        }
        Ok(Self {
            decision_id,
            tools,
            succeeded,
        })
    }
    pub fn decision_id(&self) -> Option<&str> {
        self.decision_id.as_deref()
    }
    pub fn tools(&self) -> &[String] {
        &self.tools
    }
    pub fn succeeded(&self) -> bool {
        self.succeeded
    }
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
