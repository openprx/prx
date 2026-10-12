//! Q67-only controlled predictions. The pinned generation remains disabled.
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
        let wire = serde_json::from_slice(body.bytes()).map_err(|_| DecisionError::Unavailable)?;
        let answer = crate::agent::agent::native_decision_fixture::decision_answers(&wire, self.action, self.family)
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
