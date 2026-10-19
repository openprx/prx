//! Accounting injected only into explicitly supported auxiliary calls.
use super::traits::{ChatRequest, ChatTrace, Provider};
use crate::llm::route_decision::{TokenUsage, TokenUsageSource};
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use std::{sync::Arc, time::Instant};

pub(crate) mod sealed {
    pub trait AccountingOwner {}
    pub trait Receipt {}
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhysicalKind {
    Chat,
    Embedding,
}
/// Borrowed transport material. Never log, debug, or persist these fields.
pub(crate) struct PhysicalRequest<'a> {
    pub(super) model: &'a str,
    pub(super) endpoint: &'a str,
    pub(super) credential: Option<&'a str>,
    pub(super) body: &'a [u8],
    pub(super) kind: PhysicalKind,
}
impl PhysicalRequest<'_> {
    pub(crate) const fn endpoint(&self) -> &str {
        self.endpoint
    }
    pub(crate) const fn credential(&self) -> Option<&str> {
        self.credential
    }
    pub(crate) const fn body(&self) -> &[u8] {
        self.body
    }
    pub(crate) const fn kind(&self) -> PhysicalKind {
        self.kind
    }
}
#[async_trait]
pub(crate) trait AttemptAccounting: sealed::AccountingOwner + Send + Sync {
    fn deadline(&self) -> Instant;
    /// Owners with a durable provider outcome retain their own preparation
    /// provenance. Metering-only owners do not create an outcome recorder.
    fn parameter_capture(&self) -> Option<super::parameter_capture::Capture> {
        None
    }
    async fn begin(&self, request: PhysicalRequest<'_>) -> anyhow::Result<Box<dyn AttemptReceipt>>;
}
#[async_trait]
pub(crate) trait AttemptReceipt: sealed::Receipt + Send {
    fn deadline(&self) -> Instant;
    /// Own durable settlement before awaiting. Drop keeps unknown liability.
    async fn settle(self: Box<Self>, usage: TokenUsage) -> anyhow::Result<()>;
}
#[derive(Debug, thiserror::Error)]
#[error("auxiliary provider accounting refused locally")]
pub(crate) struct AccountingRefusal;
tokio::task_local! { static ACCOUNTING: Arc<dyn AttemptAccounting>; }

pub(crate) fn ensure_supported(provider: &dyn Provider) -> anyhow::Result<()> {
    if ACCOUNTING.try_with(|_| ()).is_ok() && !provider.supports_attempt_accounting() {
        return Err(AccountingRefusal.into());
    }
    Ok(())
}
pub(crate) fn is_local_refusal(error: &anyhow::Error) -> bool {
    error.is::<AccountingRefusal>()
}
pub(crate) async fn chat_traced(
    provider: &dyn Provider,
    request: ChatRequest<'_>,
    model: &str,
    temperature: f64,
    accounting: Arc<dyn AttemptAccounting>,
) -> anyhow::Result<ChatTrace> {
    if !provider.supports_attempt_accounting() {
        return Err(AccountingRefusal.into());
    }
    let deadline = tokio::time::Instant::from_std(accounting.deadline());
    let capture = accounting
        .parameter_capture()
        .unwrap_or_else(|| super::parameter_capture::Capture::new(false));
    tokio::time::timeout_at(
        deadline,
        ACCOUNTING.scope(
            accounting,
            capture.scope(provider.chat_traced(request, model, temperature)),
        ),
    )
    .await
    .map_err(|_| AccountingRefusal)?
}
#[cfg(feature = "llm-router")]
pub(crate) async fn embed(
    provider: &dyn crate::memory::embeddings::EmbeddingProvider,
    texts: &[&str],
    accounting: Arc<dyn AttemptAccounting>,
) -> anyhow::Result<Vec<Vec<f32>>> {
    if !provider.supports_attempt_accounting() {
        return Err(AccountingRefusal.into());
    }
    let deadline = tokio::time::Instant::from_std(accounting.deadline());
    tokio::time::timeout_at(deadline, ACCOUNTING.scope(accounting, provider.embed(texts)))
        .await
        .map_err(|_| AccountingRefusal)?
}

/// Live leaves the ordinary path's response ownership/allocation unchanged.
pub(crate) enum Response {
    Live(reqwest::Response),
    Accounted {
        status: reqwest::StatusCode,
        retry_after_ms: Option<u64>,
        body: Vec<u8>,
    },
}
impl Response {
    pub(crate) fn status(&self) -> reqwest::StatusCode {
        match self {
            Self::Live(r) => r.status(),
            Self::Accounted { status, .. } => *status,
        }
    }
    pub(crate) async fn text(self) -> anyhow::Result<String> {
        match self {
            Self::Live(r) => Ok(r.text().await?),
            Self::Accounted { body, .. } => Ok(String::from_utf8_lossy(&body).into_owned()),
        }
    }
    pub(crate) async fn json<T: DeserializeOwned>(self) -> anyhow::Result<T> {
        match self {
            Self::Live(r) => Ok(r.json().await?),
            Self::Accounted { body, .. } => Ok(serde_json::from_slice(&body)?),
        }
    }
    pub(crate) async fn parse_native<T: DeserializeOwned>(self) -> anyhow::Result<T> {
        match self {
            Self::Live(r) => {
                let body = r.bytes().await?;
                serde_json::from_slice(&body).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to parse response: {e}\nBody: {}",
                        String::from_utf8_lossy(&body)
                    )
                })
            }
            Self::Accounted { body, .. } => serde_json::from_slice(&body).map_err(Into::into),
        }
    }
    pub(crate) async fn error_body(self) -> String {
        match self {
            Self::Live(r) => super::read_error_body(r).await,
            other => other.text().await.unwrap_or_default(),
        }
    }
    pub(crate) async fn api_error(self, provider: &str) -> anyhow::Error {
        match self {
            Self::Live(r) => super::api_error(provider, r).await,
            Self::Accounted {
                status,
                retry_after_ms,
                body,
            } => {
                let message = format!(
                    "{provider} API error ({status}): {}",
                    super::sanitize_api_error(&String::from_utf8_lossy(&body))
                );
                match retry_after_ms {
                    Some(millis) => anyhow::Error::new(super::rate_limit::RetryAfterHint { millis }).context(message),
                    None => anyhow::anyhow!(message),
                }
            }
        }
    }
}
/// One serialization supplies both the reservation estimate and actual body.
/// Each invocation begins a separate receipt, including nested fallbacks.
pub(crate) async fn send_json<T: Serialize + ?Sized>(
    builder: reqwest::RequestBuilder,
    model: &str,
    endpoint: &str,
    credential: Option<&str>,
    request: &T,
    kind: PhysicalKind,
) -> anyhow::Result<Response> {
    let Ok(owner) = ACCOUNTING.try_with(Arc::clone) else {
        return Ok(Response::Live(builder.json(request).send().await?));
    };
    let body = serde_json::to_vec(request)?;
    let receipt = owner
        .begin(PhysicalRequest {
            model,
            endpoint,
            credential,
            body: &body,
            kind,
        })
        .await
        .map_err(|e| e.context(AccountingRefusal))?;
    let deadline = tokio::time::Instant::from_std(receipt.deadline());
    if deadline <= tokio::time::Instant::now() {
        return Err(AccountingRefusal.into());
    }
    let response = tokio::time::timeout_at(deadline, async {
        let response = builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let retry_after_ms = super::traits::retry_after_ms_from_headers(response.headers());
        let body = response.bytes().await?.to_vec();
        Ok::<_, reqwest::Error>((status, retry_after_ms, body))
    })
    .await
    .map_err(|_| AccountingRefusal)??;
    let (status, retry_after_ms, body) = response;
    // Parse accounting independently of semantic response decoding. A malformed
    // choice/data object must not discard otherwise reported billable usage.
    if let Some(usage) = reported_usage(&body, kind) {
        receipt.settle(usage).await.map_err(|e| e.context(AccountingRefusal))?;
    }
    Ok(Response::Accounted {
        status,
        retry_after_ms,
        body,
    })
}
fn reported_usage(body: &[u8], kind: PhysicalKind) -> Option<TokenUsage> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage = json.get("usage")?;
    let number = |name: &str| {
        usage
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    let prompt = number("prompt_tokens").or_else(|| number("input_tokens"))?;
    let completion = match kind {
        PhysicalKind::Embedding => Some(0),
        PhysicalKind::Chat => number("completion_tokens").or_else(|| number("output_tokens")),
    }?;
    let cached = usage
        .get("prompt_tokens_details")
        .or_else(|| usage.get("input_tokens_details"))
        .and_then(|v| v.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    Some(TokenUsage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        total_tokens: number("total_tokens").or_else(|| prompt.checked_add(completion)),
        cache_creation_input_tokens: number("cache_creation_input_tokens"),
        cache_read_input_tokens: number("cache_read_input_tokens").or(cached),
        source: TokenUsageSource::Reported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_partial_or_total_only_usage_keeps_liability() {
        for body in [
            br#"{}"#.as_slice(),
            br#"{"usage":{"total_tokens":9}}"#,
            br#"{"usage":{"prompt_tokens":8}}"#,
        ] {
            assert!(reported_usage(body, PhysicalKind::Chat).is_none());
        }
    }

    #[test]
    fn raw_usage_survives_invalid_semantic_response_and_preserves_cache() {
        let usage = reported_usage(br#"{"choices":null,"usage":{"input_tokens":8,"output_tokens":2,"input_tokens_details":{"cached_tokens":3}}}"#, PhysicalKind::Chat).expect("reported usage");
        assert_eq!(usage.prompt_tokens, Some(8));
        assert_eq!(usage.completion_tokens, Some(2));
        assert_eq!(usage.cache_read_input_tokens, Some(3));
        assert_eq!(usage.source, TokenUsageSource::Reported);
    }

    #[test]
    fn embedding_known_input_has_no_generated_output() {
        let usage =
            reported_usage(br#"{"usage":{"prompt_tokens":8}}"#, PhysicalKind::Embedding).expect("embedding usage");
        assert_eq!(usage.completion_tokens, Some(0));
        assert!(reported_usage(br#"{"usage":{"total_tokens":8}}"#, PhysicalKind::Embedding).is_none());
    }

    #[test]
    fn contextual_local_refusal_remains_typed() {
        let error = anyhow::anyhow!("budget unavailable")
            .context(AccountingRefusal)
            .context("nested fallback");
        assert!(is_local_refusal(&error));
    }
}

/// Opaque snapshots of supported concrete transport targets. Never Debug or
/// serialize: credentials and endpoints are borrowed only by exact matching.
pub struct AccountingTargets {
    targets: Vec<AccountingTarget>,
}
struct AccountingTarget {
    provider: Option<String>,
    model: String,
    endpoint: String,
    credential: Option<String>,
    kind: PhysicalKind,
}
impl AccountingTargets {
    pub(super) fn chat(model: &str, endpoints: Vec<String>, credential: Option<&str>) -> Self {
        Self {
            targets: endpoints
                .into_iter()
                .map(|endpoint| AccountingTarget {
                    provider: None,
                    model: model.to_owned(),
                    endpoint,
                    credential: credential.map(str::to_owned),
                    kind: PhysicalKind::Chat,
                })
                .collect(),
        }
    }
    pub(crate) fn embedding(model: &str, endpoint: String, credential: &str) -> Self {
        Self {
            targets: vec![AccountingTarget {
                provider: None,
                model: model.to_owned(),
                endpoint,
                credential: Some(credential.to_owned()),
                kind: PhysicalKind::Embedding,
            }],
        }
    }
    pub(super) const fn empty() -> Self {
        Self { targets: Vec::new() }
    }
    pub(super) fn append_member(&mut self, mut member: Self, provider: &str) {
        for target in &mut member.targets {
            if target.provider.is_none() {
                target.provider = Some(provider.to_owned());
            }
        }
        self.targets.append(&mut member.targets);
    }
    pub(super) fn nonempty(self) -> Option<Self> {
        (!self.targets.is_empty()).then_some(self)
    }

    /// The caller obtained this descriptor from its own verified PMM factory
    /// result; the default label is that selected PMM identity, never wire data.
    /// Multiple distinct matching identities are ambiguous and refused.
    pub(crate) fn match_request<'a>(
        &'a self,
        request: &PhysicalRequest<'_>,
        default_provider: &'a str,
    ) -> Option<(&'a str, &'a str)> {
        let mut selected = None;
        for target in &self.targets {
            if target.kind != request.kind
                || target.model != request.model
                || target.endpoint != request.endpoint
                || target.credential.as_deref() != request.credential
            {
                continue;
            }
            let identity = (
                target.provider.as_deref().unwrap_or(default_provider),
                target.model.as_str(),
            );
            if selected.is_some_and(|previous| previous != identity) {
                return None;
            }
            selected = Some(identity);
        }
        selected
    }
}
