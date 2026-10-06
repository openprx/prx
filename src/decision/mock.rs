//! `MockDecisionBackend`: a deterministic, scripted decision backend for
//! unit, integration, timeout, corruption and circuit-breaker tests (CDP 4.3
//! item 5).
//!
//! This module is compiled only under `cfg(test)` or the `test-mock` feature
//! (AG-05); no production binary contains it.
//!
//! Each call to [`MockDecisionBackend::evaluate`] consumes one scripted
//! [`MockStep`]. A raw-body step goes through the real System One codec, so
//! corrupted, duplicated or replayed bodies are rejected exactly as an HTTP
//! backend's would be.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::error::DecisionError;
use super::key::DecisionKey;
use super::protocol::{BackendIdentity, BackendKind, DecisionRequest, DecisionResponse};
use super::wire::{ResponseEnvelope, SystemOneCodec};

/// What one scripted call returns.
#[derive(Debug, Clone)]
pub enum MockOutcome {
    /// Return this response as is.
    Respond(DecisionResponse),
    /// Decode this wire body with the request's codec, as an HTTP backend
    /// would.
    RespondRaw(Vec<u8>),
    /// Fail with this error.
    Fail(DecisionError),
    /// Never answer: wait for the deadline, then time out.
    Hang,
}

/// One scripted call: an optional delay, then an outcome.
#[derive(Debug, Clone)]
pub struct MockStep {
    /// How long the call takes before its outcome.
    pub delay: Option<Duration>,
    /// What the call returns.
    pub outcome: MockOutcome,
}

impl MockStep {
    /// A step with no delay.
    #[must_use]
    pub const fn now(outcome: MockOutcome) -> Self {
        Self { delay: None, outcome }
    }

    /// A step that takes `delay` before its outcome.
    #[must_use]
    pub const fn after(delay: Duration, outcome: MockOutcome) -> Self {
        Self {
            delay: Some(delay),
            outcome,
        }
    }
}

/// The scripted backend.
#[derive(Debug)]
pub struct MockDecisionBackend {
    key: Arc<DecisionKey>,
    script: Mutex<VecDeque<MockStep>>,
    calls: AtomicUsize,
}

impl MockDecisionBackend {
    /// The name the mock reports as its backend claim.
    pub const NAME: &'static str = "mock";

    /// A mock that plays `script` in order; `key` salts raw-body decoding.
    #[must_use]
    pub fn new(key: Arc<DecisionKey>, script: Vec<MockStep>) -> Self {
        Self {
            key,
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
        }
    }

    /// The configured identity of the mock.
    #[must_use]
    pub fn identity() -> BackendIdentity {
        BackendIdentity {
            kind: BackendKind::Mock,
            provider: None,
            configured_model: Arc::from("mock-model"),
        }
    }

    /// How many times `evaluate` was called.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Play the next scripted step for `request`, honouring `deadline`: a
    /// delay or hang that would pass it ends at the deadline with
    /// [`DecisionError::Timeout`]. An exhausted script answers
    /// [`DecisionError::Unavailable`].
    pub async fn evaluate(
        &self,
        request: &DecisionRequest,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        let Some(step) = self.script.lock().pop_front() else {
            tracing::warn!("mock decision backend script is exhausted");
            return Err(DecisionError::Unavailable);
        };
        if let Some(delay) = step.delay {
            if started + delay > deadline {
                tokio::time::sleep_until(deadline.into()).await;
                return Err(DecisionError::Timeout);
            }
            tokio::time::sleep(delay).await;
        }
        match step.outcome {
            MockOutcome::Respond(response) => Ok(response),
            MockOutcome::RespondRaw(body) => SystemOneCodec::new(request, &self.key)?.decode_response(
                &body,
                ResponseEnvelope {
                    backend: Self::NAME,
                    provider_request_id: None,
                    elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                },
            ),
            MockOutcome::Fail(error) => Err(error),
            MockOutcome::Hang => {
                tokio::time::sleep_until(deadline.into()).await;
                Err(DecisionError::Timeout)
            }
        }
    }
}
