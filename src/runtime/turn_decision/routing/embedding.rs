//! Paid embedding used only after the selected, persisted Router history read.
#[cfg(test)]
use crate::decision::budget::DecisionAttemptReceipt;
#[cfg(test)]
use crate::decision::{DecisionKey, KeyDomain};
#[cfg(test)]
use crate::llm::route_decision::TokenUsage;
#[cfg(test)]
use crate::providers::attempt_accounting::{self, AttemptAccounting, AttemptReceipt, PhysicalKind, PhysicalRequest};
#[cfg(test)]
use crate::runtime::decision_accounting::{AccountingScope, OwnedReceipt, PhysicalEstimate};
#[cfg(test)]
use anyhow::ensure;
#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
use std::{sync::Arc, time::Instant};
#[cfg(test)]
use tokio_util::sync::CancellationToken;

#[cfg(test)]
struct RouterEmbeddingAccounting {
    scope: Arc<AccountingScope>,
    key: Arc<DecisionKey>,
    targets: crate::providers::AccountingTargets,
    provider: String,
    model: String,
    domain: [u8; 32],
    query: [u8; 32],
    deadline: Instant,
}
#[cfg(test)]
impl attempt_accounting::sealed::AccountingOwner for RouterEmbeddingAccounting {}
#[cfg(test)]
#[async_trait]
impl AttemptAccounting for RouterEmbeddingAccounting {
    fn deadline(&self) -> Instant {
        self.deadline
    }
    async fn begin(&self, request: PhysicalRequest<'_>) -> anyhow::Result<Box<dyn AttemptReceipt>> {
        ensure!(
            request.kind() == PhysicalKind::Embedding,
            "Router accounting only admits embedding"
        );
        let (provider, model) = self
            .targets
            .match_request(&request, &self.provider)
            .ok_or_else(|| anyhow::anyhow!("Router embedding target changed"))?;
        ensure!(model == self.model, "Router embedding model changed");
        let binding = self.key.hmac(
            KeyDomain::TerminalClaim,
            &[
                b"router-embedding-attempt-v1",
                &self.domain,
                &self.query,
                provider.as_bytes(),
                model.as_bytes(),
                request.endpoint().as_bytes(),
                request.credential().unwrap_or("").as_bytes(),
                request.body(),
            ],
        );
        let request_id = uuid::Uuid::new_v4().to_string();
        let receipt = self
            .scope
            .begin_physical(PhysicalEstimate {
                purpose: "router_embedding",
                provider,
                model,
                body: request.body(),
                transport_binding: &binding,
                request_id: &request_id,
                deadline: self.deadline,
                remaining_attempts: 1,
            })
            .await?;
        Ok(Box::new(RouterEmbeddingReceipt {
            receipt: Box::new(receipt),
            deadline: self.deadline,
        }))
    }
}
#[cfg(test)]
struct RouterEmbeddingReceipt {
    receipt: Box<OwnedReceipt>,
    deadline: Instant,
}
#[cfg(test)]
impl attempt_accounting::sealed::Receipt for RouterEmbeddingReceipt {}
#[cfg(test)]
#[async_trait]
impl AttemptReceipt for RouterEmbeddingReceipt {
    fn deadline(&self) -> Instant {
        self.deadline.min(self.receipt.deadline())
    }
    fn retain_response_chunk(&self, bytes: usize) -> anyhow::Result<()> {
        self.receipt.retain_response_chunk(bytes).map_err(Into::into)
    }
    async fn settle(self: Box<Self>, usage: TokenUsage) -> anyhow::Result<()> {
        self.receipt.settle_provider_usage(usage).map_err(Into::into)
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn embed_selected(
    configured: crate::memory::ConfiguredPaidEmbedding,
    scope: Arc<AccountingScope>,
    key: Arc<DecisionKey>,
    domain: [u8; 32],
    query: [u8; 32],
    text: &str,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> anyhow::Result<Vec<f32>> {
    ensure!(
        !cancellation.is_cancelled() && crate::causal_tree::logical_clock::deadline_now(deadline) < deadline,
        "Router embedding cancelled or expired"
    );
    let targets = configured
        .embedder
        .accounting_targets()
        .ok_or_else(|| anyhow::anyhow!("Router paid embedding descriptor missing"))?;
    let accounting = Arc::new(RouterEmbeddingAccounting {
        scope,
        key,
        targets,
        provider: configured.provider,
        model: configured.model,
        domain,
        query,
        deadline,
    });
    let texts = [text];
    let mut vectors = tokio::select! {
        biased;
        _ = cancellation.cancelled() => anyhow::bail!("Router embedding cancelled"),
        result = attempt_accounting::embed(configured.embedder.as_ref(), &texts, accounting) => result?,
    };
    ensure!(vectors.len() == 1, "Router embedding response cardinality differs");
    let vector = vectors
        .pop()
        .ok_or_else(|| anyhow::anyhow!("Router embedding response missing"))?;
    ensure!(
        vector.len() == configured.dimensions && vector.iter().all(|value| value.is_finite()),
        "Router embedding shape differs"
    );
    ensure!(
        !cancellation.is_cancelled() && crate::causal_tree::logical_clock::deadline_now(deadline) < deadline,
        "Router embedding cancelled or expired"
    );
    Ok(vector)
}
