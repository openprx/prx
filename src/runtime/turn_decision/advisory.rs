//! Advisory prediction projection and observation; never changes admission.
use super::{TurnDecisionContext, TurnDecisionService};
use crate::decision_engine::config::{DecisionEngineConfig, DecisionMode};
use crate::decision_engine::observe::{ObserveQueue, SqliteTelemetryWriter};
use crate::decision_engine::{DecisionEngine, IntentAnalysis, IntentInput, TurnOutcome};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

impl TurnDecisionService {
    #[cfg(test)]
    pub(crate) fn advisory_initialized(&self) -> bool {
        self.advisory_engine.initialized()
    }

    pub(crate) fn advisory_config(&self) -> &DecisionEngineConfig {
        &self.generation.effective.decision_engine
    }

    async fn advisory_engine(&self) -> Option<&AdvisoryRuntime> {
        let config = self.advisory_config();
        if !config.enabled {
            return None;
        }
        Some(
            self.advisory_engine
                .get_or_init(|| async {
                    let cap = if config.mode == DecisionMode::Pre {
                        0
                    } else {
                        config.telemetry_cap
                    };
                    let queue_size = config.observe_queue as usize;
                    let path = self.generation.effective.workspace_dir.join("decision-engine.sqlite");
                    let cancellation = CancellationToken::new();
                    let worker_cancellation = cancellation.clone();
                    let opened = tokio::task::spawn_blocking(move || {
                        ObserveQueue::start(queue_size, cap as usize, worker_cancellation, || {
                            Ok(Box::new(SqliteTelemetryWriter::open(&path)?))
                        })
                    })
                    .await;
                    let (observations, worker) = match opened {
                        Ok(Ok(started)) => started,
                        _ => {
                            tracing::warn!("decision engine telemetry unavailable");
                            (ObserveQueue::disabled(), None)
                        }
                    };
                    observations.register_status(&self.generation.effective.workspace_dir);
                    AdvisoryRuntime {
                        engine: crate::decision_engine::causal_adapter::CausalEngine { observations },
                        cancellation,
                        worker: tokio::sync::Mutex::new(worker),
                    }
                })
                .await,
        )
    }

    pub(super) async fn drain_advisory(&self, deadline: std::time::Instant) -> bool {
        let Some(runtime) = self.advisory_engine.get() else {
            return true;
        };
        runtime.cancellation.cancel();
        let mut worker = runtime.worker.lock().await;
        if let Some(completion) = worker.as_mut() {
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), completion).await {
                Ok(Ok(())) => {
                    worker.take();
                }
                _ => return false,
            }
        }
        true
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

    pub(crate) async fn observe_advisory(
        &self,
        context: &TurnDecisionContext,
        decision_id: Option<String>,
        succeeded: bool,
    ) {
        let config = self.advisory_config();
        if !config.enabled || config.mode == DecisionMode::Pre {
            return;
        }
        let Some(engine) = self.advisory_engine().await else {
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
                engine.engine.observe(outcome);
            }
            Err(_) => tracing::warn!("decision engine outcome exceeds observation bounds"),
        }
    }
}

pub(super) struct AdvisoryRuntime {
    engine: crate::decision_engine::causal_adapter::CausalEngine,
    cancellation: CancellationToken,
    worker: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}
impl Drop for AdvisoryRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision_engine::observe::TelemetryWriter;

    struct GatedWriter {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl TelemetryWriter for GatedWriter {
        fn append_fifo(&mut self, _: TurnOutcome, _: usize) -> anyhow::Result<()> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            self.release.recv()?;
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn late_advisory_drain_preserves_catalog_acknowledgement() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config {
            workspace_dir: temp.path().into(),
            ..Default::default()
        };
        config.causal_tree.enabled = false;
        let generation = crate::config::new_shared(config.clone()).pin();
        let service = super::super::tests::disabled_agent_service(
            &generation,
            &crate::providers::catalog::CatalogView::from_config(&config),
        )
        .await;
        let cancellation = CancellationToken::new();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (observations, worker) = ObserveQueue::start(2, 10, cancellation.clone(), || {
            Ok(Box::new(GatedWriter {
                entered: Some(entered_tx),
                release: release_rx,
            }))
        })
        .unwrap();
        assert_eq!(
            observations.observe(TurnOutcome::new(None, vec![], true).unwrap()),
            crate::decision_engine::ObserveAdmission::Accepted
        );
        entered_rx.await.unwrap();
        assert!(
            service
                .advisory_engine
                .set(AdvisoryRuntime {
                    engine: crate::decision_engine::causal_adapter::CausalEngine { observations },
                    cancellation,
                    worker: tokio::sync::Mutex::new(worker),
                })
                .is_ok()
        );
        let drained = service
            .drain_writes(std::time::Instant::now() + std::time::Duration::from_millis(20))
            .await;
        // The worker is still gated, but catalog acknowledgement permits the chat loop to continue.
        assert!(drained);
        assert!(service.advisory_engine.get().unwrap().worker.lock().await.is_some());
        release_tx.send(()).unwrap();
        tokio::time::resume();
        assert!(
            service
                .drain_advisory(std::time::Instant::now() + std::time::Duration::from_secs(2))
                .await
        );
    }
}
