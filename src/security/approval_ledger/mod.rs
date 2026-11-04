//! Durable approval lifecycle over the existing MemoryFabric event ledger.
//! Runtime admission checks remain in the adapter; persisted authentication,
//! review arbitration and metadata reads belong here.
mod grant;
pub(crate) mod keys;
pub(crate) mod pending;
pub(crate) mod resolution;
mod resume;
pub(crate) use resume::ResumeRequestScope;
mod review;
#[cfg(test)]
mod tests;

use crate::acl::approval_grant::{DecisionApprovalBinding, RiskLevel, Subject};
use crate::causal_tree::store::CausalDecisionStore;
use crate::decision::DecisionKey;
use crate::memory::Memory;
use pending::{PendingRecord, VerifiedPendingDecisionApproval};
use std::sync::Arc;

pub(crate) struct GrantScope<'a> {
    pub(crate) subject: &'a Subject,
    pub(crate) binding: &'a DecisionApprovalBinding,
    pub(crate) op_id: &'a str,
    pub(crate) risk: RiskLevel,
}

/// Runtime admission checks remain outside the durable storage lifecycle.
#[async_trait::async_trait]
pub(crate) trait ApprovalRuntime: Send + Sync {
    fn check_live_parity(&self) -> anyhow::Result<()>;
    async fn verify_pending_console_file_operation(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<()>;
    fn open_pending_call(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<crate::tools::ToolExecutionCommand>;
    fn verify_h2_review_requirement(
        &self,
        pending: &VerifiedPendingDecisionApproval,
        approved: bool,
        stamp: Option<&crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<()>;

    fn retire_pending(&self, record: &PendingRecord);
}

/// A view of the pinned authority; constructing it grants no execution rights.
pub(crate) struct ApprovalLedger<'a> {
    pub(crate) memory: &'a Arc<dyn Memory>,
    pub(crate) state_key: &'a DecisionKey,
    pub(crate) store: &'a CausalDecisionStore,
    pub(crate) workspace: String,
    pub(crate) catalog_generation: [u8; 32],
    pub(crate) policy_version: &'a str,
    pub(crate) enabled: bool,
    pub(crate) port: &'a dyn ApprovalRuntime,
}
