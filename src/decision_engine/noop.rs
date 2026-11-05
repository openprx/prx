//! Zero-cost disabled engine.
use super::contract::*;
#[derive(Debug, Default)]
pub struct NoopEngine;
#[async_trait::async_trait]
impl DecisionEngine for NoopEngine {
    async fn analyze(&self, _: IntentInput<'_>) -> Result<IntentAnalysis, EngineError> {
        Ok(IntentAnalysis::default())
    }
    fn observe(&self, _: TurnOutcome) -> ObserveAdmission {
        ObserveAdmission::Disabled
    }
    fn identity(&self) -> &'static str {
        "noop"
    }
}
