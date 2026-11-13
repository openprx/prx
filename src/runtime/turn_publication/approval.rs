//! The pinned publication's approval runtime for historical decision-approval
//! resume (R54). R26-HOLDER: the impls stay on the service until Part 2
//! deletes it, but live with the publication builders.
use std::sync::Arc;

use crate::acl::approval_grant::RiskLevel;
use crate::memory::{Memory, MessageEvent};
use crate::runtime::turn_decision::TurnDecisionService;
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::approval_ledger::historical::{self, HistoricalApprovalRuntime};
use crate::security::approval_ledger::pending::{PendingRecord, VerifiedPendingDecisionApproval};
use crate::security::approval_ledger::{ApprovalLedger, ApprovalRuntime};
use crate::tools::capability::catalog::CapabilityCatalog;
use crate::tools::execution::ToolExecutionService;
use crate::tools::owner_filesystem::OwnerFilesystem;

#[async_trait::async_trait]
impl HistoricalApprovalRuntime for TurnDecisionService {
    fn approval_ledger(&self) -> ApprovalLedger<'_> {
        ApprovalLedger {
            memory: &self.memory,
            state_key: &self.state_key,
            store: &self.store,
            workspace: self.generation.effective.workspace_dir.to_string_lossy().into_owned(),
            workspace_dir: &self.generation.effective.workspace_dir,
            catalog_generation: self.catalog.generation().0,
            policy_version: &self.policy_version,
            enabled: self.recordable && self.durable_key && self.generation.effective.causal_tree.enabled,
            port: self,
        }
    }

    fn memory(&self) -> &Arc<dyn Memory> {
        &self.memory
    }

    fn catalog(&self) -> &CapabilityCatalog {
        &self.catalog
    }

    fn execution(&self) -> &ToolExecutionService {
        &self.execution
    }

    fn policy_version(&self) -> &str {
        &self.policy_version
    }

    fn service_id(&self) -> uuid::Uuid {
        self.service_id
    }

    fn resolve_turn_model(&self, proof: &dyn TurnSourceProof) -> anyhow::Result<()> {
        self.resolve_agent_model(proof)?;
        Ok(())
    }

    fn check_shell_path_arguments(&self, command: &str) -> anyhow::Result<()> {
        Self::check_shell_path_arguments(self, command)
    }

    async fn pending_console_filesystem(&self, source: &MessageEvent) -> anyhow::Result<Option<Arc<OwnerFilesystem>>> {
        Self::pending_console_filesystem(self, source).await
    }

    async fn scoped_file_operation(
        &self,
        filesystem: Option<&Arc<OwnerFilesystem>>,
        tool: &str,
        path: &str,
    ) -> anyhow::Result<(String, RiskLevel)> {
        Self::scoped_file_operation(self, filesystem, tool, path).await
    }
}

#[async_trait::async_trait]
impl ApprovalRuntime for TurnDecisionService {
    fn check_live_parity(&self) -> anyhow::Result<()> {
        Ok(Self::check_live_parity(self)?)
    }
    async fn verify_pending_console_file_operation(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<()> {
        historical::verify_pending_console_file_operation(self, pending).await
    }
    fn open_pending_call(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<crate::tools::ToolExecutionCommand> {
        Self::open_pending_call(self, pending)
    }
    fn verify_h2_review_requirement(
        &self,
        pending: &VerifiedPendingDecisionApproval,
        approved: bool,
        stamp: Option<&crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<()> {
        Self::verify_h2_review_requirement(self, pending, approved, stamp)
    }

    fn retire_pending(&self, record: &PendingRecord) {
        self.runtime.recursive_allowances().retire_pending(
            &record.subject.workspace_id,
            &record.subject.owner_id,
            record.subject.session_key.as_deref().unwrap_or_default(),
            &record.run_id,
        );
    }
}
