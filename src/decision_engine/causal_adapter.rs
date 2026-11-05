//! Copy typed prediction answers without selecting an action or issuing a directive.
use super::{contract::*, observe::ObserveQueue};
use crate::decision::{CandidateQuestion, DecisionAnswer, GlobalQuestion, QuestionId};
pub struct CausalEngine {
    pub observations: ObserveQueue,
}
#[async_trait::async_trait]
impl DecisionEngine for CausalEngine {
    async fn analyze(&self, input: IntentInput<'_>) -> Result<IntentAnalysis, EngineError> {
        let mut analysis = IntentAnalysis {
            decision_id: input.decision_id.into(),
            ..Default::default()
        };
        for q in GlobalQuestion::ALL {
            if let Some(answer) = input.answers.get(&QuestionId::global(q)) {
                analysis.global.insert(q.as_str().into(), map(answer));
            }
        }
        for candidate in input.candidates {
            let mut answers = std::collections::BTreeMap::new();
            for q in CandidateQuestion::ALL {
                if let Some(answer) = input.answers.get(&QuestionId::candidate(candidate, q)) {
                    answers.insert(q.as_str().into(), map(answer));
                }
            }
            analysis.candidates.insert(candidate.as_str().into(), answers);
        }
        Ok(analysis)
    }
    fn observe(&self, outcome: TurnOutcome) -> ObserveAdmission {
        self.observations.observe(outcome)
    }
    fn identity(&self) -> &'static str {
        "causal"
    }
}
fn map(answer: &DecisionAnswer) -> IntentValue {
    match answer {
        DecisionAnswer::Choice(a) => IntentValue::Choice {
            probabilities: a
                .probabilities
                .iter()
                .map(|(k, v)| (k.as_str().into(), v.get()))
                .collect(),
            confidence: a.confidence.get(),
        },
        DecisionAnswer::Score(a) => IntentValue::Score {
            distribution: a.distribution.iter().map(|v| v.get()).collect(),
            expected: a.expected,
            confidence: a.confidence.get(),
        },
        DecisionAnswer::Noul(a) => IntentValue::Probability(a.probability_true.get()),
    }
}
