//! The seam between the skill proposer (SL05) and the managed provider path.
//!
//! The proposer never builds a provider and never reads a credential. A
//! [`ProposerModel`] is handed in by the runtime: in production
//! [`ManagedProviderModel`] wraps the provider `create_provider_for` produced
//! plus the sealed attempt-accounting owner the decision service issued, so
//! every physical request is reserved and settled through the existing
//! liability path. Tests inject deterministic models; they never reach a
//! network.

use crate::providers::Provider;
use crate::providers::attempt_accounting::{self, AttemptAccounting, AttemptReceipt, PhysicalRequest, sealed};
use crate::providers::traits::{ChatMessage, ChatRequest};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;
use thiserror::Error;

/// Longest provider error text kept in a proposal record.
const MAX_ERROR_DETAIL_BYTES: usize = 240;

/// The frozen model binding of a proposal: what produced the candidate.
/// Credentials and endpoints are never part of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    /// Redacted provider label (the resolved model's display label).
    pub provider_label: String,
    pub model: String,
    /// Sampling temperature in thousandths, so the binding is comparable.
    pub temperature_milli: u32,
}

impl ModelBinding {
    /// Stable digest of the binding.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"prx-proposer-model-binding-v1\0");
        for part in [self.provider_label.as_str(), self.model.as_str()] {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        hasher.update(self.temperature_milli.to_be_bytes());
        hex::encode(hasher.finalize())
    }

    fn temperature(&self) -> f64 {
        f64::from(self.temperature_milli) / 1000.0
    }
}

/// Usage the provider reported for one call. `reported == false` means the
/// provider gave no complete figures: the liability stays unknown and the
/// record says so instead of inventing numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedUsage {
    pub reported: bool,
    /// Physical requests the accounting owner reserved for this call (a retry
    /// or fallback inside the provider chain makes more than one).
    #[serde(default)]
    pub physical_requests: u32,
    /// How many of them were settled. `physical_requests - settled_requests`
    /// requests keep an open liability.
    #[serde(default)]
    pub settled_requests: u32,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct ModelReply {
    pub text: String,
    pub usage: ReportedUsage,
}

#[derive(Debug, Error)]
pub enum ModelCallError {
    /// No budget reservation could be issued: nothing was sent.
    #[error("budget reservation was denied; the model was not called")]
    BudgetDenied,
    /// The call failed or its answer was unusable. `usage` carries what is
    /// known about the physical requests; when requests were reserved and not
    /// all settled, the liability is unknown, not zero.
    #[error("model call failed: {detail}")]
    Failed { detail: String, usage: ReportedUsage },
}

impl ModelCallError {
    /// A failure with no physical request behind it.
    #[must_use]
    pub fn failed(detail: impl Into<String>) -> Self {
        Self::Failed {
            detail: detail.into(),
            usage: ReportedUsage::default(),
        }
    }
}

/// A model that can answer one proposal prompt. Cancellation and the call
/// deadline are applied by the caller by dropping the returned future; an
/// implementation must therefore keep any accounting liability alive until it
/// is settled (the managed adapter does: a dropped receipt stays unknown).
#[async_trait]
pub trait ProposerModel: Send + Sync {
    fn binding(&self) -> &ModelBinding;
    async fn complete(&self, system: &str, user: &str) -> Result<ModelReply, ModelCallError>;

    /// The physical requests the last call reserved and settled so far. The
    /// proposer reads it after dropping a call (deadline, cancellation), when
    /// `complete` can no longer report it. The default knows of none.
    fn physical_tally(&self) -> ReportedUsage {
        ReportedUsage::default()
    }
}

/// Counters shared between a managed model and the call in flight.
#[derive(Debug, Default)]
pub struct TallyCounters {
    begun: AtomicU32,
    settled: AtomicU32,
}

/// The production model: the managed provider path with attempt accounting.
///
/// Fields are crate-visible and there is deliberately no constructor: a
/// `pub(crate)` one would be dead code until the proposal-purpose accounting
/// owner exists, and a public one would expose a crate-private type. The
/// runtime wires it with the objects its own
/// provider construction and decision service already issued; the proposer
/// owns no provider registry and reads no credential.
pub struct ManagedProviderModel {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) binding: ModelBinding,
    pub(crate) accounting: Arc<dyn AttemptAccounting>,
    pub(crate) counters: Arc<TallyCounters>,
}

