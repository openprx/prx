#![cfg(test)]

//! Explicit flat NextAction distribution for one plain-answer abstention fixture.
//! This module is compiled only for tests. It never changes registry or gates:
//! the Builtin answer is kept, its NextAction distribution is flattened onto a
//! non-Answer choice, and read recovery is made unattractive, so Answer is the
//! top candidate, misses the confidence bar, and the selector abstains.
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
    static FLAT_NEXT_ACTION: ();
}

#[inline(never)]
pub(crate) fn with_flat_next_action<'a, F: Future + 'a>(
    work: F,
) -> std::pin::Pin<Box<dyn Future<Output = F::Output> + 'a>> {
    Box::pin(async move { FLAT_NEXT_ACTION.scope((), work).await })
}

pub(super) fn wrap_factory(
    original: DecisionBackendFactory,
    config: &crate::config::Config,
    key: Arc<DecisionKey>,
    events: Arc<dyn CausalEventSink>,
) -> DecisionBackendFactory {
    if FLAT_NEXT_ACTION.try_with(|()| ()).is_err() {
        return original;
    }
    assert!(
        config.causal_tree.enabled,
        "the flat NextAction fixture replaces the enabled prediction factory"
    );
    DecisionBackendFactory::from_slots(
        vec![Box::new(FlatNextActionBackend {
            identity: BackendIdentity {
                kind: BackendKind::Mock,
                provider: None,
                configured_model: Arc::from("prx-plain-abstention-fixture"),
            },
        })],
        key,
        OutboundSecrets::default(),
        SlotLimits::default(),
    )
    .with_events(events)
}

struct FlatNextActionBackend {
    identity: BackendIdentity,
}

#[async_trait::async_trait]
impl DecisionBackend for FlatNextActionBackend {
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
        let question_id = QuestionId::global(GlobalQuestion::NextAction);
        let Some(DecisionAnswer::Choice(answer)) = response.answers.get_mut(&question_id) else {
            panic!("flat NextAction fixture requires a genuine Builtin NextAction answer");
        };
        let options = answer.probabilities.len();
        assert!(options > 1, "flat NextAction fixture requires at least two options");
        // Every option ties, so any of them is a valid chosen option. Choose a
        // legal non-Answer action: a chosen Answer would let a completed
        // catalog read commit Answer without the confidence bar.
        answer.chosen = answer
            .probabilities
            .keys()
            .find(|id| !matches!(id.as_str(), "answer" | "use_tool" | "other" | "none"))
            .expect("flat NextAction fixture requires a legal non-Answer action option")
            .clone();
        let flat = Probability::new(1.0 / options as f64)?;
        for probability in answer.probabilities.values_mut() {
            *probability = flat;
        }
        answer.confidence = flat;
        // Keep the turn off every read recovery: evidence is sufficient and
        // fresh, and no working candidate other than Answer applies or can
        // succeed, so Discover or Retrieve never outranks Answer.
        for (id, answer) in &mut response.answers {
            let DecisionAnswer::Noul(noul) = answer else {
                continue;
            };
            let value = match id.as_str() {
                "evidence_sufficient" => 1.0,
                "needs_fresh_state" => 0.0,
                other
                    if other.starts_with("candidate.")
                        && !other.starts_with("candidate.answer-")
                        && (other.ends_with(".applicable") || other.ends_with(".success")) =>
                {
                    0.0
                }
                _ => continue,
            };
            noul.probability_true = Probability::new(value)?;
        }
        Ok(response)
    }
}
