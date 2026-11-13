#![cfg(test)]

//! Explicit controlled predictions for turn mechanics; registry, gates and bars stay real.
use super::*;
use crate::config::schema::causal_tree::{BackendKind, BackendLocality};
use crate::cost::decision_attempt::BackendIdentity;
use crate::decision_engine::prediction::backend::DecisionBackend;
use crate::decision_engine::prediction::factory::SlotLimits;
use crate::decision_engine::prediction::outbound::{GuardedBody, OutboundSecrets};
use crate::decision_engine::prediction::protocol::DecisionResponse;
use crate::decision_engine::prediction::validate::ModelVersionPolicy;
use crate::decision_engine::prediction::wire::{ResponseEnvelope, SystemOneCodec};

struct OrdinaryFixtureBackend {
    identity: BackendIdentity,
    key: Arc<DecisionKey>,
    action: &'static str,
    family: &'static str,
    follow_builtin: bool,
    publication_family: Option<&'static str>,
}
#[async_trait::async_trait]
impl DecisionBackend for OrdinaryFixtureBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }
    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::ExactPinned(Arc::clone(&self.identity.configured_model))
    }
    async fn evaluate_with_accounting(
        &self,
        request: &crate::decision_engine::prediction::protocol::DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        _accounting: Option<&dyn crate::cost::decision_attempt::DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, crate::cost::decision_attempt::DecisionError> {
        // In-process fixture prediction performs no provider I/O or physical charge.
        self.evaluate(request, body, deadline).await
    }
    async fn evaluate(
        &self,
        request: &crate::decision_engine::prediction::protocol::DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
    ) -> Result<DecisionResponse, crate::cost::decision_attempt::DecisionError> {
        use crate::cost::decision_attempt::DecisionError;
        if Instant::now() >= deadline {
            return Err(DecisionError::Timeout);
        }
        let wire: serde_json::Value = serde_json::from_slice(body.bytes()).map_err(|_| DecisionError::Unavailable)?;
        let builtin = crate::decision_engine::prediction::builtin::BuiltinDecisionBackend::answer(request)?;
        let selected = |question| -> Result<&str, DecisionError> {
            match builtin
                .answers
                .get(&crate::decision_engine::prediction::QuestionId::global(question))
            {
                Some(crate::decision_engine::prediction::DecisionAnswer::Choice(choice)) => Ok(choice.chosen.as_str()),
                _ => Err(DecisionError::Unavailable),
            }
        };
        let (action, family) = if let Some(family) = self.publication_family {
            let first_hop = wire.pointer("/state/hop").and_then(serde_json::Value::as_u64) == Some(0);
            let discover = first_hop
                && wire
                    .pointer("/state/candidates")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|candidates| {
                        candidates
                            .iter()
                            .any(|candidate| candidate["action"] == "discover" && candidate["target"] == family)
                    });
            if discover {
                ("discover", family)
            } else {
                ("answer", "none")
            }
        } else if self.follow_builtin {
            (
                selected(crate::decision_engine::prediction::GlobalQuestion::NextAction)?,
                selected(crate::decision_engine::prediction::GlobalQuestion::CapabilityTarget)?,
            )
        } else {
            (self.action, self.family)
        };
        let answer = crate::agent::agent::native_decision_fixture::decision_answers_for_exact(&wire, action, family)
            .map_err(|_| DecisionError::Unavailable)?;
        let bytes = serde_json::to_vec(&answer).map_err(|_| DecisionError::Unavailable)?;
        SystemOneCodec::new(request, &self.key)?.decode_response(
            &bytes,
            ResponseEnvelope {
                backend: "mock",
                provider_request_id: None,
                elapsed_ms: 0,
            },
        )
    }
}
impl TurnDecisionService {
    /// Install only before publication, on the exact ordinary fixture service.
    pub(crate) fn install_ordinary_child_fixture(
        service: &mut Arc<Self>,
        action: &'static str,
        family: &'static str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                (action, family),
                ("delegate", "session") | ("use_tool", "shell" | "native_tool")
            ),
            "unreviewed ordinary fixture action"
        );
        let owned = Arc::get_mut(service).context("ordinary fixture service already shared")?;
        anyhow::ensure!(
            !owned.generation.effective.causal_tree.enabled,
            "ordinary fixture plane must remain disabled"
        );
        owned.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(OrdinaryFixtureBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-native-decision"),
                },
                key: Arc::clone(&owned.state_key),
                action,
                family,
                follow_builtin: false,
                publication_family: None,
            })],
            Arc::clone(&owned.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        Ok(())
    }
}
// Positive mechanics fixtures need an actually above-bar prediction. This
// opt-in never changes the production builtin, policy or admitted references.
#[derive(Clone, Copy)]
enum ScoredPrediction {
    BuiltinChoice,
    Publication(&'static str),
}

tokio::task_local! {
    static SCORED_PREDICTION: ScoredPrediction;
}

pub(crate) fn with_scored_prediction<F: std::future::Future>(work: F) -> impl std::future::Future<Output = F::Output> {
    SCORED_PREDICTION.scope(ScoredPrediction::BuiltinChoice, Box::pin(work))
}

pub(crate) fn with_scored_publication<F: std::future::Future>(
    family: &'static str,
    work: F,
) -> impl std::future::Future<Output = F::Output> {
    SCORED_PREDICTION.scope(ScoredPrediction::Publication(family), Box::pin(work))
}

// Capture before spawning, then scope only the explicitly controlled child.
// An unscoped caller keeps its original factory and builtin predictions.
pub(crate) fn with_inherited_scored_prediction<F: std::future::Future>(
    work: F,
) -> impl std::future::Future<Output = F::Output> {
    let inherited = SCORED_PREDICTION.try_with(|choice| *choice).ok();
    async move {
        if let Some(choice) = inherited {
            SCORED_PREDICTION.scope(choice, Box::pin(work)).await
        } else {
            work.await
        }
    }
}

pub(super) fn wrap_factory(
    original: DecisionBackendFactory,
    key: Arc<DecisionKey>,
    events: Arc<dyn CausalEventSink>,
) -> DecisionBackendFactory {
    let Ok(choice) = SCORED_PREDICTION.try_with(|choice| *choice) else {
        return original;
    };
    let (action, family) = ("answer", "none");
    DecisionBackendFactory::from_slots(
        vec![Box::new(OrdinaryFixtureBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-native-decision"),
            },
            key: Arc::clone(&key),
            action,
            family,
            follow_builtin: matches!(choice, ScoredPrediction::BuiltinChoice),
            publication_family: match choice {
                ScoredPrediction::Publication(family) => Some(family),
                _ => None,
            },
        })],
        key,
        OutboundSecrets::default(),
        SlotLimits::default(),
    )
    .with_events(events)
}
