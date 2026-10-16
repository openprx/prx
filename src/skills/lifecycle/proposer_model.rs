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
use crate::providers::attempt_accounting::{self, AttemptAccounting};
use crate::providers::traits::{ChatMessage, ChatRequest};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
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
    #[error("model call failed: {0}")]
    Failed(String),
}

/// A model that can answer one proposal prompt. Cancellation and the call
/// deadline are applied by the caller by dropping the returned future; an
/// implementation must therefore keep any accounting liability alive until it
/// is settled (the managed adapter does: a dropped receipt stays unknown).
#[async_trait]
pub trait ProposerModel: Send + Sync {
    fn binding(&self) -> &ModelBinding;
    async fn complete(&self, system: &str, user: &str) -> Result<ModelReply, ModelCallError>;
}

/// The production model: the managed provider path with attempt accounting.
///
/// Fields are crate-visible so the runtime wires it with the objects its own
/// provider construction and decision service already issued; the proposer
/// owns no provider registry and reads no credential.
pub struct ManagedProviderModel {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) binding: ModelBinding,
    pub(crate) accounting: Arc<dyn AttemptAccounting>,
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

#[async_trait]
impl ProposerModel for ManagedProviderModel {
    fn binding(&self) -> &ModelBinding {
        &self.binding
    }

    async fn complete(&self, system: &str, user: &str) -> Result<ModelReply, ModelCallError> {
        let messages = [ChatMessage::system(system), ChatMessage::user(user)];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };
        let trace = attempt_accounting::chat_traced(
            self.provider.as_ref(),
            request,
            &self.binding.model,
            self.binding.temperature(),
            Arc::clone(&self.accounting),
        )
        .await
        .map_err(|error| {
            if attempt_accounting::is_local_refusal(&error) {
                ModelCallError::BudgetDenied
            } else {
                ModelCallError::Failed(bounded_detail(&error.to_string()))
            }
        })?;
        let usage = &trace.tokens_used;
        let reported = usage.is_reported() && usage.prompt_tokens.is_some() && usage.completion_tokens.is_some();
        let text = trace.response.text_or_empty().to_string();
        if text.trim().is_empty() {
            return Err(ModelCallError::Failed("the model returned an empty body".to_string()));
        }
        Ok(ModelReply {
            text,
            usage: ReportedUsage {
                reported,
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
            },
        })
    }
}
