//! Persisted approval adapter, borrowed for the duration of one call.
use super::run_call::ApprovalLedgerPort;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::execution::{ApprovalStrategy, ToolApprovalDecision, ToolApprovalRequest};

pub(crate) struct LedgerApprovalStrategy<'a> {
    pub(crate) port: ApprovalLedgerPort<'a>,
    pub(crate) envelope: RuntimeEnvelope,
    pub(crate) catalog: &'a crate::tools::capability::catalog::CapabilityCatalog,
}

#[async_trait::async_trait]
impl ApprovalStrategy for LedgerApprovalStrategy<'_> {
    async fn resolve(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let result = async {
            anyhow::ensure!(request.context.envelope == self.envelope, "approval envelope changed");
            let entry = self
                .catalog
                .entry_for_public_name(&request.descriptor.public_name)
                .ok_or_else(|| anyhow::anyhow!("approval capability unavailable"))?;
            anyhow::ensure!(
                entry.root == request.descriptor.backend_name,
                "approval backend changed"
            );
            let pending = self
                .port
                .record_pending_entry(&self.envelope, entry, &request.command.arguments)
                .await?;
            // Claim is atomic and precedes backend dispatch.
            let grant = self
                .port
                .claim_if_approved(
                    self.envelope.owner_id.as_deref().unwrap_or(""),
                    &self.envelope.session_key,
                    pending.authority(),
                )
                .await?;
            Ok::<_, anyhow::Error>((
                pending.authority().call_id(),
                grant
                    .map(|grant| {
                        serde_json::to_value(crate::security::policy::ApprovalGrant::from_verified_v2(
                            &request.descriptor.public_name,
                            "approval_ledger",
                            grant,
                        ))
                    })
                    .transpose()?,
            ))
        }
        .await;
        match result {
            Ok((_, Some(grant))) => ToolApprovalDecision::Approved {
                runtime_approval_granted: true,
                runtime_grant: Some(grant),
            },
            Ok((id, None)) => ToolApprovalDecision::Cancelled {
                reason: format!("awaiting approval: {id}"),
            },
            Err(error) => ToolApprovalDecision::Denied {
                reason: error.to_string(),
            },
        }
    }
}

/// Shared pinned authority for execution contexts. Cloning shares keys; it never copies secrets.
#[derive(Clone)]
pub(crate) struct ExecutionApprovalLedger {
    pub(crate) memory: std::sync::Arc<dyn crate::memory::Memory>,
    pub(crate) state_key: std::sync::Arc<crate::decision::DecisionKey>,
    pub(crate) workspace: String,
    pub(crate) catalog: std::sync::Arc<crate::tools::capability::catalog::CapabilityCatalog>,
}
impl std::fmt::Debug for ExecutionApprovalLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionApprovalLedger").finish_non_exhaustive()
    }
}
impl PartialEq for ExecutionApprovalLedger {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.memory, &other.memory)
            && std::sync::Arc::ptr_eq(&self.state_key, &other.state_key)
            && std::sync::Arc::ptr_eq(&self.catalog, &other.catalog)
            && self.workspace == other.workspace
    }
}
impl Eq for ExecutionApprovalLedger {}
impl ExecutionApprovalLedger {
    pub(crate) async fn resolve(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let keyring = match crate::acl::approval_grant::WitnessKeyring::global() {
            Ok(keyring) => keyring,
            Err(error) => {
                return ToolApprovalDecision::Denied {
                    reason: error.to_string(),
                };
            }
        };
        LedgerApprovalStrategy {
            port: ApprovalLedgerPort {
                memory: &self.memory,
                state_key: &self.state_key,
                workspace: &self.workspace,
                keyring,
            },
            envelope: request.context.envelope.clone(),
            catalog: &self.catalog,
        }
        .resolve(request)
        .await
    }
}
