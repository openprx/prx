//! Chat advice adapter: issues the turn decision for a bound chat input and
//! exposes the advisory analysis. The chat turn binding lives in `chat::binding`.

use super::binding::{ChatAdmissionMode, ChatDecision, ChatTurnBinding};
use std::sync::Arc;

impl ChatTurnBinding {
    pub(super) async fn issue_decision(
        &self,
        service: &Arc<crate::runtime::turn_publication::TurnRuntimeService>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<&Arc<crate::runtime::turn_context::OpenedTurn>> {
        anyhow::ensure!(
            matches!(self.admission_mode, ChatAdmissionMode::Model(_)),
            "operator command cannot issue a model-turn decision"
        );
        let closed = self.admission_closed.lock().await;
        anyhow::ensure!(!*closed, "chat input was closed before decision issuance");
        let decided = self
            .decision
            .get_or_try_init(|| async {
                let kind = crate::runtime::turn_context::AgentTurnKind::Repl;
                let opened = service.open_turn_context(self, kind, cancellation).await?;
                let context = service.admit_opened_context(self, opened, cancellation)?;
                let advice = service.advisory().analyze_turn(context.runtime(), cancellation).await;
                Ok::<_, anyhow::Error>(ChatDecision {
                    advice,
                    service: Arc::clone(service),
                    context,
                })
            })
            .await?;
        anyhow::ensure!(Arc::ptr_eq(&decided.service, service), "chat decision service changed");
        Ok(&decided.context)
    }

    pub(super) fn decision_advice(&self) -> Option<crate::decision_engine::IntentAnalysis> {
        self.decision.get().and_then(|decided| decided.advice.clone())
    }

    pub(super) fn decision_routing(&self) -> Option<crate::agent::turn_request::TurnDecision<'_>> {
        self.decision
            .get()
            .map(|decided| crate::agent::turn_request::TurnDecision {
                service: &decided.service,
                context: &decided.context,
                runtime: decided.context.runtime(),
                proof: self,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::super::binding::tests::{fixture_with_advisory, service};

    #[tokio::test]
    async fn advisory_decide_mode_projects_prediction_and_lazily_opens_store() {
        use crate::config::schema::decision_engine::{DecisionEngineConfig, DecisionMode};
        for (enabled, mode) in [
            (false, DecisionMode::Pre),
            (true, DecisionMode::Pre),
            (true, DecisionMode::Post),
            (true, DecisionMode::Both),
        ] {
            let (dir, fabric, binding) = fixture_with_advisory(
                true,
                DecisionEngineConfig {
                    enabled,
                    mode,
                    ..Default::default()
                },
            )
            .await;
            let service = service(&fabric, &binding).await;
            let cancellation = tokio_util::sync::CancellationToken::new();
            // Chat runs every turn under a cleanup owner, which owns the
            // turn's accounting scope.
            let cleanup = std::sync::Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
            let context = cleanup
                .scope(binding.issue_decision(&service, &cancellation))
                .await
                .unwrap();
            let advice = binding.decision_advice();
            if enabled && mode != DecisionMode::Post {
                let advice = advice.expect("pre/both analysis from actual chat decision");
                assert_eq!(advice.decision_id, context.decision_id().to_string());
                // Chat decide no longer runs a prediction port; advice copies
                // whatever answers the opened context has (none when engine-off
                // construction is used).
            } else {
                assert!(advice.is_none());
            }
            assert!(!service.advisory().initialized());
            assert!(!dir.path().join("decision-engine.sqlite").exists());
            service.advisory().observe(
                || context.terminal_execution_trace(),
                binding.decision_advice().map(|a| a.decision_id),
                crate::runtime::decision_advisory::TurnEnd {
                    result: crate::decision_engine::TurnResultClass::Completed,
                    duration_ms: 1,
                    usage: None,
                },
            );
            // P1-04: the store is opened in the background, never on the
            // turn's path; shutdown drain waits for that opening.
            assert!(
                service
                    .drain_writes(std::time::Instant::now() + std::time::Duration::from_secs(2))
                    .await
            );
            assert_eq!(service.advisory().initialized(), enabled && mode != DecisionMode::Pre);
            if enabled && mode != DecisionMode::Pre {
                let store = crate::decision_engine::observe::SqliteTelemetryWriter::open(
                    &dir.path().join("decision-engine.sqlite"),
                )
                .unwrap();
                assert_eq!(
                    store.outcomes(10).unwrap().len(),
                    1,
                    "shutdown flushes accepted outcome"
                );
            }
        }
    }
}
