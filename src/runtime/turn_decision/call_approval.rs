//! Run-keyed approval access using the service's pinned authority.
use super::TurnDecisionService;
use crate::acl::approval_grant::WitnessKeyring;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::approval_ledger::{run_call::ApprovalLedgerPort, strategy::LedgerApprovalStrategy};
impl TurnDecisionService {
    pub(crate) fn call_approval_strategy(
        &self,
        envelope: RuntimeEnvelope,
    ) -> anyhow::Result<LedgerApprovalStrategy<'_>> {
        anyhow::ensure!(
            self.recordable && self.durable_key && self.generation.effective.causal_tree.enabled,
            "call approvals are disabled"
        );
        anyhow::ensure!(
            envelope.workspace_id == self.generation.effective.workspace_dir.to_string_lossy(),
            "call approval workspace changed"
        );
        Ok(LedgerApprovalStrategy {
            port: ApprovalLedgerPort {
                memory: &self.memory,
                state_key: &self.state_key,
                workspace: self
                    .generation
                    .effective
                    .workspace_dir
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("approval workspace is not UTF-8"))?,
                keyring: WitnessKeyring::global()?,
            },
            envelope,
            catalog: &self.catalog,
        })
    }
}
