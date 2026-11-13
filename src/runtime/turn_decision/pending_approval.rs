//! Adapter of the pinned publication for the historical decision-approval
//! API, which lives in `security::approval_ledger::historical` (R54). The
//! service only supplies its pinned live checks and the fresh resume turn.
mod h2;
mod sealed_call;
use super::{TurnDecisionContext, TurnDecisionService};
use crate::security::approval_ledger::historical::{self, AdmittedDecisionResume, ResumeTurn};
use crate::security::approval_ledger::pending::{
    DecisionApprovalReceipt, DecisionApprovalView, DecisionRevocationReceipt, PendingApprovalPage,
    VerifiedPendingDecisionApproval,
};
use crate::security::decision_id::DecisionId;
use std::sync::Arc;

/// Historical approval API on the service; bodies live in
/// `security::approval_ledger::historical` and these delegate until Part 2.
impl TurnDecisionService {
    pub(crate) async fn inspect_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<DecisionApprovalView> {
        historical::inspect_pending_decision_approval(self, owner, session, decision_id).await
    }

    pub(crate) async fn list_pending_decision_approvals(
        &self,
        owner: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<PendingApprovalPage> {
        historical::list_pending_decision_approvals(self, owner, after_id, limit).await
    }

    pub(crate) async fn review_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        approved: bool,
    ) -> anyhow::Result<DecisionApprovalReceipt> {
        historical::review_pending_decision_approval(self, owner, session, decision_id, approved).await
    }

    async fn review_pending_with_local(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        approved: bool,
        local_owner: Option<crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<DecisionApprovalReceipt> {
        historical::review_pending_with_local(self, owner, session, decision_id, approved, local_owner).await
    }

    pub(crate) async fn validate_pending_decision_resume(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<()> {
        historical::validate_pending_decision_resume(self, owner, session, decision_id).await
    }

    pub(crate) async fn revoke_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<DecisionRevocationReceipt> {
        historical::revoke_pending_decision_approval(self, owner, session, decision_id).await
    }

    pub(crate) async fn load_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<VerifiedPendingDecisionApproval> {
        historical::load_pending_decision_approval(self, owner, session, decision_id).await
    }
}

/// The fresh resume turn: its runtime context holds the admitted resume, and
/// its own decision chain is re-read through the opening service.
pub(super) struct DecisionResumeTurn {
    service: Arc<TurnDecisionService>,
    context: Arc<TurnDecisionContext>,
}

impl DecisionResumeTurn {
    pub(super) const fn new(service: Arc<TurnDecisionService>, context: Arc<TurnDecisionContext>) -> Self {
        Self { service, context }
    }
}

#[async_trait::async_trait]
impl ResumeTurn for DecisionResumeTurn {
    fn resume(&self) -> Option<&AdmittedDecisionResume> {
        self.context.runtime().resume.as_deref()
    }

    fn service_id(&self) -> uuid::Uuid {
        self.context.service_id
    }

    fn decision_id(&self) -> DecisionId {
        self.context.decision_id()
    }

    fn execution_active(&self) -> bool {
        !self.context.execution_closed.load(std::sync::atomic::Ordering::Acquire)
            && self
                .context
                .execution_trace
                .get()
                .is_some_and(|trace| !trace.is_sealed())
    }

    async fn validate_durable_chain(&self) -> anyhow::Result<()> {
        self.service
            .settlement_ref(Arc::clone(&self.context))?
            .validate_durable_chain()
            .await
    }
}
