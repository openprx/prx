//! One provider adapter for the existing sealed physical receipt. Callers
//! retain their own source, purpose and transport-binding checks.
use super::OwnedReceipt;
use crate::providers::attempt_accounting::{AttemptReceipt, PhysicalAttemptIdentity, sealed};
struct ProviderReceipt(OwnedReceipt);
impl OwnedReceipt {
    pub(in crate::runtime) fn into_provider_receipt(self) -> Box<dyn AttemptReceipt> {
        Box::new(ProviderReceipt(self))
    }
}
impl sealed::Receipt for ProviderReceipt {}
#[async_trait::async_trait]
impl AttemptReceipt for ProviderReceipt {
    fn deadline(&self) -> std::time::Instant {
        self.0.deadline
    }
    fn physical_identity(&self) -> Option<PhysicalAttemptIdentity<'_>> {
        Some(self.0.physical_identity())
    }
    fn retain_response_chunk(&self, bytes: usize) -> anyhow::Result<()> {
        self.0.retain_response_chunk(bytes)
    }
    async fn settle(self: Box<Self>, usage: crate::llm::route_decision::TokenUsage) -> anyhow::Result<()> {
        Box::new(self.0).settle_provider_usage(usage).map_err(Into::into)
    }
}
