#![cfg(test)]

//! Explicit body-local evidence prerequisite for approved turn-mechanics tests.
//! This module is compiled only for tests. It never changes registry or gates.
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use crate::decision::backend::DecisionBackend;
use crate::decision::builtin::BuiltinDecisionBackend;
use crate::decision::factory::{DecisionBackendFactory, SlotLimits};
use crate::decision::outbound::{GuardedBody, OutboundSecrets};
use crate::decision::protocol::{
    BackendIdentity, BackendKind, BackendLocality, DecisionAnswer, DecisionRequest, DecisionResponse, GlobalQuestion,
    Probability, QuestionId,
};
use crate::decision::telemetry::CausalEventSink;
use crate::decision::{DecisionError, DecisionKey, ModelVersionPolicy};

tokio::task_local! {
    static EVIDENCE_SUFFICIENT: ();
}

#[inline(never)]
pub(crate) fn with_evidence_sufficient<F: Future>(work: F) -> impl Future<Output = F::Output> {
    // Keep the complete admission/turn fixture on the heap instead of copying
    // it through another async state machine on the default test-thread stack.
    // The same runtime path and assertions still run under the task-local scope.
    EVIDENCE_SUFFICIENT.scope((), Box::pin(work))
}

pub(super) fn wrap_factory(
    original: DecisionBackendFactory,
    config: &crate::config::Config,
    key: Arc<DecisionKey>,
    events: Arc<dyn CausalEventSink>,
) -> DecisionBackendFactory {
    if EVIDENCE_SUFFICIENT.try_with(|()| ()).is_err() {
        return original;
    }
    assert!(
        !config.causal_tree.enabled,
        "Q45 only replaces a builtin-only fixture factory"
    );
    DecisionBackendFactory::from_slots(
        vec![Box::new(EvidenceSufficientBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-evidence-fixture"),
            },
        })],
        key,
        OutboundSecrets::default(),
        SlotLimits::default(),
    )
    .with_events(events)
}

struct EvidenceSufficientBackend {
    identity: BackendIdentity,
}

#[async_trait::async_trait]
impl DecisionBackend for EvidenceSufficientBackend {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
    fn locality(&self) -> BackendLocality {
        BackendLocality::Builtin
    }
    fn version_policy(&self) -> ModelVersionPolicy {
        ModelVersionPolicy::Unpinned
    }
    async fn evaluate_with_accounting(
        &self,
        request: &DecisionRequest,
        body: &GuardedBody,
        deadline: Instant,
        _accounting: Option<&dyn crate::decision::budget::DecisionAttemptAccounting>,
    ) -> Result<DecisionResponse, DecisionError> {
        // Local deterministic computation does not invent a physical attempt.
        self.evaluate(request, body, deadline).await
    }
    async fn evaluate(
        &self,
        request: &DecisionRequest,
        _: &GuardedBody,
        _: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        let mut response = BuiltinDecisionBackend::answer(request)?;
        response.backend = "mock".to_owned();
        response.resolved_model = self.identity.configured_model.to_string();
        if let Some(DecisionAnswer::Noul(answer)) = response
            .answers
            .get_mut(&QuestionId::global(GlobalQuestion::EvidenceSufficient))
        {
            answer.probability_true = Probability::new(1.0)?;
        }
        Ok(response)
    }
}
