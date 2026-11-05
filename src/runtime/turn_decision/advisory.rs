//! Advisory prediction projection and observation; never changes admission.
use super::{TurnDecisionContext, TurnDecisionService};
use crate::decision_engine::config::{DecisionEngineConfig, DecisionMode};
use crate::decision_engine::observe::{ObserveQueue, SqliteTelemetryWriter};
use crate::decision_engine::{DecisionEngine, IntentAnalysis, IntentInput, TurnOutcome};
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;

impl TurnDecisionService {
    pub(crate) fn advisory_config(&self) -> &DecisionEngineConfig {
        &self.generation.effective.decision_engine
    }

    fn advisory_engine(&self) -> Option<&Arc<crate::decision_engine::causal_adapter::CausalEngine>> {
        let config = self.advisory_config();
        if !config.enabled {
            return None;
        }
        Some(self.advisory_engine.get_or_init(|| {
            let cap = if config.mode == DecisionMode::Pre {
                0
            } else {
                config.telemetry_cap
            };
            let path = self.generation.effective.workspace_dir.join("decision-engine.sqlite");
            let observations = match ObserveQueue::start(
                config.observe_queue as usize,
                cap as usize,
                CancellationToken::new(),
                || Ok(Box::new(SqliteTelemetryWriter::open(&path)?)),
            ) {
                Ok((queue, _worker)) => queue,
                Err(_) => {
                    tracing::warn!("decision engine telemetry unavailable");
                    ObserveQueue::disabled()
                }
            };
            Arc::new(crate::decision_engine::causal_adapter::CausalEngine { observations })
        }))
    }

    pub(crate) async fn analyze_advisory(
        &self,
        context: &TurnDecisionContext,
        cancellation: &CancellationToken,
    ) -> Option<IntentAnalysis> {
        let config = self.advisory_config();
        if !config.enabled || config.mode == DecisionMode::Post {
            return None;
        }
        // Analysis needs only validated answers. Defer store construction until
        // the final observation so telemetry I/O cannot extend its timeout.
        let engine = crate::decision_engine::causal_adapter::CausalEngine {
            observations: ObserveQueue::disabled(),
        };
        let decision_id = context.decision_id().to_string();
        let candidates = match &context.engine.candidates {
            crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } => candidates
                .iter()
                .map(|candidate| candidate.candidate_id().key())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let mut answers = BTreeMap::new();
        if let Some(prediction) = &context.engine.prediction {
            let questions = crate::decision::GlobalQuestion::ALL
                .into_iter()
                .map(crate::decision::QuestionId::global)
                .chain(candidates.iter().flat_map(|candidate| {
                    crate::decision::CandidateQuestion::ALL
                        .into_iter()
                        .map(move |question| crate::decision::QuestionId::candidate(candidate, question))
                }));
            for question in questions {
                if let Some(answer) = prediction.served.answer(&question) {
                    answers.insert(question, answer.clone());
                }
            }
        }
        crate::decision_engine::turn::analyze(
            &engine,
            config,
            IntentInput {
                decision_id: &decision_id,
                answers: &answers,
                candidates: &candidates,
            },
            cancellation,
        )
        .await
    }

    pub(crate) fn observe_advisory(&self, context: &TurnDecisionContext, decision_id: Option<String>, succeeded: bool) {
        let config = self.advisory_config();
        if !config.enabled || config.mode == DecisionMode::Pre {
            return;
        }
        let Some(engine) = self.advisory_engine() else {
            return;
        };
        let tools = context
            .terminal_execution_trace()
            .calls
            .into_iter()
            .map(|call| call.capability_id)
            .collect();
        match TurnOutcome::new(decision_id, tools, succeeded) {
            Ok(outcome) => {
                engine.observe(outcome);
            }
            Err(_) => tracing::warn!("decision engine outcome exceeds observation bounds"),
        }
    }
}
