//! Optional editor-only final-wire delivery proof through the existing owner.
use super::turn_cleanup::{CleanupTracker, current_tracker};
use crate::decision::{DecisionKey, KeyDomain};
use crate::providers::attempt_accounting::{AttemptReceipt, PhysicalAttemptIdentity, sealed};
use crate::skills::lifecycle::{ModelBinding, PreparedLessonContext};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use sha2::Digest;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod tests;
mod wire;

tokio::task_local! {
    static CURRENT: Arc<PreparedLessonContext>;
}

/// A prepares this context for one development editor invocation only. The
/// task-local scope ends with that call; no verifier/judge scope is inherited.
pub(crate) async fn scope<F: std::future::Future>(prepared: Arc<PreparedLessonContext>, work: F) -> F::Output {
    CURRENT.scope(prepared, work).await
}

/// Capture at the editor model invocation, before any provider-internal spawn
/// boundary. Retries retain the same accounting adapter and this opaque view;
/// they must not rediscover None merely because a task-local was not inherited.
pub(crate) struct LessonInvocation {
    prepared: Arc<PreparedLessonContext>,
    cleanup: Option<Arc<CleanupTracker>>,
    stage: Option<Arc<super::experiment_stage::ExperimentStageRuntime>>,
}

pub(crate) fn capture_invocation() -> Option<Arc<LessonInvocation>> {
    let prepared = CURRENT.try_with(Arc::clone).ok()?;
    Some(Arc::new(LessonInvocation {
        prepared,
        cleanup: current_tracker(),
        stage: super::experiment_stage::current(),
    }))
}

/// Construction after a durable physical reservation must be pure and cannot
/// refuse: outer Tally must first receive and count the reservation. All checks
/// and writes happen in prepare_handoff, after that outer admission completes.
pub(crate) fn wrap_receipt(
    inner: Box<dyn AttemptReceipt>,
    key: Arc<DecisionKey>,
    actual_model: ModelBinding,
    purpose: &str,
    invocation: Option<Arc<LessonInvocation>>,
) -> Box<dyn AttemptReceipt> {
    let Some(invocation) = invocation else {
        return inner;
    };
    Box::new(LessonReceipt {
        inner,
        key,
        actual_model,
        prepared: Arc::clone(&invocation.prepared),
        cleanup: invocation.cleanup.as_ref().map(Arc::clone),
        stage: invocation.stage.as_ref().map(Arc::clone),
        editor_purpose: purpose == "evolution_proposer",
        prepared_once: false,
    })
}

struct LessonReceipt {
    inner: Box<dyn AttemptReceipt>,
    key: Arc<DecisionKey>,
    actual_model: ModelBinding,
    prepared: Arc<PreparedLessonContext>,
    cleanup: Option<Arc<CleanupTracker>>,
    stage: Option<Arc<super::experiment_stage::ExperimentStageRuntime>>,
    editor_purpose: bool,
    prepared_once: bool,
}

impl sealed::Receipt for LessonReceipt {}

#[async_trait]
impl AttemptReceipt for LessonReceipt {
    fn deadline(&self) -> Instant {
        self.inner.deadline()
    }

    fn physical_identity(&self) -> Option<PhysicalAttemptIdentity<'_>> {
        self.inner.physical_identity()
    }

    async fn prepare_handoff(&mut self, final_body: &[u8]) -> Result<()> {
        ensure!(!self.prepared_once, "lesson receipt preparation cannot be repeated");
        self.prepared_once = true;
        ensure!(self.editor_purpose, "lesson context escaped its editor purpose");
        let stage = self.stage.as_ref().context("lesson context has no editor stage")?;
        ensure!(
            stage.binding() == *self.prepared.binding(),
            "lesson context escaped its prepared operation"
        );
        stage.ensure_active()?;
        self.inner.prepare_handoff(final_body).await?;
        ensure!(
            self.prepared.placement() == crate::skills::lifecycle::LessonPlacement::FirstSoleUserText,
            "lesson logical placement is unsupported"
        );
        ensure!(
            hex::encode(sha2::Sha256::digest(self.prepared.block().as_bytes())) == self.prepared.block_sha256(),
            "lesson formatted block digest changed"
        );
        let physical = self
            .inner
            .physical_identity()
            .context("lesson receipt has no physical identity")?;
        ensure_physical_owner(physical, &stage.scope_id())?;
        let final_context = wire::verify(
            final_body,
            &self.actual_model.model,
            self.prepared.block(),
            self.prepared.expected_user_slot_bytes(),
            self.prepared.expected_user_slot_sha256(),
        )?;
        let identity = crate::skills::lifecycle::LessonPhysicalIdentity {
            turn_id: physical.turn_id.to_owned(),
            attempt_id: physical.attempt_id.to_owned(),
            binding_sha256: physical.binding_sha256.to_owned(),
        };
        let body_binding = hex::encode(self.key.hmac(
            KeyDomain::AttemptAccounting,
            &[
                b"lesson-final-wire-v1",
                self.prepared.snapshot_sha256().as_bytes(),
                identity.turn_id.as_bytes(),
                identity.attempt_id.as_bytes(),
                identity.binding_sha256.as_bytes(),
                final_body,
            ],
        ));
        let cleanup = self.cleanup.as_ref().context("lesson receipt has no cleanup owner")?;
        let prepared = Arc::clone(&self.prepared);
        let key = Arc::clone(&self.key);
        let actual_model = self.actual_model.clone();
        let deadline = self.inner.deadline();
        let request = crate::skills::lifecycle::LessonDeliveryRequest {
            actual_model,
            physical: identity,
            body_binding_sha256: body_binding,
            final_context_sha256: final_context.sha256,
            deadline,
        };
        owned_persistence(Arc::clone(cleanup), move |cancel| {
            crate::skills::lifecycle::persist_lesson_delivery(&prepared, &request, &key, cancel)
        })
        .await?;
        stage.ensure_active()
    }

    fn retain_response_chunk(&self, bytes: usize) -> Result<()> {
        self.inner.retain_response_chunk(bytes)
    }

    async fn settle(self: Box<Self>, usage: crate::llm::route_decision::TokenUsage) -> Result<()> {
        // A provider response alone does not prove lesson usefulness; the
        // conservative durable record remains PossibleHandoffUnknown.
        self.inner.settle(usage).await
    }
}

fn ensure_physical_owner(physical: PhysicalAttemptIdentity<'_>, expected_scope: &str) -> Result<()> {
    ensure!(
        physical.turn_id == expected_scope,
        "lesson physical reservation belongs to another budget owner"
    );
    Ok(())
}

async fn owned_persistence<F>(cleanup: Arc<CleanupTracker>, write: F) -> Result<()>
where
    F: FnOnce(&CancellationToken) -> Result<()> + Send + 'static,
{
    let cancel = CancellationToken::new();
    let cancel_on_drop = CancelOnDrop(cancel.clone());
    // Ownership is registered before spawn; a dropped waiter cannot release
    // an I/O ticket while its blocking closure is still mutating the store.
    let ticket = cleanup.register(cancel.clone())?;
    let work = tokio::task::spawn_blocking(move || {
        let result = write(&cancel);
        let confirmed = !result
            .as_ref()
            .err()
            .is_some_and(|error| error.is::<crate::skills::lifecycle::LessonDeliveryWriteUnconfirmed>());
        ticket.finish(confirmed);
        result
    });
    let result = work
        .await
        .context("lesson delivery persistence task did not complete")?;
    drop(cancel_on_drop);
    result
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