fn bounded_detail(text: &str) -> String {
    let sanitized = crate::providers::sanitize_api_error(text);
    if sanitized.len() <= MAX_ERROR_DETAIL_BYTES {
        return sanitized;
    }
    let mut end = MAX_ERROR_DETAIL_BYTES;
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized.get(..end).unwrap_or_default().to_string()
}

/// Counts the reservations an accounting owner issues and settles during one
/// call, so the proposer can tell a refusal before any request left from a
/// failure after one was sent, and can record every nested attempt.
struct Tally {
    inner: Arc<dyn AttemptAccounting>,
    counters: Arc<TallyCounters>,
}

impl sealed::AccountingOwner for Tally {}

#[async_trait]
impl AttemptAccounting for Tally {
    fn deadline(&self) -> Instant {
        self.inner.deadline()
    }
    async fn begin(&self, request: PhysicalRequest<'_>) -> anyhow::Result<Box<dyn AttemptReceipt>> {
        let receipt = self.inner.begin(request).await?;
        self.counters.begun.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(TallyReceipt {
            inner: receipt,
            counters: Arc::clone(&self.counters),
        }))
    }
}

struct TallyReceipt {
    inner: Box<dyn AttemptReceipt>,
    counters: Arc<TallyCounters>,
}

impl sealed::Receipt for TallyReceipt {}

#[async_trait]
impl AttemptReceipt for TallyReceipt {
    fn deadline(&self) -> Instant {
        self.inner.deadline()
    }
    async fn settle(self: Box<Self>, usage: crate::llm::route_decision::TokenUsage) -> anyhow::Result<()> {
        let Self { inner, counters } = *self;
        inner.settle(usage).await?;
        counters.settled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl ProposerModel for ManagedProviderModel {
    fn binding(&self) -> &ModelBinding {
        &self.binding
    }

    fn physical_tally(&self) -> ReportedUsage {
        ReportedUsage {
            physical_requests: self.counters.begun.load(Ordering::SeqCst),
            settled_requests: self.counters.settled.load(Ordering::SeqCst),
            ..ReportedUsage::default()
        }
    }

    async fn complete(&self, system: &str, user: &str) -> Result<ModelReply, ModelCallError> {
        let messages = [
            ChatMessage::system(system),
            crate::agent::turn_contracts::inbound_user_message(user),
        ];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };
        self.counters.begun.store(0, Ordering::SeqCst);
        self.counters.settled.store(0, Ordering::SeqCst);
        let tally = Arc::new(Tally {
            inner: Arc::clone(&self.accounting),
            counters: Arc::clone(&self.counters),
        });
        let counts = |tally: &Tally| {
            (
                tally.counters.begun.load(Ordering::SeqCst),
                tally.counters.settled.load(Ordering::SeqCst),
            )
        };
        let result = attempt_accounting::chat_traced(
            self.provider.as_ref(),
            request,
            &self.binding.model,
            self.binding.temperature(),
            Arc::clone(&tally) as Arc<dyn AttemptAccounting>,
        )
        .await;
        let (begun, settled) = counts(&tally);
        let trace = match result {
            Ok(trace) => trace,
            Err(error) => {
                // A refusal is "nothing was sent" only if no reservation was
                // ever issued. The same local refusal after a request left
                // (deadline, failed settlement) leaves a liability open.
                if begun == 0 && attempt_accounting::is_local_refusal(&error) {
                    return Err(ModelCallError::BudgetDenied);
                }
                let detail = if begun > 0 {
                    format!(
                        "{} (a reservation was issued: {begun} reserved, {settled} settled; liability unknown)",
                        bounded_detail(&error.to_string())
                    )
                } else {
                    bounded_detail(&error.to_string())
                };
                return Err(ModelCallError::Failed {
                    detail,
                    usage: ReportedUsage {
                        reported: false,
                        physical_requests: begun,
                        settled_requests: settled,
                        ..ReportedUsage::default()
                    },
                });
            }
        };
        // A successful chain may have retried or fallen back; every physical
        // request it made is counted, the token figures are the serving one's.
        let usage = &trace.tokens_used;
        let usage = ReportedUsage {
            reported: usage.is_reported() && usage.prompt_tokens.is_some() && usage.completion_tokens.is_some(),
            physical_requests: begun,
            settled_requests: settled,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        };
        let text = trace.response.text_or_empty().to_string();
        if text.trim().is_empty() {
            return Err(ModelCallError::Failed {
                detail: "the model returned an empty body".to_string(),
                usage,
            });
        }
        Ok(ModelReply { text, usage })
    }
}
