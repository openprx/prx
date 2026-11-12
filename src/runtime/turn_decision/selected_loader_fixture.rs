#![cfg(test)]

//! Q54: explicit first-response other choice for one selected-loader fixture.
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
    static FIRST_OTHER: Arc<std::sync::atomic::AtomicBool>;
}

#[inline(never)]
pub(crate) fn with_first_other<'a, F: Future + 'a>(work: F) -> std::pin::Pin<Box<dyn Future<Output = F::Output> + 'a>> {
    Box::pin(async move {
        FIRST_OTHER
            .scope(Arc::new(std::sync::atomic::AtomicBool::new(true)), work)
            .await
    })
}

pub(super) fn wrap_factory(
    original: DecisionBackendFactory,
    config: &crate::config::Config,
    key: Arc<DecisionKey>,
    events: Arc<dyn CausalEventSink>,
) -> DecisionBackendFactory {
    let Ok(first) = FIRST_OTHER.try_with(Arc::clone) else {
        return original;
    };
    assert!(
        !config.causal_tree.enabled,
        "Q54 only replaces the selected builtin-only fixture factory"
    );
    DecisionBackendFactory::from_slots(
        vec![Box::new(FirstOtherBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-selected-loader-fixture"),
            },
            first,
        })],
        key,
        OutboundSecrets::default(),
        SlotLimits::default(),
    )
    .with_events(events)
}

struct FirstOtherBackend {
    identity: BackendIdentity,
    first: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl DecisionBackend for FirstOtherBackend {
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
        if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let question_id = QuestionId::global(GlobalQuestion::NextAction);
            let Some(crate::decision::protocol::DecisionQuestion::Choice { criteria, .. }) =
                request.questions().get(&question_id)
            else {
                panic!("Q54 requires actual compiled NextAction Choice");
            };
            let other = criteria
                .keys()
                .find(|id| id.as_str() == "other")
                .expect("Q54 requires compiled other option")
                .clone();
            let Some(DecisionAnswer::Choice(answer)) = response.answers.get_mut(&question_id) else {
                panic!("Q54 requires genuine Builtin NextAction answer");
            };
            assert!(
                answer.probabilities.keys().eq(criteria.keys()),
                "Q54 exact compiled option coverage"
            );
            answer.chosen = other.clone();
            for (option, probability) in &mut answer.probabilities {
                *probability = Probability::new(if option == &other { 1.0 } else { 0.0 })?;
            }
        }
        Ok(response)
    }
}
