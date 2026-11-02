//! Explicit controlled predictions for turn mechanics; registry, gates and bars stay real.
use super::*;
use crate::decision::backend::DecisionBackend;
use crate::decision::factory::{ServedBy, SlotLimits};
use crate::decision::outbound::{GuardedBody, OutboundSecrets};
use crate::decision::protocol::{BackendIdentity, BackendKind, BackendLocality, DecisionResponse};
use crate::decision::validate::ModelVersionPolicy;
use crate::decision::wire::{ResponseEnvelope, SystemOneCodec};

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
        request: &crate::decision::protocol::DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        _accounting: Option<&dyn crate::decision::budget::DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, crate::decision::DecisionError> {
        // In-process fixture prediction performs no provider I/O or physical charge.
        self.evaluate(request, body, deadline).await
    }
    async fn evaluate(
        &self,
        request: &crate::decision::protocol::DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
    ) -> Result<DecisionResponse, crate::decision::DecisionError> {
        use crate::decision::DecisionError;
        if Instant::now() >= deadline {
            return Err(DecisionError::Timeout);
        }
        let wire: serde_json::Value = serde_json::from_slice(body.bytes()).map_err(|_| DecisionError::Unavailable)?;
        let builtin = crate::decision::builtin::BuiltinDecisionBackend::answer(request)?;
        let selected = |question| -> Result<&str, DecisionError> {
            match builtin.answers.get(&crate::decision::QuestionId::global(question)) {
                Some(crate::decision::DecisionAnswer::Choice(choice)) => Ok(choice.chosen.as_str()),
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
                selected(crate::decision::GlobalQuestion::NextAction)?,
                selected(crate::decision::GlobalQuestion::CapabilityTarget)?,
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
impl TurnDecisionContext {
    pub(crate) fn ordinary_fixture_prediction_verified(&self) -> bool {
        self.audit_hops().any(|hop| {
            hop.prediction.as_ref().is_some_and(|prediction| {
                prediction.served_by == ServedBy::Primary
                    && prediction.served.claimed_backend() == "mock"
                    && prediction
                        .attempts
                        .iter()
                        .any(|attempt| attempt.identity.kind == BackendKind::Mock)
            })
        })
    }
}

// Positive mechanics fixtures need an actually above-bar prediction. This
// opt-in never changes the production builtin, policy or admitted references.
tokio::task_local! {
    static SCORED_PREDICTION: Option<(&'static str, &'static str)>;
}

pub(crate) fn with_scored_prediction<F: std::future::Future>(work: F) -> impl std::future::Future<Output = F::Output> {
    SCORED_PREDICTION.scope(None, Box::pin(work))
}

pub(crate) fn with_scored_action<F: std::future::Future>(
    action: &'static str,
    family: &'static str,
    work: F,
) -> impl std::future::Future<Output = F::Output> {
    SCORED_PREDICTION.scope(Some((action, family)), Box::pin(work))
}

pub(super) fn wrap_factory(
    original: DecisionBackendFactory,
    key: Arc<DecisionKey>,
    events: Arc<dyn CausalEventSink>,
) -> DecisionBackendFactory {
    let Ok(choice) = SCORED_PREDICTION.try_with(|choice| *choice) else {
        return original;
    };
    let (action, family) = choice.unwrap_or(("answer", "none"));
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
            follow_builtin: choice.is_none(),
            publication_family: None,
        })],
        key,
        OutboundSecrets::default(),
        SlotLimits::default(),
    )
    .with_events(events)
}

impl TurnDecisionService {
    /// A schema-publication witness: discover once, then use an above-bar
    /// ordinary Answer. This never selects an unapproved Destructive Commit.
    pub(crate) fn install_ordinary_publication_fixture(
        service: &mut Arc<Self>,
        family: &'static str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(family, "shell" | "native_tool"),
            "unreviewed publication fixture family"
        );
        let owned = Arc::get_mut(service).context("publication fixture already shared")?;
        anyhow::ensure!(
            !owned.generation.effective.causal_tree.enabled,
            "publication fixture must remain disabled"
        );
        owned.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(OrdinaryFixtureBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-native-decision"),
                },
                key: Arc::clone(&owned.state_key),
                action: "answer",
                family: "none",
                follow_builtin: false,
                publication_family: Some(family),
            })],
            Arc::clone(&owned.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        Ok(())
    }
}
